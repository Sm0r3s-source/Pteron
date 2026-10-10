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
pub(crate) const BUILD: u32 = 42;

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
    let dir = update_dir();
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("fetch.out");
    let err = dir.join("fetch.err");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&err);
    let args = |command: &mut std::process::Command| {
        command
            .args(["-sS", "--fail", "-L", "--proto", "=https", "--max-time", "20", "-A", UPSTREAM_USER_AGENT])
            .args(["-H", "Accept: application/vnd.github+json", "-o"])
            .arg(&out)
            .arg("--stderr")
            .arg(&err)
            .arg("--")
            .arg(url);
    };
    // First way: no pipes at all (curl writes to files). Windows has refused piped starts.
    let first = with_curl(|mut command| {
        args(&mut command);
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    });
    let (ok, text, why) = match first {
        Ok(status) => (
            status.success(),
            std::fs::read_to_string(&out).unwrap_or_default(),
            std::fs::read_to_string(&err).unwrap_or_default(),
        ),
        Err(_) => {
            // Second way: pipes.
            let output = with_curl(|mut command| {
                args(&mut command);
                command.output()
            })?;
            (
                output.status.success(),
                std::fs::read_to_string(&out).unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).to_string(),
            )
        },
    };
    if !ok {
        let why = why.trim().to_string();
        return Err(if why.contains("404") {
            "No release found yet.".to_string()
        } else {
            format!("Could not reach GitHub: {}", why.chars().take(160).collect::<String>())
        });
    }
    Ok(text)
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

/// Arguments for the download. Every option must come before the `--`: after it curl takes
/// everything as an address.
fn download_args(zip: &Path, errfile: &Path, url: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = ["-sS", "--fail", "-L", "--proto", "=https", "--max-time", "1800", "-A", UPSTREAM_USER_AGENT, "-o"]
        .iter()
        .map(|a| (*a).into())
        .collect();
    args.push(zip.into());
    args.push("--stderr".into());
    args.push(errfile.into());
    args.push("--".into());
    args.push(url.into());
    args
}

fn update_dir() -> PathBuf {
    std::env::temp_dir().join("pteron_update")
}

/// SHA-256 in plain Rust, so no other program has to be started to check a download.
pub(crate) fn sha256_bytes(mut reader: impl std::io::Read) -> std::io::Result<String> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    fn block(h: &mut [u32; 8], chunk: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut buffer = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        pending.extend_from_slice(&buffer[..n]);
        let whole = pending.len() / 64 * 64;
        for chunk in pending[..whole].chunks(64) {
            block(&mut h, chunk);
        }
        pending.drain(..whole);
    }
    pending.push(0x80);
    while pending.len() % 64 != 56 {
        pending.push(0);
    }
    pending.extend_from_slice(&(total * 8).to_be_bytes());
    for chunk in pending.chunks(64) {
        block(&mut h, chunk);
    }
    Ok(h.iter().map(|v| format!("{v:08x}")).collect())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Could not check the download: {e}"))?;
    sha256_bytes(std::io::BufReader::new(file)).map_err(|e| format!("Could not check the download: {e}"))
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
    let errfile = dir.join("download.err");
    let mut child = with_curl(|mut command| {
        command
            .args(download_args(&zip, &errfile, &release.url))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    })?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let why = std::fs::read_to_string(&errfile).unwrap_or_default();
                    let why = why.trim().chars().take(160).collect::<String>();
                    return Err(format!("The download failed: {why}"));
                }
                break;
            },
            Ok(None) => {
                let done = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
                set(Status::Downloading { done, total: release.size });
                std::thread::sleep(Duration::from_millis(300));
            },
            Err(e) => return Err(e.to_string()),
        }
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
        let mut last = String::new();
        for attempt in 0..4 {
            let mut command = std::process::Command::new(if attempt < 2 { "powershell.exe".to_string() } else {
                format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))
            });
            command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
            .arg(&script)
            .args(["-ProcId", &std::process::id().to_string(), "-Zip"])
            .arg(&zip)
            .arg("-Dir")
            .arg(&dir)
            .arg("-Exe")
            .arg(&exe)
            // DETACHED_PROCESS | CREATE_NO_WINDOW: keeps running after Pteron closes.
            .creation_flags(0x0000_0008 | 0x0800_0000);
            match command.spawn() {
                Ok(_) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        Err(format!("Could not start the update: {last}"))
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

    #[test]
    fn download_options_come_before_the_separator() {
        let args = download_args(Path::new("a.zip"), Path::new("e.txt"), "https://x/y.zip");
        let separator = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(separator, args.len() - 2);
        assert_eq!(args.last().unwrap(), "https://x/y.zip");
        assert!(args[..separator].iter().any(|a| a == "--stderr"));
    }

    #[test]
    fn sha256_matches_known_values() {
        assert_eq!(
            sha256_bytes(&b""[..]).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_bytes(&b"abc"[..]).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let big = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_bytes(&big[..]).unwrap(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
