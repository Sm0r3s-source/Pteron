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
    /// Folder name; empty when the bookmark is not in a folder.
    pub folder: String,
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
                            folder: item
                                .get("folder")
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
    crate::sync::changed_soon();
    let items: Vec<serde_json::Value> = data
        .bookmarks
        .iter()
        .map(|b| serde_json::json!({ "url": b.url, "title": b.title, "folder": b.folder }))
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
                folder: String::new(),
            });
        }
        save_bookmarks(data);
    });
}

/// Puts a bookmark into a folder (an empty name takes it out of its folder).
pub(crate) fn set_bookmark_folder(index: usize, folder: &str) {
    let folder = folder.trim().chars().take(40).collect::<String>();
    with(|data| {
        if let Some(bookmark) = data.bookmarks.get_mut(index) {
            if bookmark.folder != folder {
                bookmark.folder = folder;
                save_bookmarks(data);
            }
        }
    });
}

/// Moves a bookmark to where another one is (drag and drop in the sidebar).
pub(crate) fn move_bookmark(from: usize, to: usize) {
    with(|data| {
        if from != to && from < data.bookmarks.len() && to < data.bookmarks.len() {
            let item = data.bookmarks.remove(from);
            data.bookmarks.insert(to, item);
            save_bookmarks(data);
        }
    });
}

