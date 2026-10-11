/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: file downloads.
//!
//! Servo itself has no download support, so the shell catches navigations to links that
//! point at files (by extension), cancels the navigation, and downloads the file in the
//! background with the system `curl` (shipped with Windows 10 and later).
//! Safety rules:
//! * Only http and https links.
//! * A download only starts right after a click or key press (no drive-by downloads).
//! * Programs and scripts (exe, msi, bat, ...) need an extra confirmation, and are never
//!   opened from the browser.
//! * Files are written to `<Downloads>/name.part` and renamed when finished.
//! Not covered: files that a server forces to download through a `Content-Disposition`
//! header on an ordinary page address, and downloads that need the page's login cookies.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::warn;
use url::Url;

/// Extensions that are downloaded instead of opened (Servo cannot show them).
const DOWNLOAD_EXTENSIONS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "xz", "bz2", "zst", "iso", "img", "exe", "msi", "msix",
    "appx", "dmg", "pkg", "deb", "rpm", "apk", "jar", "bin", "torrent", "pdf", "doc", "docx",
    "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "epub", "bat", "cmd", "ps1", "vbs", "scr",
    "lnk", "hta", "dll", "msp", "reg", "sh",
];

/// Extensions that can run code. They need an extra confirmation and are never opened
/// from Pteron.
const DANGEROUS_EXTENSIONS: &[&str] = &[
    "exe", "msi", "msix", "appx", "bat", "cmd", "ps1", "vbs", "scr", "lnk", "hta", "dll", "msp",
    "reg", "jar", "apk", "sh", "bin", "pkg", "dmg", "deb", "rpm",
];

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum State {
    /// A program or script: waiting for the user to confirm.
    Confirm,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub id: u64,
    pub url: String,
    pub name: String,
    /// Final location (set when the download finishes).
    pub path: PathBuf,
    pub part: PathBuf,
    pub state: State,
    pub total: Option<u64>,
    pub received: u64,
    pub dangerous: bool,
}

struct Entry {
    item: Item,
    cancel: Arc<AtomicBool>,
}

static LIST: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NOTICE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

const NOTICE_SECS: u64 = 5;

fn set_notice(text: String) {
    *NOTICE.lock().unwrap() = Some((text, Instant::now()));
}

/// A short message to show on screen for a few seconds, if any.
pub(crate) fn notice() -> Option<String> {
    let guard = NOTICE.lock().unwrap();
    guard
        .as_ref()
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(NOTICE_SECS))
        .map(|(text, _)| text.clone())
}

fn extension_of(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 8 {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A safe file name for a Windows file system.
pub(crate) fn sanitize_file_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    name = name.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if name.chars().count() > 120 {
        // Keep the extension when shortening.
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.chars().take(8).collect::<String>());
        let stem: String = name.chars().take(100).collect();
        name = match ext {
            Some(ext) if !ext.is_empty() && !stem.ends_with(&ext) => format!("{stem}.{ext}"),
            _ => stem,
        };
    }
    let stem_upper = name
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if RESERVED_NAMES.contains(&stem_upper.as_str()) {
        name = format!("_{name}");
    }
    if name.is_empty() {
        name = "download".to_string();
    }
    name
}

fn file_name_from_url(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    sanitize_file_name(&percent_decode(last))
}

/// Whether following this link should download a file instead of showing a page.
pub(crate) fn is_download_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let name = file_name_from_url(url);
    extension_of(&name).is_some_and(|ext| DOWNLOAD_EXTENSIONS.contains(&ext.as_str()))
}

fn is_dangerous(name: &str) -> bool {
    extension_of(name).is_some_and(|ext| DANGEROUS_EXTENSIONS.contains(&ext.as_str()))
}

fn download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `dir/name`, or `dir/name (1).ext`, `dir/name (2).ext`, ... if it already exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

fn update(id: u64, f: impl FnOnce(&mut Item)) {
    if let Some(entry) = LIST.lock().unwrap().iter_mut().find(|e| e.item.id == id) {
        f(&mut entry.item);
    }
}

