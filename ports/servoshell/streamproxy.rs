/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: a tiny local web server (127.0.0.1 only) that turns YouTube's separate video and
//! audio streams into one MP4 file for a plain `<video>` element. See `mp4mux` for the format
//! work. This file does the networking:
//!
//! * `register` stores the stream addresses for a video and returns an unguessable token.
//! * `GET /v/<token>/<itag>.mp4` builds the merged file layout the first time (the stream
//!   headers plus the small `moof` header of every segment, downloaded in parallel), then
//!   answers byte-range requests by downloading one segment at a time (the next one is
//!   fetched while the current one is being sent).
//! * Memory use is a couple of segments (a few MB) however long the video is.
//!
//! Downloads from YouTube use `curl.exe` (part of Windows 10 and later), so Pteron needs no
//! extra libraries. Only `https://*.googlevideo.com/` addresses are ever fetched.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use log::{info, warn};

use crate::mp4mux::{Init, Kind, Layout, MuxError, build_layout, load_track, parse_init};

const MAX_SETS: usize = 24;
/// The "quality" number of the audio-only file of a track (Pteron Music).
pub(crate) const AUDIO_ONLY: u32 = 0;
const MAX_SESSIONS: usize = 8;
/// The first part of a video that is made ready at once (seconds), so playback can start
/// while the whole file layout is still being worked out.
pub(crate) const PARTIAL_SECS: f64 = 120.0;
const MAX_CONNECTIONS: usize = 12;
/// Same identity yt-dlp presents for this client.
pub(crate) const UPSTREAM_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StreamInfo {
    pub itag: u32,
    pub url: String,
    /// Last byte of the index (`sidx`) box; the first bytes of the stream up to here are
    /// the headers.
    pub index_end: u64,
    pub height: u32,
    /// Frames per second (30 when YouTube does not say).
    pub fps: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoSet {
    pub videos: Vec<StreamInfo>,
    pub audio: StreamInfo,
    pub duration_ms: u64,
}

pub(crate) type Fetcher = Arc<dyn Fn(&str, u64, u64) -> Result<Vec<u8>, String> + Send + Sync>;
/// Several `(start, len)` ranges of one address at once; results in the same order.
pub(crate) type BatchFetcher = Arc<dyn Fn(&str, &[(u64, u64)]) -> Result<Vec<Vec<u8>>, String> + Send + Sync>;

#[derive(Clone)]
struct Net {
    one: Fetcher,
    many: BatchFetcher,
}

/// How far ahead of the player the helper downloads (it keeps this much in memory).
const AHEAD_PIECES: usize = 5;
const AHEAD_BYTES: u64 = 32 * 1024 * 1024;

/// One downloaded (or downloading) piece of the merged file.
struct Slot {
    data: Mutex<Option<Result<Arc<Vec<u8>>, String>>>,
    ready: std::sync::Condvar,
}

struct Session {
    layout: Layout,
    video_url: String,
    audio_url: String,
    net: Net,
    slots: Mutex<std::collections::HashMap<usize, Arc<Slot>>>,
}

impl Session {
    fn download(&self, index: usize) -> Result<Vec<u8>, MuxError> {
        let fetch = |kind: Kind, start: u64, len: u64| -> Result<Vec<u8>, String> {
            let url = if kind == Kind::Video { &self.video_url } else { &self.audio_url };
            (self.net.one)(url, start, len)
        };
        self.layout.load_piece(index, &fetch)
    }

    /// Start downloading piece `index` in the background unless it already is.
    fn start(self: &Arc<Self>, index: usize) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get(&index) {
            return slot.clone();
        }
        let slot = Arc::new(Slot { data: Mutex::new(None), ready: std::sync::Condvar::new() });
        slots.insert(index, slot.clone());
        let (session, filler) = (self.clone(), slot.clone());
        let spawned = std::thread::Builder::new().name("pteron-piece".into()).spawn(move || {
            let result = session.download(index).map(Arc::new).map_err(|e| e.to_string());
            if result.is_err() {
                // a failed piece is asked for again next time
                session.slots.lock().unwrap().remove(&index);
            }
            *filler.data.lock().unwrap() = Some(result);
            filler.ready.notify_all();
        });
        if spawned.is_err() {
            slots.remove(&index);
            *slot.data.lock().unwrap() = Some(Err("could not start a download".into()));
        }
        slot
    }

    /// Wait for piece `index`.
    fn get(self: &Arc<Self>, index: usize) -> Result<Arc<Vec<u8>>, String> {
        let slot = self.start(index);
        let mut data = slot.data.lock().unwrap();
        while data.is_none() {
            data = slot.ready.wait(data).unwrap();
        }
        data.as_ref().unwrap().clone()
    }

    /// Keep downloading the pieces after `index` (up to a limit) and forget the ones the
    /// player has passed, so memory stays small.
    fn read_ahead(self: &Arc<Self>, index: usize) {
        let mut bytes = 0u64;
        let mut last = index;
        for next in index + 1..self.layout.pieces.len().min(index + 1 + AHEAD_PIECES) {
            bytes += self.layout.pieces[next].len;
            if bytes > AHEAD_BYTES {
                break;
            }
            self.start(next);
            last = next;
        }
        self.slots.lock().unwrap().retain(|i, _| *i + 2 > index && *i <= last + 2);
    }
}

#[derive(Default)]
struct State {
    sets: Vec<(String, Arc<VideoSet>)>,
    sessions: Vec<((String, u32, bool), Arc<Session>)>,
    /// Full layouts that are being built in the background.
    starting: Vec<(String, u32)>,
    /// The audio headers of a video are the same for every quality, so they are kept.
    audio: Vec<(String, Arc<(Init, crate::mp4mux::TrackData)>)>,
    /// Secret part of the address the pages use to ask for search suggestions.
    suggest_token: String,
}

pub(crate) struct Proxy {
    port: u16,
    state: Arc<Mutex<State>>,
    net: Net,
    /// Only one layout is built at a time, so two requests for the same video share the work.
    building: Arc<Mutex<()>>,
}

fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

