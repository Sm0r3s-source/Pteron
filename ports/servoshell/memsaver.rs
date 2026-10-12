//! Pteron: "Memory saver". Gives up some speed and comfort to use less memory, for older or
//! low-end computers (Settings > Tabs & memory).
//!
//! * the engine's thread pools and web cache are made smaller (applied at start-up),
//! * the video helper reads less of the video ahead of the player,
//! * unused tabs go to sleep sooner,
//! * memory the window no longer needs is handed back to Windows when Pteron is in the
//!   background.
//!
//! A reading of the memory in use is also written to the crash log now and then, so a
//! problem report shows how much Pteron was using over time.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Normal,
    Low,
    Lowest,
}

impl Level {
    pub(crate) fn from_id(id: &str) -> Level {
        match id {
            "low" => Level::Low,
            "lowest" => Level::Lowest,
            _ => Level::Normal,
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Level::Normal => "normal",
            Level::Low => "low",
            Level::Lowest => "lowest",
        }
    }

    fn number(self) -> u8 {
        match self {
            Level::Normal => 0,
            Level::Low => 1,
            Level::Lowest => 2,
        }
    }

    fn from_number(n: u8) -> Level {
        match n {
            1 => Level::Low,
            2 => Level::Lowest,
            _ => Level::Normal,
        }
    }
}

/// What a level changes. `None` leaves the engine's own value alone.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Profile {
    pub layout_threads: Option<i64>,
    pub workers: Option<u64>,
    pub fallback_workers: Option<u64>,
    pub async_workers: Option<u64>,
    pub canvas_workers: Option<u64>,
    pub webrender_workers: Option<u64>,
    /// Size of the engine's in-memory web cache.
    pub http_cache: Option<u64>,
    /// How many video pieces, and at most how many bytes, the helper reads ahead.
    pub read_ahead_pieces: usize,
    pub read_ahead_bytes: u64,
    /// Unused tabs sleep after at most this many seconds.
    pub tab_sleep_cap_secs: Option<u64>,
    /// Seconds in the background before memory is handed back to Windows.
    pub trim_after_secs: Option<u64>,
}

const MB: u64 = 1024 * 1024;

