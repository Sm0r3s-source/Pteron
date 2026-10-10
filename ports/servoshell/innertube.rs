/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: a small client for YouTube's own app interface ("InnerTube"), the same interface
//! the YouTube apps and tools like FreeTube use. No account is used and nothing is stored.
//!
//! * Video streams: the `visionos` client, which needs no JavaScript and no proof-of-work token.
//! * Search and channels: the `web` client.
//!
//! The client details (names, versions, user agent) mirror what yt-dlp uses. `pteron_yt_check.ps1`
//! compares them with yt-dlp's current values so a YouTube change is noticed early.
//!
//! The parsers only look for well-known object names anywhere in the answer (for example
//! `videoRenderer`), so small layout changes do not break them.

use std::io::Write;
use std::process::Stdio;

use log::warn;
use serde_json::{Value, json};

use crate::streamproxy::{StreamInfo, VideoSet, curl_command};

const VISIONOS_VERSION: &str = "1.02";
const VISIONOS_UA: &str = crate::streamproxy::UPSTREAM_USER_AGENT;
const WEB_VERSION: &str = "2.20260708.00.00";
const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
/// The "Videos" tab of a channel.
const CHANNEL_VIDEOS_PARAMS: &str = "EgZ2aWRlb3PyBgQKAjoA";

const TV_VERSION: &str = "7.20260707.07.00";
const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Client {
    Visionos,
    Web,
    /// The player YouTube embeds in other websites; plays many videos the others refuse.
    WebEmbedded,
    Tv,
}

fn context(client: Client) -> Value {
    match client {
        Client::Visionos => json!({"client": {
            "clientName": "VISIONOS", "clientVersion": VISIONOS_VERSION,
            "deviceMake": "Apple", "deviceModel": "RealityDevice17,1",
            "userAgent": VISIONOS_UA, "osName": "visionOS", "osVersion": "26.5.23O471",
            "hl": "en", "gl": "US"}}),
        Client::Web => json!({"client": {
            "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": "en", "gl": "US"}}),
        Client::WebEmbedded => json!({
            "client": {"clientName": "WEB_EMBEDDED_PLAYER", "clientVersion": WEB_VERSION, "hl": "en", "gl": "US"},
            "thirdParty": {"embedUrl": "https://www.reddit.com/"}}),
        Client::Tv => json!({"client": {
            "clientName": "TVHTML5", "clientVersion": TV_VERSION, "userAgent": TV_UA,
            "hl": "en", "gl": "US"}}),
    }
}

fn post(endpoint: &str, body: Value, client: Client) -> Result<Value, String> {
    post_with(endpoint, body, client, None)
}

/// `visitor` is the visitor id of a browsing session; YouTube is much less likely to ask for
/// a bot check when requests carry one.
fn post_with(endpoint: &str, mut body: Value, client: Client, visitor: Option<&str>) -> Result<Value, String> {
    body["context"] = context(client);
    if let Some(visitor) = visitor {
        body["context"]["client"]["visitorData"] = json!(visitor);
    }
    let (name_header, version, ua) = match client {
        Client::Visionos => ("101", VISIONOS_VERSION, VISIONOS_UA),
        Client::Web => ("1", WEB_VERSION, WEB_UA),
        Client::WebEmbedded => ("56", WEB_VERSION, WEB_UA),
        Client::Tv => ("7", TV_VERSION, TV_UA),
    };
    let mut command = curl_command();
    command
        .args(["-sS", "--fail-with-body", "--proto", "=https", "--max-time", "25", "-X", "POST"])
        .arg(format!("https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"))
        .args(["-H", "content-type: application/json"])
        .args(["-H", &format!("X-YouTube-Client-Name: {name_header}")])
        .args(["-H", &format!("X-YouTube-Client-Version: {version}")])
        .args(["-H", "Origin: https://www.youtube.com"])
        .args(["-A", ua])
        .args(["--data-binary", "@-"]);
    if let Some(visitor) = visitor {
        command.args(["-H", &format!("X-Goog-Visitor-Id: {visitor}")]);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start curl: {e}. Pteron needs Windows 10 or later."))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.to_string().as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() && out.stdout.is_empty() {
        let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if message.is_empty() {
            "Could not reach YouTube.".into()
        } else {
            format!("Could not reach YouTube: {}", message.chars().take(200).collect::<String>())
        });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("Unreadable answer from YouTube: {e}"))
}

// ---------------------------------------------------------------- helpers

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `simpleText`, or the joined `runs`, of a text object.
fn text_of(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(s) = v.get("simpleText").and_then(|s| s.as_str()) {
        return s.to_string();
    }
    if let Some(runs) = v.get("runs").and_then(|r| r.as_array()) {
        return runs
            .iter()
            .filter_map(|r| r.get("text").and_then(|t| t.as_str()))
            .collect::<String>();
    }
    if let Some(s) = v.get("content").and_then(|s| s.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// "3:32" or "1:02:05" -> seconds.
pub(crate) fn parse_clock(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut parts = 0;
    for part in text.trim().split(':') {
        total = total * 60 + part.trim().parse::<u64>().ok()?;
        parts += 1;
    }
    (1..=3).contains(&parts).then_some(total)
}

fn find_first<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|child| find_first(child, key))
        },
        Value::Array(list) => list.iter().find_map(|child| find_first(child, key)),
        _ => None,
    }
}

// ---------------------------------------------------------------- video