impl Proxy {
    pub(crate) fn start(one: Fetcher, many: BatchFetcher) -> Result<Proxy, String> {
        let net = Net { one, many };
        let building = Arc::new(Mutex::new(()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let state = Arc::new(Mutex::new(State {
            suggest_token: random_token(),
            ..State::default()
        }));
        let thread_state = state.clone();
        let thread_net = net.clone();
        let thread_building = building.clone();
        let active = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("pteron-stream-proxy".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let mut stream = stream;
                        let _ = stream.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        continue;
                    }
                    let state = thread_state.clone();
                    let net = thread_net.clone();
                    let building = thread_building.clone();
                    let active = active.clone();
                    let spawned = std::thread::Builder::new()
                        .name("pteron-stream-conn".into())
                        .spawn({
                            let active = active.clone();
                            move || {
                                handle_connection(stream, &state, &net, &building);
                                active.fetch_sub(1, Ordering::SeqCst);
                            }
                        });
                    if spawned.is_err() {
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Proxy { port, state, net, building })
    }

    /// Remember the streams of one video. Returns the token for its URLs.
    pub(crate) fn register(&self, set: VideoSet) -> String {
        let token = random_token();
        let mut state = self.state.lock().unwrap();
        state.sets.push((token.clone(), Arc::new(set)));
        if state.sets.len() > MAX_SETS {
            let (old, _) = state.sets.remove(0);
            state.sessions.retain(|((t, _, _), _)| *t != old);
        }
        token
    }

    /// Build the file layout of one quality now, so the first play request is answered at once.
    pub(crate) fn prepare(&self, token: &str, itag: u32, partial: bool) -> Result<(), String> {
        session_for(&self.state, &self.net, &self.building, token, itag, partial).map(|_| ())
    }

    /// Address (without the `?q=...`) a page can use to get search suggestions.
    pub(crate) fn suggest_url(&self) -> String {
        format!("http://127.0.0.1:{}/s/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn ready_url(&self) -> String {
        format!("http://127.0.0.1:{}/w/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn memory_url(&self) -> String {
        format!("http://127.0.0.1:{}/o/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn search_more_url(&self) -> String {
        format!("http://127.0.0.1:{}/q/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn comments_url(&self) -> String {
        format!("http://127.0.0.1:{}/c/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn more_url(&self) -> String {
        format!("http://127.0.0.1:{}/m/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    /// Address the Music page sends its commands to (`?x=search:...`).
    pub(crate) fn music_url(&self) -> String {
        format!("http://127.0.0.1:{}/u/{}", self.port, self.state.lock().unwrap().suggest_token)
    }

    pub(crate) fn url(&self, token: &str, itag: u32) -> String {
        format!("http://127.0.0.1:{}/v/{token}/{itag}.mp4", self.port)
    }

    /// The first couple of minutes only; ready much sooner than the whole file.
    pub(crate) fn partial_url(&self, token: &str, itag: u32) -> String {
        format!("http://127.0.0.1:{}/v/{token}/{itag}p.mp4", self.port)
    }

    /// Address that starts building the whole file in the background and answers `1` once it
    /// is ready (`?k=<token>_<itag>`).
    pub(crate) fn full_url(&self) -> String {
        format!("http://127.0.0.1:{}/f/{}", self.port, self.state.lock().unwrap().suggest_token)
    }
}

// ---------------------------------------------------------------- HTTP

struct RequestHead {
    method: String,
    path: String,
    range: Option<String>,
}

fn read_head(stream: &mut TcpStream) -> Option<RequestHead> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let mut range = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            }
        }
    }
    Some(RequestHead { method, path, range })
}

fn respond(stream: &mut TcpStream, code: &str, extra: &str, length: u64) {
    let head = format!(
        "HTTP/1.1 {code}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\n\
         Access-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\
         {extra}Content-Length: {length}\r\n\r\n"
    );
    let _ = stream.write_all(head.as_bytes());
}

fn fail(stream: &mut TcpStream, code: &str) {
    respond(stream, code, "", 0);
}

/// Parse `bytes=a-b`, `bytes=a-`, `bytes=-n`. `None` means not satisfiable.
pub(crate) fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end: u64 = if b.is_empty() { total - 1 } else { b.parse().ok()? };
        (start, end.min(total - 1))
    };
    (start <= end && start < total).then_some((start, end))
}

/// `/v/<token>/<itag>.mp4` (whole file) or `/v/<token>/<itag>p.mp4` (first part only).
fn parse_path(path: &str) -> Option<(String, u32, bool)> {
    let rest = path.split('?').next()?.strip_prefix("/v/")?;
    let (token, file) = rest.split_once('/')?;
    let name = file.strip_suffix(".mp4")?;
    let (digits, partial) = match name.strip_suffix('p') {
        Some(d) => (d, true),
        None => (name, false),
    };
    let itag = digits.parse::<u32>().ok()?;
    (token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit())).then(|| (token.to_string(), itag, partial))
}

fn session_for(
    state: &Arc<Mutex<State>>,
    net: &Net,
    building: &Arc<Mutex<()>>,
    token: &str,
    itag: u32,
    partial: bool,
) -> Result<Arc<Session>, String> {
    let find = || {
        let guard = state.lock().unwrap();
        guard.sessions.iter().find(|((t, i, p), _)| t == token && *i == itag && *p == partial).map(|(_, s)| s.clone())
    };
    if let Some(s) = find() {
        return Ok(s);
    }
    // Whole files are built one at a time; the quick first part never waits for them.
    let _turn = if partial { None } else { Some(building.lock().unwrap_or_else(|e| e.into_inner())) };
    if let Some(s) = find() {
        return Ok(s);
    }
    let set = {
        let guard = state.lock().unwrap();
        guard.sets.iter().find(|(t, _)| t == token).map(|(_, s)| s.clone()).ok_or("unknown video")?
    };
    if itag == AUDIO_ONLY {
        // Pteron Music: only the audio track, no video is downloaded at all.
        let (audio_init, audio_data) = prepare_stream(&set.audio, net, None)?;
        let layout = crate::mp4mux::build_audio_layout(&audio_init, &audio_data).map_err(|e| e.to_string())?;
        crate::crashlog::note(&format!(
            "music helper: audio file ready, {} pieces, {} bytes",
            layout.pieces.len(),
            layout.total
        ));
        let session = Arc::new(Session {
            layout,
            video_url: set.audio.url.clone(),
            audio_url: set.audio.url.clone(),
            net: net.clone(),
            slots: Mutex::new(std::collections::HashMap::new()),
        });
        let mut guard = state.lock().unwrap();
        guard.sessions.push(((token.to_string(), itag, false), session.clone()));
        if guard.sessions.len() > MAX_SESSIONS {
            guard.sessions.remove(0);
        }
        return Ok(session);
    }
    let video = set.videos.iter().find(|v| v.itag == itag).ok_or("unknown quality")?.clone();
    // The two streams are independent: headers first, then the per-segment tables.
    let limit = partial.then_some(PARTIAL_SECS);
    let cached_audio = if partial {
        None
    } else {
        let guard = state.lock().unwrap();
        guard.audio.iter().find(|(t, _)| t == token).map(|(_, a)| a.clone())
    };
    let audio_job = match cached_audio {
        Some(a) => Err(a),
        None => {
            let audio_info = set.audio.clone();
            let audio_net = net.clone();
            Ok(std::thread::spawn(move || prepare_stream(&audio_info, &audio_net, limit)))
        },
    };
    let (video_init, video_data) = prepare_stream(&video, net, limit)?;
    let audio = match audio_job {
        Err(cached) => cached,
        Ok(job) => {
            let made = Arc::new(job.join().map_err(|_| "audio download failed".to_string())??);
            if !partial {
                let mut guard = state.lock().unwrap();
                guard.audio.push((token.to_string(), made.clone()));
                if guard.audio.len() > 4 {
                    guard.audio.remove(0);
                }
            }
            made
        },
    };
    let (audio_init, audio_data) = (&audio.0, &audio.1);
    let layout = build_layout(&video_init, &video_data, audio_init, audio_data).map_err(|e| e.to_string())?;
    info!(
        "Pteron stream: {} pieces, {} bytes for itag {itag}",
        layout.pieces.len(),
        layout.total
    );
    crate::crashlog::note(&format!(
        "video helper: itag {itag}{} ready, {} pieces, {} bytes in total",
        if partial { " (first part)" } else { "" },
        layout.pieces.len(),
        layout.total
    ));
    let session = Arc::new(Session {
        layout,
        video_url: video.url.clone(),
        audio_url: set.audio.url.clone(),
        net: net.clone(),
        slots: Mutex::new(std::collections::HashMap::new()),
    });
    let mut guard = state.lock().unwrap();
    guard.sessions.push(((token.to_string(), itag, partial), session.clone()));
    if guard.sessions.len() > MAX_SESSIONS {
        guard.sessions.remove(0);
    }
    Ok(session)
}

/// `1` when the whole-file layout of `token_itag` is ready; otherwise starts building it in
/// the background (once) and answers `0`.
fn full_status(state: &Arc<Mutex<State>>, net: &Net, building: &Arc<Mutex<()>>, key: &str) -> &'static str {
    let Some((token, itag)) = key.rsplit_once('_').and_then(|(t, i)| Some((t, i.parse::<u32>().ok()?))) else {
        return "0";
    };
    {
        let mut guard = state.lock().unwrap();
        if guard.sessions.iter().any(|((t, i, p), _)| t == token && *i == itag && !*p) {
            return "1";
        }
        if guard.starting.iter().any(|(t, i)| t == token && *i == itag) {
            return "0";
        }
        guard.starting.push((token.to_string(), itag));
    }
    let (state, net, building, token) = (state.clone(), net.clone(), building.clone(), token.to_string());
    std::thread::spawn(move || {
        let result = session_for(&state, &net, &building, &token, itag, false);
        if let Err(message) = &result {
            crate::crashlog::note(&format!("video helper: whole file for itag {itag} failed: {message}"));
        }
        // a failure may be tried again later
        state.lock().unwrap().starting.retain(|(t, i)| !(*t == token && *i == itag));
    });
    "0"
}

fn prepare_stream(info: &StreamInfo, net: &Net, limit: Option<f64>) -> Result<(Init, crate::mp4mux::TrackData), String> {
    let bytes = (net.one)(&info.url, 0, info.index_end + 1)?;
    let init = parse_init(&bytes).map_err(|e| e.to_string())?;
    let fetch = |ranges: &[(u64, u64)]| (net.many)(&info.url, ranges);
    let data = load_track(&init, &fetch, limit).map_err(|e| e.to_string())?;
    Ok((init, data))
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<State>>, net: &Net, building: &Arc<Mutex<()>>) {
    let Some(head) = read_head(&mut stream) else { return };
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    if head.method == "OPTIONS" {
        respond(&mut stream, "204 No Content", "Access-Control-Allow-Headers: range\r\n", 0);
        return;
    }
    if head.method != "GET" && head.method != "HEAD" {
        return fail(&mut stream, "405 Method Not Allowed");
    }
    let helper_token = state.lock().unwrap().suggest_token.clone();
    if let Some((kind, value)) = helper_request(&head.path, &helper_token) {
        let (content_type, body) = match kind {
            's' => ("application/json", serde_json::to_string(&suggestions(&value)).unwrap_or_else(|_| "[]".into())),
            'f' => ("text/plain", full_status(state, net, building, &value).to_string()),
            'w' => ("text/plain", if crate::youtube::is_ready(&value) { "1" } else { "0" }.to_string()),
            'o' => ("text/plain", crate::youtube::player_memory(&value)),
            'q' => ("application/json", crate::youtube::search_more_json(&value)),
            'c' => ("application/json", crate::youtube::comments_json(&value)),
            'u' => ("application/json", crate::music::command(&value)),
            _ => ("application/json", crate::youtube::more_json(&value)),
        };
        let head_text = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\n\
             Cache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head_text.as_bytes());
        if head.method == "GET" {
            let _ = stream.write_all(body.as_bytes());
        }
        return;
    }
    let Some((token, itag, partial)) = parse_path(&head.path) else {
        return fail(&mut stream, "404 Not Found");
    };
    let session = match session_for(state, net, building, &token, itag, partial) {
        Ok(s) => s,
        Err(message) => {
            warn!("Pteron stream: {message}");
            crate::crashlog::note(&format!("video helper: itag {itag} could not start: {message}"));
            return fail(&mut stream, "502 Bad Gateway");
        },
    };
    let total = session.layout.total;
    let (code, start, end, extra) = match head.range.as_deref() {
        None => ("200 OK", 0, total - 1, String::new()),
        Some(r) => match parse_range(r, total) {
            Some((s, e)) => ("206 Partial Content", s, e, format!("Content-Range: bytes {s}-{e}/{total}\r\n")),
            None => {
                return respond(
                    &mut stream,
                    "416 Range Not Satisfiable",
                    &format!("Content-Range: bytes */{total}\r\n"),
                    0,
                );
            },
        },
    };
    respond(&mut stream, code, &extra, end - start + 1);
    crate::crashlog::note(&format!("video helper: itag {itag} {} bytes {start}-{end} of {total}", head.method));
    if head.method == "HEAD" {
        return;
    }
    if let Err(e) = stream_range(&session, start, end, &mut stream) {
        warn!("Pteron stream stopped: {e}");
        crate::crashlog::note(&format!("video helper: itag {itag} stopped at a request for {start}-{end}: {e}"));
    }
}

