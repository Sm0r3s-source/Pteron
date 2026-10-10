/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: browsing history and bookmarks. Both are small JSON files next to the
//! settings (`history.json`, `bookmarks.json`), kept in memory while the app runs.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::warn;

use crate::pteron::APP_NAME;

/// Most pages kept in the history.
const MAX_HISTORY: usize = 3000;
/// Wait at least this long between two writes of the history file.
const FLUSH_EVERY_SECS: u64 = 10;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub url: String,
    pub title: String,
    pub visits: u32,
    /// Seconds since 1970 of the last visit.
    pub last: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Bookmark {
    pub url: String,
    pub title: String,
}

/// A page offered while typing in the address bar (or as a shortcut on the new tab page).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Suggestion {
    pub url: String,
    pub title: String,
    pub bookmarked: bool,
}

struct Data {
    history: Vec<Entry>,
    bookmarks: Vec<Bookmark>,
    history_dirty: bool,
    last_flush: Option<Instant>,
}

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
fn file(name: &str) -> Option<PathBuf> {
    ::dirs::config_dir().map(|dir| dir.join(APP_NAME).join(name))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn file(_name: &str) -> Option<PathBuf> {
    None
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_json(name: &str) -> Option<serde_json::Value> {
    let path = file(name)?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            warn!("Could not read {}: {error}", path.display());
            None
        },
    }
}

fn write_json(name: &str, value: &serde_json::Value) {
    let Some(path) = file(name) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(&path, value.to_string()) {
        warn!("Could not save {}: {error}", path.display());
    }
}