pub(crate) fn profile(level: Level) -> Profile {
    match level {
        Level::Normal => Profile {
            layout_threads: None,
            workers: None,
            fallback_workers: None,
            async_workers: None,
            canvas_workers: None,
            webrender_workers: None,
            http_cache: None,
            read_ahead_pieces: 5,
            read_ahead_bytes: 32 * MB,
            tab_sleep_cap_secs: None,
            trim_after_secs: None,
        },
        Level::Low => Profile {
            layout_threads: Some(2),
            workers: Some(3),
            fallback_workers: Some(2),
            async_workers: Some(4),
            canvas_workers: Some(2),
            webrender_workers: None,
            http_cache: Some(1500),
            read_ahead_pieces: 3,
            read_ahead_bytes: 16 * MB,
            tab_sleep_cap_secs: Some(180),
            trim_after_secs: Some(15),
        },
        Level::Lowest => Profile {
            layout_threads: Some(1),
            workers: Some(2),
            fallback_workers: Some(2),
            async_workers: Some(2),
            canvas_workers: Some(1),
            webrender_workers: Some(2),
            http_cache: Some(300),
            read_ahead_pieces: 2,
            read_ahead_bytes: 8 * MB,
            tab_sleep_cap_secs: Some(60),
            trim_after_secs: Some(8),
        },
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_level(level: Level) {
    LEVEL.store(level.number(), Ordering::Relaxed);
}

pub(crate) fn level() -> Level {
    Level::from_number(LEVEL.load(Ordering::Relaxed))
}

/// (pieces, bytes) the video helper may read ahead right now.
pub(crate) fn read_ahead() -> (usize, u64) {
    let p = profile(level());
    (p.read_ahead_pieces, p.read_ahead_bytes)
}

/// The time after which an unused tab sleeps, given the user's own setting in seconds
/// (0 = never). A memory saver level shortens it, and turns sleeping on if it was off.
pub(crate) fn tab_sleep_secs(user_secs: u64) -> u64 {
    match profile(level()).tab_sleep_cap_secs {
        Some(cap) if user_secs == 0 => cap,
        Some(cap) => user_secs.min(cap),
        None => user_secs,
    }
}

// ---------------------------------------------------------------- background trimming

static FOCUS_ROUND: AtomicU64 = AtomicU64::new(0);

/// The window got or lost the keyboard focus. After a while in the background, memory the
/// window no longer needs is handed back to Windows (only when a saver level is on).
pub(crate) fn note_focus(focused: bool) {
    let round = FOCUS_ROUND.fetch_add(1, Ordering::SeqCst) + 1;
    if focused {
        return;
    }
    let Some(secs) = profile(level()).trim_after_secs else { return };
    let _ = std::thread::Builder::new().name("pteron-trim".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        if FOCUS_ROUND.load(Ordering::SeqCst) == round {
            crate::window::trim_process_memory();
        }
    });
}

// ---------------------------------------------------------------- memory readings

static TABS: AtomicUsize = AtomicUsize::new(0);
static LOGGER_STARTED: AtomicBool = AtomicBool::new(false);

/// How many tabs are open (for the log).
pub(crate) fn set_tabs(count: usize) {
    TABS.store(count, Ordering::Relaxed);
}

/// Memory in use by Pteron in MB (the working set, close to what Task Manager shows).
#[cfg(windows)]
pub(crate) fn process_mb() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the structure is zeroed and sized as the call requires, and the handle is the
    // current process, which the OS gives us.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) != 0 {
            Some(counters.WorkingSetSize as u64 / MB)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn process_mb() -> Option<u64> {
    None
}

/// Write a line to the crash log when the memory changed by a fifth or ten minutes passed.
pub(crate) fn should_log(last_logged_mb: Option<u64>, now_mb: u64, secs_since_log: u64) -> bool {
    match last_logged_mb {
        None => true,
        Some(last) => {
            let diff = now_mb.abs_diff(last);
            diff * 5 >= last.max(1) || secs_since_log >= 600
        },
    }
}

/// Starts the once-per-half-minute memory reading (once).
pub(crate) fn start_logger() {
    if LOGGER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("pteron-memory-log".into()).spawn(|| {
        let mut last: Option<(u64, std::time::Instant)> = None;
        loop {
            std::thread::sleep(Duration::from_secs(30));
            let Some(mb) = process_mb() else { return };
            let since = last.map(|(_, at)| at.elapsed().as_secs()).unwrap_or(0);
            if should_log(last.map(|(m, _)| m), mb, since) {
                crate::crashlog::note(&format!(
                    "memory: {mb} MB in use, {} tabs, memory saver {}",
                    TABS.load(Ordering::Relaxed),
                    level().id()
                ));
                last = Some((mb, std::time::Instant::now()));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_unknown_means_normal() {
        for level in [Level::Normal, Level::Low, Level::Lowest] {
            assert_eq!(Level::from_id(level.id()), level);
            assert_eq!(Level::from_number(level.number()), level);
        }
        assert_eq!(Level::from_id("???"), Level::Normal);
    }

    #[test]
    fn normal_changes_nothing() {
        let p = profile(Level::Normal);
        assert!(p.layout_threads.is_none() && p.workers.is_none() && p.http_cache.is_none());
        assert!(p.tab_sleep_cap_secs.is_none() && p.trim_after_secs.is_none());
        assert_eq!((p.read_ahead_pieces, p.read_ahead_bytes), (5, 32 * MB));
    }

    #[test]
    fn lower_levels_use_less() {
        let (low, lowest) = (profile(Level::Low), profile(Level::Lowest));
        assert!(low.layout_threads > lowest.layout_threads);
        assert!(low.http_cache > lowest.http_cache);
        assert!(low.read_ahead_bytes > lowest.read_ahead_bytes);
        assert!(low.tab_sleep_cap_secs > lowest.tab_sleep_cap_secs);
        // Nothing may be set to zero: a pool with no workers would hang the engine.
        for p in [low, lowest] {
            assert!(p.layout_threads.is_none_or(|n| n >= 1));
            for n in [p.workers, p.fallback_workers, p.async_workers, p.canvas_workers, p.webrender_workers] {
                assert!(n.is_none_or(|n| n >= 1));
            }
            assert!(p.read_ahead_pieces >= 1);
        }
    }

    #[test]
    fn tab_sleep_follows_the_level() {
        set_level(Level::Normal);
        assert_eq!((tab_sleep_secs(0), tab_sleep_secs(900)), (0, 900));
        set_level(Level::Lowest);
        assert_eq!((tab_sleep_secs(0), tab_sleep_secs(900), tab_sleep_secs(30)), (60, 60, 30));
        assert_eq!(read_ahead(), (2, 8 * MB));
        set_level(Level::Normal);
    }

    #[test]
    fn logging_rule() {
        assert!(should_log(None, 100, 0));
        assert!(!should_log(Some(100), 105, 30));
        assert!(should_log(Some(100), 130, 30));
        assert!(should_log(Some(100), 70, 30));
        assert!(should_log(Some(100), 101, 600));
    }
}