#[derive(Debug, PartialEq)]
pub(crate) struct PlayerInfo {
    pub title: String,
    pub author: String,
    pub description: String,
    pub duration_secs: u64,
    pub thumbnail: String,
    pub views: Option<u64>,
    pub channel_id: String,
    /// `2024-05-17` style date, when YouTube says it.
    pub published: String,
    pub set: VideoSet,
}

fn range_end(format: &Value, key: &str) -> Option<u64> {
    format.get(key).and_then(|r| r.get("end")).and_then(as_u64)
}

pub(crate) fn parse_player(v: &Value) -> Result<PlayerInfo, String> {
    let status = v
        .pointer("/playabilityStatus/status")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    if status != "OK" {
        let reason = v
            .pointer("/playabilityStatus/reason")
            .and_then(|s| s.as_str())
            .unwrap_or("YouTube did not give a reason.");
        let hint = if status == "LOGIN_REQUIRED" {
            " (YouTube asks for a sign-in or a bot check. This happens with some restricted, age-limited or made-for-kids videos, and sometimes after many requests. Try again later.)"
        } else {
            ""
        };
        return Err(format!("{reason}{hint}"));
    }
    let details = v.get("videoDetails").cloned().unwrap_or(Value::Null);
    if details.get("isLive").and_then(|b| b.as_bool()) == Some(true) {
        return Err("This is a live stream. Pteron cannot play live streams yet.".into());
    }
    let formats = v
        .pointer("/streamingData/adaptiveFormats")
        .and_then(|f| f.as_array())
        .ok_or("YouTube did not return any streams for this video.")?;

    let mut videos: Vec<(StreamInfo, u64)> = Vec::new();
    let mut audios: Vec<(StreamInfo, u64)> = Vec::new();
    let mut duration_ms: Option<u64> = None;
    for f in formats {
        let mime = f.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");
        let Some(url) = f.get("url").and_then(|u| u.as_str()) else { continue };
        let (Some(index_end), Some(_init_end)) = (range_end(f, "indexRange"), range_end(f, "initRange")) else {
            continue;
        };
        let itag = f.get("itag").and_then(as_u64).unwrap_or(0) as u32;
        let bitrate = f.get("bitrate").and_then(as_u64).unwrap_or(0);
        if duration_ms.is_none() {
            duration_ms = f.get("approxDurationMs").and_then(as_u64);
        }
        let info = StreamInfo {
            itag,
            url: url.to_string(),
            index_end,
            height: f.get("height").and_then(as_u64).unwrap_or(0) as u32,
            fps: f.get("fps").and_then(as_u64).unwrap_or(30) as u32,
        };
        if mime.starts_with("video/mp4") && mime.contains("avc1") && info.height > 0 {
            videos.push((info, bitrate));
        } else if mime.starts_with("audio/mp4") && mime.contains("mp4a.40.2") {
            // Many videos also offer dubbed or translated audio tracks. Only the original
            // language (the default track) is used.
            let dubbed = f.pointer("/audioTrack/audioIsDefault").and_then(|b| b.as_bool()) == Some(false) ||
                f.get("xtags").and_then(|x| x.as_str()).is_some_and(|x| x.contains("acont=dubbed"));
            if !dubbed {
                audios.push((info, bitrate));
            }
        }
    }
    // One entry per height and frame rate class (30 or 60), the highest bitrate of each, best first.
    let class = |i: &StreamInfo| (i.height, i.fps > 30);
    videos.sort_by(|a, b| (class(&b.0), b.1).cmp(&(class(&a.0), a.1)));
    videos.dedup_by(|b, a| class(&a.0) == class(&b.0));
    let audio = audios
        .into_iter()
        .max_by_key(|(info, bitrate)| (*bitrate, info.itag == 140))
        .map(|(info, _)| info)
        .ok_or("YouTube did not offer a compatible audio stream for this video.")?;
    if videos.is_empty() {
        return Err("YouTube did not offer a compatible video stream for this video.".into());
    }
    let length_secs = details
        .get("lengthSeconds")
        .and_then(as_u64)
        .unwrap_or(0);
    let duration_ms = duration_ms.unwrap_or(length_secs * 1000);
    let text = |key: &str| details.get(key).and_then(|s| s.as_str()).unwrap_or("").to_string();
    let thumbnail = details
        .pointer("/thumbnail/thumbnails")
        .and_then(|t| t.as_array())
        .and_then(|t| t.last())
        .and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    Ok(PlayerInfo {
        title: text("title"),
        author: text("author"),
        description: text("shortDescription"),
        duration_secs: duration_ms / 1000,
        thumbnail,
        views: details.get("viewCount").and_then(as_u64),
        channel_id: {
            let id = text("channelId");
            if id.starts_with("UC") && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                id
            } else {
                String::new()
            }
        },
        published: v
            .pointer("/microformat/playerMicroformatRenderer/publishDate")
            .and_then(|d| d.as_str())
            .map(|d| d.chars().take(10).collect::<String>())
            .filter(|d| d.len() == 10 && d.chars().all(|c| c.is_ascii_digit() || c == '-'))
            .unwrap_or_default(),
        set: VideoSet {
            videos: videos.into_iter().map(|(i, _)| i).collect(),
            audio,
            duration_ms,
        },
    })
}

static VISITOR: std::sync::Mutex<Option<(String, std::time::Instant)>> = std::sync::Mutex::new(None);

