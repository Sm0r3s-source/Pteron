/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: keeps bookmarks and settings in step between computers through a folder the user
//! picks (OneDrive, Google Drive, Dropbox, Syncthing ...). Pteron only reads and writes one
//! small file there, `Pteron-sync.json`; the folder's own app moves it between computers.
//! Whichever side was changed last wins, for the bookmarks and for the settings separately.
//! History, passwords and cookies are never synced.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::library::Bookmark;

const FILE_NAME: &str = "Pteron-sync.json";
/// Timestamps closer than this many seconds count as the same moment.
const SLACK: u64 = 1;

/// The settings that travel between computers (not hardware, paths or first-run flags).
const SYNCED: [&str; 17] = [
    "engine", "searxng_url", "theme", "custom_bg", "accent", "accent_custom", "newtab_bg", "newtab_color",
    "newtab_tiles", "tab_sleep_minutes", "sidebar_pinned", "startup", "homepage_url", "youtube_player",
    "fast_start", "https_warning", "adblock",
];

#[derive(Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub bookmarks: Vec<Bookmark>,
    pub bookmarks_modified: u64,
    pub settings: serde_json::Value,
    pub settings_modified: u64,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum Step {
    Nothing,
    /// The other computer's copy is newer: take it.
    Pull,
    /// This computer's copy is newer (or there is no copy yet): write it.
    Push,
}

pub(crate) fn decide(local: u64, remote: Option<u64>) -> Step {
    match remote {
        None => Step::Push,
        Some(remote) if remote > local + SLACK => Step::Pull,
        Some(remote) if local > remote + SLACK => Step::Push,
        Some(_) => Step::Nothing,
    }
}

pub(crate) fn to_json(snapshot: &Snapshot) -> serde_json::Value {
    let items: Vec<serde_json::Value> = snapshot
        .bookmarks
        .iter()
        .map(|b| serde_json::json!({ "url": b.url, "title": b.title, "folder": b.folder }))
        .collect();
    serde_json::json!({
        "app": "pteron",
        "version": 1,
        "bookmarks": items,
        "bookmarks_modified": snapshot.bookmarks_modified,
        "settings": snapshot.settings,
        "settings_modified": snapshot.settings_modified,
    })
}

pub(crate) fn from_json(value: &serde_json::Value) -> Option<Snapshot> {
    if value.get("app")?.as_str()? != "pteron" {
        return None;
    }
    let bookmarks = value
        .get("bookmarks")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let url = item.get("url")?.as_str()?.to_string();
            // Only ordinary web addresses are accepted from the file.
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return None;
            }
            Some(Bookmark {
                url,
                title: item.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                folder: item.get("folder").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect();
    Some(Snapshot {
        bookmarks,
        bookmarks_modified: value.get("bookmarks_modified")?.as_u64()?,
        settings: value.get("settings").cloned().unwrap_or(serde_json::Value::Null),
        settings_modified: value.get("settings_modified")?.as_u64()?,
    })
}

/// Only the settings that are meant to travel, as JSON.
pub(crate) fn synced_settings(settings: &crate::pteron::Settings) -> serde_json::Value {
    let all = serde_json::json!({
        "engine": settings.engine,
        "searxng_url": settings.searxng_url,
        "theme": settings.theme,
        "custom_bg": settings.custom_bg,
        "accent": settings.accent,
        "accent_custom": settings.accent_custom,
        "newtab_bg": settings.newtab_bg,
        "newtab_color": settings.newtab_color,
        "newtab_tiles": settings.newtab_tiles,
        "tab_sleep_minutes": settings.tab_sleep_minutes,
        "sidebar_pinned": settings.sidebar_pinned,
        "startup": settings.startup,
        "homepage_url": settings.homepage_url,
        "youtube_player": settings.youtube_player,
        "fast_start": settings.fast_start,
        "https_warning": settings.https_warning,
        "adblock": settings.adblock,
    });
    let mut kept = serde_json::Map::new();
    for key in SYNCED {
        if let Some(value) = all.get(key) {
            kept.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(kept)
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn modified(path: &Path) -> u64 {
    std::fs::metadata(path).and_then(|m| m.modified()).map(secs).unwrap_or(0)
}

fn set_modified(path: &Path, when: u64) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(when));
    }
}

fn bookmarks_path() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    {
        ::dirs::config_dir().map(|dir| dir.join(crate::pteron::APP_NAME).join("bookmarks.json"))
    }
    #[cfg(any(target_os = "android", target_env = "ohos"))]
    {
        None
    }
}

fn settings_path() -> Option<PathBuf> {
    let path = crate::pteron::settings_file_path();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn read_remote(file: &Path) -> Result<Option<Snapshot>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Could not read the sync file: {e}")),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "The sync file is not readable (it may still be uploading).".to_string())?;
    from_json(&value).map(Some).ok_or_else(|| "The sync file is not a Pteron sync file.".to_string())
}

