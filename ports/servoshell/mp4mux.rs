/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: merge a separate video stream and audio stream (both fragmented MP4, as YouTube
//! serves them) into ONE ordinary MP4 file that a plain `<video>` element can play and seek in.
//!
//! GStreamer (Servo's media engine) cannot seek in a *fragmented* MP4 when the data arrives the
//! way Servo feeds it, so the result has to be a classic MP4: one `moov` with complete sample
//! tables, then one `mdat` with all the media data. The file is *virtual*:
//!
//! ```text
//! ftyp | moov (video + audio sample tables) | mdat header | chunk | chunk | chunk | ...
//! ```
//!
//! * The sample tables come from the small `moof` header of every segment (a few KB each), which
//!   Pteron downloads once, in parallel, when a video is opened.
//! * Every chunk is the raw sample data of one segment, copied unchanged and fetched from
//!   YouTube only when the player asks for those bytes (one range request per chunk).
//! * The exact size of the whole file is known up front, so byte-range requests (seeking) work.
//!
//! Everything here is pure computation on byte slices.

use std::fmt;

#[derive(Debug, PartialEq)]
pub(crate) struct MuxError(pub String);

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

type Res<T> = Result<T, MuxError>;

fn err<T>(message: &str) -> Res<T> {
    Err(MuxError(message.to_string()))
}

// ---------------------------------------------------------------- reading boxes

#[derive(Clone, Copy, Debug)]
struct Atom {
    typ: [u8; 4],
    start: usize,
    /// First byte after the box header.
    body: usize,
    end: usize,
}

fn be32(data: &[u8], at: usize) -> Res<u32> {
    match data.get(at..at + 4) {
        Some(b) => Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
        None => err("truncated box"),
    }
}

fn be64(data: &[u8], at: usize) -> Res<u64> {
    match data.get(at..at + 8) {
        Some(b) => Ok(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])),
        None => err("truncated box"),
    }
}

fn set32(data: &mut [u8], at: usize, value: u32) -> Res<()> {
    match data.get_mut(at..at + 4) {
        Some(b) => {
            b.copy_from_slice(&value.to_be_bytes());
            Ok(())
        },
        None => err("truncated box"),
    }
}

fn set64(data: &mut [u8], at: usize, value: u64) -> Res<()> {
    match data.get_mut(at..at + 8) {
        Some(b) => {
            b.copy_from_slice(&value.to_be_bytes());
            Ok(())
        },
        None => err("truncated box"),
    }
}

fn atoms(data: &[u8], from: usize, to: usize) -> Res<Vec<Atom>> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos + 8 <= to {
        let size32 = be32(data, pos)? as u64;
        let typ = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let (size, body) = match size32 {
            1 => (be64(data, pos + 8)?, pos + 16),
            0 => ((to - pos) as u64, pos + 8),
            n => (n, pos + 8),
        };
        if size < (body - pos) as u64 || pos as u64 + size > to as u64 {
            return err("damaged box size");
        }
        out.push(Atom {
            typ,
            start: pos,
            body,
            end: pos + size as usize,
        });
        pos += size as usize;
    }
    Ok(out)
}

fn children(data: &[u8], parent: &Atom) -> Res<Vec<Atom>> {
    atoms(data, parent.body, parent.end)
}

fn find(list: &[Atom], typ: &[u8; 4]) -> Option<Atom> {
    list.iter().find(|a| &a.typ == typ).copied()
}

fn slice(data: &[u8], a: &Atom) -> Vec<u8> {
    data[a.start..a.end].to_vec()
}

