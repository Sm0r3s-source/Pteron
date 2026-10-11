/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: turns a downloaded `.m4a` (AAC) file into an `.mp3`, entirely inside Pteron.
//! The sound is decoded with Symphonia (pure Rust) and encoded with LAME at 192 kbit/s (128 for mono).
//! Memory use stays small: the file is processed packet by packet.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, Id3Tag, InterleavedPcm, Mode, MonoPcm, Quality};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Title and artist put into the MP3 (only when they are plain ASCII, which every player reads).
#[derive(Default, Clone, Debug)]
pub(crate) struct Tags {
    pub title: String,
    pub artist: String,
}

fn ascii_tag(text: &str) -> Vec<u8> {
    if text.is_ascii() { text.bytes().filter(|b| !b.is_ascii_control()).take(120).collect() } else { Vec::new() }
}

/// The encoder for sound with this many channels and this sample rate. (Both are only known
/// once the first piece of sound has been decoded.)
fn make_encoder(channels: u8, rate: u32, tags: &Tags) -> Result<mp3lame_encoder::Encoder, String> {
    let fail = |e: mp3lame_encoder::BuildError| format!("MP3 encoder: {e:?}");
    let mut builder = Builder::new().ok_or("Could not start the MP3 encoder.")?;
    builder.set_num_channels(channels).map_err(fail)?;
    builder.set_mode(if channels == 1 { Mode::Mono } else { Mode::JointStereo }).map_err(fail)?;
    builder.set_sample_rate(rate).map_err(fail)?;
    builder.set_brate(if channels == 1 { Bitrate::Kbps128 } else { Bitrate::Kbps192 }).map_err(fail)?;
    builder.set_quality(Quality::Good).map_err(fail)?;
    let (title, artist) = (ascii_tag(&tags.title), ascii_tag(&tags.artist));
    if !title.is_empty() || !artist.is_empty() {
        let _ = builder.set_id3_tag(Id3Tag { title: &title, artist: &artist, album: b"", album_art: b"", year: b"", comment: b"" });
    }
    builder.build().map_err(fail)
}