fn write_remote(file: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let temp = file.with_extension("json.tmp");
    std::fs::write(&temp, to_json(snapshot).to_string()).map_err(|e| format!("Could not write the sync file: {e}"))?;
    std::fs::rename(&temp, file).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("Could not write the sync file: {e}")
    })
}

/// Brings this computer and the sync folder in step. Returns a short message about what happened.
pub(crate) fn sync_now(allow_pull: bool) -> Result<String, String> {
    let settings = crate::pteron::settings();
    if settings.sync_folder.trim().is_empty() {
        return Err("No sync folder chosen.".into());
    }
    let dir = PathBuf::from(settings.sync_folder.trim());
    if !dir.is_dir() {
        return Err("The sync folder is not available right now.".into());
    }
    let file = dir.join(FILE_NAME);
    let remote = read_remote(&file)?;
    let bookmarks_file = bookmarks_path().ok_or("Syncing is not available here.")?;
    let settings_file = settings_path().ok_or("Syncing is not available here.")?;

    let mut notes: Vec<&str> = Vec::new();
    let mut push = false;
    match decide(modified(&bookmarks_file), remote.as_ref().map(|r| r.bookmarks_modified)) {
        Step::Pull if !allow_pull => {},
        Step::Pull => {
            let remote = remote.as_ref().expect("a pull needs a remote copy");
            crate::library::replace_bookmarks(remote.bookmarks.clone());
            set_modified(&bookmarks_file, remote.bookmarks_modified);
            notes.push("bookmarks updated from the other computer");
        },
        Step::Push => push = true,
        Step::Nothing => {},
    }
    match decide(modified(&settings_file), remote.as_ref().map(|r| r.settings_modified)) {
        Step::Pull if !allow_pull => {},
        Step::Pull => {
            let remote = remote.as_ref().expect("a pull needs a remote copy");
            let mut updated = crate::pteron::settings();
            crate::pteron::apply_json(&mut updated, &remote.settings);
            crate::pteron::update(updated);
            set_modified(&settings_file, remote.settings_modified);
            notes.push("settings updated (restart Pteron to see all of them)");
        },
        Step::Push => push = true,
        Step::Nothing => {},
    }
    if push {
        let now = secs(SystemTime::now()).max(2);
        let nonzero = |t: u64| if t == 0 { now } else { t };
        let snapshot = Snapshot {
            bookmarks: crate::library::bookmarks(),
            bookmarks_modified: nonzero(modified(&bookmarks_file)),
            settings: synced_settings(&crate::pteron::settings()),
            settings_modified: nonzero(modified(&settings_file)),
        };
        write_remote(&file, &snapshot)?;
        notes.push("saved to the sync folder");
    }
    Ok(if notes.is_empty() { "Already in sync.".to_string() } else { format!("Synced: {}.", notes.join(", ")) })
}

/// Called right after a folder was chosen. If the folder already holds another computer's copy,
/// that copy is adopted (this computer's bookmarks are added to it, not lost); otherwise this
/// computer's copy is written there.
pub(crate) fn join() -> Result<String, String> {
    let settings = crate::pteron::settings();
    let dir = PathBuf::from(settings.sync_folder.trim());
    if !dir.is_dir() {
        return Err("The sync folder is not available right now.".into());
    }
    let file = dir.join(FILE_NAME);
    let Some(remote) = read_remote(&file)? else {
        return sync_now(true);
    };
    let bookmarks_file = bookmarks_path().ok_or("Syncing is not available here.")?;
    let settings_file = settings_path().ok_or("Syncing is not available here.")?;
    let mine = crate::library::bookmarks();
    crate::library::replace_bookmarks(remote.bookmarks.clone());
    let added = crate::library::import_bookmarks(mine);
    if added == 0 {
        set_modified(&bookmarks_file, remote.bookmarks_modified);
    }
    let mut updated = crate::pteron::settings();
    crate::pteron::apply_json(&mut updated, &remote.settings);
    crate::pteron::update(updated);
    set_modified(&settings_file, remote.settings_modified);
    // anything this computer added is now newer than the file, so it goes out too
    let rest = sync_now(true)?;
    Ok(format!("Joined the sync folder: settings and bookmarks taken from the other computer. {rest}"))
}

/// At launch, before the window opens, so a newer copy is in place from the start.
pub(crate) fn startup() {
    if crate::pteron::settings().sync_folder.trim().is_empty() {
        return;
    }
    match sync_now(true) {
        Ok(message) => crate::crashlog::note(&format!("sync: {message}")),
        Err(message) => crate::crashlog::note(&format!("sync: {message}")),
    }
}

static PENDING: AtomicBool = AtomicBool::new(false);

