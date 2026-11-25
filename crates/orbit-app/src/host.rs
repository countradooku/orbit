//! The background host in the interface (see `docs/HOST.md`).
//!
//! * Notices: the host kept working while Orbit was closed; the engine runs
//!   in this window because the host could not start; a host from another
//!   version of Orbit must restart before this one can connect.
//! * The preference "Keep agents running after closing Orbit", kept in
//!   `<data>/host.json` where the host reads it.
//! * [`relaunch`]: restart the window (an update) without stopping the host.
//!
//! The host has no tray or menu-bar icon: GPUI has no status-item API and
//! the host is deliberately headless (no GPUI, a few MB). Work that went on
//! while Orbit was closed is told when the window opens again instead.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use orbit_net::host::HostStatus;
use orbit_proto::*;

use crate::app_state::AppState;

/// Something to tell the user about the background host.
#[derive(Clone, Debug)]
pub enum HostNotice {
    /// The host kept working while Orbit was closed ("2 agents are working").
    KeptWorking(String),
    /// A host from another version of Orbit runs: it must restart before this
    /// window can connect. `status` says what it is doing, when it answered.
    Outdated { version: String, status: Option<HostStatus> },
}

/// Shows `notice` in `window`.
pub fn show(notice: &HostNotice, app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    match notice {
        HostNotice::KeptWorking(summary) => {
            let note = Notification::info(format!("{}.", capitalized(summary)))
                .title("Kept running while Orbit was closed")
                .autohide(true);
            window.push_notification(note, cx);
        }
        HostNotice::Outdated { version, status } => match status.as_ref().and_then(HostStatus::summary) {
            // Nothing would stop: restart it without asking.
            None if status.is_some() => {
                restart_outdated(app, RestartWhen::Now, cx);
                let note = Notification::info(format!("Starting this version’s background host in place of Orbit {version}’s."))
                    .autohide(true);
                window.push_notification(note, cx);
            }
            summary => ask_restart(app, version.clone(), summary, window, cx),
        },
    }
}

/// The engine runs in this window: say why, once.
pub fn show_fallback(reason: &str, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::warning(reason.to_owned()).title("No background host"), cx);
}

/// A host from another version runs work: restart it now or when it is done.
fn ask_restart(app: &Entity<AppState>, version: String, summary: Option<String>, window: &mut Window, cx: &mut App) {
    let description = match &summary {
        Some(summary) => format!(
            "It runs Orbit {version}, and {summary}. Restarting now stops them; their threads keep what they did so far. \
             This window connects once the host restarts."
        ),
        None => format!("It runs Orbit {version}. This window connects once the host restarts."),
    };
    let app = app.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (now, later) = (app.clone(), app.clone());
        alert
            .title("Restart the background host?")
            .description(description.clone())
            .footer(
                DialogFooter::new()
                    .child(Button::new("host-restart-now").danger().label("Restart now").on_click(move |_, window, cx| {
                        restart_outdated(&now, RestartWhen::Now, cx);
                        window.close_dialog(cx);
                    }))
                    .child(Button::new("host-restart-later").primary().label("Restart when finished").on_click(
                        move |_, window, cx| {
                            restart_outdated(&later, RestartWhen::WhenIdle, cx);
                            window.close_dialog(cx);
                        },
                    )),
            )
    });
}

fn restart_outdated(app: &Entity<AppState>, when: RestartWhen, cx: &mut App) {
    let Some(host) = app.read(cx).host().cloned() else { return };
    cx.background_spawn(async move {
        if let Err(err) = host.restart_outdated(when) {
            log::warn!("couldn’t restart the background host: {err}");
        }
    })
    .detach();
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

// ── The preference ───────────────────────────────────────────────────────

struct Policy {
    data_dir: PathBuf,
    current: KeepRunning,
}

impl Global for Policy {}

/// Reads the preference from the data folder.
pub fn init(data_dir: &Path, cx: &mut App) {
    let current = orbit_net::host::read_policy(data_dir);
    cx.set_global(Policy { data_dir: data_dir.to_owned(), current });
}

fn policy(cx: &App) -> KeepRunning {
    cx.try_global::<Policy>().map(|p| p.current).unwrap_or_default()
}

fn set_policy(value: KeepRunning, cx: &mut App) {
    let Some(policy) = cx.try_global::<Policy>() else { return };
    if policy.current == value {
        return;
    }
    let dir = policy.data_dir.clone();
    cx.global_mut::<Policy>().current = value;
    // The host reads the file whenever it decides; nothing else to tell it.
    cx.background_spawn(async move {
        if let Err(err) = orbit_net::host::write_policy(&dir, value) {
            log::warn!("couldn’t save the background host preference: {err}");
        }
    })
    .detach();
    cx.refresh_windows();
}

/// Settings › General › Background.
pub fn settings_group() -> SettingGroup {
    let items = KeepRunning::ALL.iter().map(|k| (SharedString::from(k.key()), SharedString::from(k.label()))).collect();
    let field = SettingField::dropdown(
        items,
        |cx| SharedString::from(policy(cx).key()),
        |picked: SharedString, cx| {
            if let Some(value) = KeepRunning::from_key(&picked) {
                set_policy(value, cx);
            }
        },
    )
    .default_value(SharedString::from(KeepRunning::default().key()));
    SettingGroup::new().title("Background").item(
        SettingItem::new("Keep agents running after closing Orbit", field)
            .description(
                "While working: agents, commands in terminals and automations due soon carry on after the window \
                 closes, and Orbit’s background host stops a minute after they finish. Terminals are there with their \
                 output when you come back.",
            )
            .keywords(["background", "host", "orbitd", "quit", "close", "daemon"]),
    )
}

// ── Restarting the window ────────────────────────────────────────────────

/// How long the host waits for the window to come back after [`relaunch`].
#[allow(dead_code)] // With `relaunch`.
const RELAUNCH_HOLD_MS: u64 = 60_000;

/// Restarts the window (after an update) without stopping the background
/// host: asks the host to wait for the new window, starts `exe` once this
/// process has exited (`--wait-pid`), and quits. Agents, terminals and
/// automations keep running; the new window re-attaches to them.
#[allow(dead_code)] // The updater's entry point (installs land separately).
pub fn relaunch(app: &Entity<AppState>, exe: &Path, cx: &mut App) -> std::io::Result<()> {
    app.read(cx).send(Command::Host(HostCommand::Hold { ms: RELAUNCH_HOLD_MS }));
    std::process::Command::new(exe).arg("--wait-pid").arg(std::process::id().to_string()).spawn()?;
    app.update(cx, |app, _| app.detach());
    cx.quit();
    Ok(())
}

/// `--wait-pid <pid>` (from [`relaunch`]): waits up to half a minute for
/// the previous window's process to exit, so this one takes its place.
pub fn wait_for_previous(args: &[String]) {
    let Some(pid) = args.iter().position(|a| a == "--wait-pid").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok()) else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while orbit_net::endpoint::process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
