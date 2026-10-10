/* Pteron: update from inside the app. Looks at the newest GitHub release, downloads the zip,
 * checks it, and lets a small helper swap the files once Pteron has closed. */

//! Everything here talks to GitHub through the system `curl` (like the video helper does), so
//! no extra libraries are needed.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::streamproxy::UPSTREAM_USER_AGENT;

/// This program's own number. Every release is tagged `build-<number>`; a bigger number than
/// this one means an update is available. (Raised with every update.)
pub(crate) const BUILD: u32 = 38;

/// Where releases are published: `https://github.com/<REPO>/releases`.
pub(crate) const REPO: &str = "Sm0r3s-source/pteron";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Release {
    pub build: u32,
    pub name: String,
    pub notes: String,
    pub url: String,
    pub size: u64,
    pub sha256_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Status {
    /// Nothing asked yet.
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { done: u64, total: u64 },
    /// Downloaded and checked; waiting for the restart.
    Ready(PathBuf),
    Failed(String),
}

static STATE: Mutex<Status> = Mutex::new(Status::Idle);
static CHECKED_THIS_RUN: Mutex<bool> = Mutex::new(false);

pub(crate) fn status() -> Status {
    STATE.lock().map(|s| s.clone()).unwrap_or(Status::Idle)
}

fn set(status: Status) {
    if let Ok(mut guard) = STATE.lock() {
        *guard = status;
    }
}

/// True while something is running and the page should keep redrawing.
pub(crate) fn busy() -> bool {
    matches!(status(), Status::Checking | Status::Downloading { .. })
}

/// Updates work for the Windows build only (the others come from the package manager).
pub(crate) fn supported() -> bool {
    cfg!(windows)
}

// ---------------------------------------------------------------- reading GitHub's answer

/// `build-35`, `Build 35`, `v35` ... -> 35 (the number at the end of the tag).
pub(crate) fn parse_build(tag: &str) -> Option<u32> {
    let digits: String = tag
        .trim()
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() || digits.len() > 6 {
        return None;
    }
    digits.parse().ok()
}

fn download_prefix() -> String {
    format!("https://github.com/{REPO}/releases/download/")
}

/// Whether `url` is a download link of this repository (GitHub ignores upper/lower case in
/// repository names and may hand out either spelling).
fn is_release_url(url: &str) -> bool {
    let prefix = download_prefix();
    url.len() >= prefix.len() && url.is_char_boundary(prefix.len()) && url[..prefix.len()].eq_ignore_ascii_case(&prefix)
}

/// The release described by GitHub's `releases/latest` answer, if it is a Pteron zip release.
pub(crate) fn parse_release(text: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let build = parse_build(tag)?;
    let assets = value.get("assets")?.as_array()?;
    let zip = assets.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".zip") && n.contains("Pteron"))
    })?;
    let zip_name = zip.get("name")?.as_str()?;
    let url = zip.get("browser_download_url")?.as_str()?.to_string();
    if !is_release_url(&url) || url.contains(['"', ' ', '\'', '`']) {
        return None;
    }
    let sha256_url = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(&format!("{zip_name}.sha256")))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .filter(|u| is_release_url(u))
        .map(str::to_string);
    let notes: String = value
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .chars()
        .take(900)
        .collect();
    Some(Release {
        build,
        name: value
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or(tag)
            .chars()
            .take(80)
            .collect(),
        notes,
        url,
        size: zip.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
        sha256_url,
    })
}

/// The 64 hex digits out of a `.sha256` file ("<hash>  name") or `certutil` output.
pub(crate) fn parse_sha256(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| word.len() == 64)
        .map(|word| word.to_ascii_lowercase())
}

// ---------------------------------------------------------------- talking to the network

/// Programs to try for curl: the plain name first, then the copy in the Windows folder.
fn curl_programs() -> Vec<String> {
    if !cfg!(windows) {
        return vec!["curl".to_string()];
    }
    let mut programs = vec!["curl.exe".to_string()];
    if let Ok(root) = std::env::var("SystemRoot") {
        programs.push(format!("{root}\\System32\\curl.exe"));
    }
    programs
}

fn curl_with(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.stdin(std::process::Stdio::null());
    command
}

/// Starts curl and runs `start` on it. If Windows refuses to start the program (it has been seen
/// to do so now and then), try again a moment later and then with the full path.
fn with_curl<T>(
    mut start: impl FnMut(std::process::Command) -> std::io::Result<T>,
) -> Result<T, String> {
    let mut last = String::new();
    for program in curl_programs() {
        for attempt in 0..3 {
            match start(curl_with(&program)) {
                Ok(value) => return Ok(value),
                Err(error) => last = error.to_string(),
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(400));
            }
        }
    }
    Err(format!("Could not start curl: {last}"))
}