fn load() -> Data {
    let history = read_json("history.json")
        .and_then(|value| {
            value.get("pages").and_then(|v| v.as_array()).map(|pages| {
                pages
                    .iter()
                    .filter_map(|page| {
                        Some(Entry {
                            url: page.get("url")?.as_str()?.to_string(),
                            title: page
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            visits: page.get("visits").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
                            last: page.get("last").and_then(|v| v.as_u64()).unwrap_or(0),
                        })
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    let bookmarks = read_json("bookmarks.json")
        .and_then(|value| {
            value.get("bookmarks").and_then(|v| v.as_array()).map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(Bookmark {
                            url: item.get("url")?.as_str()?.to_string(),
                            title: item
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    Data {
        history,
        bookmarks,
        history_dirty: false,
        last_flush: None,
    }
}

fn data() -> &'static Mutex<Data> {
    static DATA: OnceLock<Mutex<Data>> = OnceLock::new();
    DATA.get_or_init(|| Mutex::new(load()))
}

fn with<R>(f: impl FnOnce(&mut Data) -> R) -> Option<R> {
    data().lock().ok().map(|mut data| f(&mut data))
}

/// The address without its fragment, so `page#a` and `page#b` are one page.
fn normalize(url: &str) -> String {
    url.split('#').next().unwrap_or(url).to_string()
}

fn is_web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Same page, ignoring a trailing slash.
fn same_page(a: &str, b: &str) -> bool {
    normalize(a).trim_end_matches('/') == normalize(b).trim_end_matches('/')
}

fn save_history(data: &mut Data) {
    let pages: Vec<serde_json::Value> = data
        .history
        .iter()
        .map(|entry| {
            serde_json::json!({
                "url": entry.url, "title": entry.title,
                "visits": entry.visits, "last": entry.last,
            })
        })
        .collect();
    write_json("history.json", &serde_json::json!({ "pages": pages }));
    data.history_dirty = false;
    data.last_flush = Some(Instant::now());
}

fn save_bookmarks(data: &Data) {
    let items: Vec<serde_json::Value> = data
        .bookmarks
        .iter()
        .map(|b| serde_json::json!({ "url": b.url, "title": b.title }))
        .collect();
    write_json("bookmarks.json", &serde_json::json!({ "bookmarks": items }));
}

/// Remember a finished page load.
pub(crate) fn record_visit(url: &str, title: &str) {
    if !is_web(url) || !crate::pteron::settings().history_enabled {
        return;
    }
    let url = normalize(url);
    let now = now_secs();
    with(|data| {
        if let Some(entry) = data.history.iter_mut().find(|e| same_page(&e.url, &url)) {
            entry.visits = entry.visits.saturating_add(1);
            entry.last = now;
            if !title.is_empty() {
                entry.title = title.to_string();
            }
        } else {
            data.history.push(Entry {
                url,
                title: title.to_string(),
                visits: 1,
                last: now,
            });
            if data.history.len() > MAX_HISTORY {
                // Drop the page that was visited least recently.
                if let Some(oldest) = data
                    .history
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.last)
                    .map(|(i, _)| i)
                {
                    data.history.swap_remove(oldest);
                }
            }
        }
        data.history_dirty = true;
    });
}

/// A page got its title after loading.
pub(crate) fn update_title(url: &str, title: &str) {
    if title.is_empty() {
        return;
    }
    with(|data| {
        let mut changed = false;
        if let Some(entry) = data.history.iter_mut().find(|e| same_page(&e.url, url)) {
            if entry.title != title {
                entry.title = title.to_string();
                changed = true;
            }
        }
        if let Some(bookmark) = data.bookmarks.iter_mut().find(|b| same_page(&b.url, url)) {
            if bookmark.title.is_empty() {
                bookmark.title = title.to_string();
                save_bookmarks(data);
            }
        }
        data.history_dirty |= changed;
    });
}

/// Write the history to disk if it changed (at most every few seconds). Cheap to call often.
pub(crate) fn flush() {
    with(|data| {
        if !data.history_dirty {
            return;
        }
        let due = data
            .last_flush
            .is_none_or(|last| last.elapsed().as_secs() >= FLUSH_EVERY_SECS);
        if due {
            save_history(data);
        }
    });
}

pub(crate) fn clear_history() {
    with(|data| {
        data.history.clear();
        save_history(data);
    });
}

pub(crate) fn history_len() -> usize {
    with(|data| data.history.len()).unwrap_or(0)
}

pub(crate) fn bookmarks() -> Vec<Bookmark> {
    with(|data| data.bookmarks.clone()).unwrap_or_default()
}

pub(crate) fn is_bookmarked(url: &str) -> bool {
    with(|data| data.bookmarks.iter().any(|b| same_page(&b.url, url))).unwrap_or(false)
}

/// Add the page to the bookmarks, or remove it if it is already there.
pub(crate) fn toggle_bookmark(url: &str, title: &str) {
    if !is_web(url) {
        return;
    }
    with(|data| {
        if let Some(index) = data.bookmarks.iter().position(|b| same_page(&b.url, url)) {
            data.bookmarks.remove(index);
        } else {
            data.bookmarks.push(Bookmark {
                url: normalize(url),
                title: title.to_string(),
            });
        }
        save_bookmarks(data);
    });
}

pub(crate) fn remove_bookmark(index: usize) {
    with(|data| {
        if index < data.bookmarks.len() {
            data.bookmarks.remove(index);
            save_bookmarks(data);
        }
    });
}

fn bare(url: &str) -> String {
    let url = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    url.trim_end_matches('/').to_lowercase()
}

/// Pages matching what is typed in the address bar, best first.
pub(crate) fn suggest(query: &str, limit: usize) -> Vec<Suggestion> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let now = now_secs();
    with(|data| {
        let mut scored: Vec<(i64, Suggestion)> = Vec::new();
        let mut consider = |url: &str, title: &str, bookmarked: bool, visits: u32, last: u64| {
            if is_error_title(title) {
                return;
            }
            let bare_url = bare(url);
            let in_url = bare_url.contains(&query);
            let in_title = title.to_lowercase().contains(&query);
            if !in_url && !in_title {
                return;
            }
            let mut score = visits as i64 * 3;
            if bookmarked {
                score += 60;
            }
            if bare_url.starts_with(&query) {
                score += 80;
            }
            if in_title {
                score += 10;
            }
            // Recent pages first: lose a point per day, at most 30.
            let days = now.saturating_sub(last) / 86_400;
            score -= days.min(30) as i64;
            scored.push((
                score,
                Suggestion {
                    url: url.to_string(),
                    title: title.to_string(),
                    bookmarked,
                },
            ));
        };
        for bookmark in &data.bookmarks {
            consider(&bookmark.url, &bookmark.title, true, 0, now);
        }
        for entry in &data.history {
            let bookmarked = data.bookmarks.iter().any(|b| same_page(&b.url, &entry.url));
            consider(&entry.url, &entry.title, bookmarked, entry.visits, entry.last);
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for (_, suggestion) in scored {
            let key = normalize(&suggestion.url).trim_end_matches('/').to_string();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(suggestion);
            if out.len() >= limit {
                break;
            }
        }
        out
    })
    .unwrap_or_default()
}

/// Pages for the new tab page: bookmarks first, then the most visited pages.
/// Titles of Pteron's own error pages: never worth suggesting.
fn is_error_title(title: &str) -> bool {
    matches!(
        title,
        "Can't open this page"
            | "Connection not private"
            | "Your connection is not private"
            | "Error loading page"
            | "Certificate error"
            | "Getting the video"
            | "Could not get this from YouTube"
    )
}

pub(crate) fn shortcuts(limit: usize, include_history: bool) -> Vec<Suggestion> {
    with(|data| {
        let mut out: Vec<Suggestion> = data
            .bookmarks
            .iter()
            .take(limit)
            .map(|b| Suggestion {
                url: b.url.clone(),
                title: b.title.clone(),
                bookmarked: true,
            })
            .collect();
        if include_history && out.len() < limit {
            let mut frequent: Vec<&Entry> = data.history.iter().collect();
            frequent.sort_by(|a, b| b.visits.cmp(&a.visits).then(b.last.cmp(&a.last)));
            for entry in frequent {
                if out.len() >= limit {
                    break;
                }
                if entry.visits < 2 ||
                    is_error_title(&entry.title) ||
                    out.iter().any(|s| same_page(&s.url, &entry.url))
                {
                    continue;
                }
                out.push(Suggestion {
                    url: entry.url.clone(),
                    title: entry.title.clone(),
                    bookmarked: false,
                });
            }
        }
        out
    })
    .unwrap_or_default()
}