// ---------------------------------------------------------------- sidx

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SidxRef {
    pub size: u32,
    pub duration: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sidx {
    pub timescale: u32,
    pub earliest: u64,
    /// Distance from the end of the sidx box to the first referenced byte.
    pub first_offset: u64,
    pub refs: Vec<SidxRef>,
}

fn parse_sidx(data: &[u8], a: &Atom) -> Res<Sidx> {
    let b = a.body;
    let version = *data.get(b).unwrap_or(&0);
    let timescale = be32(data, b + 8)?;
    let (earliest, first_offset, mut at) = if version == 0 {
        (be32(data, b + 12)? as u64, be32(data, b + 16)? as u64, b + 20)
    } else {
        (be64(data, b + 12)?, be64(data, b + 20)?, b + 28)
    };
    at += 2; // reserved
    let count = match data.get(at..at + 2) {
        Some(c) => u16::from_be_bytes([c[0], c[1]]) as usize,
        None => return err("truncated sidx"),
    };
    at += 2;
    let mut refs = Vec::with_capacity(count);
    for _ in 0..count {
        let word = be32(data, at)?;
        if word >> 31 != 0 {
            return err("hierarchical sidx is not supported");
        }
        refs.push(SidxRef {
            size: word & 0x7fff_ffff,
            duration: be32(data, at + 4)?,
        });
        at += 12;
    }
    if timescale == 0 || refs.is_empty() {
        return err("empty sidx");
    }
    Ok(Sidx {
        timescale,
        earliest,
        first_offset,
        refs,
    })
}

/// The parts of a stream's first bytes (everything up to the end of its sidx).
#[derive(Clone, Debug)]
pub(crate) struct Init {
    pub ftyp: Vec<u8>,
    pub moov: Vec<u8>,
    pub sidx: Sidx,
    /// Byte position in the source stream where the first segment starts.
    pub data_start: u64,
}

pub(crate) fn parse_init(bytes: &[u8]) -> Res<Init> {
    let top = atoms(bytes, 0, bytes.len())?;
    let ftyp = find(&top, b"ftyp").ok_or(MuxError("no ftyp".into()))?;
    let moov = find(&top, b"moov").ok_or(MuxError("no moov".into()))?;
    let sidx_atom = find(&top, b"sidx").ok_or(MuxError("no sidx (stream is not indexed)".into()))?;
    let sidx = parse_sidx(bytes, &sidx_atom)?;
    let data_start = sidx_atom.end as u64 + sidx.first_offset;
    Ok(Init {
        ftyp: slice(bytes, &ftyp),
        moov: slice(bytes, &moov),
        sidx,
        data_start,
    })
}

// ---------------------------------------------------------------- sample tables

#[derive(Clone, Copy, Debug, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

fn trex_defaults(moov: &[u8]) -> Res<Defaults> {
    let top = atoms(moov, 0, moov.len())?;
    let moov_atom = find(&top, b"moov").ok_or(MuxError("no moov".into()))?;
    let kids = children(moov, &moov_atom)?;
    let mvex = find(&kids, b"mvex").ok_or(MuxError("not a fragmented stream (no mvex)".into()))?;
    let trex = find(&children(moov, &mvex)?, b"trex").ok_or(MuxError("no trex".into()))?;
    Ok(Defaults {
        duration: be32(moov, trex.body + 12)?,
        size: be32(moov, trex.body + 16)?,
        flags: be32(moov, trex.body + 20)?,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    pub cto: i32,
}

/// A run of sample data that sits contiguously in the source stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Chunk {
    pub src_start: u64,
    pub len: u64,
    pub samples: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Segment {
    pub start_secs: f64,
    pub samples: Vec<Sample>,
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TrackData {
    pub segments: Vec<Segment>,
}

/// Where the `moof` box of a segment ends, judging from the first bytes of the segment.
/// `None` when the bytes do not reach a `moof` header.
fn moof_end(buf: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    loop {
        if pos + 8 > buf.len() {
            return None;
        }
        let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        let typ = &buf[pos + 4..pos + 8];
        let size = match size32 {
            1 => {
                if pos + 16 > buf.len() {
                    return None;
                }
                u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().ok()?) as usize
            },
            0 => return None,
            n => n,
        };
        if typ == b"moof" {
            return Some(pos + size);
        }
        pos += size;
    }
}

/// Samples and chunks described by the `moof` at the start of `buf` (which must cover the whole
/// `moof`). `seg_start` is where the segment begins in the source stream.
fn parse_segment(buf: &[u8], seg_start: u64, defaults: &Defaults) -> Res<(Vec<Sample>, Vec<Chunk>)> {
    let end = match moof_end(buf) {
        Some(end) if end <= buf.len() => end,
        _ => return err("segment header is cut off"),
    };
    let top = atoms(buf, 0, end)?;
    let mut samples = Vec::new();
    let mut chunks = Vec::new();
    for moof in top.iter().filter(|a| &a.typ == b"moof") {
        for traf in children(buf, moof)?.iter().filter(|a| &a.typ == b"traf") {
            let kids = children(buf, traf)?;
            let tfhd = find(&kids, b"tfhd").ok_or(MuxError("no tfhd".into()))?;
            let tf_flags = be32(buf, tfhd.body)? & 0x00ff_ffff;
            if tf_flags & 0x1 != 0 {
                return err("segment uses absolute data offsets (not supported)");
            }
            let mut d = *defaults;
            let mut p = tfhd.body + 8;
            if tf_flags & 0x2 != 0 {
                p += 4;
            }
            if tf_flags & 0x8 != 0 {
                d.duration = be32(buf, p)?;
                p += 4;
            }
            if tf_flags & 0x10 != 0 {
                d.size = be32(buf, p)?;
                p += 4;
            }
            if tf_flags & 0x20 != 0 {
                d.flags = be32(buf, p)?;
            }
            for trun in kids.iter().filter(|a| &a.typ == b"trun") {
                let vf = be32(buf, trun.body)?;
                let flags = vf & 0x00ff_ffff;
                let count = be32(buf, trun.body + 4)?;
                let mut p = trun.body + 8;
                let data_offset = if flags & 0x1 != 0 {
                    let v = be32(buf, p)? as i32;
                    p += 4;
                    Some(v)
                } else {
                    None
                };
                let first_flags = if flags & 0x4 != 0 {
                    let v = be32(buf, p)?;
                    p += 4;
                    Some(v)
                } else {
                    None
                };
                let mut total = 0u64;
                for i in 0..count {
                    let mut s = Sample {
                        duration: d.duration,
                        size: d.size,
                        flags: d.flags,
                        cto: 0,
                    };
                    if flags & 0x100 != 0 {
                        s.duration = be32(buf, p)?;
                        p += 4;
                    }
                    if flags & 0x200 != 0 {
                        s.size = be32(buf, p)?;
                        p += 4;
                    }
                    if flags & 0x400 != 0 {
                        s.flags = be32(buf, p)?;
                        p += 4;
                    } else if i == 0 {
                        if let Some(f) = first_flags {
                            s.flags = f;
                        }
                    }
                    if flags & 0x800 != 0 {
                        s.cto = be32(buf, p)? as i32;
                        p += 4;
                    }
                    if s.size == 0 {
                        return err("a sample has no size");
                    }
                    total += s.size as u64;
                    samples.push(s);
                }
                let start = match data_offset {
                    Some(off) => (seg_start as i64 + moof.start as i64 + off as i64) as u64,
                    None => seg_start + moof.end as u64 + 8,
                };
                chunks.push(Chunk {
                    src_start: start,
                    len: total,
                    samples: count,
                });
            }
        }
    }
    if samples.is_empty() {
        return err("segment has no samples");
    }
    Ok((samples, chunks))
}

/// How many bytes at the start of each segment are fetched first; nearly always enough for the
/// whole `moof`.
const HEAD_GUESS: u64 = 6 * 1024;

pub(crate) type BatchFetch<'a> = &'a dyn Fn(&[(u64, u64)]) -> Result<Vec<Vec<u8>>, String>;

/// Download the `moof` header of every segment of a stream and read the sample tables from them.
pub(crate) fn load_track(init: &Init, fetch: BatchFetch, limit_secs: Option<f64>) -> Res<TrackData> {
    let defaults = trex_defaults(&init.moov)?;
    let mut segs: Vec<(u64, u64, f64)> = Vec::with_capacity(init.sidx.refs.len());
    let mut pos = init.data_start;
    let mut t = init.sidx.earliest as f64;
    for r in &init.sidx.refs {
        segs.push((pos, r.size as u64, t / init.sidx.timescale as f64));
        pos += r.size as u64;
        t += r.duration as f64;
    }
    if let Some(limit) = limit_secs {
        let keep = segs.iter().take_while(|(_, _, start)| *start < limit).count().max(1);
        segs.truncate(keep);
    }
    let first: Vec<(u64, u64)> = segs.iter().map(|(s, size, _)| (*s, (*size).min(HEAD_GUESS))).collect();
    let mut heads = fetch(&first).map_err(MuxError)?;
    if heads.len() != segs.len() {
        return err("the server returned the wrong number of pieces");
    }
    // Headers longer than the guess are fetched again with the exact length.
    let mut again: Vec<(usize, (u64, u64))> = Vec::new();
    for (i, (start, size, _)) in segs.iter().enumerate() {
        match moof_end(&heads[i]) {
            Some(end) if end <= heads[i].len() => {},
            Some(end) => again.push((i, (*start, (end as u64).min(*size)))),
            None => again.push((i, (*start, (*size).min(256 * 1024)))),
        }
    }
    if !again.is_empty() {
        let ranges: Vec<(u64, u64)> = again.iter().map(|(_, r)| *r).collect();
        let more = fetch(&ranges).map_err(MuxError)?;
        if more.len() != again.len() {
            return err("the server returned the wrong number of pieces");
        }
        for ((i, _), bytes) in again.iter().zip(more) {
            heads[*i] = bytes;
        }
    }
    let mut segments = Vec::with_capacity(segs.len());
    for (i, (start, _, secs)) in segs.iter().enumerate() {
        let (samples, chunks) = parse_segment(&heads[i], *start, &defaults)?;
        segments.push(Segment {
            start_secs: *secs,
            samples,
            chunks,
        });
    }
    Ok(TrackData { segments })
}

// ---------------------------------------------------------------- writing boxes

fn boxed(typ: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let size: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(typ);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

fn fullbox(typ: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let head = [version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    boxed(typ, &[&head, payload])
}

/// The box `typ` with the children of `parent`, each kept, replaced or removed by `f`.
fn rebuild(
    data: &[u8],
    parent: &Atom,
    typ: &[u8; 4],
    f: &mut dyn FnMut(&Atom) -> Res<Option<Vec<u8>>>,
) -> Res<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    for child in children(data, parent)? {
        match f(&child)? {
            Some(replacement) => body.extend_from_slice(&replacement),
            None => body.extend_from_slice(&data[child.start..child.end]),
        }
    }
    Ok(boxed(typ, &[&body]))
}

fn rle<T: PartialEq + Copy>(items: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for item in items {
        match out.last_mut() {
            Some((count, last)) if *last == item => *count += 1,
            _ => out.push((1, item)),
        }
    }
    out
}

struct Tables {
    stts: Vec<u8>,
    ctts: Option<Vec<u8>>,
    stss: Option<Vec<u8>>,
    stsz: Vec<u8>,
    stsc: Vec<u8>,
    chunk_count: u32,
    total_duration: u64,
    min_cto: i32,
}

fn sample_tables(track: &TrackData, video: bool) -> Tables {
    let samples: Vec<&Sample> = track.segments.iter().flat_map(|s| s.samples.iter()).collect();
    let mut stts_payload = Vec::new();
    let stts = rle(samples.iter().map(|s| s.duration));
    stts_payload.extend_from_slice(&(stts.len() as u32).to_be_bytes());
    for (count, delta) in &stts {
        stts_payload.extend_from_slice(&count.to_be_bytes());
        stts_payload.extend_from_slice(&delta.to_be_bytes());
    }
    let any_cto = samples.iter().any(|s| s.cto != 0);
    let negative = samples.iter().any(|s| s.cto < 0);
    let min_cto = samples.iter().map(|s| s.cto).min().unwrap_or(0);
    let ctts = any_cto.then(|| {
        let runs = rle(samples.iter().map(|s| s.cto));
        let mut p = Vec::new();
        p.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (count, offset) in &runs {
            p.extend_from_slice(&count.to_be_bytes());
            p.extend_from_slice(&offset.to_be_bytes());
        }
        fullbox(b"ctts", if negative { 1 } else { 0 }, 0, &p)
    });
    // sample_is_non_sync_sample is bit 16 of the sample flags
    let sync: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.flags & 0x0001_0000 == 0)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let stss = (video && sync.len() != samples.len()).then(|| {
        let mut p = Vec::new();
        p.extend_from_slice(&(sync.len() as u32).to_be_bytes());
        for n in &sync {
            p.extend_from_slice(&n.to_be_bytes());
        }
        fullbox(b"stss", 0, 0, &p)
    });
    let mut stsz_payload = Vec::new();
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in &samples {
        stsz_payload.extend_from_slice(&s.size.to_be_bytes());
    }
    let chunk_samples: Vec<u32> = track
        .segments
        .iter()
        .flat_map(|s| s.chunks.iter().map(|c| c.samples))
        .collect();
    let mut stsc_payload = Vec::new();
    let mut entries: Vec<(u32, u32)> = Vec::new(); // (first_chunk, samples_per_chunk)
    for (i, n) in chunk_samples.iter().enumerate() {
        if entries.last().map(|(_, last)| last) != Some(n) {
            entries.push((i as u32 + 1, *n));
        }
    }
    stsc_payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (first, n) in &entries {
        stsc_payload.extend_from_slice(&first.to_be_bytes());
        stsc_payload.extend_from_slice(&n.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
    }
    Tables {
        stts: fullbox(b"stts", 0, 0, &stts_payload),
        ctts,
        stss,
        stsz: fullbox(b"stsz", 0, 0, &stsz_payload),
        stsc: fullbox(b"stsc", 0, 0, &stsc_payload),
        chunk_count: chunk_samples.len() as u32,
        total_duration: samples.iter().map(|s| s.duration as u64).sum(),
        min_cto,
    }
}

fn chunk_offsets_box(offsets: &[u64], wide: bool) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        if wide {
            p.extend_from_slice(&o.to_be_bytes());
        } else {
            p.extend_from_slice(&(*o as u32).to_be_bytes());
        }
    }
    fullbox(if wide { b"co64" } else { b"stco" }, 0, 0, &p)
}

fn dur_in(media_duration: u64, from_scale: u32, to_scale: u32) -> u64 {
    (media_duration as u128 * to_scale as u128 / from_scale.max(1) as u128) as u64
}

/// One finished `trak`: the source track with new sample tables, durations and track id.
fn build_trak(
    init: &Init,
    id: u32,
    tables: &Tables,
    offsets: &[u64],
    wide: bool,
    movie_scale: u32,
    video: bool,
) -> Res<(Vec<u8>, u64)> {
    let moov = &init.moov;
    let top = atoms(moov, 0, moov.len())?;
    let moov_atom = find(&top, b"moov").ok_or(MuxError("no moov".into()))?;
    let trak = find(&children(moov, &moov_atom)?, b"trak").ok_or(MuxError("no trak".into()))?;
    let mut media_scale = 1u32;
    let mut movie_duration = 0u64;
    let trak_bytes = rebuild(moov, &trak, b"trak", &mut |child| match &child.typ {
        b"tkhd" => {
            let mut b = slice(moov, child);
            let version = b[8];
            let (id_at, dur_at) = if version == 1 { (8 + 20, 8 + 28) } else { (8 + 12, 8 + 20) };
            set32(&mut b, id_at, id)?;
            // the media timescale is only known inside mdia; it is patched below
            let _ = dur_at;
            Ok(Some(b))
        },
        b"edts" => Ok(Some(Vec::new())),
        b"mdia" => {
            let mdia = *child;
            let out = rebuild(moov, &mdia, b"mdia", &mut |m| match &m.typ {
                b"mdhd" => {
                    let mut b = slice(moov, m);
                    let version = b[8];
                    let (ts_at, dur_at) = if version == 1 { (8 + 20, 8 + 24) } else { (8 + 12, 8 + 16) };
                    media_scale = be32(&b, ts_at)?;
                    if version == 1 {
                        set64(&mut b, dur_at, tables.total_duration)?;
                    } else {
                        set32(&mut b, dur_at, tables.total_duration.min(u32::MAX as u64) as u32)?;
                    }
                    Ok(Some(b))
                },
                b"minf" => {
                    let minf = *m;
                    let out = rebuild(moov, &minf, b"minf", &mut |n| match &n.typ {
                        b"stbl" => {
                            let stbl = *n;
                            let kids = children(moov, &stbl)?;
                            let stsd = find(&kids, b"stsd").ok_or(MuxError("no stsd".into()))?;
                            let mut parts: Vec<Vec<u8>> = vec![slice(moov, &stsd), tables.stts.clone()];
                            if let Some(c) = &tables.ctts {
                                parts.push(c.clone());
                            }
                            if let Some(s) = &tables.stss {
                                parts.push(s.clone());
                            }
                            parts.push(tables.stsc.clone());
                            parts.push(tables.stsz.clone());
                            parts.push(chunk_offsets_box(offsets, wide));
                            let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
                            Ok(Some(boxed(b"stbl", &refs)))
                        },
                        _ => Ok(None),
                    })?;
                    Ok(Some(out))
                },
                _ => Ok(None),
            })?;
            Ok(Some(out))
        },
        _ => Ok(None),
    })?;
    movie_duration = dur_in(tables.total_duration, media_scale, movie_scale).max(movie_duration);

    // Now that the durations are known, patch tkhd and add the edit list.
    let mut out = trak_bytes;
    let root = Atom {
        typ: *b"trak",
        start: 0,
        body: 8,
        end: out.len(),
    };
    let kids = children(&out, &root)?;
    let tkhd = find(&kids, b"tkhd").ok_or(MuxError("no tkhd".into()))?;
    let version = out[tkhd.body];
    if version == 1 {
        set64(&mut out, tkhd.body + 28, movie_duration)?;
    } else {
        set32(&mut out, tkhd.body + 20, movie_duration.min(u32::MAX as u64) as u32)?;
    }
    if video && tables.min_cto > 0 {
        // shift presentation so the first picture shows at time 0
        let seg_dur = dur_in(
            tables.total_duration + tables.min_cto as u64,
            media_scale,
            movie_scale,
        );
        let mut elst = Vec::new();
        elst.extend_from_slice(&1u32.to_be_bytes());
        elst.extend_from_slice(&(seg_dur.min(u32::MAX as u64) as u32).to_be_bytes());
        elst.extend_from_slice(&(tables.min_cto as u32).to_be_bytes());
        elst.extend_from_slice(&[0, 1, 0, 0]);
        let edts = boxed(b"edts", &[&fullbox(b"elst", 0, 0, &elst)]);
        // edts goes right after tkhd
        let at = tkhd.end;
        out.splice(at..at, edts);
        let new_size = out.len() as u32;
        set32(&mut out, 0, new_size)?;
    }
    Ok((out, movie_duration))
}

// ---------------------------------------------------------------- layout

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Video,
    Audio,
}

#[derive(Clone, Debug)]
pub(crate) struct Piece {
    pub kind: Kind,
    /// Where the bytes live in their source stream.
    pub src_start: u64,
    pub len: u64,
    /// Where they sit in the merged file.
    pub out_start: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Layout {
    /// ftyp + moov + mdat header, built in memory.
    pub prefix: Vec<u8>,
    pub pieces: Vec<Piece>,
    pub total: u64,
}

struct Placed {
    kind: Kind,
    chunk: Chunk,
}

fn ftyp() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"isom");
    p.extend_from_slice(&512u32.to_be_bytes());
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        p.extend_from_slice(brand);
    }
    boxed(b"ftyp", &[&p])
}

