/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron's on-device crash log.
//!
//! * One small text file per day in `<config>/Pteron/crashes/`, named `crash-YYYY-MM-DD.log`.
//! * Files older than yesterday are deleted automatically (on start-up and whenever a new day
//!   begins), and every file and every entry has a size cap, so the folder can never grow.
//! * Panics are written with a backtrace. Hard crashes that skip the panic hook (a native crash
//!   on Windows, an abort, power loss) are detected on the next start by a "session marker"
//!   that is removed on clean exit.
//! * Nothing is ever sent anywhere. Page addresses are not logged by Pteron itself.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Keep today and this many previous days.
const KEEP_PREVIOUS_DAYS: i64 = 1;
/// One day's file stops growing at this size.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// A single entry (message + backtrace) is cut to this size.
const MAX_ENTRY_BYTES: usize = 24 * 1024;
const MARKER_NAME: &str = "session.running";

// ---------------------------------------------------------------- date helpers (UTC)

pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

pub(crate) fn day_number(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn file_name_for_day(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("crash-{:04}-{:02}-{:02}.log", y, m, d)
}

pub(crate) fn parse_day(name: &str) -> Option<i64> {
    let rest = name.strip_prefix("crash-")?.strip_suffix(".log")?;
    let mut it = rest.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub(crate) fn timestamp(secs: i64) -> String {
    let (y, mo, d) = civil_from_days(day_number(secs));
    let s = secs.rem_euclid(86_400);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y,
        mo,
        d,
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

// ---------------------------------------------------------------- file work (testable)

/// Delete crash files older than the retention window. Returns how many were removed.
pub(crate) fn prune_dir(dir: &Path, today: i64) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(day) = parse_day(&name) {
            if day < today - KEEP_PREVIOUS_DAYS && fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Append one entry to today's file (creating it, and pruning old days, when a new day starts).
pub(crate) fn append_to(dir: &Path, now_secs: i64, text: &str) -> bool {
    let today = day_number(now_secs);
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let path = dir.join(file_name_for_day(today));
    let is_new = !path.exists();
    if is_new {
        prune_dir(dir, today);
    }
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size >= MAX_FILE_BYTES {
        return false;
    }
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return false;
    };
    if is_new {
        let _ = writeln!(
            f,
            "Pteron crash log for {}\nversion {} on {}\nThis file stays on your computer and is deleted automatically after 2 days.\n",
            timestamp(now_secs).split(' ').next().unwrap_or(""),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        );
    }
    let body = truncate_utf8(text, MAX_ENTRY_BYTES);
    let cut = if body.len() < text.len() {
        "\n[entry cut]"
    } else {
        ""
    };
    writeln!(f, "[{}] {}{}\n", timestamp(now_secs), body, cut).is_ok()
}

/// Start-up check. Returns the text of an "unclean exit" entry when the last session left its
/// marker behind, and always writes a fresh marker.
pub(crate) fn begin_session_in(dir: &Path, now_secs: i64) -> Option<String> {
    let _ = fs::create_dir_all(dir);
    prune_dir(dir, day_number(now_secs));
    let marker = dir.join(MARKER_NAME);
    let previous = fs::read_to_string(&marker).ok();
    let _ = fs::write(&marker, now_secs.to_string());
    previous.map(|p| {
        let started = p.trim().parse::<i64>().map(timestamp).unwrap_or_default();
        format!(
            "Previous session did not exit cleanly (started {}). Pteron probably crashed or was \
             closed by the system (for example out of memory). No panic message was recorded, \
             so this was most likely a native crash.",
            started
        )
    })
}

pub(crate) fn end_session_in(dir: &Path) {
    let _ = fs::remove_file(dir.join(MARKER_NAME));
}

// ---------------------------------------------------------------- public entry points

#[cfg(not(any(target_os = "android", target_env = "ohos")))]
pub(crate) fn crash_dir() -> Option<PathBuf> {
    ::dirs::config_dir().map(|d| d.join(crate::pteron::APP_NAME).join("crashes"))
}

#[cfg(any(target_os = "android", target_env = "ohos"))]
pub(crate) fn crash_dir() -> Option<PathBuf> {
    None
}

fn now() -> i64 {
    unix_secs(SystemTime::now())
}

/// Call once, in the main (browser) process, after start-up.
pub(crate) fn begin_session() {
    let Some(dir) = crash_dir() else { return };
    if let Some(entry) = begin_session_in(&dir, now()) {
        append_to(&dir, now(), &entry);
        // Newer web features stay off for this session (see compat.rs).
        crate::compat::set_safe_mode(true);
    }
}

/// Call on a normal shutdown.
pub(crate) fn end_session() {
    if let Some(dir) = crash_dir() {
        end_session_in(&dir);
    }
}

/// Write a short diagnostic line (not a crash) into today's log, for example what the video
/// helper is doing. Does nothing in unit tests.
pub(crate) fn note(text: &str) {
    #[cfg(not(test))]
    if let Some(dir) = crash_dir() {
        append_to(&dir, now(), text);
    }
    #[cfg(test)]
    let _ = text;
}

/// Record a panic. Safe to call from the panic hook (guards against recursion).
pub(crate) fn record_panic(message: &str, thread: &str, location: Option<(&str, u32)>) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(dir) = crash_dir() {
        let at = match location {
            Some((file, line)) => format!("{}:{}", file, line),
            None => "unknown location".to_string(),
        };
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "PANIC in thread \"{}\" at {}\n{}\n{}",
            thread, at, message, bt
        );
        append_to(&dir, now(), &text);
    }
    BUSY.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pteron_crashlog_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        for day in [0, 59, 60, 365, 11_016, 20_000, 20_735, 25_000] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
        // 2026-10-09 20:00 UTC
        let secs = 1_791_576_000;
        assert_eq!(timestamp(secs), "2026-10-09 20:00:00 UTC");
        assert_eq!(file_name_for_day(day_number(secs)), "crash-2026-10-09.log");
    }

    #[test]
    fn file_names_parse() {
        let day = day_number(1_791_576_000);
        assert_eq!(parse_day(&file_name_for_day(day)), Some(day));
        assert_eq!(parse_day("crash-2026-13-01.log"), None);
        assert_eq!(parse_day("settings.json"), None);
        assert_eq!(parse_day("crash-2026-10-09.log.bak"), None);
    }

    #[test]
    fn old_days_are_deleted_and_others_kept() {
        let dir = temp("prune");
        let today = day_number(1_791_576_000);
        for back in 0..5 {
            fs::write(dir.join(file_name_for_day(today - back)), "x").unwrap();
        }
        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        let removed = prune_dir(&dir, today);
        assert_eq!(removed, 3);
        assert!(dir.join(file_name_for_day(today)).exists());
        assert!(dir.join(file_name_for_day(today - 1)).exists());
        assert!(!dir.join(file_name_for_day(today - 2)).exists());
        assert!(dir.join("notes.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_day_starts_a_fresh_file_and_prunes() {
        let dir = temp("rotate");
        let t0 = 1_791_576_000;
        assert!(append_to(&dir, t0, "first"));
        assert!(append_to(&dir, t0 + 60, "second"));
        let f0 = fs::read_to_string(dir.join(file_name_for_day(day_number(t0)))).unwrap();
        assert!(f0.contains("first") && f0.contains("second"));
        // three days later the first file is gone
        assert!(append_to(&dir, t0 + 3 * 86_400, "later"));
        assert!(!dir.join(file_name_for_day(day_number(t0))).exists());
        assert!(dir.join(file_name_for_day(day_number(t0 + 3 * 86_400))).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_caps_hold() {
        let dir = temp("caps");
        let t = 1_791_576_000;
        let big = "é".repeat(MAX_ENTRY_BYTES); // multi-byte, forces a char-boundary cut
        assert!(append_to(&dir, t, &big));
        let text = fs::read_to_string(dir.join(file_name_for_day(day_number(t)))).unwrap();
        assert!(text.contains("[entry cut]"));
        assert!((text.len() as u64) < MAX_FILE_BYTES);
        let mut n = 0;
        while append_to(&dir, t, &big) {
            n += 1;
            assert!(n < 50, "file never stopped growing");
        }
        let size = fs::metadata(dir.join(file_name_for_day(day_number(t)))).unwrap().len();
        assert!(size < MAX_FILE_BYTES + MAX_ENTRY_BYTES as u64 + 1024);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unclean_exit_is_detected() {
        let dir = temp("session");
        let t = 1_791_576_000;
        assert!(begin_session_in(&dir, t).is_none()); // first ever run
        end_session_in(&dir); // clean exit
        assert!(begin_session_in(&dir, t + 100).is_none());
        // no end_session: simulated crash
        let msg = begin_session_in(&dir, t + 200).expect("should notice the crash");
        assert!(msg.contains("did not exit cleanly"));
        assert!(msg.contains("2026-10-09"));
        let _ = fs::remove_dir_all(&dir);
    }
}