/// A visitor id for this run of Pteron, fetched once and renewed every hour.
fn visitor() -> Option<String> {
    let mut cache = VISITOR.lock().ok()?;
    if let Some((id, when)) = cache.as_ref() {
        if when.elapsed() < std::time::Duration::from_secs(3600) {
            return Some(id.clone());
        }
    }
    let answer = post("browse", json!({"browseId": "FEwhat_to_watch"}), Client::Web).ok()?;
    let id = answer
        .pointer("/responseContext/visitorData")
        .and_then(|v| v.as_str())
        .filter(|v| v.len() < 400 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '%' | '=')))?
        .to_string();
    *cache = Some((id.clone(), std::time::Instant::now()));
    Some(id)
}

pub(crate) fn player(video_id: &str) -> Result<PlayerInfo, String> {
    // The first client is tried alone; the others only help with videos it refuses
    // (made-for-kids, some age or region limits) and are only used when it fails.
    let mut first_error = None;
    let visitor = visitor();
    for client in [Client::Visionos, Client::WebEmbedded, Client::Tv] {
        let body = json!({"videoId": video_id, "contentCheckOk": true, "racyCheckOk": true});
        match post_with("player", body, client, visitor.as_deref()).and_then(|answer| parse_player(&answer)) {
            Ok(info) => return Ok(info),
            Err(message) => {
                warn!("Pteron: {client:?} could not play {video_id}: {message}");
                first_error.get_or_insert(message);
            },
        }
    }
    Err(first_error.unwrap_or_else(|| "YouTube did not answer.".into()))
}

// ---------------------------------------------------------------- lists of videos

#[derive(Debug, PartialEq, Clone)]
pub(crate) struct VideoItem {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration_secs: Option<u64>,
    pub views: String,
}

fn valid_id(id: &str) -> bool {
    crate::youtube::valid_video_id(id)
}

fn item_from_renderer(r: &Value) -> Option<VideoItem> {
    let id = r.get("videoId")?.as_str()?;
    if !valid_id(id) {
        return None;
    }
    let owner = ["ownerText", "longBylineText", "shortBylineText"]
        .iter()
        .find_map(|k| r.get(*k).map(text_of).filter(|t| !t.is_empty()))
        .unwrap_or_default();
    let views = ["shortViewCountText", "viewCountText"]
        .iter()
        .find_map(|k| r.get(*k).map(text_of).filter(|t| !t.is_empty()))
        .unwrap_or_default();
    Some(VideoItem {
        id: id.to_string(),
        title: r.get("title").map(text_of).unwrap_or_default(),
        channel: owner,
        duration_secs: r
            .get("lengthText")
            .map(text_of)
            .and_then(|t| parse_clock(&t))
            .or_else(|| r.get("lengthSeconds").and_then(as_u64)),
        views,
    })
}

fn item_from_lockup(r: &Value) -> Option<VideoItem> {
    if r.get("contentType").and_then(|c| c.as_str()) != Some("LOCKUP_CONTENT_TYPE_VIDEO") {
        return None;
    }
    let id = r.get("contentId")?.as_str()?;
    if !valid_id(id) {
        return None;
    }
    let meta = r.pointer("/metadata/lockupMetadataViewModel")?;
    let title = meta.get("title").map(text_of).unwrap_or_default();
    // metadata rows hold the channel name and view count / age
    let mut rows: Vec<String> = Vec::new();
    if let Some(list) = meta
        .pointer("/metadata/contentMetadataViewModel/metadataRows")
        .and_then(|r| r.as_array())
    {
        for row in list {
            if let Some(parts) = row.get("metadataParts").and_then(|p| p.as_array()) {
                for part in parts {
                    if let Some(t) = part.get("text") {
                        let t = text_of(t);
                        if !t.is_empty() {
                            rows.push(t);
                        }
                    }
                }
            }
        }
    }
    let duration = find_first(r, "thumbnailBadgeViewModel")
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .and_then(parse_clock);
    Some(VideoItem {
        id: id.to_string(),
        title,
        channel: rows.first().cloned().unwrap_or_default(),
        duration_secs: duration,
        views: rows.get(1).cloned().unwrap_or_default(),
    })
}

fn collect(v: &Value, out: &mut Vec<VideoItem>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let item = match key.as_str() {
                    "videoRenderer" | "gridVideoRenderer" | "playlistVideoRenderer" | "playlistPanelVideoRenderer" => {
                        item_from_renderer(child)
                    },
                    "lockupViewModel" => item_from_lockup(child),
                    _ => None,
                };
                match item {
                    Some(item) => {
                        if !out.iter().any(|o| o.id == item.id) && out.len() < limit {
                            out.push(item);
                        }
                    },
                    None => collect(child, out, limit),
                }
            }
        },
        Value::Array(list) => {
            for child in list {
                collect(child, out, limit);
            }
        },
        _ => {},
    }
}

pub(crate) fn parse_videos(v: &Value, limit: usize) -> Vec<VideoItem> {
    let mut out = Vec::new();
    collect(v, &mut out, limit);
    out
}

pub(crate) fn search(query: &str) -> Result<(Vec<VideoItem>, Option<String>), String> {
    let answer = post("search", json!({"query": query}), Client::Web)?;
    Ok((parse_videos(&answer, 40), continuation_token(&answer)))
}

