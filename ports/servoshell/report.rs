/* Pteron: "Report a problem". Saves a small text file on the Desktop with the facts that help to
 * find a problem (versions, computer, settings, the newest crash log and the user's own note)
 * and opens the folder, so the person can send that one file to whoever gave them Pteron.
 * Nothing is sent anywhere by Pteron itself. */

use std::path::PathBuf;

/// How many lines at the end of the newest crash log go into the report.
const LOG_LINES: usize = 150;

/// The last `count` lines of `text`.
pub(crate) fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

/// Puts the pieces together into the text of the report.
pub(crate) fn compose(
    build: u32,
    when: &str,
    machine: &str,
    settings: &str,
    log_name: &str,
    log_tail: &str,
    note: &str,
) -> String {
    let note = note.trim();
    format!(
        "Pteron problem report\n\
         =====================\n\
         Pteron build {build} (Servo 0.7.0)\n\
         Made: {when}\n\n\
         What happened (written by the user):\n{}\n\n\
         This computer:\n{}\n\n\
         Settings file:\n{}\n\n\
         Newest crash log ({log_name}), last lines:\n{}\n",
        if note.is_empty() { "(nothing written)" } else { note },
        machine.trim(),
        settings.trim(),
        if log_tail.trim().is_empty() { "(no log yet)" } else { log_tail.trim() },
    )
}

#[cfg(windows)]
fn machine_info() -> String {
    use std::os::windows::process::CommandExt;
    let script = "$o=Get-CimInstance Win32_OperatingSystem;\
        $c=Get-CimInstance Win32_Processor|Select-Object -First 1;\
        'OS: '+$o.Caption+' '+$o.Version;\
        'CPU: '+$c.Name;\
        'RAM: '+[math]::Round($o.TotalVisibleMemorySize/1MB,1)+' GB';\
        Get-CimInstance Win32_VideoController|ForEach-Object{'GPU: '+$_.Name+' (driver '+$_.DriverVersion+')'}";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => format!("OS: {} (details could not be read)", std::env::consts::OS),
    }
}

#[cfg(not(windows))]
fn machine_info() -> String {
    format!("OS: {} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn newest_crash_log() -> Option<(String, String)> {
    let dir = crate::crashlog::crash_dir()?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("crash-") && name.ends_with(".log"))
        .collect();
    names.sort();
    let name = names.pop()?;
    let text = std::fs::read_to_string(dir.join(&name)).ok()?;
    Some((name, tail_lines(&text, LOG_LINES)))
}

/// A file name that sorts by time, from the current time: `Pteron-report-2026-10-10-1530.txt`.
fn file_name(now_secs: i64) -> String {
    let stamp = crate::crashlog::timestamp(now_secs); // "YYYY-MM-DD HH:MM:SS UTC"
    let digits: String = stamp
        .chars()
        .take(16)
        .map(|c| if c == ' ' { '-' } else { c })
        .filter(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    format!("Pteron-report-{digits}.txt")
}

/// Writes the report on the Desktop (or the home folder) and returns where it is.
pub(crate) fn save(note: &str) -> Result<PathBuf, String> {
    let now = std::time::SystemTime::now();
    let secs = crate::crashlog::unix_secs(now);
    let (log_name, log_tail) = newest_crash_log().unwrap_or_else(|| ("none".to_string(), String::new()));
    let settings = std::fs::read_to_string(crate::pteron::settings_file_path()).unwrap_or_default();
    let text = compose(
        crate::updater::BUILD,
        &crate::crashlog::timestamp(secs),
        &machine_info(),
        &settings,
        &log_name,
        &log_tail,
        note,
    );
    let dir = ::dirs::desktop_dir()
        .or_else(::dirs::home_dir)
        .ok_or_else(|| "Could not find the Desktop folder.".to_string())?;
    let path = dir.join(file_name(secs));
    std::fs::write(&path, text).map_err(|error| format!("Could not save the report: {error}"))?;
    Ok(path)
}

/// Opens the folder with the file selected (Windows only; elsewhere does nothing).
pub(crate) fn show_in_folder(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines("a\nb", 10), "a\nb");
        assert_eq!(tail_lines("", 3), "");
    }

    #[test]
    fn report_has_every_part() {
        let text = compose(35, "2026-10-10 15:30:00 UTC", "OS: Windows 11\nGPU: Test", "{\"a\":1}", "crash-2026-10-10.log", "line", "video froze");
        for part in ["build 35", "video froze", "GPU: Test", "{\"a\":1}", "crash-2026-10-10.log", "line"] {
            assert!(text.contains(part), "missing {part}");
        }
        assert!(compose(1, "", "", "", "none", "", "  ").contains("(nothing written)"));
        assert!(compose(1, "", "", "", "none", "", "").contains("(no log yet)"));
    }

    #[test]
    fn file_name_is_safe_for_windows() {
        let name = file_name(1_791_000_000);
        assert!(name.starts_with("Pteron-report-") && name.ends_with(".txt"), "{name}");
        assert!(!name.contains(':') && !name.contains(' '), "{name}");
    }
}
