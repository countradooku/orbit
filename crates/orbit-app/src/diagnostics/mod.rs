//! Diagnostics: the log file, crash reports, the redacted diagnostics
//! bundle, and About. All of it stays on this computer; telemetry is none,
//! as Settings › Data & Privacy promises.

mod about;
pub mod build;
mod bundle;
mod crash;
pub mod logging;
pub mod redact;
mod view;

use std::path::Path;

use gpui_kit::*;

pub use about::open_about;
pub use view::register_actions;

gpui_kit::actions!(
    orbit,
    [
        /// Saves a redacted zip of logs and version information.
        CollectDiagnostics,
        /// The About dialog.
        ShowAbout,
    ]
);

/// A crash report found at start, until the window can offer it.
struct PendingCrash(Option<crash::CrashReport>);

impl Global for PendingCrash {}

/// Before anything else in `main`: the log file and the panic hook. Returns
/// the report a crash left last time, if any.
pub fn install(data_dir: &Path) {
    logging::init(data_dir);
    crash::install_hook(data_dir);
    build::warm_up();
}

/// Reads last run's crash report (and marks this run as running); call
/// once, early, then [`init`] inside the app to keep it for the window.
pub fn startup(data_dir: &Path) -> Option<crash::CrashReport> {
    crash::startup(data_dir)
}

pub fn init(report: Option<crash::CrashReport>, cx: &mut App) {
    cx.set_global(PendingCrash(report));
}

/// Once the window is up: offer last run's crash report.
pub fn on_window_open(window: &mut Window, cx: &mut App) {
    let report = cx.try_global::<PendingCrash>().and_then(|p| p.0.clone());
    if let Some(report) = report {
        cx.set_global(PendingCrash(None));
        view::offer_crash_report(report, window, cx);
    }
}

/// This run is ending normally (quit, or a restart to update).
pub fn note_clean_exit(data_dir: &Path) {
    crash::note_clean_exit(data_dir);
}