/// The next batch of search results.
pub(crate) fn search_more(token: &str) -> Result<(Vec<VideoItem>, Option<String>), String> {
    if !valid_continuation(token) {
        return Err("bad token".into());
    }
    let answer = post("search", json!({"continuation": token}), Client::Web)?;
    Ok((parse_videos(&answer, 60), continuation_token(&answer)))
}

/// Videos YouTube suggests next to this one.
pub(crate) fn related(video_id: &str) -> Result<Vec<VideoItem>, String> {
    let answer = post("next", json!({"videoId": video_id}), Client::Web)?;
    let mut items = parse_videos(&answer, 30);
    items.retain(|item| item.id != video_id);
    items.truncate(20);
    Ok(items)
}

// ---------------------------------------------------------------- comments

#[derive(Debug, PartialEq, Default, Clone)]
pub(crate) struct Comment {
    pub author: String,
    pub text: String,
    pub when: String,
    pub likes: String,
}

/// The token that opens the comment section of a watch page (`next` answer).
pub(crate) fn comments_token(answer: &Value) -> Option<String> {
    fn walk(v: &Value) -> Option<String> {
        match v {
            Value::Object(map) => {
                if let Some(section) = map.get("itemSectionRenderer") &&
                    section.get("sectionIdentifier").and_then(|s| s.as_str()) == Some("comment-item-section") &&
                    let Some(token) = continuation_token(section)
                {
                    return Some(token);
                }
                map.values().find_map(walk)
            },
            Value::Array(list) => list.iter().find_map(walk),
            _ => None,
        }
    }
    walk(answer)
}

fn collect_threads<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            if let Some(thread) = map.get("commentThreadRenderer") {
                out.push(thread);
                return; // do not look into replies
            }
            map.values().for_each(|child| collect_threads(child, out));
        },
        Value::Array(list) => list.iter().for_each(|child| collect_threads(child, out)),
        _ => {},
    }
}

/// Comments and the token for the next batch, from a continuation answer. Understands both the
/// current format (`commentEntityPayload`) and the older `commentRenderer`.
pub(crate) fn parse_comments(answer: &Value) -> (Vec<Comment>, Option<String>) {
    let mut entities: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
    if let Some(mutations) = answer.pointer("/frameworkUpdates/entityBatchUpdate/mutations").and_then(|m| m.as_array()) {
        for mutation in mutations {
            if let Some(payload) = mutation.pointer("/payload/commentEntityPayload") &&
                let Some(key) = payload.get("key").and_then(|k| k.as_str())
            {
                entities.insert(key, payload);
            }
        }
    }
    let mut threads = Vec::new();
    collect_threads(answer, &mut threads);
    let mut comments = Vec::new();
    for thread in threads {
        let key = thread.pointer("/commentViewModel/commentViewModel/commentKey").and_then(|k| k.as_str());
        let comment = if let Some(payload) = key.and_then(|k| entities.get(k)) {
            let s = |p: &str| payload.pointer(p).map(text_of).unwrap_or_default();
            Comment {
                author: s("/author/displayName"),
                text: s("/properties/content"),
                when: s("/properties/publishedTime"),
                likes: payload
                    .pointer("/toolbar/likeCountNotliked")
                    .map(text_of)
                    .filter(|l| l != "0")
                    .unwrap_or_default(),
            }
        } else if let Some(r) = thread.pointer("/comment/commentRenderer") {
            let s = |k: &str| r.get(k).map(text_of).unwrap_or_default();
            Comment {
                author: s("authorText"),
                text: s("contentText"),
                when: s("publishedTimeText"),
                likes: s("voteCount"),
            }
        } else {
            continue;
        };
        if !comment.text.trim().is_empty() {
            comments.push(comment);
        }
    }
    // The "more" token is a direct item of a continuation list, not one inside a thread.
    let mut next = None;
    if let Some(endpoints) = answer.get("onResponseReceivedEndpoints").and_then(|e| e.as_array()) {
        for endpoint in endpoints {
            let Some(items) = endpoint.as_object().and_then(|o| {
                o.values().find_map(|c| c.get("continuationItems").and_then(|i| i.as_array()))
            }) else {
                continue;
            };
            for item in items {
                if let Some(token) = item.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token").and_then(|t| t.as_str()) &&
                    valid_continuation(token)
                {
                    next = Some(token.to_string());
                }
            }
        }
    }
    (comments, next)
}

/// `video_or_token` is a video id (first batch) or a token from the previous batch.
pub(crate) fn comments(video_or_token: &str) -> Result<(Vec<Comment>, Option<String>), String> {
    let token = if valid_id(video_or_token) {
        let answer = post("next", json!({"videoId": video_or_token}), Client::Web)?;
        comments_token(&answer).ok_or_else(|| "Comments are off or not available for this video.".to_string())?
    } else if valid_continuation(video_or_token) {
        video_or_token.to_string()
    } else {
        return Err("bad token".into());
    };
    let answer = post("next", json!({"continuation": token}), Client::Web)?;
    Ok(parse_comments(&answer))
}

// ---------------------------------------------------------------- channels

#[derive(Debug, PartialEq, Default)]
pub(crate) struct Channel {
    pub title: String,
    pub handle: String,
    pub avatar: String,
    pub banner: String,
    pub subscribers: String,
    pub description: String,
    pub videos: Vec<VideoItem>,
    /// Token for the next page of videos.
    pub next: Option<String>,
}

fn safe_image(url: &str) -> bool {
    url.starts_with("https://") && url.len() < 600 && !url.contains(['"', '<', '>', ' ', '\\'])
}