/// Send bytes `start..=end`, fetching the next segment while the current one is written.
fn stream_range(session: &Arc<Session>, start: u64, end: u64, out: &mut TcpStream) -> Result<(), String> {
    let mut sent = 0u64;
    stream_range_counted(session, start, end, out, &mut sent).map_err(|e| format!("{e} (after sending {sent} bytes)"))
}

fn stream_range_counted(
    session: &Arc<Session>,
    start: u64,
    end: u64,
    out: &mut TcpStream,
    sent: &mut u64,
) -> Result<(), String> {
    let layout = &session.layout;
    let mut pos = start;
    let prefix_len = layout.prefix.len() as u64;
    if pos < prefix_len {
        let upto = end.min(prefix_len - 1);
        out.write_all(&layout.prefix[pos as usize..=upto as usize]).map_err(|e| e.to_string())?;
        *sent += upto + 1 - pos;
        pos = upto + 1;
    }
    while pos <= end {
        let Some(index) = layout.piece_at(pos) else { break };
        let bytes = session.get(index)?;
        session.read_ahead(index);
        let piece_start = layout.pieces[index].out_start;
        let from = (pos - piece_start) as usize;
        let to = ((end - piece_start) as usize).min(bytes.len() - 1);
        out.write_all(&bytes[from..=to]).map_err(|e| e.to_string())?;
        *sent += (to - from + 1) as u64;
        pos = piece_start + to as u64 + 1;
    }
    Ok(())
}