fn fetch_text(url: &str) -> Result<String, String> {
    let output = with_curl(|mut command| {
        command
            .args(["-sS", "--fail", "-L", "--proto", "=https", "--max-time", "20", "-A", UPSTREAM_USER_AGENT])
            .args(["-H", "Accept: application/vnd.github+json"])
            .arg("--")
            .arg(url)
            .output()
    })?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if why.contains("404") {
            "No release found yet.".to_string()
        } else {
            format!("Could not reach GitHub: {}", why.chars().take(160).collect::<String>())
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Looks for a newer release in the background (once per few minutes at most).
pub(crate) fn check_async() {
    if !supported() || busy() {
        return;
    }
    set(Status::Checking);
    std::thread::spawn(|| {
        let result = fetch_text(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
            .and_then(|text| parse_release(&text).ok_or_else(|| "The newest release has no Pteron zip file.".to_string()));
        if let Ok(mut done) = CHECKED_THIS_RUN.lock() {
            *done = true;
        }
        set(match result {
            Ok(release) if release.build > BUILD => Status::Available(release),
            Ok(_) => Status::UpToDate,
            Err(message) => Status::Failed(message),
        });
    });
}

/// Check once when the settings are first opened in this run.
pub(crate) fn check_once() {
    let done = CHECKED_THIS_RUN.lock().map(|d| *d).unwrap_or(true);
    if !done && matches!(status(), Status::Idle) {
        check_async();
    }
}

fn update_dir() -> PathBuf {
    std::env::temp_dir().join("pteron_update")
}

fn sha256_of(path: &Path) -> Result<String, String> {
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("certutil")
            .arg("-hashfile")
            .arg(path)
            .arg("SHA256")
            .creation_flags(0x0800_0000)
            .output()
    };
    #[cfg(not(windows))]
    let output = std::process::Command::new("sha256sum").arg(path).output();
    let output = output.map_err(|e| format!("Could not check the download: {e}"))?;
    parse_sha256(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| "Could not check the download.".to_string())
}

/// Download the release zip in the background and check it.
pub(crate) fn download_async(release: Release) {
    if !supported() || busy() {
        return;
    }
    set(Status::Downloading { done: 0, total: release.size });
    std::thread::spawn(move || {
        match download(&release) {
            Ok(path) => set(Status::Ready(path)),
            Err(message) => set(Status::Failed(message)),
        }
    });
}

fn download(release: &Release) -> Result<PathBuf, String> {
    let dir = update_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not make a download folder: {e}"))?;
    let zip = dir.join(format!("Pteron-build-{}.zip", release.build));
    let mut child = with_curl(|mut command| {
        command
            .args(["-sS", "--fail", "-L", "--proto", "=https", "--max-time", "1800", "-A", UPSTREAM_USER_AGENT, "-o"])
            .arg(&zip)
            .arg("--")
            .arg(&release.url)
            .stderr(std::process::Stdio::piped())
            .spawn()
    })?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let done = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
                set(Status::Downloading { done, total: release.size });
                std::thread::sleep(Duration::from_millis(300));
            },
            Err(e) => return Err(e.to_string()),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).trim().chars().take(160).collect::<String>();
        return Err(format!("The download failed: {why}"));
    }
    let size = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
    if release.size > 0 && size != release.size {
        return Err("The download is incomplete. Try again.".into());
    }
    if let Some(url) = &release.sha256_url {
        let expected = parse_sha256(&fetch_text(url)?).ok_or("The checksum file is not readable.")?;
        if sha256_of(&zip)? != expected {
            let _ = std::fs::remove_file(&zip);
            return Err("The download does not match its checksum, so it was not used.".into());
        }
    }
    Ok(zip)
}

// ---------------------------------------------------------------- installing

/// PowerShell that waits for Pteron to close, unpacks the zip, copies it over this folder
/// and starts Pteron again. A failure is written to `update-error.txt` and the old copy is
/// started again.
const APPLY_SCRIPT: &str = r#"param([int]$ProcId, [string]$Zip, [string]$Dir, [string]$Exe)
$ErrorActionPreference = 'Stop'
$data = Join-Path $env:APPDATA 'Pteron'
New-Item -ItemType Directory -Force -Path $data | Out-Null
$log = Join-Path $data 'update.log'
function Log($m) { "$([DateTime]::Now.ToString('s')) $m" | Add-Content -Path $log }
try {
    Log "update started, waiting for Pteron ($ProcId) to close"
    try { Wait-Process -Id $ProcId -Timeout 60 } catch {}
    if (Get-Process -Id $ProcId -ErrorAction SilentlyContinue) { Stop-Process -Id $ProcId -Force; Start-Sleep -Seconds 1 }
    $stage = Join-Path $env:TEMP 'pteron_update_stage'
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    Expand-Archive -Path $Zip -DestinationPath $stage -Force
    $items = @(Get-ChildItem $stage)
    if ($items.Count -eq 1 -and $items[0].PSIsContainer) { $root = $items[0].FullName } else { $root = $stage }
    if (-not (Test-Path (Join-Path $root 'Pteron.exe'))) { throw 'The update file does not contain Pteron.exe.' }
    robocopy $root $Dir /E /R:15 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "Copying the new files failed (code $LASTEXITCODE)." }
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $Zip -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $data 'update-error.txt') -Force -ErrorAction SilentlyContinue
    Log 'update done'
} catch {
    Log "update FAILED: $_"
    Set-Content -Path (Join-Path $data 'update-error.txt') -Value "$_"
}
Start-Process -FilePath $Exe
"#;