/// The first text that looks like "1.23M subscribers".
fn find_subscribers(v: &Value) -> Option<String> {
    match v {
        Value::String(text) => {
            let t = text.trim();
            (t.len() <= 32 &&
                t.chars().next().is_some_and(|c| c.is_ascii_digit()) &&
                (t.ends_with(" subscribers") || t.ends_with(" subscriber")))
            .then(|| t.to_string())
        },
        Value::Object(map) => map.values().find_map(find_subscribers),
        Value::Array(list) => list.iter().find_map(find_subscribers),
        _ => None,
    }
}

/// The biggest image address in a `thumbnails` / `sources` list.
fn best_image(list: &Value) -> Option<String> {
    list.as_array()?
        .iter()
        .filter_map(|i| {
            let url = i.get("url").and_then(|u| u.as_str())?;
            safe_image(url).then(|| (i.get("width").and_then(as_u64).unwrap_or(0), url.to_string()))
        })
        .max_by_key(|(w, _)| *w)
        .map(|(_, u)| u)
}

/// Token that loads the next batch of videos of a grid.
pub(crate) fn continuation_token(v: &Value) -> Option<String> {
    let item = find_first(v, "continuationItemRenderer")?;
    let token = item.pointer("/continuationEndpoint/continuationCommand/token")?.as_str()?;
    valid_continuation(token).then(|| token.to_string())
}

pub(crate) fn valid_continuation(token: &str) -> bool {
    !token.is_empty() &&
        token.len() <= 1800 &&
        token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '%' | '_' | '=' | '-' | '.' | '~'))
}

/// The next videos of a channel grid.
pub(crate) fn more(token: &str) -> Result<(Vec<VideoItem>, Option<String>), String> {
    if !valid_continuation(token) {
        return Err("bad token".into());
    }
    let answer = post("browse", json!({"continuation": token}), Client::Web)?;
    let items = parse_videos(&answer, 60);
    let next = continuation_token(&answer);
    Ok((items, next))
}

pub(crate) fn parse_channel(answer: &Value, videos: Vec<VideoItem>) -> Channel {
    let meta = answer.pointer("/metadata/channelMetadataRenderer");
    let text = |key: &str| {
        meta.and_then(|m| m.get(key))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string()
    };
    let handle = text("vanityChannelUrl")
        .rsplit('/')
        .next()
        .filter(|h| h.starts_with('@') && h.len() < 60)
        .unwrap_or("")
        .to_string();
    let avatar = meta
        .and_then(|m| m.pointer("/avatar/thumbnails"))
        .and_then(best_image)
        .unwrap_or_default();
    let banner = find_first(answer, "banner")
        .and_then(|b| b.get("thumbnails"))
        .and_then(best_image)
        .or_else(|| {
            find_first(answer, "imageBannerViewModel")
                .and_then(|b| b.pointer("/image/sources"))
                .and_then(best_image)
        })
        .unwrap_or_default();
    Channel {
        title: channel_title(answer).unwrap_or_else(|| "Channel".into()),
        handle,
        avatar,
        banner,
        subscribers: find_subscribers(answer).unwrap_or_default(),
        description: text("description"),
        videos,
        next: continuation_token(answer),
    }
}