// ---------------------------------------------------------------- search suggestions

/// Requests the pages make to the helper: `/s/<secret>?q=` (search suggestions),
/// `/w/<secret>?k=` (is this page ready) and `/m/<secret>?c=` (more videos of a channel).
/// Returns the kind and the value, if the secret is right.
fn helper_request(path: &str, token: &str) -> Option<(char, String)> {
    let (kind, wanted) = match path.get(..3)? {
        "/s/" => ('s', "q"),
        "/w/" => ('w', "k"),
        "/m/" => ('m', "c"),
        "/f/" => ('f', "k"),
        "/o/" => ('o', "p"),
        "/q/" => ('q', "c"),
        "/c/" => ('c', "c"),
        "/u/" => ('u', "x"),
        _ => return None,
    };
    let (given, query) = path[3..].split_once('?')?;
    if given != token || token.is_empty() {
        return None;
    }
    let value = url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == wanted)
        .map(|(_, v)| v.into_owned())?;
    let value: String = value.chars().filter(|c| !c.is_control()).take(1900).collect();
    let value = if kind == 's' { value.chars().take(100).collect::<String>() } else { value };
    Some((kind, value.trim().to_string())).filter(|(_, v)| !v.is_empty())
}

/// YouTube answers `window.google.ac.h([...])`; the suggestions are the strings in item 1.
pub(crate) fn parse_suggestions(text: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (text.find('('), text.rfind(')')) else { return Vec::new() };
    if close <= open {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[open + 1..close]) else { return Vec::new() };
    value
        .get(1)
        .and_then(|l| l.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|item| item.get(0).and_then(|s| s.as_str()))
                .filter(|s| !s.is_empty() && s.len() <= 120)
                .take(8)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn suggestions(query: &str) -> Vec<String> {
    let address = format!(
        "https://suggestqueries-clients6.youtube.com/complete/search?client=youtube&ds=yt&hl=en&q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let output = curl_command()
        .args(["-sS", "--fail", "--proto", "=https", "--max-time", "6", "-A", UPSTREAM_USER_AGENT, "--"])
        .arg(&address)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => parse_suggestions(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------- downloads from YouTube

fn allowed_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else { return false };
    parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|h| h == "googlevideo.com" || h.ends_with(".googlevideo.com"))
}

fn curl_program() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

pub(crate) fn curl_command() -> Command {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(curl_program());
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
    };
    #[cfg(not(windows))]
    let command = Command::new(curl_program());
    command
}

// ---------------------------------------------------------------- being gentle with YouTube
//
// YouTube answers a burst of requests with empty replies or "401", and then keeps refusing for
// a few seconds, which breaks playback. So: at most a few downloads at once, the bulk
// header downloads step aside while the player is asking for video data, and after a refusal
// everything pauses for a moment.

static BLOCKED_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static LAST_DATA: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static DATA_SLOTS: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());
const MAX_DATA_DOWNLOADS: usize = 3;
const COOL_DOWN: Duration = Duration::from_secs(5);

fn wait_gate() {
    loop {
        let until = *BLOCKED_UNTIL.lock().unwrap();
        match until {
            Some(t) if t > std::time::Instant::now() => {
                std::thread::sleep((t - std::time::Instant::now()).min(Duration::from_millis(400)));
            },
            _ => return,
        }
    }
}

fn throttled(why: &str) {
    throttled_for(why, COOL_DOWN);
}

/// Everything pauses for `pause` (never shortening a longer pause already running).
fn throttled_for(why: &str, pause: Duration) {
    let mut guard = BLOCKED_UNTIL.lock().unwrap();
    let now = std::time::Instant::now();
    let fresh = guard.is_none_or(|t| t < now);
    let until = now + pause;
    if guard.is_none_or(|t| t < until) {
        *guard = Some(until);
    }
    if fresh {
        crate::crashlog::note(&format!("video helper: YouTube refused requests ({why}); pausing downloads for a few seconds"));
    }
}

