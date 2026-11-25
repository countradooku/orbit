//! Crash reports: a panic anywhere in the app writes one to
//! `<data folder>/crashes/` before the process ends (release builds abort on
//! panic), and the next start offers it. Nothing is sent anywhere.
//!
//! A `running` marker beside the reports tells a crash from a clean quit:
//! written at start, removed on quit. A report found with the marker still
//! there means Orbit closed unexpectedly; without it (a panic on a thread
//! that didn't end the app, in a debug build) it means Orbit hit an error.

use std::path::{Path, PathBuf};

use super::build;
use super::logging;
use super::redact;

const MARKER: &str = "running";
/// Reports kept; older ones are removed.
const KEEP: usize = 10;

pub fn crashes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes")
}

/// A report the user hasn't dismissed yet.
#[derive(Clone, Debug, PartialEq)]
pub struct CrashReport {
    pub path: PathBuf,
    pub text: String,
    /// The process ended (the running marker was left behind).
    pub closed: bool,
}

/// Installs the panic hook. The previous hook (printing to stderr) still
/// runs after the report is written.
pub fn install_hook(data_dir: &Path) {
    let dir = crashes_dir(data_dir);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = compose(info);
        let _ = write_report(&dir, &report);
        previous(info);
    }));
}

fn compose(info: &std::panic::PanicHookInfo<'_>) -> String {
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned());
    let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
    let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
    let backtrace = std::backtrace::Backtrace::force_capture();
    let mut report = String::new();
    report.push_str("Orbit crash report\n\n");
    report.push_str(&format!("Version:  {}\n", build::summary()));
    report.push_str(&format!("System:   {}\n", build::system()));
    report.push_str(&format!("Time:     {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z")));
    report.push_str(&format!("Thread:   {thread}\n"));
    report.push_str(&format!("Panic:    {message}\n"));
    report.push_str(&format!("Location: {location}\n\n"));
    report.push_str("Backtrace:\n");
    report.push_str(&backtrace.to_string());
    report.push_str("\n\nLast log lines:\n");
    let lines = logging::recent_lines();
    if lines.is_empty() {
        report.push_str("(none)\n");
    }
    for line in lines.iter().rev().take(60).rev() {
        report.push_str(line);
        report.push('\n');
    }
    // Reports stay on this machine, but they are made to be shared.
    redact::redact(&report, &[])
}

fn write_report(dir: &Path, report: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!("crash-{}-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S"), std::process::id());
    let path = dir.join(name);
    std::fs::write(&path, report)?;
    Ok(path)
}

/// At start: the newest undismissed report, if any, then marks this run as
/// running. Old reports beyond the newest [`KEEP`] are removed.
pub fn startup(data_dir: &Path) -> Option<CrashReport> {
    let dir = crashes_dir(data_dir);
    let closed = dir.join(MARKER).exists();
    let mut reports: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("crash-")))
                .collect()
        })
        .unwrap_or_default();
    // Names sort by time.
    reports.sort();
    for old in reports.iter().take(reports.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(old);
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(MARKER), std::process::id().to_string());
    let newest = reports.last()?.clone();
    if newest.to_string_lossy().ends_with(".seen.txt") {
        return None;
    }
    let text = std::fs::read_to_string(&newest).ok()?;
    Some(CrashReport { path: newest, text, closed })
}

/// This run is ending normally.
pub fn note_clean_exit(data_dir: &Path) {
    let _ = std::fs::remove_file(crashes_dir(data_dir).join(MARKER));
}

/// The user has seen the report: it stays on disk, renamed so it isn't
/// offered again.
pub fn dismiss(report: &CrashReport) -> Option<PathBuf> {
    let name = report.path.file_name()?.to_string_lossy().replace(".txt", ".seen.txt");
    let seen = report.path.with_file_name(name);
    std::fs::rename(&report.path, &seen).ok()?;
    Some(seen)
}

/// Every report on disk, for the diagnostics bundle.
pub fn all_reports(data_dir: &Path) -> Vec<PathBuf> {
    let mut reports: Vec<PathBuf> = std::fs::read_dir(crashes_dir(data_dir))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("crash-")))
                .collect()
        })
        .unwrap_or_default();
    reports.sort();
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_offered_once_and_tells_a_crash_from_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // First start: nothing to offer.
        assert_eq!(startup(dir.path()), None);
        // A panic during that run, then no clean exit.
        write_report(&crashes_dir(dir.path()), "boom").unwrap();
        let report = startup(dir.path()).unwrap();
        assert!(report.closed);
        assert_eq!(report.text, "boom");
        dismiss(&report).unwrap();
        note_clean_exit(dir.path());
        // Dismissed: not offered again, still on disk for diagnostics.
        assert_eq!(startup(dir.path()), None);
        assert_eq!(all_reports(dir.path()).len(), 1);
        // A report from a run that then quit cleanly.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_report(&crashes_dir(dir.path()), "thread panic").unwrap();
        note_clean_exit(dir.path());
        let report = startup(dir.path()).unwrap();
        assert!(!report.closed);
    }

    #[test]
    fn the_panic_hook_writes_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crashes_dir(dir.path());
        let hook_dir = crashes.clone();
        // A hook like `install_hook`'s, scoped to this thread's panic.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = write_report(&hook_dir, &compose(info));
        }));
        let result = std::thread::Builder::new().name("crash-test".into()).spawn(|| panic!("test panic 42")).unwrap().join();
        std::panic::set_hook(previous);
        assert!(result.is_err());
        let reports = all_reports(dir.path());
        assert_eq!(reports.len(), 1);
        let text = std::fs::read_to_string(&reports[0]).unwrap();
        assert!(text.contains("Panic:    test panic 42"), "{text}");
        assert!(text.contains("Thread:   crash-test"));
        assert!(text.contains("Backtrace:"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }
}
