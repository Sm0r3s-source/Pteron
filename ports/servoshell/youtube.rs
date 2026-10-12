/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: YouTube in Pteron's own pages.
//!
//! YouTube's own website needs Media Source Extensions, which Servo does not have yet. Instead,
//! a link to a YouTube video, search or channel is turned into a small local page:
//!
//! * `innertube` asks YouTube's app interface for the video streams, search results or channel
//!   videos (no account, no extra program).
//! * `streamproxy` / `mp4mux` merge the separate video and audio streams into one MP4 on the
//!   fly, so a plain `<video>` element can play them, up to 1080p.
//!
//! Safety:
//! * The video id is checked (11 letters, digits, `-` or `_`) before it is used anywhere.
//! * Everything that comes from the network is HTML-escaped in the generated page.
//! * Only `https://*.googlevideo.com/` addresses are downloaded, and the local server only
//!   answers on 127.0.0.1 for unguessable addresses.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use log::warn;
use url::Url;

use crate::innertube::{self, VideoItem};
use crate::pteron::APP_NAME;
use crate::streamproxy;

const FRAGMENT_DIRECT: &str = "pteron-direct";

/// Something Pteron shows itself instead of the real YouTube page.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Request {
    /// A video, by id.
    Video(String),
    /// Search results for a text, with how to sort and which upload dates to show.
    Search(String, Filter),
    /// The videos of a channel, by its path on youtube.com (`/@name`, `/channel/ID`, ...).
    Channel(String),
}

/// Search filter: sort 0 best match, 1 top rated, 2 newest, 3 most views; date 0 any time,
/// 2 today, 3 this week, 4 this month, 5 this year.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct Filter {
    pub sort: u8,
    pub date: u8,
}

impl Request {
    /// File name stem of the generated page.
    fn key(&self) -> String {
        match self {
            Request::Video(id) => id.clone(),
            Request::Search(query, f) if *f == Filter::default() => format!("s-{:016x}", fnv(query)),
            Request::Search(query, f) => format!("s-{:016x}", fnv(&format!("{query}\u{1}{}\u{1}{}", f.sort, f.date))),
            Request::Channel(path) => format!("c-{:016x}", fnv(path)),
        }
    }
}

fn fnv(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// What Pteron should do with a navigation.
#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    /// The Pteron YouTube start page.
    Home,
    Open(Request),
    /// Plume Music, optionally opened on a playlist or album.
    Music(Option<String>),
}

static IN_FLIGHT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn begin_job(key: &str) -> bool {
    let mut guard = IN_FLIGHT.lock().unwrap();
    guard.get_or_insert_with(HashSet::new).insert(key.to_string())
}

/// Whether the page for `key` has finished loading (nothing is still working on it).
pub(crate) fn is_ready(key: &str) -> bool {
    !IN_FLIGHT.lock().unwrap().as_ref().is_some_and(|set| set.contains(key))
}

fn end_job(key: &str) {
    if let Some(set) = IN_FLIGHT.lock().unwrap().as_mut() {
        set.remove(key);
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 40
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether `text` is safe to use as a YouTube channel path (`/@name`, `/channel/ID`, ...).
fn channel_path(path: &str) -> Option<String> {
    let mut parts = path.trim_matches('/').split('/');
    let first = parts.next()?;
    let name = if first.starts_with('@') {
        first.to_string()
    } else if matches!(first, "channel" | "c" | "user") {
        format!("{first}/{}", parts.next()?)
    } else {
        return None;
    };
    let clean = name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '@' | '-' | '_' | '.' | '/' | '%'));
    (clean && name.len() <= 100).then(|| format!("/{name}"))
}

/// Decide whether a navigation is for YouTube (or one of Pteron's own links on the
/// generated pages).
pub(crate) fn classify(url: &Url) -> Option<Action> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if url.fragment() == Some(FRAGMENT_DIRECT) {
        return None;
    }
    match host.as_str() {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
            let path = url.path();
            if path == "/watch" {
                let id = url
                    .query_pairs()
                    .find(|(key, _)| key == "v")
                    .map(|(_, value)| value.into_owned())?;
                return valid_video_id(&id).then_some(Action::Open(Request::Video(id)));
            }
            if let Some(rest) = ["/shorts/", "/live/"]
                .iter()
                .find_map(|prefix| path.strip_prefix(prefix))
            {
                let id = rest.split('/').next().unwrap_or("").to_string();
                return valid_video_id(&id).then_some(Action::Open(Request::Video(id)));
            }
            if path == "/results" {
                let query = url
                    .query_pairs()
                    .find(|(key, _)| key == "search_query")
                    .map(|(_, value)| clean_query(&value))?;
                let number = |name: &str, max: u8| {
                    url.query_pairs()
                        .find(|(key, _)| key == name)
                        .and_then(|(_, value)| value.parse::<u8>().ok())
                        .filter(|n| *n <= max)
                        .unwrap_or(0)
                };
                let filter = Filter { sort: number("sort", 3), date: number("date", 5) };
                return if query.is_empty() {
                    Some(Action::Home)
                } else {
                    Some(Action::Open(Request::Search(query, filter)))
                };
            }
            if path == "/playlist" &&
                let Some(list) = url
                    .query_pairs()
                    .find(|(key, _)| key == "list")
                    .map(|(_, value)| value.into_owned())
                    .filter(|list| innertube::valid_playlist_id(list))
            {
                return Some(Action::Music(Some(list)));
            }
            if path == "/" || path.is_empty() || path.starts_with("/feed") {
                return Some(Action::Home);
            }
            channel_path(path).map(|path| Action::Open(Request::Channel(path)))
        },
        "music.youtube.com" => {
            let list = url
                .query_pairs()
                .find(|(key, _)| key == "list")
                .map(|(_, value)| value.into_owned())
                .filter(|list| innertube::valid_playlist_id(list));
            Some(Action::Music(list))
        },
        "youtu.be" => {
            let id = url.path_segments().and_then(|mut s| s.next())?.to_string();
            valid_video_id(&id).then_some(Action::Open(Request::Video(id)))
        },
        _ => None,
    }
}

/// A search text without control characters and with a sane length.
fn clean_query(query: &str) -> String {
    query
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

fn base_dir() -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(APP_NAME))
}

fn player_dir() -> Option<PathBuf> {
    base_dir().map(|dir| dir.join("player"))
}


// ---------------------------------------------------------------- watch later, history, resume

const MAX_LATER: usize = 300;
const MAX_HISTORY: usize = 150;
const MAX_POSITIONS: usize = 300;

/// What Plume remembers: a watch-later list, recently watched videos and where you stopped.
#[derive(Debug, Default, PartialEq)]
struct Store {
    later: Vec<VideoItem>,
    history: Vec<VideoItem>,
    positions: Vec<(String, u64)>,
}