/// The folder names in use, sorted, without repeats.
pub(crate) fn folder_names(list: &[Bookmark]) -> Vec<String> {
    let mut names: Vec<String> = list
        .iter()
        .filter(|b| !b.folder.is_empty())
        .map(|b| b.folder.clone())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

pub(crate) fn remove_bookmark(index: usize) {
    with(|data| {
        if index < data.bookmarks.len() {
            data.bookmarks.remove(index);
            save_bookmarks(data);
        }
    });
}

// ---------------------------------------------------------------- import and export

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The bookmarks in the file format every browser can import ("Netscape bookmark file").
pub(crate) fn bookmarks_to_html(list: &[Bookmark]) -> String {
    let mut out = String::from(
        "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n<META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n<TITLE>Bookmarks</TITLE>\n<H1>Bookmarks</H1>\n<DL><p>\n",
    );
    let line = |item: &Bookmark, indent: &str| {
        let title = if item.title.is_empty() { &item.url } else { &item.title };
        format!(
            "{indent}<DT><A HREF=\"{}\">{}</A>\n",
            escape_html(&item.url),
            escape_html(title)
        )
    };
    for item in list.iter().filter(|b| b.folder.is_empty()) {
        out.push_str(&line(item, "    "));
    }
    for name in folder_names(list) {
        out.push_str(&format!("    <DT><H3>{}</H3>\n    <DL><p>\n", escape_html(&name)));
        for item in list.iter().filter(|b| b.folder == name) {
            out.push_str(&line(item, "        "));
        }
        out.push_str("    </DL><p>\n");
    }
    out.push_str("</DL><p>\n");
    out
}

/// Reads the links out of a bookmark file from Chrome, Edge, Firefox or Pteron.
/// A link keeps the name of the folder it was in; the browsers' own top folders
/// (bookmarks bar, other bookmarks) count as "no folder".
pub(crate) fn parse_bookmarks_html(text: &str) -> Vec<Bookmark> {
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    // Names of the folders we are inside ("" for an unnamed or top-level one).
    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    let mut at = 0;
    loop {
        let next = [("<a ", 0), ("<h3", 1), ("<dl", 2), ("</dl", 3)]
            .iter()
            .filter_map(|(pat, kind)| lower[at..].find(pat).map(|i| (at + i, *kind)))
            .min();
        let Some((tag_start, kind)) = next else { break };
        let Some(tag_end) = lower[tag_start..].find('>').map(|i| tag_start + i) else { break };
        match kind {
            1 => {
                let close = lower[tag_end..].find("</h3>").map(|i| tag_end + i).unwrap_or(text.len());
                let attrs = &lower[tag_start..tag_end];
                let name = if attrs.contains("personal_toolbar_folder") ||
                    attrs.contains("unfiled_bookmarks_folder")
                {
                    String::new()
                } else {
                    unescape_html(text[tag_end + 1..close].trim())
                };
                pending = Some(name);
                at = close.min(text.len());
            },
            2 => {
                stack.push(pending.take().unwrap_or_default());
                at = tag_end;
            },
            3 => {
                stack.pop();
                at = tag_end;
            },
            _ => {
                let tag = &text[tag_start..tag_end];
                let tag_lower = &lower[tag_start..tag_end];
                let close = lower[tag_end..].find("</a>").map(|i| tag_end + i).unwrap_or(text.len());
                at = close.min(text.len());
                let Some(href_at) = tag_lower.find("href=") else { continue };
                let rest = &tag[href_at + 5..];
                let (quote, rest) = match rest.chars().next() {
                    Some(c @ ('"' | '\'')) => (Some(c), &rest[1..]),
                    _ => (None, rest),
                };
                let url = match quote {
                    Some(c) => rest.split(c).next().unwrap_or(""),
                    None => rest.split_whitespace().next().unwrap_or(""),
                };
                let url = unescape_html(url);
                if !is_web(&url) {
                    continue;
                }
                let title = unescape_html(text[tag_end + 1..close].trim());
                let folder = stack
                    .iter()
                    .rev()
                    .find(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_default();
                found.push(Bookmark { url, title, folder });
            },
        }
    }
    found
}

/// Replaces all bookmarks (used when another computer's list is newer).
pub(crate) fn replace_bookmarks(list: Vec<Bookmark>) {
    with(|data| {
        data.bookmarks = list;
        save_bookmarks(data);
    });
}

/// Adds the bookmarks that are not saved yet. Returns how many were new.
pub(crate) fn import_bookmarks(list: Vec<Bookmark>) -> usize {
    with(|data| {
        let mut added = 0;
        for item in list {
            if data.bookmarks.iter().any(|b| same_page(&b.url, &item.url)) {
                continue;
            }
            data.bookmarks.push(Bookmark { url: normalize(&item.url), title: item.title, folder: item.folder });
            added += 1;
        }
        if added > 0 {
            save_bookmarks(data);
        }
        added
    })
    .unwrap_or(0)
}

/// Writes the bookmarks to the Desktop. Returns the file.
pub(crate) fn export_bookmarks() -> Result<PathBuf, String> {
    let list = bookmarks();
    if list.is_empty() {
        return Err("There are no bookmarks to export yet.".into());
    }
    let dir = ::dirs::desktop_dir()
        .or_else(::dirs::home_dir)
        .ok_or_else(|| "Could not find the Desktop folder.".to_string())?;
    let path = dir.join("Pteron-bookmarks.html");
    std::fs::write(&path, bookmarks_to_html(&list)).map_err(|e| format!("Could not save the file: {e}"))?;
    Ok(path)
}

/// Looks on the Desktop and in Downloads for the newest bookmark file (an .html file with
/// "bookmark" in its name, which is what Chrome, Edge and Firefox export) and imports it.
pub(crate) fn import_from_downloads() -> Result<usize, String> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in [::dirs::desktop_dir(), ::dirs::download_dir()].into_iter().flatten() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !name.contains("bookmark") || !(name.ends_with(".html") || name.ends_with(".htm")) {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
            if best.as_ref().map_or(true, |(time, _)| modified > *time) {
                best = Some((modified, entry.path()));
            }
        }
    }
    let (_, path) = best.ok_or_else(|| {
        "No bookmark file found. Export one from Chrome, Edge or Firefox into Downloads or the Desktop first.".to_string()
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let list = parse_bookmarks_html(&text);
    if list.is_empty() {
        return Err(format!("{} has no bookmarks in it.", path.display()));
    }
    Ok(import_bookmarks(list))
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

/// Bookmark folders and their pages, for the new tab page.
pub(crate) fn folder_shortcuts() -> Vec<(String, Vec<Suggestion>)> {
    with(|data| {
        folder_names(&data.bookmarks)
            .into_iter()
            .map(|name| {
                let pages = data
                    .bookmarks
                    .iter()
                    .filter(|b| b.folder == name)
                    .map(|b| Suggestion { url: b.url.clone(), title: b.title.clone(), bookmarked: true })
                    .collect();
                (name, pages)
            })
            .collect()
    })
    .unwrap_or_default()
}

pub(crate) fn shortcuts(limit: usize, include_history: bool) -> Vec<Suggestion> {
    with(|data| {
        let mut out: Vec<Suggestion> = data
            .bookmarks
            .iter()
            .filter(|b| b.folder.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn b(url: &str, title: &str) -> Bookmark {
        Bookmark { url: url.into(), title: title.into(), folder: String::new() }
    }

    #[test]
    fn export_then_import_round_trips() {
        let list = vec![b("https://example.com/a?x=1&y=2", "Tom & \"Jerry\" <3"), b("https://rust-lang.org/", "")];
        let html = bookmarks_to_html(&list);
        let back = parse_bookmarks_html(&html);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], list[0]);
        assert_eq!(back[1].url, "https://rust-lang.org/");
    }

    #[test]
    fn reads_a_chrome_style_file() {
        let html = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
  <DT><H3 ADD_DATE="1" PERSONAL_TOOLBAR_FOLDER="true">Bookmarks bar</H3>
  <DL><p>
    <DT><A HREF="https://www.wikipedia.org/" ADD_DATE="16" ICON="data:image/png;base64,AAAA">Wikipedia</A>
    <DT><a href='https://news.ycombinator.com/' >Hacker News</a>
    <DT><A HREF="javascript:alert(1)">bad</A>
    <DT><A HREF="chrome://settings">internal</A>
  </DL><p>
</DL><p>"#;
        let list = parse_bookmarks_html(html);
        assert_eq!(list, vec![b("https://www.wikipedia.org/", "Wikipedia"), b("https://news.ycombinator.com/", "Hacker News")]);
        assert!(parse_bookmarks_html("nothing here").is_empty());
    }

    #[test]
    fn folders_round_trip_and_nest() {
        let mut list = vec![b("https://a.example/", "A"), b("https://b.example/", "B"), b("https://c.example/", "C")];
        list[1].folder = "Work & Study".into();
        list[2].folder = "Work & Study".into();
        let back = parse_bookmarks_html(&bookmarks_to_html(&list));
        assert_eq!(back.len(), 3);
        assert_eq!(back.iter().filter(|x| x.folder == "Work & Study").count(), 2);
        assert_eq!(back.iter().find(|x| x.url.contains("a.example")).unwrap().folder, "");
        assert_eq!(folder_names(&list), vec!["Work & Study".to_string()]);
    }

    #[test]
    fn reads_folders_from_a_browser_file() {
        let html = r#"<DL><p>
<DT><H3 PERSONAL_TOOLBAR_FOLDER="true">Bookmarks bar</H3>
<DL><p>
<DT><A HREF="https://top.example/">Top</A>
<DT><H3>News</H3>
<DL><p>
<DT><A HREF="https://news.example/">N</A>
</DL><p>
<DT><A HREF="https://after.example/">After</A>
</DL><p></DL><p>"#;
        let list = parse_bookmarks_html(html);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].folder, "");
        assert_eq!(list[1].folder, "News");
        assert_eq!(list[2].folder, "");
    }
}