/// Bulk work waits (a little) while the player is asking for video data.
fn yield_to_data() {
    for _ in 0..30 {
        let busy = LAST_DATA
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        if !busy {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct DataSlot;

impl DataSlot {
    fn take() -> DataSlot {
        let (lock, cv) = &DATA_SLOTS;
        let mut used = lock.lock().unwrap();
        while *used >= MAX_DATA_DOWNLOADS {
            used = cv.wait(used).unwrap();
        }
        *used += 1;
        DataSlot
    }
}

impl Drop for DataSlot {
    fn drop(&mut self) {
        let (lock, cv) = &DATA_SLOTS;
        *lock.lock().unwrap() -= 1;
        cv.notify_one();
    }
}

fn refused(message: &str) -> bool {
    message.contains("error: 4") || message.contains("429") || message.contains("403") || message.contains("401")
}

/// Download `len` bytes starting at `start`. Retries a few times, a little slower each time.
pub(crate) fn curl_range(url: &str, start: u64, len: u64) -> Result<Vec<u8>, String> {
    if !allowed_host(url) {
        return Err("refusing to download from this address".into());
    }
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut last = String::new();
    let _slot = DataSlot::take();
    // Small requests are the file headers: retry them more, with short pauses, so a video
    // starts as soon as YouTube answers. Big ones are the video data itself.
    let small = len <= 65536;
    let tries = if small { 8u64 } else { 5 };
    for attempt in 0..tries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis((400 * attempt).min(if small { 1500 } else { 4000 })));
        }
        wait_gate();
        *LAST_DATA.lock().unwrap() = Some(std::time::Instant::now());
        let mut command = curl_command();
        command
            .args(["-sS", "--fail", "--proto", "=https", "--max-time", "60", "-A", UPSTREAM_USER_AGENT])
            .arg("-r")
            .arg(format!("{}-{}", start, start + len - 1))
            .arg("--")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match command.spawn() {
            Err(e) => return Err(format!("could not start curl: {e}")),
            Ok(child) => match child.wait_with_output() {
                Ok(out) if out.status.success() && out.stdout.len() as u64 == len => return Ok(out.stdout),
                Ok(out) => {
                    last = if out.status.success() {
                        format!("got {} bytes, expected {len}", out.stdout.len())
                    } else {
                        String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect()
                    };
                    if refused(&last) || out.stdout.is_empty() {
                        throttled_for(&last, if small { Duration::from_millis(1200) } else { COOL_DOWN });
                    }
                },
                Err(e) => last = e.to_string(),
            },
        }
    }
    Err(last)
}

/// Several ranges of one address with `curl --parallel` runs (one connection, no process start
/// per range). YouTube sometimes answers a burst of requests with an empty reply or a 401, so
/// only the pieces that failed are asked for again, a little slower each round.
pub(crate) fn curl_ranges(url: &str, ranges: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, String> {
    if !allowed_host(url) {
        return Err("refusing to download from this address".into());
    }
    let mut parts: Vec<Option<Vec<u8>>> = vec![None; ranges.len()];
    let mut last_error = String::new();
    for round in 0..8usize {
        let missing: Vec<usize> = (0..ranges.len()).filter(|i| parts[*i].is_none()).collect();
        if missing.is_empty() {
            break;
        }
        if round > 0 {
            std::thread::sleep(Duration::from_millis(200 * round as u64));
            crate::crashlog::note(&format!(
                "video helper: {} of {} pieces missing, trying again (round {round})",
                missing.len(),
                ranges.len()
            ));
        }
        // small steps, so the player's own downloads are never stuck behind a big batch
        let batch = if MULTI.load(Ordering::Relaxed) == MULTI_NO { 40 } else { MULTI_BATCH };
        for chunk in missing.chunks(batch) {
            yield_to_data();
            wait_gate();
            let wanted: Vec<(u64, u64)> = chunk.iter().map(|&i| ranges[i]).collect();
            match fetch_batch(url, &wanted, 4) {
                Ok(got) => {
                    let lost = got.iter().filter(|p| p.is_none()).count();
                    for (&i, bytes) in chunk.iter().zip(got) {
                        parts[i] = bytes;
                    }
                    if lost * 3 > chunk.len() {
                        // Most of this batch was refused: everyone waits a moment.
                        throttled_for("empty replies", Duration::from_millis(2500));
                    } else if lost > 0 {
                        // A few lost pieces: just ask for them again a little later.
                        std::thread::sleep(Duration::from_millis(250));
                    }
                },
                Err(why) => {
                    if refused(&why) {
                        throttled_for(&why, Duration::from_millis(2500));
                    }
                    last_error = why;
                },
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Some(bytes) => out.push(bytes),
            None => {
                return Err(if last_error.is_empty() {
                    "YouTube did not send some parts of the video (it may be limiting requests). Try again in a minute.".into()
                } else {
                    last_error
                });
            },
        }
    }
    Ok(out)
}

// ---- many ranges in one request ("multipart/byteranges")
//
// A seekable file needs the header of every 4-second piece: over a thousand small ranges per
// video. If the server answers a request with several ranges at once, that becomes a few dozen
// requests. Whether YouTube does is found out on first use; when it does not, the one-by-one
// way below is used (and remembered).

const MULTI_UNKNOWN: u8 = 0;
const MULTI_YES: u8 = 1;
const MULTI_NO: u8 = 2;
static MULTI: AtomicU8 = AtomicU8::new(MULTI_UNKNOWN);
const MULTI_BATCH: usize = 48;
const UNSUPPORTED: &str = "multi-range not supported";

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() || hay.len() - from < needle.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// `(first byte, bytes)` of every part of a `multipart/byteranges` body.
fn parse_multipart(body: &[u8], boundary: &str) -> Vec<(u64, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut pos = 0usize;
    while let Some(at) = find_bytes(body, delimiter.as_bytes(), pos) {
        let after = at + delimiter.len();
        if body.get(after..after + 2) == Some(b"--") {
            break;
        }
        let Some(head_end) = find_bytes(body, b"\r\n\r\n", after) else { break };
        let head = String::from_utf8_lossy(&body[after..head_end]).to_ascii_lowercase();
        let range = head.lines().find_map(|l| l.trim().strip_prefix("content-range:").map(str::to_string));
        let Some(range) = range else { break };
        let range = range.trim().trim_start_matches("bytes").trim();
        let Some((span, _)) = range.split_once('/') else { break };
        let Some((a, b)) = span.trim().split_once('-') else { break };
        let (Ok(a), Ok(b)) = (a.trim().parse::<u64>(), b.trim().parse::<u64>()) else { break };
        if b < a {
            break;
        }
        let start = head_end + 4;
        let end = start + (b - a + 1) as usize;
        if end > body.len() {
            break;
        }
        parts.push((a, body[start..end].to_vec()));
        pos = end;
    }
    parts
}

/// Ask for all `group` ranges in one request. `Err(UNSUPPORTED)` when the server does not
/// answer with separate parts.
fn curl_multi(url: &str, group: &[(u64, u64)], proto: &str) -> Result<Vec<Option<Vec<u8>>>, String> {
    if group.is_empty() || group.iter().any(|(_, len)| *len == 0) {
        return Err("empty range".into());
    }
    let spec: Vec<String> = group.iter().map(|(s, l)| format!("{}-{}", s, s + l - 1)).collect();
    let cap = group.iter().map(|(_, l)| *l as usize).sum::<usize>() + group.len() * 400 + 16384;
    let mut child = curl_command()
        .args(["-sS", "-i", "--proto", proto, "--max-time", "60", "-A", UPSTREAM_USER_AGENT])
        .arg("-H")
        .arg(format!("Range: bytes={}", spec.join(",")))
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start curl: {e}"))?;
    let mut raw = Vec::new();
    if let Some(out) = child.stdout.take() {
        let _ = out.take(cap as u64).read_to_end(&mut raw);
    }
    // A server that ignores the ranges would send the whole file: stop after what we expect.
    let _ = child.kill();
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if raw.is_empty() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().chars().take(200).collect::<String>());
    }
    // headers (skipping any "100 Continue" block)
    let mut at = 0usize;
    let (status, headers) = loop {
        let Some(end) = find_bytes(&raw, b"\r\n\r\n", at) else { return Err(UNSUPPORTED.into()) };
        let block = String::from_utf8_lossy(&raw[at..end]).to_string();
        let code: u32 = block.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        at = end + 4;
        if (100..200).contains(&code) {
            continue;
        }
        break (code, block);
    };
    if matches!(status, 401 | 403 | 429) {
        return Err(format!("curl: (22) The requested URL returned error: {status}"));
    }
    let lower = headers.to_ascii_lowercase();
    let boundary = lower
        .lines()
        .find(|l| l.starts_with("content-type:") && l.contains("multipart/byteranges"))
        .and_then(|l| l.split("boundary=").nth(1))
        .map(|b| b.trim().trim_matches('"').to_string());
    // keep the original spelling of the boundary
    let boundary = boundary.and_then(|_| {
        headers.lines().find(|l| l.to_ascii_lowercase().starts_with("content-type:")).and_then(|l| {
            let i = l.to_ascii_lowercase().find("boundary=")? + "boundary=".len();
            Some(l[i..].trim().trim_matches('"').to_string())
        })
    });
    let (true, Some(boundary)) = (status == 206, boundary) else { return Err(UNSUPPORTED.into()) };
    let parts = parse_multipart(&raw[at..], &boundary);
    Ok(group
        .iter()
        .map(|(s, l)| {
            parts
                .iter()
                .find(|(a, bytes)| a == s && bytes.len() as u64 >= *l)
                .map(|(_, bytes)| bytes[..*l as usize].to_vec())
        })
        .collect())
}