/// Called when the user followed a link to a file.
pub(crate) fn begin(url: &Url) {
    let name = file_name_from_url(url);
    let dir = download_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        set_notice(format!("Cannot create the Downloads folder: {error}"));
        return;
    }
    let path = unique_path(&dir, &name);
    let mut part = path.clone().into_os_string();
    part.push(".pteronpart");
    let dangerous = is_dangerous(&name);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let item = Item {
        id,
        url: url.to_string(),
        name: name.clone(),
        path,
        part: PathBuf::from(part),
        state: if dangerous { State::Confirm } else { State::Running },
        total: None,
        received: 0,
        dangerous,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    LIST.lock().unwrap().push(Entry {
        item,
        cancel: cancel.clone(),
    });
    if dangerous {
        set_notice(format!(
            "{name} is a program. Open the sidebar to confirm the download."
        ));
    } else {
        set_notice(format!("Downloading {name}"));
        spawn(id, cancel);
    }
}

/// A download was refused because it did not follow a click or key press.
pub(crate) fn note_blocked(url: &Url) {
    set_notice(format!(
        "Download blocked (no click): {}",
        file_name_from_url(url)
    ));
}

/// The user confirmed a program or script download.
pub(crate) fn confirm(id: u64) {
    let cancel = {
        let mut list = LIST.lock().unwrap();
        let Some(entry) = list.iter_mut().find(|e| e.item.id == id) else {
            return;
        };
        if entry.item.state != State::Confirm {
            return;
        }
        entry.item.state = State::Running;
        entry.cancel.clone()
    };
    spawn(id, cancel);
}

/// Cancel a running or waiting download.
pub(crate) fn cancel(id: u64) {
    let mut list = LIST.lock().unwrap();
    if let Some(entry) = list.iter_mut().find(|e| e.item.id == id) {
        entry.cancel.store(true, Ordering::Relaxed);
        if entry.item.state == State::Confirm {
            entry.item.state = State::Cancelled;
        }
    }
}

/// Remove an entry from the list (the downloaded file stays on disk).
pub(crate) fn remove(id: u64) {
    let mut list = LIST.lock().unwrap();
    if let Some(index) = list.iter().position(|e| e.item.id == id) {
        let running = list[index].item.state == State::Running;
        if running {
            list[index].cancel.store(true, Ordering::Relaxed);
        }
        list.remove(index);
    }
}

pub(crate) fn snapshot() -> Vec<Item> {
    LIST.lock().unwrap().iter().map(|e| e.item.clone()).collect()
}

/// Whether a download is running (the window should keep repainting).
pub(crate) fn any_active() -> bool {
    LIST.lock()
        .unwrap()
        .iter()
        .any(|e| e.item.state == State::Running)
}

/// Whether anything is waiting for a confirmation.
pub(crate) fn any_waiting() -> bool {
    LIST.lock()
        .unwrap()
        .iter()
        .any(|e| e.item.state == State::Confirm)
}