fn mvhd_for(init: &Init, duration: u64) -> Res<(Vec<u8>, u32)> {
    let moov = &init.moov;
    let top = atoms(moov, 0, moov.len())?;
    let moov_atom = find(&top, b"moov").ok_or(MuxError("no moov".into()))?;
    let mvhd = find(&children(moov, &moov_atom)?, b"mvhd").ok_or(MuxError("no mvhd".into()))?;
    let mut b = slice(moov, &mvhd);
    let version = b[8];
    let (ts_at, dur_at) = if version == 1 { (8 + 20, 8 + 24) } else { (8 + 12, 8 + 16) };
    let scale = be32(&b, ts_at)?;
    if version == 1 {
        set64(&mut b, dur_at, duration)?;
    } else {
        set32(&mut b, dur_at, duration.min(u32::MAX as u64) as u32)?;
    }
    let len = b.len();
    set32(&mut b, len - 4, 3)?; // next_track_ID
    Ok((b, scale))
}

fn movie_scale(init: &Init) -> Res<u32> {
    Ok(mvhd_for(init, 0)?.1)
}

pub(crate) fn build_layout(video: &Init, vdata: &TrackData, audio: &Init, adata: &TrackData) -> Res<Layout> {
    // 1. interleave the chunks by time: all of a video segment, then the audio that starts
    //    before the next video segment
    let mut order: Vec<Placed> = Vec::new();
    let mut ai = 0usize;
    let push_audio = |ai: &mut usize, order: &mut Vec<Placed>, limit: f64| {
        while *ai < adata.segments.len() && adata.segments[*ai].start_secs < limit {
            for c in &adata.segments[*ai].chunks {
                order.push(Placed { kind: Kind::Audio, chunk: *c });
            }
            *ai += 1;
        }
    };
    let first_video = vdata.segments.first().map(|s| s.start_secs).unwrap_or(0.0);
    push_audio(&mut ai, &mut order, first_video);
    for (i, seg) in vdata.segments.iter().enumerate() {
        for c in &seg.chunks {
            order.push(Placed { kind: Kind::Video, chunk: *c });
        }
        let next = vdata.segments.get(i + 1).map(|s| s.start_secs).unwrap_or(f64::INFINITY);
        push_audio(&mut ai, &mut order, next);
    }
    let payload: u64 = order.iter().map(|p| p.chunk.len).sum();
    let wide = payload > 0xF000_0000;

    // 2. tables
    let vt = sample_tables(vdata, true);
    let at = sample_tables(adata, false);
    let mscale = movie_scale(video)?;

    // 3. the moov is built twice: the chunk offsets depend on how long the moov is, but not the
    //    other way round (they have a fixed width)
    let mut moov_len = 0usize;
    let mut result: Option<(Vec<u8>, Vec<u64>)> = None; // (prefix, relative offsets)
    for pass in 0..2 {
        let prefix_len = if pass == 0 { 0 } else { ftyp().len() + moov_len + 16 } as u64;
        let mut rel = 0u64;
        let mut voff = Vec::new();
        let mut aoff = Vec::new();
        for p in &order {
            match p.kind {
                Kind::Video => voff.push(prefix_len + rel),
                Kind::Audio => aoff.push(prefix_len + rel),
            }
            rel += p.chunk.len;
        }
        let (vtrak, vdur) = build_trak(video, 1, &vt, &voff, wide, mscale, true)?;
        let (atrak, adur) = build_trak(audio, 2, &at, &aoff, wide, mscale, false)?;
        let (mvhd, _) = mvhd_for(video, vdur.max(adur))?;
        let moov = boxed(b"moov", &[&mvhd, &vtrak, &atrak]);
        moov_len = moov.len();
        if pass == 1 {
            let mut prefix = ftyp();
            prefix.extend_from_slice(&moov);
            // mdat with a 64-bit size, so the payload can be any length
            prefix.extend_from_slice(&1u32.to_be_bytes());
            prefix.extend_from_slice(b"mdat");
            prefix.extend_from_slice(&(16 + payload).to_be_bytes());
            let mut all = voff;
            all.extend(aoff);
            result = Some((prefix, all));
        }
    }
    let (prefix, _) = result.ok_or(MuxError("could not build the file".into()))?;
    let mut pieces = Vec::with_capacity(order.len());
    let mut out = prefix.len() as u64;
    for p in order {
        pieces.push(Piece {
            kind: p.kind,
            src_start: p.chunk.src_start,
            len: p.chunk.len,
            out_start: out,
        });
        out += p.chunk.len;
    }
    let _ = (vt.chunk_count, at.chunk_count);
    Ok(Layout {
        prefix,
        pieces,
        total: out,
    })
}