/// One batch of header ranges: in one request when the server allows it, else one by one.
fn fetch_batch(url: &str, wanted: &[(u64, u64)], parallel: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mode = MULTI.load(Ordering::Relaxed);
    if mode != MULTI_NO && wanted.len() > 1 {
        match curl_multi(url, wanted, "=https") {
            Ok(got) => {
                if got.iter().all(|p| p.is_some()) {
                    if MULTI.swap(MULTI_YES, Ordering::Relaxed) == MULTI_UNKNOWN {
                        crate::crashlog::note("video helper: YouTube answers several ranges at once (faster start)");
                    }
                    return Ok(got);
                }
                if mode == MULTI_UNKNOWN {
                    // The first try did not bring everything: do not rely on it.
                    MULTI.store(MULTI_NO, Ordering::Relaxed);
                    crate::crashlog::note("video helper: several ranges at once did not work, using one by one");
                } else {
                    return Ok(got);
                }
            },
            Err(why) if why == UNSUPPORTED => {
                MULTI.store(MULTI_NO, Ordering::Relaxed);
                crate::crashlog::note("video helper: YouTube does not answer several ranges at once, using one by one");
            },
            Err(why) => return Err(why),
        }
    }
    curl_group(url, wanted, "=https", parallel)
}

/// Download the listed ranges. Pieces that did not arrive in full are `None`.
fn curl_group(url: &str, group: &[(u64, u64)], proto: &str, parallel: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pteron_dl_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        let quote = |t: &str| t.replace('\\', "/").replace('"', "");
        let mut config = String::new();
        for (i, (start, len)) in group.iter().enumerate() {
            if *len == 0 {
                return Err("empty range".to_string());
            }
            if i > 0 {
                config.push_str("next\n");
            }
            config.push_str(&format!(
                "url = \"{}\"\nrange = \"{}-{}\"\noutput = \"{}\"\n",
                quote(url),
                start,
                start + len - 1,
                quote(&dir.join(format!("{i}.part")).to_string_lossy())
            ));
        }
        let config_path = dir.join("curl.cfg");
        std::fs::write(&config_path, config).map_err(|e| e.to_string())?;
        let output = curl_command()
            .args(["-sS", "--fail", "--parallel", "--parallel-max", &parallel.to_string(), "--proto", proto, "--max-time", "90"])
            .args(["-A", UPSTREAM_USER_AGENT, "-K"])
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("could not start curl: {e}"))?;
        let mut parts = Vec::with_capacity(group.len());
        for (i, (_, len)) in group.iter().enumerate() {
            let bytes = std::fs::read(dir.join(format!("{i}.part"))).ok();
            parts.push(bytes.filter(|b| b.len() as u64 == *len));
        }
        if parts.iter().all(|p| p.is_none()) && !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().chars().take(200).collect());
        }
        Ok(parts)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

static GLOBAL: OnceLock<Option<Proxy>> = OnceLock::new();

