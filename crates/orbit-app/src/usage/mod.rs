//! Usage, quota and accounts in the interface.
//!
//! * [`ring`]: the meter every surface shares: quota rings and the
//!   composer's context meter use one ring and one color ramp.
//! * [`rings`]: one ring per account in the title bar, with the accounts'
//!   windows, resets, pace, run-out time and top threads in a popover.
//! * [`page`]: the Usage page: limits, then tokens and cost over a range,
//!   by day, model, account, project and thread, with CSV export.
//! * [`failover`]: the limit banner's offer to continue on another account
//!   (or, with a warning, another agent).
//! * [`alerts`]: optional notifications at 80 % and 95 % of a window.
//! * [`settings`]: Settings › Usage: the failover policy, alerts, reading
//!   limits, and account pins.
//!
//! The engine reads limits only while a window was in front recently, so the
//! window tells it when it comes to the front ([`front`]).

pub mod alerts;
pub mod failover;
pub mod page;
pub mod ring;
pub mod rings;
pub mod settings;

use std::time::Duration;

use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

gpui_kit::actions!(
    orbit_usage,
    [
        /// The Usage page: limits, tokens and cost.
        GoUsage,
        /// Read every account's limits now.
        RefreshUsage
    ]
);

/// Edits wait this long before they are saved, so a run of toggles is one
/// write.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// The usage settings as the interface edits them: the engine's copy, with
/// edits applied at once and saved after a short pause.
pub struct UsageModel {
    app: Entity<AppState>,
    /// An edit not saved yet; it wins over the engine's copy.
    pending: Option<UsageSettings>,
    save: Option<Task<()>>,
    alerts: alerts::Tracker,
}

impl Global for UsageModel {}

pub fn init(app: Entity<AppState>, cx: &mut App) {
    cx.bind_keys([KeyBinding::new(crate::actions::platform("cmd-u", "ctrl-u"), GoUsage, None)]);
    // The window opens in front: read stale limits now.
    app.read(cx).send(Command::Usage(UsageCommand::Seen));
    cx.set_global(UsageModel { app, pending: None, save: None, alerts: alerts::Tracker::default() });
}

/// A window came to the front: the engine reads limits that are stale and
/// keeps them current while it stays there.
pub fn front(app: &Entity<AppState>, cx: &App) {
    app.read(cx).send(Command::Usage(UsageCommand::Seen));
}

/// The usage settings in force (an unsaved edit included).
pub fn settings(cx: &App) -> UsageSettings {
    let model = cx.global::<UsageModel>();
    model.pending.clone().unwrap_or_else(|| model.app.read(cx).state().quota.settings.clone())
}

/// Edits the usage settings; saved after [`SAVE_DELAY`].
pub fn update_settings(cx: &mut App, edit: impl FnOnce(&mut UsageSettings)) {
    let mut next = settings(cx);
    let before = next.clone();
    edit(&mut next);
    if next == before {
        return;
    }
    let model = cx.global_mut::<UsageModel>();
    model.pending = Some(next);
    let app = model.app.clone();
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(|cx| {
            let model = cx.global_mut::<UsageModel>();
            model.save = None;
            if let Some(settings) = model.pending.take() {
                app.read(cx).send(Command::Usage(UsageCommand::SetSettings(settings)));
            }
        });
    });
    cx.global_mut::<UsageModel>().save = Some(task);
    cx.refresh_windows();
}

/// Asks for fresh limits (one account's, or every account's).
pub fn refresh(app: &Entity<AppState>, instance: Option<String>, window: &mut Window, cx: &mut App) {
    app.update(cx, |app, cx| app.run(Command::Usage(UsageCommand::Refresh { instance }), window, cx));
}

/// `Resets 15:40` today, `Resets Mon 10:00` later, from the reader's clock.
pub fn reset_time(resets_ms: u64, now_ms: u64) -> String {
    if resets_ms == 0 {
        return "Reset time unknown".into();
    }
    format!("Resets {}", crate::format::timestamp(resets_ms, now_ms))
}

/// `in 2h 10m`, `in 3 days`, for a moment ahead.
pub fn time_until(at_ms: u64, now_ms: u64) -> String {
    let mins = at_ms.saturating_sub(now_ms) / 60_000;
    match mins {
        0 => "in under a minute".into(),
        1..=59 => format!("in {mins} min"),
        60..=1_439 => format!("in {}h {:02}m", mins / 60, mins % 60),
        1_440..=2_879 => "in 1 day".into(),
        _ => format!("in {} days", mins / 1_440),
    }
}

/// What a window's pace says, in one line: `At 25%/h, runs out at 14:55
/// (in 1h 30m)`, `At 5%/h, lasts until the reset`, or nothing to project.
pub fn pace_line(window: &QuotaWindowState, now_ms: u64) -> Option<String> {
    let rate = window.burn_per_hour?;
    let per_hour = (rate * 100.0).round();
    Some(match window.runs_out_ms {
        _ if window.window.rejected => return None,
        Some(at) => format!(
            "At {per_hour:.0}%/h, runs out at {} ({})",
            crate::format::timestamp(at, now_ms),
            time_until(at, now_ms)
        ),
        None if rate > 0.0 => format!("At {per_hour:.0}%/h, lasts until the reset"),
        None => "Unused in the last hour".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::{pace_line, time_until};
    use orbit_proto::*;

    fn window(used: f32, rejected: bool, burn: Option<f32>, runs_out: Option<u64>) -> QuotaWindowState {
        QuotaWindowState {
            window: QuotaWindow { window: "five_hour".into(), utilization: Some(used), resets_ms: 0, rejected },
            label: "5-hour".into(),
            span_ms: 0,
            burn_per_hour: burn,
            runs_out_ms: runs_out,
        }
    }

    #[test]
    fn pace_reads_as_one_line() {
        let now = 1_000_000_000_000;
        assert_eq!(pace_line(&window(0.4, false, None, None), now), None, "no history, no claim");
        let lasts = pace_line(&window(0.4, false, Some(0.05), None), now).unwrap();
        assert_eq!(lasts, "At 5%/h, lasts until the reset");
        let out = pace_line(&window(0.4, false, Some(0.3), Some(now + 2 * 3_600_000 + 5 * 60_000)), now).unwrap();
        assert!(out.starts_with("At 30%/h, runs out at "), "{out}");
        assert!(out.ends_with("(in 2h 05m)"), "{out}");
        assert_eq!(pace_line(&window(0.4, false, Some(0.0), None), now).as_deref(), Some("Unused in the last hour"));
        assert_eq!(pace_line(&window(1.0, true, Some(0.3), Some(now)), now), None, "a refused window says so elsewhere");
    }

    #[test]
    fn time_until_reads_naturally() {
        assert_eq!(time_until(30_000, 0), "in under a minute");
        assert_eq!(time_until(45 * 60_000, 0), "in 45 min");
        assert_eq!(time_until(130 * 60_000, 0), "in 2h 10m");
        assert_eq!(time_until(3 * 86_400_000, 0), "in 3 days");
    }
}