/// Open the folder with the file selected. Safe for every file type.
pub(crate) fn show_in_folder(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `explorer /select,"path"` needs the quotes exactly like this, so the argument is
        // passed raw instead of letting Rust quote the whole `/select,path` string.
        let result = Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(error) = result {
            set_notice(format!("Could not open the folder: {error}"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = path.parent() {
            let _ = Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// Open a finished file with its default program. Never used for programs or scripts.
pub(crate) fn open_file(item: &Item) {
    if item.dangerous || item.state != State::Done {
        return;
    }
    if !item.path.exists() {
        set_notice("The file is gone (moved or deleted).".to_string());
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The system "open this file" handler; the path is a single argument, so no shell
        // ever parses it.
        let result = Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(&item.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn();
        if let Err(error) = result {
            set_notice(format!("Could not open the file: {error}"));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(&item.path).spawn();
    }
}

/// Opens a web address in the computer's default browser (for sites that Pteron cannot get
/// into, such as pages that insist on a browser check).
pub(crate) fn open_in_default_browser(url: &str) {
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.chars().any(|c| c.is_control() || c == ' ') {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn curl_command() -> Command {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("curl.exe");
        // CREATE_NO_WINDOW
        command.creation_flags(0x0800_0000);
        command
    };
    #[cfg(not(windows))]
    let command = Command::new("curl");
    command
}

/// The size from the `Content-Length` header of the last response, if the server tells it.
fn head_length(url: &str) -> Option<u64> {
    let output = curl_command()
        .args([
            "-sIL",
            "--max-time",
            "20",
            "--max-redirs",
            "10",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
        .last()
}

fn spawn(id: u64, cancel: Arc<AtomicBool>) {
    let (url, part) = {
        let list = LIST.lock().unwrap();
        let Some(entry) = list.iter().find(|e| e.item.id == id) else {
            return;
        };
        (entry.item.url.clone(), entry.item.part.clone())
    };
    std::thread::Builder::new()
        .name(format!("pteron-download-{id}"))
        .spawn(move || run(id, url, part, cancel))
        .ok();
}

fn run(id: u64, url: String, part: PathBuf, cancel: Arc<AtomicBool>) {
    if let Some(total) = head_length(&url) {
        update(id, |item| item.total = Some(total));
    }
    let mut child = match curl_command()
        .args([
            "-L",
            "--fail",
            "-sS",
            "--max-redirs",
            "10",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--connect-timeout",
            "20",
            "-A",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Pteron/0.1",
            "-o",
        ])
        .arg(&part)
        .arg("--")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            warn!("Could not start curl: {error}");
            update(id, |item| {
                item.state = State::Failed(
                    "Could not start the download tool (curl). Is Windows up to date?".into(),
                )
            });
            return;
        },
    };

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&part);
            update(id, |item| item.state = State::Cancelled);
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let received = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                update(id, |item| item.received = received);
                std::thread::sleep(Duration::from_millis(250));
            },
            Err(error) => {
                let _ = std::fs::remove_file(&part);
                update(id, |item| item.state = State::Failed(error.to_string()));
                return;
            },
        }
    };

    if status.success() {
        let name = LIST
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.item.id == id)
            .map(|e| e.item.name.clone())
            .unwrap_or_else(|| "download".into());
        let final_path = unique_path(&download_dir(), &name);
        match std::fs::rename(&part, &final_path) {
            Ok(()) => {
                let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
                update(id, |item| {
                    item.path = final_path;
                    item.received = size;
                    item.total = Some(size);
                    item.state = State::Done;
                });
                set_notice(format!("Downloaded {name}"));
            },
            Err(error) => {
                update(id, |item| {
                    item.state = State::Failed(format!("Could not save the file: {error}"))
                });
            },
        }
    } else {
        let mut message = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut message);
        }
        let _ = std::fs::remove_file(&part);
        let message = message.trim();
        update(id, |item| {
            item.state = State::Failed(if message.is_empty() {
                "The download failed.".into()
            } else {
                message.chars().take(160).collect()
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_download_links() {
        let yes = |s: &str| is_download_url(&Url::parse(s).unwrap());
        assert!(yes("https://x.org/a/file.zip"));
        assert!(yes("https://x.org/a/Report%20Final.PDF?x=1"));
        assert!(!yes("https://x.org/index.html"));
        assert!(!yes("https://x.org/"));
        assert!(!yes("https://x.org/video.mp4"));
        assert!(!yes("ftp://x.org/file.zip"));
        assert!(!yes("file:///c:/file.zip"));
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("a/b\\c:d.zip"), "a_b_c_d.zip");
        assert_eq!(sanitize_file_name("..."), "download");
        assert_eq!(sanitize_file_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_file_name("name. "), "name");
        assert!(sanitize_file_name(&"a".repeat(300)).chars().count() <= 130);
    }

    #[test]
    fn dangerous_types() {
        assert!(is_dangerous("setup.EXE"));
        assert!(is_dangerous("run.ps1"));
        assert!(!is_dangerous("doc.pdf"));
        assert!(!is_dangerous("archive.zip"));
    }

    #[test]
    fn decodes_percent() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