/// A file with only the audio track (for listening to music without downloading any video).
pub(crate) fn build_audio_layout(audio: &Init, adata: &TrackData) -> Res<Layout> {
    let mut order: Vec<Placed> = Vec::new();
    for seg in &adata.segments {
        for c in &seg.chunks {
            order.push(Placed { kind: Kind::Audio, chunk: *c });
        }
    }
    let payload: u64 = order.iter().map(|p| p.chunk.len).sum();
    let wide = payload > 0xF000_0000;
    let at = sample_tables(adata, false);
    let mscale = movie_scale(audio)?;
    let mut moov_len = 0usize;
    let mut prefix_out: Option<Vec<u8>> = None;
    for pass in 0..2 {
        let prefix_len = if pass == 0 { 0 } else { ftyp().len() + moov_len + 16 } as u64;
        let mut rel = 0u64;
        let mut aoff = Vec::with_capacity(order.len());
        for p in &order {
            aoff.push(prefix_len + rel);
            rel += p.chunk.len;
        }
        let (atrak, adur) = build_trak(audio, 1, &at, &aoff, wide, mscale, false)?;
        let (mvhd, _) = mvhd_for(audio, adur)?;
        let moov = boxed(b"moov", &[&mvhd, &atrak]);
        moov_len = moov.len();
        if pass == 1 {
            let mut prefix = ftyp();
            prefix.extend_from_slice(&moov);
            prefix.extend_from_slice(&1u32.to_be_bytes());
            prefix.extend_from_slice(b"mdat");
            prefix.extend_from_slice(&(16 + payload).to_be_bytes());
            prefix_out = Some(prefix);
        }
    }
    let prefix = prefix_out.ok_or(MuxError("could not build the file".into()))?;
    let mut pieces = Vec::with_capacity(order.len());
    let mut out = prefix.len() as u64;
    for p in order {
        pieces.push(Piece { kind: p.kind, src_start: p.chunk.src_start, len: p.chunk.len, out_start: out });
        out += p.chunk.len;
    }
    Ok(Layout { prefix, pieces, total: out })
}