/// Something was saved: look at the sync folder in a moment (several changes in a row count once).
pub(crate) fn changed_soon() {
    if PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1500));
        PENDING.store(false, Ordering::SeqCst);
        if !crate::pteron::settings().sync_folder.trim().is_empty() {
            // In the background only this computer's changes are written out; another computer's
            // changes are taken in at launch or with "Sync now".
            let _ = sync_now(false);
        }
    });
}

// ---------------------------------------------------------------- choosing the folder

static PICKED: Mutex<Option<Result<String, String>>> = Mutex::new(None);
static CHOOSING: AtomicBool = AtomicBool::new(false);

pub(crate) fn choosing() -> bool {
    CHOOSING.load(Ordering::SeqCst)
}

/// The folder chosen in the last dialog (once).
pub(crate) fn take_chosen() -> Option<Result<String, String>> {
    PICKED.lock().ok().and_then(|mut g| g.take())
}

const FOLDER_SCRIPT: &str = r#"param([string]$Out)
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = 'Choose a folder that your other computer also has (for example inside OneDrive, Google Drive or Dropbox)'
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { Set-Content -Path $Out -Value $d.SelectedPath -Encoding UTF8 }
"#;

pub(crate) fn choose_folder_async() {
    if CHOOSING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let result = crate::appearance::run_chooser_script(FOLDER_SCRIPT).transpose();
        if let Ok(mut g) = PICKED.lock() {
            *g = result;
        }
        CHOOSING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newer_side_wins() {
        assert_eq!(decide(100, None), Step::Push);
        assert_eq!(decide(100, Some(500)), Step::Pull);
        assert_eq!(decide(500, Some(100)), Step::Push);
        assert_eq!(decide(500, Some(500)), Step::Nothing);
        assert_eq!(decide(500, Some(501)), Step::Nothing);
        assert_eq!(decide(0, Some(1)), Step::Nothing);
        assert_eq!(decide(0, Some(5000)), Step::Pull);
    }

    fn sample() -> Snapshot {
        Snapshot {
            bookmarks: vec![
                Bookmark { url: "https://example.com/a".into(), title: "A".into(), folder: "F".into() },
                Bookmark { url: "http://example.org/".into(), title: "".into(), folder: String::new() },
            ],
            bookmarks_modified: 1_700_000_000,
            settings: serde_json::json!({ "theme": "midnight", "adblock": false }),
            settings_modified: 1_700_000_100,
        }
    }

    #[test]
    fn file_round_trip() {
        let dir = std::env::temp_dir().join("pteron_sync_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE_NAME);
        assert_eq!(read_remote(&file).unwrap(), None);
        write_remote(&file, &sample()).unwrap();
        assert_eq!(read_remote(&file).unwrap(), Some(sample()));
        assert!(!dir.join("Pteron-sync.json.tmp").exists());
        std::fs::write(&file, "{ half a file").unwrap();
        assert!(read_remote(&file).unwrap_err().contains("not readable"));
        std::fs::write(&file, r#"{"app":"other"}"#).unwrap();
        assert!(read_remote(&file).unwrap_err().contains("not a Pteron"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_web_bookmarks_are_taken_from_a_file() {
        let mut json = to_json(&sample());
        json["bookmarks"] = serde_json::json!([
            { "url": "javascript:alert(1)", "title": "x" },
            { "url": "file:///C:/secret", "title": "y" },
            { "url": "https://ok.example/", "title": "z" },
        ]);
        let snap = from_json(&json).unwrap();
        assert_eq!(snap.bookmarks.len(), 1);
        assert_eq!(snap.bookmarks[0].url, "https://ok.example/");
    }

    #[test]
    fn only_the_right_settings_travel() {
        let mut s = crate::pteron::Settings::default();
        s.theme = "forest".into();
        s.sync_folder = "C:\\secret\\folder".into();
        s.welcome_shown = true;
        s.hw_video_decode = false;
        let json = synced_settings(&s);
        assert_eq!(json["theme"], "forest");
        assert!(json.get("sync_folder").is_none());
        assert!(json.get("welcome_shown").is_none());
        assert!(json.get("hw_video_decode").is_none());
        assert_eq!(json.as_object().unwrap().len(), SYNCED.len());
        // and a received file cannot set a sync folder either
        let mut other = crate::pteron::Settings::default();
        crate::pteron::apply_json(&mut other, &serde_json::json!({ "theme": "paper", "sync_folder": "X:\\evil", "adblock": false }));
        assert_eq!(other.theme, "paper");
        assert!(!other.adblock);
        assert!(other.sync_folder.is_empty());
    }

    #[test]
    fn modification_time_can_be_set() {
        let dir = std::env::temp_dir().join("pteron_sync_mtime");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.json");
        std::fs::write(&file, "{}").unwrap();
        set_modified(&file, 1_600_000_000);
        assert_eq!(modified(&file), 1_600_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