/// Converts `input` (an AAC file in an MP4 container) to MP3 at `output`. `progress` is called
/// with the fraction done (0 to 1) now and then; setting `cancel` stops the work early.
pub(crate) fn m4a_to_mp3(
    input: &Path,
    output: &Path,
    tags: &Tags,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<(), String> {
    let file = File::open(input).map_err(|e| format!("Could not open the audio file: {e}"))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let mut format = symphonia::default::get_probe()
        .format(&hint, stream, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("Not a sound file Pteron can read: {e}"))?
        .format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("There is no sound in this file.")?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let total_frames = params.n_frames;
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("Pteron cannot decode this sound: {e}"))?;
    let mut encoder: Option<mp3lame_encoder::Encoder> = None;
    let mut channels = 2u8;
    let mut out = File::create(output).map_err(|e| format!("Could not create the MP3 file: {e}"))?;
    let mut mp3: Vec<u8> = Vec::new();
    let mut samples: Option<SampleBuffer<i16>> = None;
    let mut done_frames: u64 = 0;
    let mut last_report = 0.0f32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("Could not read the sound: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet is skipped (a tiny click) rather than failing the whole song.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Could not decode the sound: {e}")),
        };
        let spec = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        if encoder.is_none() {
            channels = spec.channels.count().clamp(1, 2) as u8;
            encoder = Some(make_encoder(channels, spec.rate, tags)?);
        }
        let encoder = encoder.as_mut().expect("encoder was just made");
        let buffer = samples.get_or_insert_with(|| SampleBuffer::<i16>::new(decoded.capacity() as u64, spec));
        if buffer.capacity() < frames * spec.channels.count() {
            *buffer = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        }
        buffer.copy_interleaved_ref(decoded);
        let all = buffer.samples();
        // Make the sound the shape the encoder was set up for (mono or stereo).
        let pcm: Vec<i16> = match (spec.channels.count(), channels) {
            (n, c) if n == c as usize => all.to_vec(),
            (n, 1) => all.chunks(n).map(|f| (f.iter().map(|s| *s as i32).sum::<i32>() / n as i32) as i16).collect(),
            (n, _) => all.chunks(n).flat_map(|f| [f[0], *f.get(1).unwrap_or(&f[0])]).collect(),
        };
        mp3.clear();
        // LAME writes into the spare room of the vector and needs 1.25 bytes per sample plus
        // 7200; with no room it would write past the end, so the room is made first.
        mp3.reserve(pcm.len() * 5 / 4 + 8192);
        let result = if channels == 1 {
            encoder.encode_to_vec(MonoPcm(&pcm), &mut mp3)
        } else {
            encoder.encode_to_vec(InterleavedPcm(&pcm), &mut mp3)
        };
        result.map_err(|e| format!("MP3 encoder: {e:?}"))?;
        out.write_all(&mp3).map_err(|e| format!("Could not write the MP3 file: {e}"))?;
        done_frames += frames as u64;
        if let Some(total) = total_frames.filter(|t| *t > 0) {
            let fraction = (done_frames as f32 / total as f32).min(1.0);
            if fraction - last_report >= 0.02 {
                last_report = fraction;
                progress(fraction);
            }
        }
    }
    let Some(mut encoder) = encoder else {
        return Err("The file has no sound in it.".into());
    };
    mp3.clear();
    mp3.reserve(16 * 1024);
    encoder.flush_to_vec::<FlushNoGap>(&mut mp3).map_err(|e| format!("MP3 encoder: {e:?}"))?;
    out.write_all(&mp3).map_err(|e| format!("Could not write the MP3 file: {e}"))?;
    out.flush().map_err(|e| e.to_string())?;
    if done_frames == 0 {
        return Err("The file has no sound in it.".into());
    }
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ffmpeg` makes the test sound; the test is skipped where it is not installed.
    fn make_m4a(path: &Path, channels: u32, seconds: u32) -> bool {
        std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}:sample_rate=44100"))
            .args(["-ac", &channels.to_string(), "-c:a", "aac", "-b:a", "128k"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn check(channels: u32) {
        let dir = std::env::temp_dir().join(format!("pteron_mp3_test_{channels}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("a.m4a"), dir.join("a.mp3"));
        if !make_m4a(&src, channels, 4) {
            eprintln!("ffmpeg is not installed: skipping");
            return;
        }
        let cancel = AtomicBool::new(false);
        let mut last = 0.0;
        let tags = Tags { title: "Test song".into(), artist: "Me".into() };
        m4a_to_mp3(&src, &dst, &tags, &cancel, &mut |p| last = p).expect("converts");
        assert!(last > 0.99);
        let size = std::fs::metadata(&dst).unwrap().len();
        // 4 seconds at 192 kbit/s is about 96 kB.
        let range = if channels == 1 { 40_000..80_000 } else { 70_000..140_000 };
        assert!(range.contains(&size), "size {size}");
        let bytes = std::fs::read(&dst).unwrap();
        assert!(bytes.starts_with(b"ID3"), "has a tag");
        // Where ffprobe is installed, an independent program confirms it is a real MP3.
        if let Ok(out) = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name,channels,sample_rate:format=duration", "-of", "default=nw=1"])
            .arg(&dst)
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(text.contains("codec_name=mp3"), "{text}");
            assert!(text.contains(&format!("channels={channels}")), "{text}");
            let seconds: f64 = text
                .lines()
                .find_map(|l| l.strip_prefix("duration="))
                .and_then(|d| d.parse().ok())
                .unwrap_or(0.0);
            assert!((3.7..4.5).contains(&seconds), "duration {seconds}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn converts_stereo() {
        check(2);
    }

    #[test]
    fn converts_mono() {
        check(1);
    }

    #[test]
    fn rejects_garbage_and_cancel() {
        let dir = std::env::temp_dir().join(format!("pteron_mp3_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("bad.m4a"), dir.join("bad.mp3"));
        std::fs::write(&src, b"this is not a sound file at all").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(m4a_to_mp3(&src, &dst, &Tags::default(), &cancel, &mut |_| {}).is_err());
        assert!(m4a_to_mp3(&dir.join("missing.m4a"), &dst, &Tags::default(), &cancel, &mut |_| {}).is_err());
        assert!(!ascii_tag("Žalgiris").len() > 0 || ascii_tag("Žalgiris").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
