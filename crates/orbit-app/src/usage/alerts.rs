//! Notifications when a window passes 80 % or 95 %.
//!
//! Like every Orbit notification they report transitions only: once per
//! threshold per window period (a reset starts a new one), never for what
//! was already true when Orbit started. They follow Settings ›
//! Notifications › "Turn failed" (usage limits are announced there too) and
//! the two switches in Settings › Usage. A refused window is not announced:
//! the thread that hit it notifies on its own.

use std::collections::HashMap;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use orbit_proto::*;

/// A notification to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
    /// 95 %: closer to stopping work.
    pub urgent: bool,
}

/// What each window last looked like.
#[derive(Default)]
pub struct Tracker {
    /// (account, window) → (threshold passed, the period's reset time).
    seen: HashMap<(String, String), (u8, u64)>,
}

fn threshold(used: f32) -> u8 {
    match used {
        u if u >= super::ring::FULL => 95,
        u if u >= super::ring::WARN => 80,
        _ => 0,
    }
}

/// Reset times from different readings of one period differ a little.
fn same_period(a: u64, b: u64) -> bool {
    a == 0 || b == 0 || a.abs_diff(b) < 5 * 60_000
}

impl Tracker {
    /// Compares the board with what was seen and returns the alerts due.
    /// `enabled` is the notification setting that governs them.
    pub fn observe(&mut self, board: &QuotaBoard, enabled: bool, now_ms: u64) -> Vec<Alert> {
        let mut out = Vec::new();
        for account in &board.accounts {
            for w in &account.windows {
                let key = (account.instance.clone(), w.window.window.clone());
                let level = if w.window.rejected { 100 } else { threshold(w.used()) };
                let reset = w.window.resets_ms;
                let before = self.seen.insert(key, (level, reset));
                let Some((was, was_reset)) = before else { continue };
                let was = if same_period(was_reset, reset) { was } else { 0 };
                let wanted = match level {
                    80 => board.settings.alert_80,
                    95 => board.settings.alert_95,
                    _ => false,
                };
                if level > was && wanted && enabled {
                    let when = if reset > now_ms { format!(" {}.", super::reset_time(reset, now_ms)) } else { String::new() };
                    let pace = super::pace_line(w, now_ms).map(|p| format!(" {p}.")).unwrap_or_default();
                    out.push(Alert {
                        title: format!("{} is at {}%", account.name, (w.used() * 100.0).round()),
                        body: format!("{} window.{when}{pace}", w.label),
                        urgent: level >= 95,
                    });
                }
            }
        }
        out
    }
}

/// Shows due alerts: a toast while Orbit is in front, a system
/// notification otherwise.
pub fn check(window: &mut Window, cx: &mut App) {
    let enabled = crate::settings::get(cx).notifications.failed;
    let board = cx.global::<super::UsageModel>().app.read(cx).state().quota.clone();
    let alerts = cx.global_mut::<super::UsageModel>().alerts.observe(&board, enabled, now_ms());
    let active = window.is_window_active();
    for alert in alerts {
        let note = if alert.urgent { Notification::warning(alert.body) } else { Notification::info(alert.body) }
            .title(alert.title)
            .on_click(|_, window, cx| {
                window.activate_window();
                window.dispatch_action(Box::new(super::GoUsage), cx);
            });
        window.push_notification(if active { note } else { note.system() }, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::Tracker;
    use orbit_proto::*;

    fn board(used: f32, rejected: bool, resets_ms: u64) -> QuotaBoard {
        QuotaBoard {
            accounts: vec![AccountQuota {
                instance: "claude".into(),
                name: "Claude Code".into(),
                windows: vec![QuotaWindowState {
                    window: QuotaWindow { window: "five_hour".into(), utilization: Some(used), resets_ms, rejected },
                    label: "5-hour".into(),
                    span_ms: 0,
                    burn_per_hour: None,
                    runs_out_ms: None,
                }],
                ..Default::default()
            }],
            settings: UsageSettings::default(),
        }
    }

    #[test]
    fn each_threshold_alerts_once_per_period() {
        let mut t = Tracker::default();
        let reset = 10 * 3_600_000;
        assert!(t.observe(&board(0.85, false, reset), true, 0).is_empty(), "nothing on the first sighting");
        assert!(t.observe(&board(0.86, false, reset), true, 0).is_empty(), "80 % was already passed");
        let alerts = t.observe(&board(0.96, false, reset), true, 0);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Claude Code is at 96%");
        assert!(alerts[0].urgent);
        assert!(alerts[0].body.starts_with("5-hour window. Resets "), "{}", alerts[0].body);
        assert!(t.observe(&board(0.97, false, reset), true, 0).is_empty());
        assert!(t.observe(&board(1.0, true, reset), true, 0).is_empty(), "a refused window is the thread's to announce");
        // A new period starts over.
        assert!(t.observe(&board(0.1, false, 2 * reset), true, 0).is_empty());
        let again = t.observe(&board(0.81, false, 2 * reset), true, 0);
        assert_eq!(again.len(), 1);
        assert!(!again[0].urgent);
    }

    #[test]
    fn switches_and_the_notification_setting_hold_them_back() {
        let mut t = Tracker::default();
        t.observe(&board(0.5, false, 0), true, 0);
        assert!(t.observe(&board(0.85, false, 0), false, 0).is_empty(), "notifications for failures are off");
        let mut quiet = board(0.96, false, 0);
        quiet.settings.alert_95 = false;
        assert!(t.observe(&quiet, true, 0).is_empty());
    }
}