fn browse_id_from_resolve(v: &Value) -> Option<String> {
    let id = find_first(v, "browseId")?.as_str()?;
    (id.starts_with("UC") && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then(|| id.to_string())
}

pub(crate) fn channel_title(v: &Value) -> Option<String> {
    v.pointer("/metadata/channelMetadataRenderer/title")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .or_else(|| {
            find_first(v, "pageTitle").and_then(|t| t.as_str()).map(str::to_string)
        })
}

pub(crate) fn channel(path: &str) -> Result<Channel, String> {
    // `/channel/UC...` already carries the id; the other forms must be looked up.
    let direct = path
        .strip_prefix("/channel/")
        .map(|id| id.split('/').next().unwrap_or(""))
        .filter(|id| id.starts_with("UC"));
    let browse_id = match direct {
        Some(id) => id.to_string(),
        None => {
            let answer = post(
                "navigation/resolve_url",
                json!({"url": format!("https://www.youtube.com{path}")}),
                Client::Web,
            )?;
            browse_id_from_resolve(&answer).ok_or("YouTube could not find this channel.")?
        },
    };
    let mut answer = post(
        "browse",
        json!({"browseId": browse_id, "params": CHANNEL_VIDEOS_PARAMS}),
        Client::Web,
    )?;
    let mut videos = parse_videos(&answer, 40);
    if videos.is_empty() {
        // some channels have no Videos tab; fall back to the channel's home page
        answer = post("browse", json!({"browseId": browse_id}), Client::Web)?;
        videos = parse_videos(&answer, 40);
    }
    Ok(parse_channel(&answer, videos))
}

#[cfg(test)]
mod tests {
    #[test]
    fn comments_in_the_current_format() {
        let answer = json!({
            "frameworkUpdates": {"entityBatchUpdate": {"mutations": [
                {"payload": {"commentEntityPayload": {"key": "K1",
                    "properties": {"content": {"content": "Great video"}, "publishedTime": "2 days ago"},
                    "author": {"displayName": "@ann"},
                    "toolbar": {"likeCountNotliked": "15"}}}},
                {"payload": {"commentEntityPayload": {"key": "K2",
                    "properties": {"content": {"content": "Second"}, "publishedTime": "1 day ago"},
                    "author": {"displayName": "@bob"},
                    "toolbar": {"likeCountNotliked": "0"}}}}
            ]}},
            "onResponseReceivedEndpoints": [
                {"reloadContinuationItemsCommand": {"continuationItems": [
                    {"commentThreadRenderer": {"commentViewModel": {"commentViewModel": {"commentKey": "K1"}},
                        "replies": {"commentRepliesRenderer": {"contents": [{"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "REPLYTOKEN"}}}}]}}}},
                    {"commentThreadRenderer": {"commentViewModel": {"commentViewModel": {"commentKey": "K2"}}}},
                    {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "NEXTPAGE"}}}}
                ]}}
            ]
        });
        let (list, next) = parse_comments(&answer);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].author, "@ann");
        assert_eq!(list[0].text, "Great video");
        assert_eq!(list[0].likes, "15");
        assert_eq!(list[1].likes, "");
        assert_eq!(next.as_deref(), Some("NEXTPAGE"));
    }

    #[test]
    fn comments_in_the_older_format_and_the_opening_token() {
        let answer = json!({"onResponseReceivedEndpoints": [{"appendContinuationItemsAction": {"continuationItems": [
            {"commentThreadRenderer": {"comment": {"commentRenderer": {
                "authorText": {"simpleText": "Cy"},
                "contentText": {"runs": [{"text": "Hello "}, {"text": "there"}]},
                "publishedTimeText": {"runs": [{"text": "3 weeks ago"}]},
                "voteCount": {"simpleText": "7"}}}}}
        ]}}]});
        let (list, next) = parse_comments(&answer);
        assert_eq!(list, vec![Comment { author: "Cy".into(), text: "Hello there".into(), when: "3 weeks ago".into(), likes: "7".into() }]);
        assert_eq!(next, None);
        let watch = json!({"contents": {"results": {"contents": [
            {"itemSectionRenderer": {"sectionIdentifier": "other", "contents": [{"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "WRONG"}}}}]}},
            {"itemSectionRenderer": {"sectionIdentifier": "comment-item-section", "contents": [{"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "OPENCOMMENTS"}}}}]}}
        ]}}});
        assert_eq!(comments_token(&watch).as_deref(), Some("OPENCOMMENTS"));
        assert_eq!(comments_token(&json!({})), None);
    }

    use super::*;

    fn format(itag: u32, mime: &str, height: u64, bitrate: u64, index_end: u64) -> Value {
        json!({"itag": itag, "url": format!("https://rr1.googlevideo.com/videoplayback?itag={itag}"),
               "mimeType": mime, "height": height, "bitrate": bitrate,
               "initRange": {"start": "0", "end": (index_end - 480).to_string()},
               "indexRange": {"start": (index_end - 479).to_string(), "end": index_end.to_string()},
               "approxDurationMs": "212091"})
    }

    fn player_json() -> Value {
        json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "dQw4w9WgXcQ", "title": "Song <b>", "author": "Chan",
                "lengthSeconds": "212", "shortDescription": "hello", "viewCount": "1500",
                "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/a.jpg"}, {"url": "https://i.ytimg.com/b.jpg"}]}},
            "streamingData": {"formats": [], "adaptiveFormats": [
                format(137, "video/mp4; codecs=\"avc1.640028\"", 1080, 4_000_000, 1229),
                format(136, "video/mp4; codecs=\"avc1.4D401F\"", 720, 2_000_000, 1227),
                format(298, "video/mp4; codecs=\"avc1.4D401F\"", 720, 3_000_000, 1300),
                format(134, "video/mp4; codecs=\"avc1.4D401E\"", 360, 700_000, 1228),
                format(248, "video/webm; codecs=\"vp9\"", 1080, 2_500_000, 900),
                format(139, "audio/mp4; codecs=\"mp4a.40.5\"", 0, 48_000, 1027),
                format(140, "audio/mp4; codecs=\"mp4a.40.2\"", 0, 130_000, 1018),
                format(251, "audio/webm; codecs=\"opus\"", 0, 140_000, 700),
                {"itag": 160, "mimeType": "video/mp4; codecs=\"avc1.4D400C\"", "height": 144}
            ]}
        })
    }

    #[test]
    fn picks_streams_for_the_merger() {
        let info = parse_player(&player_json()).unwrap();
        let heights: Vec<u32> = info.set.videos.iter().map(|v| v.height).collect();
        assert_eq!(heights, vec![1080, 720, 360]);
        // the higher-bitrate 720p wins
        assert_eq!(info.set.videos[1].itag, 298);
        assert_eq!(info.set.audio.itag, 140);
        assert_eq!(info.set.audio.index_end, 1018);
        assert_eq!(info.set.duration_ms, 212_091);
        assert_eq!(info.thumbnail, "https://i.ytimg.com/b.jpg");
        assert_eq!(info.views, Some(1500));
        assert_eq!(info.title, "Song <b>");
    }

    #[test]
    fn explains_refusals() {
        let e = parse_player(&json!({"playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm you’re not a bot"}})).unwrap_err();
        assert!(e.contains("Sign in") && e.contains("bot check"));
        let e = parse_player(&json!({"playabilityStatus": {"status": "ERROR", "reason": "This video is unavailable"}})).unwrap_err();
        assert_eq!(e, "This video is unavailable");
        let mut live = player_json();
        live["videoDetails"]["isLive"] = json!(true);
        assert!(parse_player(&live).unwrap_err().contains("live"));
        let mut dubbed = player_json();
        let list = dubbed["streamingData"]["adaptiveFormats"].as_array_mut().unwrap();
        for f in list.iter_mut() {
            if f["itag"] == 140 {
                f["audioTrack"] = json!({"id": "en.4", "audioIsDefault": true});
                f["bitrate"] = json!(100_000);
            }
        }
        let mut other = list.iter().find(|f| f["itag"] == 140).unwrap().clone();
        other["audioTrack"] = json!({"id": "ru.10", "audioIsDefault": false});
        other["bitrate"] = json!(999_999);
        other["url"] = json!("https://rr1.googlevideo.com/other");
        list.push(other);
        let picked = parse_player(&dubbed).unwrap();
        assert!(!picked.set.audio.url.contains("other"));

        let mut no_audio = player_json();
        no_audio["streamingData"]["adaptiveFormats"] = json!([format(137, "video/mp4; codecs=\"avc1.640028\"", 1080, 1, 1229)]);
        assert!(parse_player(&no_audio).unwrap_err().contains("audio"));
        assert!(parse_player(&json!({"playabilityStatus": {"status": "OK"}})).is_err());
    }

    #[test]
    fn clock_text() {
        assert_eq!(parse_clock("3:32"), Some(212));
        assert_eq!(parse_clock("1:02:05"), Some(3725));
        assert_eq!(parse_clock("LIVE"), None);
        assert_eq!(parse_clock(""), None);
    }

    fn search_json() -> Value {
        json!({"contents": {"twoColumnSearchResultsRenderer": {"primaryContents": {"sectionListRenderer": {"contents": [
            {"itemSectionRenderer": {"contents": [
                {"videoRenderer": {"videoId": "dQw4w9WgXcQ",
                    "title": {"runs": [{"text": "Never "}, {"text": "Gonna"}]},
                    "ownerText": {"runs": [{"text": "Rick"}]},
                    "lengthText": {"simpleText": "3:32"},
                    "viewCountText": {"simpleText": "1,500 views"},
                    "shortViewCountText": {"simpleText": "1.5K views"}}},
                {"shelfRenderer": {"content": {"verticalListRenderer": {"items": [
                    {"videoRenderer": {"videoId": "aaaaaaaaaaa", "title": {"simpleText": "Second"}}}]}}}},
                {"videoRenderer": {"videoId": "dQw4w9WgXcQ", "title": {"simpleText": "duplicate"}}},
                {"videoRenderer": {"videoId": "bad id", "title": {"simpleText": "bad"}}},
                {"reelItemRenderer": {"videoId": "ccccccccccc"}},
                {"lockupViewModel": {"contentType": "LOCKUP_CONTENT_TYPE_VIDEO", "contentId": "bbbbbbbbbbb",
                    "contentImage": {"thumbnailViewModel": {"overlays": [{"thumbnailBottomOverlayViewModel": {"badges": [
                        {"thumbnailBadgeViewModel": {"text": "10:01"}}]}}]}},
                    "metadata": {"lockupMetadataViewModel": {"title": {"content": "New layout"},
                        "metadata": {"contentMetadataViewModel": {"metadataRows": [
                            {"metadataParts": [{"text": {"content": "Chan2"}}]},
                            {"metadataParts": [{"text": {"content": "2M views"}}, {"text": {"content": "1 year ago"}}]}]}}}}}},
                {"lockupViewModel": {"contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST", "contentId": "PLxxxxxxxxx"}}
            ]}}]}}}}})
    }

    #[test]
    fn reads_videos_in_old_and_new_layouts() {
        let items = parse_videos(&search_json(), 40);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["dQw4w9WgXcQ", "aaaaaaaaaaa", "bbbbbbbbbbb"]);
        assert_eq!(items[0].title, "Never Gonna");
        assert_eq!(items[0].channel, "Rick");
        assert_eq!(items[0].duration_secs, Some(212));
        assert_eq!(items[0].views, "1.5K views");
        assert_eq!(items[2].title, "New layout");
        assert_eq!(items[2].channel, "Chan2");
        assert_eq!(items[2].duration_secs, Some(601));
        assert_eq!(items[2].views, "2M views");
        assert_eq!(parse_videos(&search_json(), 2).len(), 2);
        assert!(parse_videos(&json!({"x": 1}), 10).is_empty());
    }

    #[test]
    fn channel_pieces() {
        let resolved = json!({"endpoint": {"browseEndpoint": {"browseId": "UCBR8-60-B28hp2BmDPdntcQ"}}});
        assert_eq!(browse_id_from_resolve(&resolved).as_deref(), Some("UCBR8-60-B28hp2BmDPdntcQ"));
        assert_eq!(browse_id_from_resolve(&json!({"browseId": "evil;rm"})), None);
        let page = json!({"metadata": {"channelMetadataRenderer": {"title": "YouTube"}}});
        assert_eq!(channel_title(&page).as_deref(), Some("YouTube"));
        let page = json!({"header": {"pageHeaderRenderer": {"pageTitle": "Other"}}});
        assert_eq!(channel_title(&page).as_deref(), Some("Other"));
    }

    #[test]
    fn prefers_30_frames_per_second() {
        let mut v = player_json();
        let list = v["streamingData"]["adaptiveFormats"].as_array_mut().unwrap();
        for f in list.iter_mut() {
            if f["itag"] == 298 {
                f["fps"] = json!(60);
            }
            if f["itag"] == 136 {
                f["fps"] = json!(30);
                f["bitrate"] = json!(2_000_000);
            }
        }
        let info = parse_player(&v).unwrap();
        // 720p60 and 720p30 are both offered, 60 first
        let kinds: Vec<(u32, u32)> = info.set.videos.iter().map(|v| (v.height, v.fps)).collect();
        assert_eq!(kinds, vec![(1080, 30), (720, 60), (720, 30), (360, 30)]);
        assert_eq!(info.set.videos[2].itag, 136);
    }

    #[test]
    fn channel_header_and_more_videos() {
        let page = json!({
            "metadata": {"channelMetadataRenderer": {
                "title": "8-BitRyan", "description": "Games & more",
                "vanityChannelUrl": "http://www.youtube.com/@8BitRyan",
                "avatar": {"thumbnails": [{"url": "https://yt3.googleusercontent.com/a=s88", "width": 88},
                                          {"url": "https://yt3.googleusercontent.com/a=s900", "width": 900},
                                          {"url": "javascript:x", "width": 5000}]}}},
            "header": {"pageHeaderRenderer": {"content": {"x": ["@8BitRyan", "2.1M subscribers", "300 videos"]}}},
            "banner": {"thumbnails": [{"url": "https://yt3.googleusercontent.com/b=w2120", "width": 2120}]},
            "contents": [
                {"chips": {"continuationCommand": {"token": "SORTCHIP"}}},
                {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "NEXT%3D_abc"}}}}
            ]
        });
        let c = parse_channel(&page, vec![]);
        assert_eq!(c.title, "8-BitRyan");
        assert_eq!(c.handle, "@8BitRyan");
        assert_eq!(c.avatar, "https://yt3.googleusercontent.com/a=s900");
        assert_eq!(c.banner, "https://yt3.googleusercontent.com/b=w2120");
        assert_eq!(c.subscribers, "2.1M subscribers");
        assert_eq!(c.description, "Games & more");
        assert_eq!(c.next.as_deref(), Some("NEXT%3D_abc"));
        assert!(valid_continuation("abc%3D-_.~") && !valid_continuation("a b") && !valid_continuation("a\"b") && !valid_continuation(""));
        let empty = parse_channel(&json!({}), vec![]);
        assert_eq!((empty.avatar.as_str(), empty.banner.as_str(), empty.next.as_deref()), ("", "", None));
    }
}