pub(crate) type PieceFetch<'a> = &'a dyn Fn(Kind, u64, u64) -> Result<Vec<u8>, String>;

impl Layout {
    /// Index of the piece that holds output byte `offset`, if it is past the prefix.
    pub(crate) fn piece_at(&self, offset: u64) -> Option<usize> {
        if offset < self.prefix.len() as u64 || offset >= self.total {
            return None;
        }
        match self.pieces.binary_search_by(|p| p.out_start.cmp(&offset)) {
            Ok(i) => Some(i),
            Err(i) => Some(i - 1),
        }
    }

    pub(crate) fn load_piece(&self, index: usize, fetch: PieceFetch) -> Res<Vec<u8>> {
        let piece = &self.pieces[index];
        let bytes = fetch(piece.kind, piece.src_start, piece.len).map_err(MuxError)?;
        if bytes.len() as u64 != piece.len {
            return err("the server returned a different amount of data than expected");
        }
        Ok(bytes)
    }

    /// Bytes `start..=end` of the merged file, calling `sink` with consecutive chunks.
    /// Stops early (without error) when `sink` returns false.
    pub(crate) fn read_range(
        &self,
        start: u64,
        end: u64,
        fetch: PieceFetch,
        sink: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Res<()> {
        let end = end.min(self.total.saturating_sub(1));
        let mut pos = start;
        let prefix_len = self.prefix.len() as u64;
        if pos < prefix_len && pos <= end {
            let upto = end.min(prefix_len - 1);
            if !sink(&self.prefix[pos as usize..=upto as usize]) {
                return Ok(());
            }
            pos = upto + 1;
        }
        while pos <= end {
            let Some(i) = self.piece_at(pos) else { break };
            let bytes = self.load_piece(i, fetch)?;
            let piece_start = self.pieces[i].out_start;
            let from = (pos - piece_start) as usize;
            let to = ((end - piece_start) as usize).min(bytes.len() - 1);
            if !sink(&bytes[from..=to]) {
                return Ok(());
            }
            pos = piece_start + to as u64 + 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn ffmpeg(args: &[&str]) -> bool {
        Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Make a YouTube-like pair of fragmented, indexed mp4 files.
    fn make_samples(dir: &Path, seconds: u32) -> (Vec<u8>, Vec<u8>) {
        std::fs::create_dir_all(dir).unwrap();
        let v = dir.join("v.mp4");
        let a = dir.join("a.mp4");
        let flags = "frag_keyframe+empty_moov+default_base_moof+global_sidx";
        assert!(ffmpeg(&[
            "-f", "lavfi", "-i", &format!("testsrc=duration={seconds}:size=320x240:rate=25"),
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "50", "-keyint_min", "50",
            "-sc_threshold", "0", "-bf", "2", "-an", "-movflags", flags, v.to_str().unwrap(),
        ]));
        assert!(ffmpeg(&[
            "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}"),
            "-c:a", "aac", "-vn", "-frag_duration", "2300000", "-movflags",
            "empty_moov+default_base_moof+global_sidx", a.to_str().unwrap(),
        ]));
        (std::fs::read(v).unwrap(), std::fs::read(a).unwrap())
    }

    fn init_of(file: &[u8]) -> Init {
        let top = atoms(file, 0, file.len()).unwrap();
        let sidx = find(&top, b"sidx").unwrap();
        parse_init(&file[..sidx.end]).unwrap()
    }

    pub(crate) fn batch<'a>(src: &'a [u8]) -> impl Fn(&[(u64, u64)]) -> Result<Vec<Vec<u8>>, String> + 'a {
        move |ranges| {
            Ok(ranges
                .iter()
                .map(|(s, l)| {
                    let s = *s as usize;
                    src[s..(s + *l as usize).min(src.len())].to_vec()
                })
                .collect())
        }
    }

    pub(crate) fn build(v: &[u8], a: &[u8]) -> Layout {
        let vi = init_of(v);
        let ai = init_of(a);
        let vd = load_track(&vi, &batch(v), None).unwrap();
        let ad = load_track(&ai, &batch(a), None).unwrap();
        build_layout(&vi, &vd, &ai, &ad).unwrap()
    }

    pub(crate) fn assemble(layout: &Layout, v: &[u8], a: &[u8]) -> Vec<u8> {
        let fetch = |kind: Kind, start: u64, len: u64| -> Result<Vec<u8>, String> {
            let src = if kind == Kind::Video { v } else { a };
            Ok(src[start as usize..(start + len) as usize].to_vec())
        };
        let mut out = Vec::new();
        layout.read_range(0, layout.total - 1, &fetch, &mut |c| { out.extend_from_slice(c); true }).unwrap();
        out
    }

    fn probe(path: &Path) -> String {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=index,codec_type,codec_name,duration",
                   "-show_entries", "format=duration", "-of", "default=nw=1"])
            .arg(path).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn merges_real_video_and_audio_into_one_playable_file() {
        if !have_ffmpeg() { return; }
        let dir = std::env::temp_dir().join(format!("pteron_mux_{}", std::process::id()));
        let (v, a) = make_samples(&dir, 20);
        let layout = build(&v, &a);
        let piece_bytes: u64 = layout.pieces.iter().map(|p| p.len).sum();
        assert_eq!(layout.total, layout.prefix.len() as u64 + piece_bytes);
        let merged = assemble(&layout, &v, &a);
        assert_eq!(merged.len() as u64, layout.total);
        let out = dir.join("merged.mp4");
        std::fs::write(&out, &merged).unwrap();
        let info = probe(&out);
        println!("{info}");
        assert!(info.contains("codec_type=video") && info.contains("codec_type=audio"), "{info}");
        assert!(info.contains("codec_name=h264") && info.contains("codec_name=aac"), "{info}");
        let decode = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]).output().unwrap();
        assert!(decode.status.success());
        assert!(decode.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&decode.stderr));
        let frames = Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "default=nw=1:nk=1"])
            .arg(&out).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&frames.stdout).trim(), "500");
        // duration close to 20 s
        let d: f64 = info.lines().find_map(|l| l.strip_prefix("duration=").and_then(|x| x.parse().ok())).unwrap();
        assert!((d - 20.0).abs() < 0.5, "{d}");
        // seeking in the middle works (ffmpeg input seek) and decodes cleanly
        let seek = Command::new("ffmpeg").args(["-v", "error", "-ss", "12", "-i"]).arg(&out).args(["-t", "3", "-f", "null", "-"]).output().unwrap();
        assert!(seek.status.success() && seek.stderr.is_empty(), "{}", String::from_utf8_lossy(&seek.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_audio_only_file_plays() {
        if !have_ffmpeg() { return; }
        let dir = std::env::temp_dir().join(format!("pteron_mux_a_{}", std::process::id()));
        let (_v, a) = make_samples(&dir, 15);
        let ai = init_of(&a);
        let ad = load_track(&ai, &batch(&a), None).unwrap();
        let layout = build_audio_layout(&ai, &ad).unwrap();
        let fetch = |_kind: Kind, start: u64, len: u64| -> Result<Vec<u8>, String> {
            Ok(a[start as usize..(start + len) as usize].to_vec())
        };
        let mut out_bytes = Vec::new();
        layout.read_range(0, layout.total - 1, &fetch, &mut |c| { out_bytes.extend_from_slice(c); true }).unwrap();
        assert_eq!(out_bytes.len() as u64, layout.total);
        let out = dir.join("audio.mp4");
        std::fs::write(&out, &out_bytes).unwrap();
        let info = probe(&out);
        assert!(info.contains("codec_type=audio") && !info.contains("codec_type=video"), "{info}");
        let d: f64 = info.lines().find_map(|l| l.strip_prefix("duration=").and_then(|x| x.parse().ok())).unwrap();
        assert!((d - 15.0).abs() < 0.5, "{d}");
        let decode = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]).output().unwrap();
        assert!(decode.status.success() && decode.stderr.is_empty(), "{}", String::from_utf8_lossy(&decode.stderr));
        let seek = Command::new("ffmpeg").args(["-v", "error", "-ss", "9", "-i"]).arg(&out).args(["-t", "3", "-f", "null", "-"]).output().unwrap();
        assert!(seek.status.success() && seek.stderr.is_empty(), "{}", String::from_utf8_lossy(&seek.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_reads_match_the_whole_file() {
        if !have_ffmpeg() { return; }
        let dir = std::env::temp_dir().join(format!("pteron_mux_r_{}", std::process::id()));
        let (v, a) = make_samples(&dir, 10);
        let layout = build(&v, &a);
        let whole = assemble(&layout, &v, &a);
        let fetch = |kind: Kind, start: u64, len: u64| -> Result<Vec<u8>, String> {
            let src = if kind == Kind::Video { &v } else { &a };
            Ok(src[start as usize..(start + len) as usize].to_vec())
        };
        for (s, e) in [(0u64, 100u64), (50, 5000), (layout.prefix.len() as u64 - 3, layout.prefix.len() as u64 + 40), (whole.len() as u64 / 2, whole.len() as u64 / 2 + 9999), (whole.len() as u64 - 10, whole.len() as u64 - 1)] {
            let mut got = Vec::new();
            layout.read_range(s, e, &fetch, &mut |c| { got.extend_from_slice(c); true }).unwrap();
            assert_eq!(got, &whole[s as usize..=e as usize], "range {s}-{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_layout_plays_and_is_shorter() {
        if !have_ffmpeg() { return; }
        let dir = std::env::temp_dir().join(format!("pteron_mux_p_{}", std::process::id()));
        let (v, a) = make_samples(&dir, 40);
        let (vi, ai) = (init_of(&v), init_of(&a));
        let vd = load_track(&vi, &batch(&v), Some(12.0)).unwrap();
        let ad = load_track(&ai, &batch(&a), Some(12.0)).unwrap();
        assert!(vd.segments.len() < init_of(&v).sidx.refs.len());
        let layout = build_layout(&vi, &vd, &ai, &ad).unwrap();
        let out = dir.join("part.mp4");
        std::fs::write(&out, assemble(&layout, &v, &a)).unwrap();
        let info = probe(&out);
        let d: f64 = info.lines().find_map(|l| l.strip_prefix("duration=").and_then(|x| x.parse().ok())).unwrap();
        assert!(d > 10.0 && d < 20.0, "{info}");
        let decode = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]).output().unwrap();
        assert!(decode.status.success() && decode.stderr.is_empty(), "{}", String::from_utf8_lossy(&decode.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes a merged sample for manual playback tests:
    /// `PTERON_DUMP=/tmp/merged.mp4 PTERON_DUMP_SECONDS=300 cargo test dump_merged -- --nocapture`
    #[test]
    fn dump_merged() {
        let Ok(path) = std::env::var("PTERON_DUMP") else { return };
        let seconds: u32 = std::env::var("PTERON_DUMP_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let dir = std::env::temp_dir().join(format!("pteron_dump_{}", std::process::id()));
        let (v, a) = make_samples(&dir, seconds);
        let layout = build(&v, &a);
        std::fs::write(path, assemble(&layout, &v, &a)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