/// Starts the helper that finishes the update. The caller then closes the window.
pub(crate) fn launch_installer() -> Result<(), String> {
    let Status::Ready(zip) = status() else {
        return Err("There is no downloaded update.".into());
    };
    if !zip.exists() {
        return Err("The downloaded file is gone. Download it again.".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("Could not find the program folder.")?.to_path_buf();
    let script = update_dir().join("apply.ps1");
    std::fs::write(&script, APPLY_SCRIPT).map_err(|e| format!("Could not prepare the update: {e}"))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
            .arg(&script)
            .args(["-ProcId", &std::process::id().to_string(), "-Zip"])
            .arg(&zip)
            .arg("-Dir")
            .arg(&dir)
            .arg("-Exe")
            .arg(&exe)
            // DETACHED_PROCESS | CREATE_NO_WINDOW: keeps running after Pteron closes.
            .creation_flags(0x0000_0008 | 0x0800_0000)
            .spawn()
            .map_err(|e| format!("Could not start the update: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (dir, exe, script);
        Err("Updates are only built in for Windows.".into())
    }
}

/// A failure left by the last update (shown once, then forgotten).
pub(crate) fn take_last_error() -> Option<String> {
    let path = ::dirs::config_dir()?.join(crate::pteron::APP_NAME).join("update-error.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(text.trim().chars().take(300).collect()).filter(|t: &String| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_numbers() {
        assert_eq!(parse_build("build-35"), Some(35));
        assert_eq!(parse_build("Build 7"), Some(7));
        assert_eq!(parse_build("v0.7.0"), Some(0));
        assert_eq!(parse_build("latest"), None);
        assert_eq!(parse_build("build-"), None);
        assert_eq!(parse_build("build-12345678"), None);
    }

    fn sample(url: &str) -> String {
        serde_json::json!({
            "tag_name": "build-35",
            "name": "Pteron build 35",
            "body": "Faster video start.\n- fixes",
            "assets": [
                {"name": "notes.txt", "browser_download_url": format!("{}notes.txt", download_prefix()), "size": 5},
                {"name": "Pteron-build-35-win64.zip", "browser_download_url": url, "size": 123456},
                {"name": "Pteron-build-35-win64.zip.sha256", "browser_download_url": format!("{}Pteron-build-35-win64.zip.sha256", download_prefix()), "size": 90}
            ]
        })
        .to_string()
    }

    #[test]
    fn reads_a_release() {
        let url = format!("{}build-35/Pteron-build-35-win64.zip", download_prefix());
        let release = parse_release(&sample(&url)).unwrap();
        assert_eq!(release.build, 35);
        assert_eq!(release.size, 123456);
        assert_eq!(release.url, url);
        assert!(release.sha256_url.unwrap().ends_with(".zip.sha256"));
        assert!(release.notes.starts_with("Faster video start."));
    }

    #[test]
    fn refuses_downloads_from_other_places() {
        assert!(parse_release(&sample("https://evil.example/Pteron.zip")).is_none());
        assert!(parse_release(&sample("http://github.com/x/y/releases/download/a/Pteron.zip")).is_none());
        assert!(parse_release("not json").is_none());
        assert!(parse_release("{\"tag_name\":\"latest\",\"assets\":[]}").is_none());
    }

    #[test]
    fn repository_name_case_does_not_matter() {
        let url = format!("{}build-35/Pteron-build-35-win64.zip", download_prefix().replace("pteron", "Pteron"));
        assert!(is_release_url(&url));
        assert!(!is_release_url("https://evil.example/Sm0r3s-source/pteron/releases/download/x.zip"));
    }

    #[test]
    fn reads_checksums() {
        let hash = "a".repeat(32) + &"B".repeat(32);
        assert_eq!(parse_sha256(&format!("{hash}  Pteron.zip\n")), Some(hash.to_ascii_lowercase()));
        let certutil = format!("SHA256 hash of file x.zip:\r\n{}\r\nCertUtil: -hashfile command completed successfully.", "0f".repeat(32));
        assert_eq!(parse_sha256(&certutil), Some("0f".repeat(32)));
        assert_eq!(parse_sha256("nothing"), None);
    }
}