// ---------------------------------------------------------------- playlists and albums

/// Whether `id` can be a YouTube playlist id (`PL...`, `OLAK5uy_...`, `RDCLAK...`).
pub(crate) fn valid_playlist_id(id: &str) -> bool {
    (10..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, PartialEq, Default)]
pub(crate) struct Playlist {
    pub title: String,
    pub items: Vec<VideoItem>,
}

pub(crate) fn parse_playlist(answer: &Value) -> Playlist {
    let title = ["/header/playlistHeaderRenderer/title", "/metadata/playlistMetadataRenderer/title"]
        .iter()
        .find_map(|p| answer.pointer(p).map(text_of).filter(|t| !t.is_empty()))
        .unwrap_or_default();
    Playlist { title, items: parse_videos(answer, 300) }
}

/// The songs of a playlist or album, in order.
pub(crate) fn playlist(id: &str) -> Result<Playlist, String> {
    if !valid_playlist_id(id) {
        return Err("That does not look like a playlist.".into());
    }
    let answer = post("browse", json!({"browseId": format!("VL{id}")}), Client::Web)?;
    let list = parse_playlist(&answer);
    if list.items.is_empty() {
        return Err("This playlist has no videos Pteron can play.".into());
    }
    Ok(list)
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    #[test]
    fn reads_a_playlist_answer() {
        let answer = json!({
            "header": {"playlistHeaderRenderer": {"title": {"simpleText": "Road trip"}}},
            "contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [{"itemSectionRenderer": {"contents": [{"playlistVideoListRenderer": {"contents": [
                {"playlistVideoRenderer": {"videoId": "dQw4w9WgXcQ", "title": {"runs": [{"text": "Never Gonna Give You Up"}]},
                    "shortBylineText": {"runs": [{"text": "Rick Astley"}]}, "lengthText": {"simpleText": "3:33"}, "lengthSeconds": "213"}},
                {"playlistVideoRenderer": {"videoId": "9bZkp7q19f0", "title": {"runs": [{"text": "Gangnam Style"}]},
                    "shortBylineText": {"runs": [{"text": "PSY"}]}, "lengthSeconds": "252"}},
                {"playlistVideoRenderer": {"videoId": "bad", "title": {"runs": [{"text": "Broken id"}]}}}
            ]}}]}}]}}}}]}}
        });
        let list = parse_playlist(&answer);
        assert_eq!(list.title, "Road trip");
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].id, "dQw4w9WgXcQ");
        assert_eq!(list.items[0].channel, "Rick Astley");
        assert_eq!(list.items[0].duration_secs, Some(213));
        assert_eq!(list.items[1].duration_secs, Some(252));
    }

    #[test]
    fn playlist_ids_are_checked() {
        assert!(valid_playlist_id("PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf"));
        assert!(valid_playlist_id("OLAK5uy_kMn0lyHcSJ5oQ4-1bhZeGfbMCEyA"));
        assert!(!valid_playlist_id("short"));
        assert!(!valid_playlist_id("PL12345678;rm -rf"));
    }
}