fn clean(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

fn item_from_json(v: &serde_json::Value) -> Option<VideoItem> {
    let id = v.get("id")?.as_str()?;
    if !valid_video_id(id) {
        return None;
    }
    let text = |key: &str, max: usize| clean(v.get(key).and_then(|t| t.as_str()).unwrap_or(""), max);
    Some(VideoItem {
        id: id.to_string(),
        title: text("title", 200),
        channel: text("channel", 120),
        duration_secs: v.get("duration").and_then(|d| d.as_u64()).filter(|d| *d > 0).map(|d| d.min(100 * 3600)),
        views: String::new(),
        published: String::new(),
    })
}

fn item_to_json(item: &VideoItem) -> serde_json::Value {
    serde_json::json!({"id": item.id, "title": item.title, "channel": item.channel, "duration": item.duration_secs.unwrap_or(0)})
}

impl Store {
    /// Saves the video for later, or takes it off the list. Returns whether it is saved now.
    fn toggle_later(&mut self, item: VideoItem) -> bool {
        if let Some(i) = self.later.iter().position(|v| v.id == item.id) {
            self.later.remove(i);
            return false;
        }
        self.later.insert(0, item);
        self.later.truncate(MAX_LATER);
        true
    }

    fn add_history(&mut self, item: VideoItem) {
        self.history.retain(|v| v.id != item.id);
        self.history.insert(0, item);
        self.history.truncate(MAX_HISTORY);
    }

    /// Remembers where the video was stopped (nothing is kept for the first seconds).
    fn set_position(&mut self, id: &str, secs: u64) {
        self.positions.retain(|(v, _)| v != id);
        if secs >= 15 {
            self.positions.push((id.to_string(), secs));
            if self.positions.len() > MAX_POSITIONS {
                self.positions.remove(0);
            }
        }
    }

    fn position(&self, id: &str) -> u64 {
        self.positions.iter().find(|(v, _)| v == id).map(|(_, s)| *s).unwrap_or(0)
    }

    fn to_json(&self) -> String {
        serde_json::json!({
            "later": self.later.iter().map(item_to_json).collect::<Vec<_>>(),
            "history": self.history.iter().map(item_to_json).collect::<Vec<_>>(),
            "positions": self.positions.iter().map(|(v, s)| serde_json::json!([v, s])).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn from_json(text: &str) -> Store {
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
        let items = |key: &str, max: usize| -> Vec<VideoItem> {
            v.get(key)
                .and_then(|l| l.as_array())
                .map(|l| l.iter().filter_map(item_from_json).take(max).collect())
                .unwrap_or_default()
        };
        Store {
            later: items("later", MAX_LATER),
            history: items("history", MAX_HISTORY),
            positions: v
                .get("positions")
                .and_then(|l| l.as_array())
                .map(|l| {
                    l.iter()
                        .filter_map(|p| {
                            let id = p.get(0)?.as_str()?;
                            valid_video_id(id).then(|| (id.to_string(), p.get(1).and_then(|s| s.as_u64()).unwrap_or(0)))
                        })
                        .take(MAX_POSITIONS)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn store_file() -> Option<PathBuf> {
    base_dir().map(|dir| dir.join("plume.json"))
}

fn with_store<T>(change: impl FnOnce(&mut Store) -> T, save: bool) -> T {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(
            store_file()
                .and_then(|f| std::fs::read_to_string(f).ok())
                .map(|t| Store::from_json(&t))
                .unwrap_or_default(),
        );
    }
    let store = guard.as_mut().expect("store was just made");
    let result = change(store);
    if save && let Some(file) = store_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, store.to_json());
    }
    result
}

/// The player page's requests about the lists (`later:`, `hist:`, `pos:`, `clear:`).
fn store_command(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("later:") {
        if let Some(id) = rest.strip_prefix("has:") {
            let saved = with_store(|s| s.later.iter().any(|v| v.id == id), false);
            return if saved { "1" } else { "0" }.into();
        }
        if let Some(id) = rest.strip_prefix("rm:") {
            with_store(|s| s.later.retain(|v| v.id != id), true);
            return "0".into();
        }
        return match serde_json::from_str(rest).ok().as_ref().and_then(item_from_json) {
            Some(item) => if with_store(|s| s.toggle_later(item), true) { "1" } else { "0" }.into(),
            None => String::new(),
        };
    }
    if let Some(rest) = value.strip_prefix("hist:") {
        if let Some(item) = serde_json::from_str(rest).ok().as_ref().and_then(item_from_json) {
            with_store(|s| s.add_history(item), true);
        }
        return String::new();
    }
    if let Some(rest) = value.strip_prefix("pos:") {
        if let Some(id) = rest.strip_prefix("get:") {
            return with_store(|s| s.position(id), false).to_string();
        }
        if let Some((id, secs)) = rest.split_once(',') &&
            valid_video_id(id) &&
            let Ok(secs) = secs.parse::<u64>()
        {
            with_store(|s| s.set_position(id, secs.min(100 * 3600)), true);
        }
        return String::new();
    }
    match value {
        "clear:history" => with_store(|s| s.history.clear(), true),
        "clear:later" => with_store(|s| s.later.clear(), true),
        _ => {},
    }
    String::new()
}

/// Volume the player used last time: "get" returns `volume,muted` (for example `0.40,0`);
/// anything else of that shape is saved.
pub(crate) fn player_memory(value: &str) -> String {
    if let Some(note) = value.strip_prefix("note:") {
        let note: String = note.chars().filter(|c| !c.is_control()).take(120).collect();
        crate::crashlog::note(&format!("player: {note}"));
        return String::new();
    }
    if let Some(rest) = value.strip_prefix("dl:") {
        // dl:<video|audio>|<file address>|<title>|<channel>
        let mut parts = rest.splitn(4, '|');
        let (kind, url, title, channel) =
            (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let ok = streamproxy::global().is_some_and(|p| p.is_media_url(url)) && matches!(kind, "video" | "audio");
        if ok {
            crate::downloads::begin_media(url, title, &clean(channel, 120), kind == "audio");
        }
        return if ok { "1" } else { "0" }.into();
    }
    if value.starts_with("later:") || value.starts_with("hist:") || value.starts_with("pos:") || value.starts_with("clear:") {
        return store_command(value);
    }
    let Some(file) = player_dir().map(|d| d.join("volume.txt")) else { return String::new() };
    if value == "get" {
        return std::fs::read_to_string(file).unwrap_or_default();
    }
    let mut parts = value.split(',');
    let volume = parts.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| (0.0..=1.0).contains(v));
    let muted = parts.next().map(|m| m == "1").unwrap_or(false);
    if let Some(volume) = volume {
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(&file));
        let _ = std::fs::write(file, format!("{volume:.2},{}", if muted { 1 } else { 0 }));
    }
    String::new()
}

fn comment_html(comment: &innertube::Comment) -> String {
    let mut line = vec![format!("<b>{}</b>", esc(&comment.author))];
    if !comment.when.is_empty() {
        line.push(esc(&comment.when));
    }
    if !comment.likes.is_empty() {
        line.push(format!("{} likes", esc(&comment.likes)));
    }
    format!(
        "<div class=\"cm\"><div class=\"meta\">{}</div><div class=\"text\">{}</div></div>",
        line.join(" &middot; "),
        linkify(&comment.text)
    )
}

/// One batch of comments for the watch page: `{"html": ..., "next": token or null}`.
pub(crate) fn comments_json(video_or_token: &str) -> String {
    match innertube::comments(video_or_token) {
        Ok((comments, next)) => serde_json::json!({
            "html": comments.iter().map(comment_html).collect::<String>(),
            "next": next,
        })
        .to_string(),
        Err(message) => serde_json::json!({"html": "", "next": null, "error": message}).to_string(),
    }
}

pub(crate) fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if (c as u32) < 32 && c != '\n' => {},
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root{color-scheme:light dark;--bg:#f6f4ef;--fg:#1c1b19;--mut:#6b675f;--card:#fff;--bd:#e2ddd2;--gold:#b8892a}
@media (prefers-color-scheme:dark){:root{--bg:#161513;--fg:#ece8df;--mut:#a19b8f;--card:#201f1c;--bd:#34322d;--gold:#d6aa48}}
html,body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 "Segoe UI",system-ui,sans-serif}
header{position:sticky;top:0;z-index:5;display:flex;align-items:center;gap:18px;padding:9px 22px;background:var(--bg);border-bottom:1px solid var(--bd)}
.brand{display:flex;align-items:center;gap:8px;color:var(--fg);text-decoration:none;font-weight:600;font-size:19px;letter-spacing:.3px}
.brand svg{width:24px;height:24px}
.search{flex:1;max-width:580px;margin:0 auto;display:flex;gap:8px}
.search input{flex:1;min-width:0;font:inherit;padding:8px 16px;border-radius:999px;border:1px solid var(--bd);background:var(--card);color:var(--fg);caret-color:var(--gold)}
.search button{font:inherit;padding:8px 18px;border-radius:999px;border:0;background:var(--gold);color:#1b1608;font-weight:600;cursor:pointer}
.search{position:relative}
.sug{display:none;position:absolute;top:100%;left:0;right:0;margin-top:6px;background:var(--card);border:1px solid var(--bd);border-radius:14px;padding:6px 0;box-shadow:0 8px 24px rgba(0,0,0,.35);z-index:20;overflow:hidden}
.sug div{padding:7px 18px;cursor:pointer;font-size:15px}
.sug div.on,.sug div:hover{background:rgba(214,170,72,.16)}
main{max-width:1320px;margin:0 auto;padding:18px 22px 40px}
h1{font-size:20px;margin:12px 0 6px;line-height:1.3}
h2{font-size:15px;margin:0 0 12px;color:var(--mut);font-weight:600}
p{color:var(--mut);margin:6px 0}
.card{background:var(--card);border:1px solid var(--bd);border-radius:12px;padding:14px 18px;margin:16px 0}
.watch{display:grid;grid-template-columns:minmax(0,1fr) 340px;gap:26px;align-items:start}
@media (max-width:920px){.watch{grid-template-columns:minmax(0,1fr)}}
video{width:100%;max-height:74vh;background:#000;border-radius:12px;display:block;user-select:none;-webkit-user-select:none;cursor:pointer}
video::selection{background:transparent}
.row{display:flex;flex-wrap:wrap;align-items:center;gap:8px 18px;margin:6px 0 10px}
.chan{font-weight:600;color:var(--fg);text-decoration:none}
.chan:hover{color:var(--gold)}
.meta{color:var(--mut);font-size:14px}
.sp{flex:1}
.pick{font-size:14px;color:var(--mut)}
.qwrap{position:relative;display:flex;align-items:center;gap:8px}
.qbtn{font:inherit;font-size:14px;padding:5px 12px;border-radius:8px;border:1px solid var(--bd);background:var(--card);color:var(--fg);cursor:pointer;white-space:nowrap}
.qbtn:hover{border-color:var(--gold)}
.qmenu{position:absolute;right:0;top:100%;margin-top:6px;z-index:20;background:var(--card);border:1px solid var(--bd);border-radius:10px;padding:4px;min-width:140px;box-shadow:0 8px 24px rgba(0,0,0,.5)}
.qi{padding:7px 14px;border-radius:7px;cursor:pointer;white-space:nowrap;font-size:14px;color:var(--fg)}
.qi:hover{background:rgba(214,170,72,.15)}
.qi.on{color:var(--gold);font-weight:600}
select{font:inherit;font-size:14px;padding:5px 10px;border-radius:8px;border:1px solid var(--bd);background:var(--card);color:var(--fg);cursor:pointer}
a.btn{font:inherit;padding:7px 14px;border-radius:8px;border:1px solid var(--bd);background:var(--card);color:var(--fg);text-decoration:none;cursor:pointer}
a.gold{background:var(--gold);color:#1b1608;font-weight:600;border-color:var(--gold)}
a.plain{color:var(--mut);font-size:14px;text-decoration:none}
a.plain:hover{color:var(--gold)}
details.desc{background:var(--card);border:1px solid var(--bd);border-radius:12px;padding:10px 16px}
details.desc summary{cursor:pointer;font-weight:600;font-size:14px;color:var(--mut)}
.desc .text a{color:var(--gold);text-decoration:none}
.desc .text a:hover{text-decoration:underline}
.desc .text{white-space:pre-wrap;font-size:14px;margin-top:10px;word-break:break-word}
.err{font-family:Consolas,monospace;font-size:13px;word-break:break-word}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(250px,1fr));gap:20px}
a.vid{color:var(--fg);text-decoration:none;display:block}
.thumb{position:relative;aspect-ratio:16/9;background:#000;border-radius:10px;overflow:hidden}
.thumb img{width:100%;height:100%;object-fit:cover;display:block}
.dur{position:absolute;right:6px;bottom:6px;background:rgba(0,0,0,.8);color:#fff;font-size:12px;padding:1px 6px;border-radius:5px}
.vt{font-weight:600;font-size:14px;margin:7px 0 2px;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.vm{color:var(--mut);font-size:13px}
.bar{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin:0 0 10px}
.bl{color:var(--mut);font-size:13px;min-width:70px}
.chip{font:inherit;font-size:13px;padding:5px 13px;border-radius:999px;border:1px solid var(--bd);background:var(--card);color:var(--fg);cursor:pointer;text-decoration:none}
.chip:hover{border-color:var(--gold)}
.chip.on{background:var(--gold);color:#1a1a1a;border-color:var(--gold)}
.next a.vid{display:flex;gap:10px;margin-bottom:12px}
.next .thumb{width:158px;flex:none}
.next .vt{margin-top:0;font-size:13.5px}
.banner{display:block;width:100%;height:150px;object-fit:cover;border-radius:12px;background:var(--card)}
.chead{display:flex;gap:20px;align-items:center;margin:16px 0 8px}
.avatar{width:92px;height:92px;border-radius:50%;object-fit:cover;background:var(--card);flex:none}
.chead h1{margin:0;font-size:26px}
.cdesc{max-width:640px;font-size:14px;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.tabs{display:flex;gap:4px;border-bottom:1px solid var(--bd);margin:12px 0 18px}
.tabs button{font:inherit;background:none;border:0;border-bottom:2px solid transparent;color:var(--mut);padding:9px 18px;cursor:pointer}
.tabs button.on{color:var(--fg);border-bottom-color:var(--gold)}
.cm{padding:10px 0;border-bottom:1px solid var(--bd)}
.cm .meta{font-size:12px;color:var(--mut)}
.cm .meta b{color:var(--fg);font-size:13px}
.cm .text{white-space:pre-wrap;font-size:14px;margin-top:4px;word-break:break-word}
.cm .text a{color:var(--gold);text-decoration:none}
#cs{margin-top:14px}
#cs .more{margin:14px 0 0}
button.more{display:block;margin:26px auto 0;font:inherit;padding:9px 26px;border-radius:999px;border:1px solid var(--bd);background:var(--card);color:var(--fg);cursor:pointer}
button.more:hover{border-color:var(--gold)}
header{background:linear-gradient(180deg,rgba(214,170,72,.10),transparent 120%),var(--bg)}
.search input:focus{outline:0;border-color:var(--gold);box-shadow:0 0 0 3px rgba(214,170,72,.22)}
.thumb{transition:box-shadow .15s,transform .15s}
.thumb img{opacity:0;transition:opacity .25s}
.thumb img.ok{opacity:1}
a.vid:hover .thumb{box-shadow:0 8px 20px rgba(0,0,0,.32);transform:translateY(-2px)}
a.vid:hover .vt{color:var(--gold)}
.dur{backdrop-filter:none}
.watch.theater{grid-template-columns:minmax(0,1fr)}
.watch.theater video{max-height:84vh}
.sec{display:flex;align-items:center;gap:12px;margin:26px 0 12px}
.sec h2{margin:0}
.sc{position:relative}
.rmv{position:absolute;top:6px;left:6px;z-index:2;width:26px;height:26px;border-radius:50%;border:0;background:rgba(0,0,0,.72);color:#fff;font-size:16px;line-height:1;cursor:pointer;opacity:0;transition:opacity .12s}
.sc:hover .rmv{opacity:1}
#rs{margin:8px 0;display:flex;gap:8px;align-items:center}
.hero{text-align:center;margin-top:14vh}
.hero h1{font-size:42px;font-weight:500;letter-spacing:1px;margin:0}
.hero svg{width:60px;height:60px}
.pulse svg{animation:pulse 1.6s ease-in-out infinite}
@keyframes pulse{0%,100%{opacity:.45}50%{opacity:1}}
"#;

const FEATHER: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"#d6aa48\" fill-opacity=\"0.16\" stroke=\"#d6aa48\" stroke-width=\"1.5\" stroke-linejoin=\"round\" stroke-linecap=\"round\"><path d=\"M20.5 3.5c-6 0-11 3-13.5 8.5L5.5 16.5c-.4 1.1-.5 2.2-.4 3.3 1.1.1 2.2 0 3.3-.4C14 17.5 19.5 12.5 20.5 3.5z\"/><path d=\"M3.5 21L13 11.5\" fill=\"none\"/></svg>";

fn page(title: &str, body: &str, refresh: bool) -> String {
    page_with(title, "", false, body, refresh)
}

const SUGGEST_JS: &str = "(function(){var f=document.querySelector('.search'),i=f.querySelector('input'),box=document.createElement('div');box.className='sug';f.appendChild(box);\
var t=null,sel=-1,rows=[],typed='';\
function hide(){box.style.display='none';sel=-1;}\
function mark(){for(var n=0;n<rows.length;n++){rows[n].className=n===sel?'on':'';}}\
function show(list){box.innerHTML='';rows=[];if(!list.length){hide();return;}\
list.forEach(function(s){var d=document.createElement('div');d.textContent=s;d.onmousedown=function(e){e.preventDefault();i.value=s;f.submit();};box.appendChild(d);rows.push(d);});\
box.style.display='block';sel=-1;}\
i.addEventListener('input',function(){clearTimeout(t);typed=i.value.trim();if(!typed){hide();return;}\
var q=typed;t=setTimeout(function(){var x=new XMLHttpRequest();x.open('GET',SUG+'?q='+encodeURIComponent(q));\
x.onload=function(){try{if(i.value.trim()===q)show(JSON.parse(x.responseText));}catch(e){hide();}};x.onerror=hide;x.send();},110);});\
i.addEventListener('keydown',function(e){if(box.style.display!=='block')return;\
if(e.key==='ArrowDown'||e.key==='ArrowUp'){e.preventDefault();sel+=e.key==='ArrowDown'?1:-1;if(sel<-1)sel=rows.length-1;if(sel>=rows.length)sel=-1;\
i.value=sel<0?typed:rows[sel].textContent;mark();}\
else if(e.key==='Escape'){hide();}});\
i.addEventListener('blur',function(){setTimeout(hide,150);});})();";

/// Every Plume page has the same slim top bar: the name and a search box.
/// Thumbnails far from the screen are dropped from memory and come back when you scroll near
/// them; the "Show more" button presses itself when you get close to the bottom.
const LAZY_JS: &str = r#"(function(){var pending=false;
function run(){pending=false;var h=window.innerHeight||800,imgs=document.querySelectorAll('img[data-src]');
for(var i=0;i<imgs.length;i++){var im=imgs[i],r=im.getBoundingClientRect(),near=r.bottom>-1500&&r.top<h+1500;
if(near){if(!im.getAttribute('src')){im.onload=function(){this.className='ok';};im.setAttribute('src',im.getAttribute('data-src'));}}
else if(im.getAttribute('src')){im.removeAttribute('src');}}
var b=document.getElementById('m');
if(b&&!b.disabled&&b.style.display!=='none'&&b.getBoundingClientRect().top<h+900)b.click();}
function sched(){if(!pending){pending=true;setTimeout(run,60);}}
window.lazyCheck=sched;addEventListener('scroll',sched);addEventListener('resize',sched);
sched();setTimeout(sched,300);setTimeout(sched,1200);})();"#;

fn page_with(title: &str, query: &str, focus: bool, body: &str, refresh: bool) -> String {
    let suggest = streamproxy::global()
        .map(|proxy| format!("<script>var SUG='{}';{SUGGEST_JS}</script>", proxy.suggest_url()))
        .unwrap_or_default();
    let reload = if refresh {
        "<script>setTimeout(function(){location.reload()},350)</script>"
    } else {
        ""
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{} - Plume</title><style>{STYLE}</style></head><body>\
         <header><a class=\"brand\" href=\"https://www.youtube.com/\">{FEATHER}Plume</a>\
         <form action=\"https://www.youtube.com/results\" method=\"get\" class=\"search\">\
         <input name=\"search_query\" value=\"{}\" placeholder=\"Search YouTube\"{}>\
         <button type=\"submit\">Search</button></form>\
         <a class=\"plain\" href=\"https://music.youtube.com/\">Music</a></header>\
         <main>{body}</main>{suggest}<script>{LAZY_JS}</script>{reload}</body></html>",
        esc(title),
        esc(query),
        if focus { " autofocus" } else { "" }
    )
}

/// Escape `text` and turn `http(s)://...` words into links. Links go through the normal
/// navigation, so YouTube links open in Plume and everything else in Pteron.
fn linkify(text: &str) -> String {
    let safe = esc(text);
    let mut out = String::with_capacity(safe.len() + 64);
    let mut rest = safe.as_str();
    while let Some(at) = rest.find("http") {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        if !(tail.starts_with("http://") || tail.starts_with("https://")) {
            out.push_str("http");
            rest = &tail[4..];
            continue;
        }
        let end = tail.find(|c: char| c.is_whitespace() || c == '<').unwrap_or(tail.len());
        let mut link = &tail[..end];
        // trailing punctuation belongs to the sentence, not the address
        while link.ends_with(|c: char| matches!(c, '.' | ',' | ')' | ';' | ':' | '!' | '?')) {
            link = &link[..link.len() - 1];
        }
        if link.len() > 12 {
            out.push_str(&format!("<a href=\"{link}\">{link}</a>"));
        } else {
            out.push_str(link);
        }
        rest = &tail[link.len()..];
    }
    out.push_str(rest);
    out
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn real_url(request: &Request) -> String {
    match request {
        Request::Video(id) => watch_url(id),
        Request::Search(query, f) => {
            let mut link = format!(
                "https://www.youtube.com/results?search_query={}",
                url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
            );
            if *f != Filter::default() {
                link.push_str(&format!("&sort={}&date={}", f.sort, f.date));
            }
            link
        },
        Request::Channel(path) => format!("https://www.youtube.com{path}"),
    }
}

fn direct_link(request: &Request) -> String {
    format!(
        "<a class=\"plain\" href=\"{}#{FRAGMENT_DIRECT}\">Open on YouTube anyway (it will probably not work)</a>",
        esc(&real_url(request))
    )
}

/// The page shown while Pteron works. It asks the local helper whether the real page is ready
/// and reloads exactly once when it is, so nothing flashes.
pub(crate) fn loading_html(message: &str, key: &str) -> String {
    let body = format!(
        "<div class=\"hero\"><div class=\"pulse\">{FEATHER}</div><h1 style=\"font-size:22px\">{}</h1>\
         <p>This takes a few seconds.</p></div>",
        esc(message)
    );
    let script = match streamproxy::global() {
        Some(proxy) => format!(
            "<script>(function poll(){{var x=new XMLHttpRequest();x.open('GET','{}?k={}');\
             x.onload=function(){{if(x.responseText==='1')location.reload();else setTimeout(poll,250);}};\
             x.onerror=function(){{setTimeout(poll,1000);}};x.send();}})();</script>",
            proxy.ready_url(),
            esc(key)
        ),
        None => "<script>setTimeout(function(){location.reload()},2500)</script>".to_string(),
    };
    page_with("Getting the video", "", false, &format!("{body}{script}"), false)
}

fn error_html(request: &Request, message: &str) -> String {
    page(
        "Could not get this from YouTube",
        &format!(
            "<h1>Could not get this from YouTube</h1>\
             <div class=\"card\"><div class=\"err\">{}</div></div>\
             <p><a class=\"btn gold\" href=\"{}\">Try again</a></p><p>{}</p>",
            esc(message),
            esc(&real_url(request)),
            direct_link(request)
        ),
        false,
    )
}

fn format_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn format_views(views: u64) -> String {
    match views {
        v if v >= 1_000_000_000 => format!("{:.1}B views", v as f64 / 1e9),
        v if v >= 1_000_000 => format!("{:.1}M views", v as f64 / 1e6),
        v if v >= 1_000 => format!("{:.1}K views", v as f64 / 1e3),
        v => format!("{v} views"),
    }
}

/// How many hours ago an upload was, from YouTube's "3 years ago" / "Streamed 2 days ago" text.
fn age_hours(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let mut words = lower.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if let Ok(n) = word.parse::<u64>() {
            let unit = words.peek()?.trim_end_matches('s');
            let hours = match unit {
                "second" | "minute" => 0,
                "hour" => 1,
                "day" => 24,
                "week" => 24 * 7,
                "month" => 24 * 30,
                "year" => 24 * 365,
                _ => continue,
            };
            return Some(n * hours);
        }
    }
    None
}

/// Upload filter and sort bar for search results.
fn filter_bar(query: &str, current: Filter) -> String {
    let link = |label: &str, f: Filter| {
        let request = Request::Search(query.to_string(), f);
        let on = if f == current { " on" } else { "" };
        format!("<a class=\"chip{on}\" href=\"{}\">{label}</a>", esc(&real_url(&request)))
    };
    let sorts = [("Best match", 0), ("Newest", 2), ("Most views", 3), ("Top rated", 1)]
        .iter()
        .map(|(label, sort)| link(label, Filter { sort: *sort, date: current.date }))
        .collect::<String>();
    let dates = [("Any time", 0), ("Today", 2), ("This week", 3), ("This month", 4), ("This year", 5)]
        .iter()
        .map(|(label, date)| link(label, Filter { sort: current.sort, date: *date }))
        .collect::<String>();
    format!(
        "<div class=\"bar\"><span class=\"bl\">Sort</span>{sorts}\
         <button class=\"chip\" id=\"old\" type=\"button\">Oldest first</button></div>\
         <div class=\"bar\"><span class=\"bl\">Uploaded</span>{dates}</div>\
         <script>(function(){{var b=document.getElementById('old'),g=document.getElementById('g');if(!b||!g)return;\
         var on=false,busy=false,orig=null;\
         function apply(){{if(busy)return;busy=true;var k=[].slice.call(g.children);\
         if(!orig)orig=k.slice();\
         if(on){{k.sort(function(x,y){{var a=+x.getAttribute('data-a'),c=+y.getAttribute('data-a');\
         if(a<0&&c<0)return 0;if(a<0)return 1;if(c<0)return -1;return c-a;}});}}\
         else{{k.sort(function(x,y){{var i=orig.indexOf(x),j=orig.indexOf(y);return (i<0?1e9:i)-(j<0?1e9:j);}});}}\
         k.forEach(function(e){{g.appendChild(e);}});busy=false;}}\
         b.onclick=function(){{on=!on;b.className=on?'chip on':'chip';apply();}};\
         if(window.MutationObserver)new MutationObserver(function(m){{if(!busy&&on)apply();}}).observe(g,{{childList:true}});}})();</script>"
    )
}

fn card(item: &VideoItem) -> String {
    let title = if item.title.is_empty() { "(no title)" } else { item.title.as_str() };
    let mut meta = Vec::new();
    if !item.channel.is_empty() {
        meta.push(esc(&item.channel));
    }
    if !item.views.is_empty() {
        meta.push(esc(&item.views));
    }
    if !item.published.is_empty() {
        meta.push(esc(&item.published));
    }
    let age = age_hours(&item.published).map(|h| h.to_string()).unwrap_or_else(|| "-1".into());
    let duration = item
        .duration_secs
        .map(|d| format!("<span class=\"dur\">{}</span>", format_duration(d)))
        .unwrap_or_default();
    format!(
        "<a class=\"vid\" data-a=\"{age}\" href=\"{}\"><div class=\"thumb\"><img \
         data-src=\"https://i.ytimg.com/vi/{}/mqdefault.jpg\" alt=\"\">{duration}</div>\
         <div><div class=\"vt\">{}</div><div class=\"vm\">{}</div></div></a>",
        esc(&watch_url(&item.id)),
        esc(&item.id),
        esc(title),
        meta.join(" &middot; ")
    )
}

/// The next batch of a channel's videos, as JSON for the "Show more" button.
pub(crate) fn more_json(token: &str) -> String {
    match innertube::more(token) {
        Ok((items, next)) => serde_json::json!({
            "html": items.iter().map(card).collect::<String>(),
            "next": next,
        })
        .to_string(),
        Err(message) => serde_json::json!({"html": "", "next": null, "error": message}).to_string(),
    }
}

/// "Show more" button (it also presses itself near the bottom of the page) that appends the
/// next batch of cards to the grid with id `g`.
fn more_block(token: &str, endpoint: &str) -> String {
    if !innertube::valid_continuation(token) {
        return String::new();
    }
    format!(
        "<button class=\"more\" id=\"m\" data-t=\"{}\">Show more</button>\
         <script>(function(){{var b=document.getElementById('m'),g=document.getElementById('g'),fails=0;\
         b.onclick=function(){{b.disabled=true;b.textContent='Loading...';var x=new XMLHttpRequest();\
         x.open('GET','{endpoint}?c='+encodeURIComponent(b.getAttribute('data-t')));\
         x.onload=function(){{try{{var r=JSON.parse(x.responseText);g.insertAdjacentHTML('beforeend',r.html||'');\
         if(r.next){{b.setAttribute('data-t',r.next);b.disabled=false;b.textContent='Show more';}}else{{b.style.display='none';}}\
         if(window.lazyCheck)lazyCheck();}}\
         catch(e){{fails++;b.disabled=fails<3;b.textContent='Try again';}}}};\
         x.onerror=function(){{fails++;b.disabled=fails<3;b.textContent='Try again';}};x.send();}};}})();</script>",
        esc(token)
    )
}

pub(crate) fn search_more_json(token: &str) -> String {
    match innertube::search_more(token) {
        Ok((items, next)) => serde_json::json!({
            "html": items.iter().map(card).collect::<String>(),
            "next": next,
        })
        .to_string(),
        Err(message) => serde_json::json!({"html": "", "next": null, "error": message}).to_string(),
    }
}

/// A channel: banner, name, subscribers, then Videos and About tabs.
fn channel_html(request: &Request, channel: &innertube::Channel) -> String {
    let banner = if channel.banner.is_empty() {
        String::new()
    } else {
        format!("<img class=\"banner\" src=\"{}\" alt=\"\">", esc(&channel.banner))
    };
    let avatar = if channel.avatar.is_empty() {
        String::new()
    } else {
        format!("<img class=\"avatar\" src=\"{}\" alt=\"\">", esc(&channel.avatar))
    };
    let mut line = Vec::new();
    if !channel.handle.is_empty() {
        line.push(esc(&channel.handle));
    }
    if !channel.subscribers.is_empty() {
        line.push(esc(&channel.subscribers));
    }
    let short: String = channel.description.chars().take(300).collect();
    let cards: String = channel.videos.iter().map(card).collect();
    let videos = if channel.videos.is_empty() {
        format!("<div class=\"card\"><p>No videos found.</p></div><p>{}</p>", direct_link(request))
    } else {
        format!("<div class=\"grid\" id=\"g\">{cards}</div>")
    };
    let more = match (&channel.next, streamproxy::global()) {
        (Some(token), Some(proxy)) => more_block(token, &proxy.more_url()),
        _ => String::new(),
    };
    let about = if channel.description.trim().is_empty() {
        "<p>No description.</p>".to_string()
    } else {
        format!("<div class=\"card\"><div class=\"text\" style=\"white-space:pre-wrap;word-break:break-word\">{}</div></div>", linkify(&channel.description))
    };
    let body = format!(
        "{banner}<div class=\"chead\">{avatar}<div><h1>{}</h1><div class=\"meta\">{}</div>\
         <div class=\"cdesc\">{}</div></div></div>\
         <div class=\"tabs\"><button class=\"on\" id=\"tv\">Videos</button><button id=\"ta\">About</button></div>\
         <div id=\"pv\">{videos}{more}</div><div id=\"pa\" class=\"desc\" style=\"display:none\">{about}</div>\
         <script>(function(){{var tv=document.getElementById('tv'),ta=document.getElementById('ta'),pv=document.getElementById('pv'),pa=document.getElementById('pa');\
         function pick(a){{tv.className=a?'':'on';ta.className=a?'on':'';pv.style.display=a?'none':'block';pa.style.display=a?'block':'none';}}\
         tv.onclick=function(){{pick(false);}};ta.onclick=function(){{pick(true);}};}})();</script>",
        esc(&channel.title),
        line.join(" &middot; "),
        esc(&short),
    );
    page_with(&channel.title, "", false, &body, false)
}

/// Search results or a channel's videos.
fn results_html(request: &Request, heading: &str, items: &[VideoItem], next: Option<&str>) -> String {
    let query = match request {
        Request::Search(query, _) => query.as_str(),
        _ => "",
    };
    let bar = match request {
        Request::Search(query, f) => filter_bar(query, *f),
        _ => String::new(),
    };
    let cards: String = items.iter().map(card).collect();
    let body = if items.is_empty() {
        format!(
            "<h1>{}</h1>{bar}<div class=\"card\"><p>Nothing found.</p></div><p>{}</p>",
            esc(heading),
            direct_link(request)
        )
    } else {
        let more = match (next, streamproxy::global()) {
            (Some(token), Some(proxy)) => more_block(token, &proxy.search_more_url()),
            _ => String::new(),
        };
        format!("<h1>{}</h1>{bar}<div class=\"grid\" id=\"g\">{cards}</div>{more}", esc(heading))
    };
    page_with(heading, query, false, &body, false)
}

/// One of the home page's lists: a heading, a "clear" button and the saved videos.
fn saved_section(title: &str, kind: &str, items: &[VideoItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let cards: String = items
        .iter()
        .take(48)
        .map(|item| {
            let remove = if kind == "later" {
                format!("<button type=\"button\" class=\"rmv\" title=\"Remove\" data-id=\"{}\">&times;</button>", esc(&item.id))
            } else {
                String::new()
            };
            format!("<div class=\"sc\">{remove}{}</div>", card(item))
        })
        .collect();
    format!(
        "<div class=\"sec\" id=\"s-{kind}\"><h2>{title}</h2><button type=\"button\" class=\"qbtn\" data-clear=\"{kind}\">Clear</button></div>\
         <div class=\"grid\" id=\"g-{kind}\">{cards}</div>"
    )
}

fn home_html() -> String {
    let (later, history) = with_store(|s| (s.later.clone(), s.history.clone()), false);
    let memory = streamproxy::global().map(|p| p.memory_url()).unwrap_or_default();
    let sections = format!(
        "{}{}",
        saved_section("Watch later", "later", &later),
        saved_section("Recently watched", "history", &history)
    );
    let script = if sections.is_empty() || memory.is_empty() {
        String::new()
    } else {
        format!(
            "<script>(function(){{var M='{memory}';function mem(p){{var x=new XMLHttpRequest();x.open('GET',M+'?p='+encodeURIComponent(p));x.send();}}\
             document.addEventListener('click',function(e){{var t=e.target;\
             if(t.getAttribute&&t.getAttribute('data-clear')){{var k=t.getAttribute('data-clear');mem('clear:'+k);\
             var a=document.getElementById('s-'+k),b=document.getElementById('g-'+k);if(a)a.remove();if(b)b.remove();}}\
             else if(t.className==='rmv'){{e.preventDefault();e.stopPropagation();mem('later:rm:'+t.getAttribute('data-id'));t.parentNode.remove();}}}},true);}})();</script>"
        )
    };
    page_with(
        "YouTube",
        "",
        true,
        &format!(
            "<div class=\"hero\" style=\"margin-top:{}\">{FEATHER}<h1>Plume</h1>\
             <p>YouTube without the clutter. Search above, or paste a video link into the address bar.</p></div>{sections}{script}",
            if sections.is_empty() { "14vh" } else { "5vh" }
        ),
        false,
    )
}

/// One quality the player can switch to.
#[derive(Debug, PartialEq)]
pub(crate) struct Choice {
    pub label: String,
    /// The whole file.
    pub url: String,
    /// The first couple of minutes only (ready much sooner); the same as `url` for short videos.
    pub part: String,
    /// `<token>_<itag>`, used to ask the helper whether the whole file is ready.
    pub key: String,
    pub height: u32,
    pub fps: u32,
}

/// The quality to start with: the best up to 720p at 30 frames per second (light on memory and
/// processor).
fn default_index(choices: &[Choice]) -> usize {
    choices
        .iter()
        .position(|choice| choice.height == 720 && choice.fps > 30)
        .or_else(|| choices.iter().position(|choice| choice.height <= 720 && choice.fps <= 30))
        .or_else(|| choices.iter().position(|choice| choice.height <= 720))
        .unwrap_or(choices.len().saturating_sub(1))
}

/// The page's player logic. It starts the first couple of minutes of a long video at once, asks
/// the helper (in the background) for the whole file, and swaps to it at the current position
/// when it is ready. Changing quality works the same way.
const PLAYER_JS: &str = r#"
(function(){
var FULL='@FULL@',MEM='@MEM@';
var q=document.getElementById('q'),e=document.getElementById('e'),slow=null,gen=0,part=false,want=null,fsAt=0;
function note(m){if(!MEM)return;var x=new XMLHttpRequest();x.open('GET',MEM+'?p='+encodeURIComponent('note:'+m));x.send();}
function say(m){e.textContent=m;e.style.display=m?'block':'none';}
function cur(){return document.getElementById('v');}
function opt(){return q.options[q.selectedIndex];}
function whole(o){return o.getAttribute('data-p')===o.value;}
function stop(v){try{v.pause();v.removeAttribute('src');v.load();}catch(x){}}
function ask(key,cb){var x=new XMLHttpRequest();x.open('GET',FULL+'?k='+encodeURIComponent(key));x.onload=function(){cb(x.responseText==='1');};x.onerror=function(){cb(false);};x.send();}
function waitFull(o,my,done){var key=o.getAttribute('data-k');(function poll(){if(my!==gen)return;ask(key,function(ok){if(my!==gen)return;if(ok)done();else setTimeout(poll,400);});})();}
function hook(v){
  v.addEventListener('selectstart',function(x){x.preventDefault();});
  v.addEventListener('dragstart',function(x){x.preventDefault();});
  v.addEventListener('pteron-fullscreen',function(){fsAt=Date.now();});
  v.addEventListener('click',function(x){
    if(Date.now()-fsAt<900)return;
    var r=v.getBoundingClientRect();
    if(x.clientY-r.top<56)return;
    if(v.paused){v.play();}else{v.__userPaused=Date.now();v.pause();}
  });
  function size(){var rs=document.getElementById('res');if(rs&&v.videoWidth)rs.textContent=v.videoWidth+'\u00D7'+v.videoHeight;}
  v.addEventListener('loadedmetadata',size);v.addEventListener('resize',size);
  v.addEventListener('playing',function(){size();note('playing '+v.videoWidth+'x'+v.videoHeight+' as '+opt().text);},{once:true});
  v.addEventListener('pteron-seek',function(x){
    if(!part)return;
    want=x.detail;var o=opt(),my=++gen;say('Loading the rest of the video...');
    waitFull(o,my,function(){say('');swap(o.value,my,true);});
  });
  v.addEventListener('ended',function(){
    var o=opt();
    if(part&&!whole(o)){var my=++gen;say('Loading the rest of the video...');waitFull(o,my,function(){say('');swap(o.value,my,true);});}
  });
  v.onerror=function(){clearTimeout(slow);say('Playback failed'+(v.error?' (code '+v.error.code+(v.error.message?': '+v.error.message:'')+')':'')+'. Try another quality, or reload the page.');};
  var ok=function(){clearTimeout(slow);say('');};
  v.addEventListener('playing',ok);v.addEventListener('canplay',ok);
  clearTimeout(slow);
  slow=setTimeout(function(){if(v.readyState<3&&!v.error)say('This is taking a long time. If it does not start, choose a lower quality: this connection or computer may be too slow for '+opt().text+'.');},15000);
}
function swap(src,my,forcePlay){
  // Same video element (so its controls, volume and listeners stay): new source, same spot.
  var v=cur();if(!v||my!==gen)return;
  var t=want!==null?want:(v.currentTime||0),playing=forcePlay||!v.paused;
  want=null;part=false;v.removeAttribute('data-partial');
  v.addEventListener('loadedmetadata',function f(){
    v.removeEventListener('loadedmetadata',f);
    try{if(t>0.05)v.currentTime=t;}catch(x){}
    if(playing){var p=v.play();if(p&&p.catch)p.catch(function(){});}
  });
  v.src=src;
}
function upgrade(my){
  var o=opt();
  if(whole(o)){part=false;return;}
  part=true;
  waitFull(o,my,function(){swap(o.value,my,false);});
}
q.onchange=function(){
  var o=opt(),old=cur(),t=old.currentTime||0,my=++gen;
  say('');
  if(whole(o)||t<100){
    var n=document.createElement('video');n.id='v';n.controls=true;n.autoplay=!old.paused||t===0;var dd=old.getAttribute('data-duration');if(dd)n.setAttribute('data-duration',dd);
    try{n.volume=old.volume;n.muted=old.muted;}catch(x){}
    if(old.poster)n.poster=old.poster;
    n.src=o.getAttribute('data-p');
    var dd2=old.getAttribute('data-duration');if(dd2)n.setAttribute('data-duration',dd2);
    if(!whole(o))n.setAttribute('data-partial','1');
    if(t>1)n.addEventListener('loadedmetadata',function f(){n.removeEventListener('loadedmetadata',f);try{n.currentTime=t;}catch(x){}});
    stop(old);
    old.parentNode.replaceChild(n,old);
    hook(n);upgrade(my);
  }else{
    say('Getting '+o.text+' ready. The video keeps playing...');
    waitFull(o,my,function(){say('');swap(o.value,my,false);});
  }
};
document.addEventListener('keydown',function(x){
  var t=x.target&&x.target.tagName;if(t==='INPUT'||t==='SELECT'||t==='TEXTAREA'||x.ctrlKey||x.altKey||x.metaKey)return;
  var v=cur();
  if(x.key===' '||x.key==='k'){x.preventDefault();if(v.paused){v.play();}else{v.__userPaused=Date.now();v.pause();}}
  else if(x.key==='ArrowRight'){x.preventDefault();v.currentTime=Math.min(v.duration||1e9,v.currentTime+5);}
  else if(x.key==='ArrowLeft'){x.preventDefault();v.currentTime=Math.max(0,v.currentTime-5);}
  else if(x.key==='m'){v.muted=!v.muted;}
});
var saveT=null;
function remember(v){
  if(!MEM)return;
  v.addEventListener('volumechange',function(){
    clearTimeout(saveT);
    saveT=setTimeout(function(){var x=new XMLHttpRequest();x.open('GET',MEM+'?p='+encodeURIComponent(v.volume.toFixed(2)+','+(v.muted?1:0)));x.send();},500);
  });
}
var baseHook=hook;
hook=function(v){baseHook(v);remember(v);};
var qb=document.getElementById('qb'),qm=document.getElementById('qm');
function qdraw(){
  qb.textContent=opt().text+' \u25BE';qm.innerHTML='';
  for(var i=0;i<q.options.length;i++){
    var d=document.createElement('div');d.className='qi'+(i===q.selectedIndex?' on':'');d.textContent=q.options[i].text;
    (function(i){d.onclick=function(x){x.stopPropagation();qm.style.display='none';if(i!==q.selectedIndex){q.selectedIndex=i;qdraw();q.onchange();}};})(i);
    qm.appendChild(d);
  }
}
qb.onclick=function(x){x.stopPropagation();qm.style.display=qm.style.display==='none'?'block':'none';};
document.addEventListener('click',function(){qm.style.display='none';});
qdraw();
// ---- Pteron: speed, theater mode, watch later, history and resume
var rate=1,told=false;
function mem(p,cb){if(!MEM){if(cb)cb('');return;}var x=new XMLHttpRequest();x.open('GET',MEM+'?p='+encodeURIComponent(p));x.onload=function(){if(cb)cb(x.responseText);};x.onerror=function(){if(cb)cb('');};x.send();}
function clock(s){s=Math.max(0,Math.floor(s||0));var m=Math.floor(s/60),r=s%60,h=Math.floor(m/60);m=m%60;return h?h+':'+(m<10?'0':'')+m+':'+(r<10?'0':'')+r:m+':'+(r<10?'0':'')+r;}
var sb=document.getElementById('sb'),sm=document.getElementById('sm'),RATES=[0.5,0.75,1,1.25,1.5,1.75,2];
function setRate(r){rate=r;try{cur().playbackRate=r;}catch(x){}sb.textContent=(r===1?'1x':r+'x')+' \u25BE';sm.innerHTML='';
  RATES.forEach(function(v){var d=document.createElement('div');d.className='qi'+(v===r?' on':'');d.textContent=v===1?'Normal':v+'x';d.onclick=function(x){x.stopPropagation();sm.style.display='none';setRate(v);};sm.appendChild(d);});}
function stepRate(k){var i=RATES.indexOf(rate)+k;if(i>=0&&i<RATES.length)setRate(RATES[i]);}
sb.onclick=function(x){x.stopPropagation();qm.style.display='none';sm.style.display=sm.style.display==='none'?'block':'none';};
document.addEventListener('click',function(){sm.style.display='none';});
setRate(1);
var th=document.getElementById('th'),wrap=document.querySelector('.watch');
function toggleTheater(){var on=wrap.className.indexOf('theater')<0;wrap.className=on?'watch theater':'watch';th.textContent=on?'Normal view':'Theater';}
th.onclick=toggleTheater;
var wl=document.getElementById('wl');
function wlPaint(on){wl.textContent=on?'Saved \u2713':'Watch later';}
wl.onclick=function(){mem('later:'+JSON.stringify(VID),function(r){wlPaint(r==='1');});};
mem('later:has:'+VID.id,function(r){wlPaint(r==='1');});
function dlButton(id,kind,url){
  var b=document.getElementById(id),old=b.textContent;
  b.onclick=function(){
    var u=url();if(!u)return;
    mem('dl:'+kind+'|'+u+'|'+VID.title.replace(/\|/g,' ')+'|'+(VID.channel||'').replace(/\|/g,' '),function(r){
      b.textContent=r==='1'?'Started \u2713':'Could not start';
      setTimeout(function(){b.textContent=old;},2500);});};
}
dlButton('dv','video',function(){return q.value;});
dlButton('da','audio',function(){return AUDIO;});
var rs=document.getElementById('rs'),rsb=document.getElementById('rsb');
mem('pos:get:'+VID.id,function(r){var s=parseInt(r,10);
  if(s>=15&&(!VID.duration||s<VID.duration-20)){rs.style.display='flex';rsb.textContent='Resume from '+clock(s);
    rsb.onclick=function(){try{cur().currentTime=s;cur().play();}catch(x){}rs.style.display='none';};
    document.getElementById('rso').onclick=function(){rs.style.display='none';};
    setTimeout(function(){rs.style.display='none';},20000);}});
function track(v){
  v.playbackRate=rate;
  v.addEventListener('loadedmetadata',function(){v.playbackRate=rate;});
  v.addEventListener('playing',function(){if(!told){told=true;mem('hist:'+JSON.stringify(VID));}});
  var last=0;
  v.addEventListener('timeupdate',function(){var n=Date.now();if(n-last<5000)return;last=n;if(v.currentTime>=15)mem('pos:'+VID.id+','+Math.floor(v.currentTime));});
  v.addEventListener('ended',function(){mem('pos:'+VID.id+',0');});
}
var hookBase=hook;
hook=function(v){hookBase(v);track(v);};
document.addEventListener('keydown',function(x){
  var t=x.target&&x.target.tagName;if(t==='INPUT'||t==='SELECT'||t==='TEXTAREA'||x.ctrlKey||x.altKey||x.metaKey)return;
  var v=cur();
  if(x.key==='j'){v.currentTime=Math.max(0,v.currentTime-10);}
  else if(x.key==='l'){v.currentTime=Math.min(v.duration||1e9,v.currentTime+10);}
  else if(x.key==='t'){toggleTheater();}
  else if(x.key==='>'){stepRate(1);}
  else if(x.key==='<'){stepRate(-1);}
  else if(/^[0-9]$/.test(x.key)&&v.duration){v.currentTime=v.duration*parseInt(x.key,10)/10;}
});
hook(cur());
if(MEM){var mx=new XMLHttpRequest();mx.open('GET',MEM+'?p=get');mx.onload=function(){
  var s=(mx.responseText||'').split(',');var vol=parseFloat(s[0]);
  if(vol>=0&&vol<=1){var v=cur();try{v.volume=vol;v.muted=s[1]==='1';}catch(x){}}
};mx.send();}
upgrade(++gen);
})();
"#;

fn player_html(id: &str, info: &innertube::PlayerInfo, choices: &[Choice], related: &[VideoItem]) -> String {
    let title = if info.title.is_empty() { "YouTube video" } else { info.title.as_str() };
    let description: String = info.description.chars().take(2500).collect();
    let start = default_index(choices);
    let options: String = choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            format!(
                "<option value=\"{}\" data-p=\"{}\" data-k=\"{}\"{}>{}</option>",
                esc(&choice.url),
                esc(&choice.part),
                esc(&choice.key),
                if index == start { " selected" } else { "" },
                esc(&choice.label)
            )
        })
        .collect();
    let partial_attr = if choices[start].part != choices[start].url { " data-partial=\"1\"" } else { "" };
    let poster = Some(info.thumbnail.as_str())
        .filter(|url| url.starts_with("https://"))
        .map(|url| format!(" poster=\"{}\"", esc(url)))
        .unwrap_or_default();
    let author = if info.author.is_empty() {
        String::new()
    } else if info.channel_id.is_empty() {
        format!("<span class=\"chan\">{}</span>", esc(&info.author))
    } else {
        format!(
            "<a class=\"chan\" href=\"https://www.youtube.com/channel/{}\">{}</a>",
            esc(&info.channel_id),
            esc(&info.author)
        )
    };
    let mut meta = Vec::new();
    if let Some(views) = info.views {
        meta.push(format_views(views));
    }
    if !info.published.is_empty() {
        meta.push(esc(&info.published));
    }
    if info.duration_secs > 0 {
        meta.push(format_duration(info.duration_secs));
    }
    let next = if related.is_empty() {
        String::new()
    } else {
        format!(
            "<aside class=\"next\"><h2>Up next</h2>{}</aside>",
            related.iter().map(card).collect::<String>()
        )
    };
    let description_block = if description.trim().is_empty() {
        String::new()
    } else {
        format!("<details class=\"desc\"><summary>Description</summary><div class=\"text\">{}</div></details>", linkify(&description))
    };
    let full_endpoint = streamproxy::global().map(|p| p.full_url()).unwrap_or_default();
    let memory_endpoint = streamproxy::global().map(|p| p.memory_url()).unwrap_or_default();
    let vid = serde_json::to_string(&item_to_json(&VideoItem {
        id: id.to_string(),
        title: title.to_string(),
        channel: info.author.clone(),
        duration_secs: Some(info.duration_secs),
        views: String::new(),
        published: String::new(),
    }))
    .unwrap_or_else(|_| "{}".into())
    .replace('<', "\\u003c");
    let audio_url = choices[start].url.rsplit_once('/').map(|(base, _)| format!("{base}/0.mp4")).unwrap_or_default();
    let script = format!(
        "var VID={vid};var AUDIO='{}';{}",
        esc(&audio_url),
        PLAYER_JS.replace("@FULL@", &full_endpoint).replace("@MEM@", &memory_endpoint)
    );
    let comments_endpoint = streamproxy::global().map(|p| p.comments_url()).unwrap_or_default();
    let comments_block = format!(
        "<div id=\"cs\"><button class=\"more\" id=\"cb\" data-t=\"{id}\">Show comments</button>\
         <div id=\"cl\"></div></div>\
         <script>(function(){{var b=document.getElementById('cb'),l=document.getElementById('cl');\
         b.onclick=function(){{b.disabled=true;b.textContent='Loading comments...';var x=new XMLHttpRequest();\
         x.open('GET','{comments_endpoint}?c='+encodeURIComponent(b.getAttribute('data-t')));\
         x.onload=function(){{try{{var r=JSON.parse(x.responseText);\
         if(r.error){{b.disabled=false;b.textContent='Show comments';var m=document.createElement('p');m.className='meta';m.textContent=r.error;l.innerHTML='';l.appendChild(m);return;}}\
         l.insertAdjacentHTML('beforeend',r.html||'');\
         if(r.next){{b.setAttribute('data-t',r.next);b.disabled=false;b.textContent='Show more comments';}}else{{b.style.display='none';}}}}\
         catch(e){{b.disabled=false;b.textContent='Try again';}}}};\
         x.onerror=function(){{b.disabled=false;b.textContent='Try again';}};x.send();}};}})();</script>",
        id = esc(id),
    );
    let body = format!(
        "<div class=\"watch\"><div><video id=\"v\" controls autoplay data-duration=\"{}\"{partial_attr}{poster} src=\"{}\"></video>\
         <p id=\"e\" class=\"err\" style=\"display:none\"></p>\
         <div id=\"rs\" style=\"display:none\"><button type=\"button\" id=\"rsb\" class=\"qbtn\"></button><button type=\"button\" id=\"rso\" class=\"qbtn\">Start over</button></div>\
         <h1>{}</h1>\
         <div class=\"row\">{author}<span class=\"meta\">{}</span><span class=\"sp\"></span>\
         <button type=\"button\" id=\"dv\" class=\"qbtn\" title=\"Save this video (.mp4) to your Downloads folder\">Download video</button>\
         <button type=\"button\" id=\"da\" class=\"qbtn\" title=\"Save only the sound as an MP3 in your Downloads folder\">Download MP3</button>\
         <button type=\"button\" id=\"wl\" class=\"qbtn\">Watch later</button><button type=\"button\" id=\"th\" class=\"qbtn\">Theater</button>\
         <div class=\"pick qwrap\"><span id=\"res\" class=\"meta\"></span><span>Speed</span><button type=\"button\" id=\"sb\" class=\"qbtn\"></button>\
         <div id=\"sm\" class=\"qmenu\" style=\"display:none\"></div>\
         <span>Quality</span><button type=\"button\" id=\"qb\" class=\"qbtn\"></button>\
         <div id=\"qm\" class=\"qmenu\" style=\"display:none\"></div><select id=\"q\" style=\"display:none\">{options}</select></div>\
         <a class=\"plain\" href=\"{}#{FRAGMENT_DIRECT}\">Open on YouTube</a></div>\
         {description_block}{comments_block}</div>{next}</div>\
         <script>{script}</script>",
        info.duration_secs,
        esc(&choices[start].part),
        esc(title),
        meta.join(" &middot; "),
        watch_url(id),
    );
    page_with(title, "", false, &body, false)
}

fn video_page(id: &str) -> String {
    let request = Request::Video(id.to_string());
    let started = std::time::Instant::now();
    let related_id = id.to_string();
    let related_job = std::thread::spawn(move || innertube::related(&related_id).unwrap_or_default());
    let info = match innertube::player(id) {
        Ok(info) => {
            crate::crashlog::note(&format!("video {id}: stream list in {} ms", started.elapsed().as_millis()));
            info
        },
        Err(message) => return error_html(&request, &message),
    };
    let Some(proxy) = streamproxy::global() else {
        return error_html(&request, "Pteron could not start its local video helper.");
    };
    let token = proxy.register(info.set.clone());
    // Long videos start from their first couple of minutes (ready almost at once); the page
    // swaps to the whole file in the background.
    let long = crate::pteron::settings().fast_start && info.set.duration_ms as f64 / 1000.0 > streamproxy::PARTIAL_SECS + 30.0;
    let choices: Vec<Choice> = info
        .set
        .videos
        .iter()
        .map(|video| Choice {
            label: if video.fps > 30 { format!("{}p{}", video.height, video.fps) } else { format!("{}p", video.height) },
            url: proxy.url(&token, video.itag),
            part: if long { proxy.partial_url(&token, video.itag) } else { proxy.url(&token, video.itag) },
            key: format!("{token}_{}", video.itag),
            height: video.height,
            fps: video.fps,
        })
        .collect();
    if let Some(video) = info.set.videos.get(default_index(&choices)) {
        let prep = std::time::Instant::now();
        let result = proxy.prepare(&token, video.itag, long);
        crate::crashlog::note(&format!(
            "video {id}: {} layout in {} ms",
            if long { "first-part" } else { "file" },
            prep.elapsed().as_millis()
        ));
        if let Err(message) = result {
            crate::crashlog::note(&format!("video helper: could not prepare {id}: {message}"));
            return error_html(&request, &format!("Pteron could not prepare this video: {message}"));
        }
    }
    let related = related_job.join().unwrap_or_default();
    player_html(id, &info, &choices, &related)
}

fn fetch(request: &Request) {
    let key = request.key();
    let html = match request {
        Request::Video(id) => video_page(id),
        Request::Search(query, f) => match innertube::search(query, &innertube::search_params(f.sort, f.date)) {
            Ok((items, next)) => results_html(request, &format!("Results for \"{query}\""), &items, next.as_deref()),
            Err(message) => error_html(request, &message),
        },
        Request::Channel(path) => match innertube::channel(path) {
            Ok(channel) => channel_html(request, &channel),
            Err(message) => error_html(request, &message),
        },
    };
    write_page(&key, &html);
}

pub(crate) fn page_path(key: &str) -> Option<PathBuf> {
    if !valid_key(key) {
        return None;
    }
    let dir = player_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{key}.html")))
}

/// Pages older than two days are removed, so the folder never grows.
fn remove_old_pages() {
    let Some(dir) = player_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let limit = std::time::Duration::from_secs(2 * 24 * 3600);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "html") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) fn write_page(key: &str, html: &str) {
    remove_old_pages();
    match page_path(key) {
        Some(path) => {
            if let Err(error) = std::fs::write(&path, html) {
                warn!("Could not write {}: {error}", path.display());
            }
        },
        None => warn!("No place to write the YouTube page"),
    }
}

pub(crate) fn spawn_job(key: &str, job: impl FnOnce() + Send + 'static) {
    if !begin_job(key) {
        return;
    }
    let key = key.to_string();
    std::thread::Builder::new()
        .name("pteron-youtube".to_string())
        .spawn(move || {
            job();
            end_job(&key);
        })
        .ok();
}

fn start(request: Request, message: &str) -> Option<Url> {
    let key = request.key();
    let path = page_path(&key)?;
    let already_running = IN_FLIGHT
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|set| set.contains(&key));
    if !already_running {
        write_page(&key, &loading_html(message, &key));
        spawn_job(&key, move || fetch(&request));
    }
    Url::from_file_path(path).ok()
}

/// Handle a navigation that `classify` matched. Returns the local page to show now.
pub(crate) fn handle(action: Action) -> Option<Url> {
    match action {
        Action::Music(list) => {
            let path = page_path("music")?;
            write_page("music", &crate::music::page_html());
            let mut url = Url::from_file_path(path).ok()?;
            if let Some(list) = list {
                url.set_fragment(Some(&format!("list={list}")));
            }
            Some(url)
        },
        Action::Home => {
            let path = page_path("home")?;
            write_page("home", &home_html());
            Url::from_file_path(path).ok()
        },
        Action::Open(request) => {
            let message = match request {
                Request::Video(_) => "Getting the video...",
                Request::Search(..) => "Searching YouTube...",
                Request::Channel(_) => "Loading the channel...",
            };
            start(request, message)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(s: &str) -> Option<Action> {
        classify(&Url::parse(s).unwrap())
    }

    fn video(id: &str) -> Option<Action> {
        Some(Action::Open(Request::Video(id.into())))
    }

    #[test]
    fn watch_later_history_and_resume() {
        let item = |id: &str| VideoItem {
            id: id.into(), title: "T".into(), channel: "C".into(), duration_secs: Some(60), views: String::new(), published: String::new(),
        };
        let mut s = Store::default();
        assert!(s.toggle_later(item("aaaaaaaaaaa")));
        assert!(s.toggle_later(item("bbbbbbbbbbb")));
        assert_eq!(s.later[0].id, "bbbbbbbbbbb");
        assert!(!s.toggle_later(item("aaaaaaaaaaa")));
        assert_eq!(s.later.len(), 1);
        s.add_history(item("aaaaaaaaaaa"));
        s.add_history(item("bbbbbbbbbbb"));
        s.add_history(item("aaaaaaaaaaa"));
        assert_eq!(s.history.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["aaaaaaaaaaa", "bbbbbbbbbbb"]);
        for i in 0..200 {
            s.add_history(item(&format!("{i:011}")));
        }
        assert_eq!(s.history.len(), MAX_HISTORY);
        s.set_position("aaaaaaaaaaa", 120);
        s.set_position("bbbbbbbbbbb", 5);
        assert_eq!((s.position("aaaaaaaaaaa"), s.position("bbbbbbbbbbb")), (120, 0));
        s.set_position("aaaaaaaaaaa", 0);
        assert_eq!(s.position("aaaaaaaaaaa"), 0);
        s.set_position("ccccccccccc", 99);
        let back = Store::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(Store::from_json("nonsense"), Store::default());
        let junk = Store::from_json(r#"{"later":[{"id":"bad"},{"id":"ddddddddddd","title":"x\u0007y"}],"positions":[["zz",5],["eeeeeeeeeee",7]]}"#);
        assert_eq!(junk.later.len(), 1);
        assert_eq!(junk.later[0].title, "xy");
        assert_eq!(junk.positions, vec![("eeeeeeeeeee".to_string(), 7)]);
    }

    #[test]
    fn saved_lists_render_and_escape() {
        let items = vec![VideoItem { id: "aaaaaaaaaaa".into(), title: "<i>x</i>".into(), channel: String::new(), duration_secs: None, views: String::new(), published: String::new() }];
        let html = saved_section("Watch later", "later", &items);
        assert!(html.contains("&lt;i&gt;x") && html.contains("data-clear=\"later\"") && html.contains("class=\"rmv\""));
        assert!(!saved_section("Recently watched", "history", &items).contains("rmv"));
        assert_eq!(saved_section("x", "later", &[]), "");
    }

    #[test]
    fn search_filters() {
        assert_eq!(innertube::search_params(0, 0), "");
        assert_eq!(innertube::search_params(3, 0), "CAMSAhAB");
        assert_eq!(innertube::search_params(2, 3), "CAISBAgDEAE=");
        assert_eq!(age_hours("3 years ago"), Some(3 * 8760));
        assert_eq!(age_hours("Streamed 2 days ago"), Some(48));
        assert_eq!(age_hours("5 minutes ago"), Some(0));
        assert_eq!(age_hours("1.5M views"), None);
        assert_eq!(age_hours(""), None);
        assert_eq!(
            watch("https://www.youtube.com/results?search_query=x&sort=3&date=9"),
            Some(Action::Open(Request::Search("x".into(), Filter { sort: 3, date: 0 })))
        );
        assert!(real_url(&Request::Search("x".into(), Filter { sort: 2, date: 4 })).ends_with("&sort=2&date=4"));
    }

    #[test]
    fn classifies_links() {
        assert_eq!(watch("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=5"), video("dQw4w9WgXcQ"));
        assert_eq!(watch("https://youtu.be/dQw4w9WgXcQ?si=x"), video("dQw4w9WgXcQ"));
        assert_eq!(watch("https://m.youtube.com/shorts/dQw4w9WgXcQ"), video("dQw4w9WgXcQ"));
        assert_eq!(watch("https://www.youtube.com/"), Some(Action::Home));
        assert_eq!(watch("https://www.youtube.com/feed/trending"), Some(Action::Home));
        assert_eq!(
            watch("https://www.youtube.com/results?search_query=8+bit+ryan"),
            Some(Action::Open(Request::Search("8 bit ryan".into(), Filter::default())))
        );
        assert_eq!(watch("https://www.youtube.com/results?search_query="), Some(Action::Home));
        assert_eq!(
            watch("https://www.youtube.com/@8BitRyan"),
            Some(Action::Open(Request::Channel("/@8BitRyan".into())))
        );
        assert_eq!(
            watch("https://www.youtube.com/channel/UCabc123/videos"),
            Some(Action::Open(Request::Channel("/channel/UCabc123".into())))
        );
        assert_eq!(watch("https://www.youtube.com/watch?v=short"), None);
        assert_eq!(watch("https://www.youtube.com/watch?v=dQw4w9WgXcQ#pteron-direct"), None);
        assert_eq!(watch("https://www.youtube.com/results?search_query=x#pteron-direct"), None);
        assert_eq!(watch("https://www.youtube.com/playlist?list=PLx"), None);
        assert_eq!(watch("https://evil.example/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(watch("https://www.youtube.com.evil.example/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(watch("https://pteron.internal/install-ytdlp#dQw4w9WgXcQ"), None);
    }

    #[test]
    fn music_links_open_plume_music() {
        assert_eq!(watch("https://music.youtube.com/"), Some(Action::Music(None)));
        assert_eq!(watch("https://music.youtube.com/watch?v=dQw4w9WgXcQ"), Some(Action::Music(None)));
        assert_eq!(
            watch("https://music.youtube.com/playlist?list=OLAK5uy_kMn0lyHcSJ5oQ4-1bhZeGfbMCEyA"),
            Some(Action::Music(Some("OLAK5uy_kMn0lyHcSJ5oQ4-1bhZeGfbMCEyA".into())))
        );
        assert_eq!(
            watch("https://www.youtube.com/playlist?list=PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf"),
            Some(Action::Music(Some("PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf".into())))
        );
        assert_eq!(watch("https://music.youtube.com/playlist?list=bad;id"), Some(Action::Music(None)));
        assert_eq!(watch("https://music.youtube.com.evil.example/"), None);
    }

    #[test]
    fn rejects_bad_ids_and_paths() {
        assert!(valid_video_id("dQw4w9WgXcQ"));
        assert!(!valid_video_id("dQw4w9WgXc;"));
        assert!(!valid_video_id("../../etc/p"));
        assert_eq!(channel_path("/@ok.name"), Some("/@ok.name".into()));
        assert_eq!(channel_path("/@a;b"), None);
        assert_eq!(channel_path("/random"), None);
    }

    #[test]
    fn keys_are_stable_and_safe() {
        let a = Request::Search("hello world".into(), Filter::default()).key();
        assert_eq!(a, Request::Search("hello world".into(), Filter::default()).key());
        assert_ne!(a, Request::Search("hello worlds".into(), Filter::default()).key());
        assert_ne!(a, Request::Search("hello world".into(), Filter { sort: 2, date: 0 }).key());
        assert!(valid_key(&a));
        assert!(valid_key(&Request::Channel("/@x".into()).key()));
    }

    #[test]
    fn escapes_html() {
        assert_eq!(esc("<b>\"x\" & 'y'</b>"), "&lt;b&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/b&gt;");
    }

    fn info() -> innertube::PlayerInfo {
        innertube::PlayerInfo {
            channel_id: "UCabc".into(),
            published: "2024-05-17".into(),
            title: "Hello <script>alert(1)</script>".into(),
            author: "Chan".into(),
            description: "line1\nline2".into(),
            duration_secs: 212,
            thumbnail: "https://i.ytimg.com/vi/x/hq.jpg".into(),
            views: Some(1500),
            set: streamproxy::VideoSet {
                videos: vec![],
                audio: streamproxy::StreamInfo { itag: 140, url: String::new(), index_end: 0, height: 0, fps: 0 },
                duration_ms: 212_000,
            },
        }
    }

    fn choices() -> Vec<Choice> {
        [1080u32, 720, 360]
            .iter()
            .map(|h| Choice {
                label: format!("{h}p"),
                url: format!("http://127.0.0.1:9/v/t/{h}.mp4"),
                part: format!("http://127.0.0.1:9/v/t/{h}p.mp4"),
                key: format!("t_{h}"),
                height: *h,
                fps: 30,
            })
            .collect()
    }

    #[test]
    fn channel_page() {
        let channel = innertube::Channel {
            title: "8-Bit <Ryan>".into(),
            handle: "@8BitRyan".into(),
            avatar: "https://yt3.googleusercontent.com/a".into(),
            banner: "https://yt3.googleusercontent.com/b".into(),
            subscribers: "2.1M subscribers".into(),
            description: "Hi https://example.com/x".into(),
            videos: vec![VideoItem { id: "dQw4w9WgXcQ".into(), title: "T".into(), channel: String::new(), duration_secs: None, views: String::new(), published: String::new() }],
            next: Some("TOKEN%3D".into()),
        };
        let html = channel_html(&Request::Channel("/@x".into()), &channel);
        assert!(html.contains("8-Bit &lt;Ryan&gt;") && html.contains("2.1M subscribers") && html.contains("@8BitRyan"));
        assert!(html.contains("class=\"banner\"") && html.contains("class=\"avatar\""));
        assert!(html.contains("watch?v=dQw4w9WgXcQ") && html.contains("<a href=\"https://example.com/x\">"));
        assert!(html.contains("data-t=\"TOKEN%3D\""));
        let none = channel_html(&Request::Channel("/@x".into()), &innertube::Channel::default());
        assert!(none.contains("No videos found"));
    }

    #[test]
    fn links_in_descriptions() {
        let out = linkify("See https://example.com/a?b=1&c=2. Also <b>x</b> http://y.org/z, and httpfoo");
        assert!(out.contains("<a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a>."));
        assert!(out.contains("<a href=\"http://y.org/z\">http://y.org/z</a>,"));
        assert!(out.contains("&lt;b&gt;x&lt;/b&gt;") && out.contains("httpfoo"));
        assert!(!out.contains("<b>"));
        assert!(!linkify("javascript:alert(1) \"onmouseover=\"x").contains("<a "));
    }

    #[test]
    fn starts_at_720p() {
        assert_eq!(default_index(&choices()), 1);
        let only_high = vec![Choice { label: "1080p".into(), url: "u".into(), part: "u".into(), key: "k".into(), height: 1080, fps: 30 }];
        assert_eq!(default_index(&only_high), 0);
        let sixty = vec![
            Choice { label: "1080p60".into(), url: "u".into(), part: "u".into(), key: "k1".into(), height: 1080, fps: 60 },
            Choice { label: "720p60".into(), url: "u".into(), part: "u".into(), key: "k2".into(), height: 720, fps: 60 },
            Choice { label: "720p".into(), url: "u".into(), part: "u".into(), key: "k3".into(), height: 720, fps: 30 },
        ];
        assert_eq!(default_index(&sixty), 1);
    }

    #[test]
    fn comment_cards_are_escaped() {
        let c = innertube::Comment { author: "<i>Bob</i>".into(), text: "hi <script>x</script> https://example.com/a".into(), when: "1 day ago".into(), likes: "12".into() };
        let html = comment_html(&c);
        assert!(html.contains("&lt;i&gt;Bob") && !html.contains("<script>"));
        assert!(html.contains("12 likes") && html.contains("href=\"https://example.com/a\""));
        let page = player_html("dQw4w9WgXcQ", &info(), &choices(), &[]);
        assert!(page.contains("Show comments") && page.contains("data-t=\"dQw4w9WgXcQ\""));
    }

    #[test]
    fn builds_a_safe_player_page() {
        let html = player_html("dQw4w9WgXcQ", &info(), &choices(), &[]);
        assert!(html.contains("<video"));
        assert!(html.contains("src=\"http://127.0.0.1:9/v/t/720p.mp4\""));
        assert!(html.contains("<option value=\"http://127.0.0.1:9/v/t/1080.mp4\" data-p=\"http://127.0.0.1:9/v/t/1080p.mp4\" data-k=\"t_1080\">1080p"));
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;alert(1)"));
        assert!(html.contains("3:32") && html.contains("1.5K views"));
    }

    #[test]
    fn results_page() {
        let items = vec![
            VideoItem { id: "dQw4w9WgXcQ".into(), title: "A <b>song</b>".into(), channel: "Chan".into(), duration_secs: Some(212), views: "1.5M views".into(), published: "3 years ago".into() },
            VideoItem { id: "aaaaaaaaaaa".into(), title: String::new(), channel: String::new(), duration_secs: None, views: String::new(), published: String::new() },
        ];
        let html = results_html(&Request::Search("8 bit \"ryan\"".into(), Filter::default()), "Results", &items, None);
        assert!(html.contains("watch?v=dQw4w9WgXcQ"));
        assert!(html.contains("3:32"));
        assert!(html.contains("1.5M views"));
        assert!(html.contains("3 years ago") && html.contains("data-a=\"26280\""));
        assert!(html.contains("Most views") && html.contains("sort=3&amp;date=0"));
        assert!(html.contains("A &lt;b&gt;song"));
        assert!(html.contains("(no title)"));
        assert!(html.contains("value=\"8 bit &quot;ryan&quot;\""));
        assert!(html.contains("action=\"https://www.youtube.com/results\""));
        let empty = results_html(&Request::Channel("/@x".into()), "Chan", &[], None);
        assert!(empty.contains("Nothing found"));
    }

    #[test]
    fn error_page_offers_retry_and_escapes() {
        let html = error_html(&Request::Video("dQw4w9WgXcQ".into()), "bad <thing>");
        assert!(html.contains("bad &lt;thing&gt;"));
        assert!(html.contains("Try again"));
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(3725), "1:02:05");
        assert_eq!(format_views(12), "12 views");
        assert_eq!(format_views(2_500), "2.5K views");
    }
}