/// The shared proxy, started on first use.
pub(crate) fn global() -> Option<&'static Proxy> {
    GLOBAL
        .get_or_init(|| {
            let one: Fetcher = Arc::new(curl_range);
            let many: BatchFetcher = Arc::new(curl_ranges);
            match Proxy::start(one, many) {
                Ok(proxy) => Some(proxy),
                Err(e) => {
                    warn!("Could not start the local stream server: {e}");
                    None
                },
            }
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use std::process::Command as Cmd;

    use super::*;

    #[test]
    fn range_headers() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-2", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
    }

    #[test]
    fn suggestions_are_parsed_and_guarded() {
        let text = r#"window.google.ac.h(["fort",[["fortnite",0,[512]],["fortnite chapter 6",0],["",0]],{"k":1}])"#;
        assert_eq!(parse_suggestions(text), vec!["fortnite", "fortnite chapter 6"]);
        assert!(parse_suggestions("garbage").is_empty());
        assert_eq!(helper_request("/s/tok?q=hello%20world", "tok"), Some(('s', "hello world".into())));
        assert_eq!(helper_request("/w/tok?k=video-abc", "tok"), Some(('w', "video-abc".into())));
        assert_eq!(helper_request("/m/tok?c=AbC%3D", "tok"), Some(('m', "AbC=".into())));
        assert_eq!(helper_request("/o/tok?p=0.5%2C0", "tok"), Some(('o', "0.5,0".into())));
        assert_eq!(helper_request("/c/tok?c=dQw4w9WgXcQ", "tok"), Some(('c', "dQw4w9WgXcQ".into())));
        assert_eq!(helper_request("/s/other?q=hi", "tok"), None);
        assert_eq!(helper_request("/s/?q=hi", ""), None);
        assert_eq!(helper_request("/s/tok?q=%20", "tok"), None);
        assert_eq!(helper_request("/x/tok?q=a", "tok"), None);
        assert_eq!(helper_request("/s/tok?k=a", "tok"), None);
    }

    #[test]
    fn paths_and_hosts() {
        let t = "0123456789abcdef0123456789abcdef";
        assert_eq!(parse_path(&format!("/v/{t}/137.mp4")), Some((t.to_string(), 137, false)));
        assert_eq!(parse_path(&format!("/v/{t}/137p.mp4")), Some((t.to_string(), 137, true)));
        assert_eq!(parse_path(&format!("/v/{t}/137.mp4?x=1")), Some((t.to_string(), 137, false)));
        assert_eq!(parse_path("/v/short/137.mp4"), None);
        assert_eq!(parse_path(&format!("/v/{t}/../137.mp4")), None);
        assert_eq!(parse_path(&format!("/x/{t}/137.mp4")), None);
        assert!(allowed_host("https://rr1---sn-abc.googlevideo.com/videoplayback?x=1"));
        assert!(!allowed_host("http://rr1.googlevideo.com/x"));
        assert!(!allowed_host("https://googlevideo.com.evil.example/x"));
        assert!(!allowed_host("https://evil.example/googlevideo.com"));
        assert!(curl_range("https://evil.example/x", 0, 10).is_err());
        let a = random_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_token());
    }

    fn make(dir: &std::path::Path, secs: u32) -> (Vec<u8>, Vec<u8>) {
        std::fs::create_dir_all(dir).unwrap();
        let run = |args: &[&str]| {
            assert!(Cmd::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap().success())
        };
        let v = dir.join("v.mp4");
        let a = dir.join("a.mp4");
        run(&["-f", "lavfi", "-i", &format!("testsrc=duration={secs}:size=320x240:rate=25"),
              "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "50", "-keyint_min", "50",
              "-sc_threshold", "0", "-bf", "2", "-an", "-movflags",
              "frag_keyframe+empty_moov+default_base_moof+global_sidx", v.to_str().unwrap()]);
        run(&["-f", "lavfi", "-i", &format!("sine=duration={secs}"), "-c:a", "aac", "-vn",
              "-frag_duration", "2300000", "-movflags",
              "empty_moov+default_base_moof+global_sidx", a.to_str().unwrap()]);
        (std::fs::read(v).unwrap(), std::fs::read(a).unwrap())
    }

    fn index_end(file: &[u8]) -> u64 {
        // find the end of the sidx box
        let mut pos = 0usize;
        while pos + 8 <= file.len() {
            let size = u32::from_be_bytes([file[pos], file[pos + 1], file[pos + 2], file[pos + 3]]) as usize;
            if &file[pos + 4..pos + 8] == b"sidx" {
                return (pos + size - 1) as u64;
            }
            pos += size;
        }
        panic!("no sidx");
    }

    fn http_get(port: u16, path: &str, range: Option<&str>) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let r = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n{r}\r\n").as_bytes()).unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (String::from_utf8_lossy(&all[..split]).to_string(), all[split + 4..].to_vec())
    }

    #[test]
    fn serves_a_playable_file_over_http_with_ranges() {
        if Cmd::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pteron_proxy_{}", std::process::id()));
        let (v, a) = make(&dir, 16);
        let (vc, ac) = (v.clone(), a.clone());
        let pick = move |url: &str| -> Result<Vec<u8>, String> {
            match url {
                "mem://video" => Ok(vc.clone()),
                "mem://audio" => Ok(ac.clone()),
                _ => Err("bad url".into()),
            }
        };
        let pick2 = pick.clone();
        let one: Fetcher = Arc::new(move |url: &str, start, len| {
            let src = pick(url)?;
            let end = (start + len) as usize;
            if end > src.len() { return Err("past the end".into()); }
            Ok(src[start as usize..end].to_vec())
        });
        let many: BatchFetcher = Arc::new(move |url: &str, ranges: &[(u64, u64)]| {
            let src = pick2(url)?;
            ranges.iter().map(|&(s, l)| {
                let end = ((s + l) as usize).min(src.len());
                Ok(src[s as usize..end].to_vec())
            }).collect()
        });
        let proxy = Proxy::start(one, many).unwrap();
        let token = proxy.register(VideoSet {
            videos: vec![StreamInfo { itag: 137, url: "mem://video".into(), index_end: index_end(&v), height: 240, fps: 25 }],
            audio: StreamInfo { itag: 140, url: "mem://audio".into(), index_end: index_end(&a), height: 0, fps: 0 },
            duration_ms: 16_000,
        });
        proxy.prepare(&token, 137, false).unwrap();
        let url = proxy.url(&token, 137);
        let path = url.split("127.0.0.1:").nth(1).unwrap().split_once('/').unwrap().1;
        let path = format!("/{path}");

        // the first-part address works too (this sample is shorter than the first part)
        let part_path = path.replace("137.mp4", "137p.mp4");
        let (ph, part_bytes) = http_get(proxy.port, &part_path, None);
        assert!(ph.starts_with("HTTP/1.1 200"), "{ph}");
        assert!(part_bytes.len() > 1000);

        // whole file
        let (head, whole) = http_get(proxy.port, &path, None);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains(&format!("Content-Length: {}", whole.len())), "{head}");
        // partial ranges agree with the whole file
        let total = whole.len() as u64;
        for (s, e) in [(0u64, 1023u64), (total / 2, total / 2 + 40_000), (total - 100, total - 1)] {
            let (h, part) = http_get(proxy.port, &path, Some(&format!("bytes={s}-{e}")));
            assert!(h.starts_with("HTTP/1.1 206"), "{h}");
            assert!(h.contains(&format!("Content-Range: bytes {s}-{e}/{total}")), "{h}");
            assert_eq!(part, &whole[s as usize..=e as usize]);
        }
        let (h, _) = http_get(proxy.port, &path, Some(&format!("bytes={}-", total + 5)));
        assert!(h.starts_with("HTTP/1.1 416"), "{h}");
        let (h, _) = http_get(proxy.port, "/v/00000000000000000000000000000000/137.mp4", None);
        assert!(h.starts_with("HTTP/1.1 502"), "{h}");
        let (h, _) = http_get(proxy.port, &path.replace("137", "999"), None);
        assert!(h.starts_with("HTTP/1.1 502"), "{h}");

        // a real media tool plays it straight from the server, including a seek
        let decode = Cmd::new("ffmpeg")
            .args(["-v", "error", "-ss", "9", "-i", &url, "-t", "4", "-f", "null", "-"])
            .output()
            .unwrap();
        assert!(decode.status.success(), "{}", String::from_utf8_lossy(&decode.stderr));
        assert!(decode.stderr.is_empty(), "{}", String::from_utf8_lossy(&decode.stderr));
        let probe = Cmd::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "default=nw=1", &url])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(text.contains("video") && text.contains("audio"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A throw-away server that understands `Range`, to try the parallel download for real.
    #[test]
    fn parallel_curl_downloads_exact_ranges() {
        if Cmd::new("curl").arg("--version").output().is_err() {
            return;
        }
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = data.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let served = served.clone();
                std::thread::spawn(move || {
                    let Some(head) = read_head(&mut stream) else { return };
                    let total = served.len() as u64;
                    let (s, e) = parse_range(head.range.as_deref().unwrap_or("bytes=0-"), total).unwrap();
                    respond(&mut stream, "206 Partial Content", "", e - s + 1);
                    let _ = stream.write_all(&served[s as usize..=e as usize]);
                });
            }
        });
        let url = format!("http://127.0.0.1:{port}/x");
        let ranges: Vec<(u64, u64)> = (0..30).map(|i| (i * 5000 + 7, 1000 + i * 13)).collect();
        let parts = curl_group(&url, &ranges, "=http", 6).unwrap();
        assert_eq!(parts.len(), ranges.len());
        for ((s, l), got) in ranges.iter().zip(&parts) {
            assert_eq!(got.as_deref().unwrap(), &data[*s as usize..(*s + *l) as usize]);
        }
        // the fallback path agrees (it refuses non-YouTube hosts, so go through curl_range's core)
        assert!(curl_ranges(&url, &ranges).is_err());
    }

    /// A throw-away server that answers several ranges at once (or, when `multi` is false, only
    /// the first one, like a server that does not support it).
    fn range_server(data: Vec<u8>, multi: bool) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let data = data.clone();
                std::thread::spawn(move || {
                    let Some(head) = read_head(&mut stream) else { return };
                    let spec = head.range.clone().unwrap_or_default();
                    let list: Vec<(u64, u64)> = spec
                        .trim_start_matches("bytes=")
                        .split(',')
                        .filter_map(|r| {
                            let (a, b) = r.trim().split_once('-')?;
                            Some((a.parse().ok()?, b.parse().ok()?))
                        })
                        .collect();
                    let total = data.len();
                    if list.len() > 1 && multi {
                        let mut body = Vec::new();
                        for (a, b) in &list {
                            body.extend_from_slice(format!("--SEP\r\nContent-Type: video/mp4\r\nContent-Range: bytes {a}-{b}/{total}\r\n\r\n").as_bytes());
                            body.extend_from_slice(&data[*a as usize..=*b as usize]);
                            body.extend_from_slice(b"\r\n");
                        }
                        body.extend_from_slice(b"--SEP--\r\n");
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: multipart/byteranges; boundary=SEP\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    } else {
                        let (a, b) = list.first().copied().unwrap_or((0, total as u64 - 1));
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes {a}-{b}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            b - a + 1
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&data[a as usize..=b as usize]);
                    }
                });
            }
        });
        port
    }

    #[test]
    fn several_ranges_in_one_request() {
        if Cmd::new("curl").arg("--version").output().is_err() {
            return;
        }
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let ranges: Vec<(u64, u64)> = (0..48).map(|i| (i * 5000 + 11, 3000 + i * 7)).collect();
        let url = format!("http://127.0.0.1:{}/x", range_server(data.clone(), true));
        let parts = curl_multi(&url, &ranges, "=http").unwrap();
        assert_eq!(parts.len(), ranges.len());
        for ((s, l), got) in ranges.iter().zip(&parts) {
            assert_eq!(got.as_deref().unwrap(), &data[*s as usize..(*s + *l) as usize]);
        }
        // a server that only answers the first range is noticed
        let url = format!("http://127.0.0.1:{}/x", range_server(data, false));
        assert_eq!(curl_multi(&url, &ranges, "=http").unwrap_err(), UNSUPPORTED);
    }

    #[test]
    fn multipart_parsing() {
        let body = b"--B\r\nContent-Type: video/mp4\r\nContent-Range: bytes 10-13/100\r\n\r\nabcd\r\n--B\r\nContent-Range: bytes 50-51/100\r\n\r\nxy\r\n--B--\r\n";
        assert_eq!(parse_multipart(body, "B"), vec![(10, b"abcd".to_vec()), (50, b"xy".to_vec())]);
        assert!(parse_multipart(b"nothing here", "B").is_empty());
    }
}
