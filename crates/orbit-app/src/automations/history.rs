//! An automation's runs as a timeline: when, why, how it went, how long it
//! took, what it cost, what it said, and its thread.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::state::ClientState;
use orbit_proto::*;

use super::{AutomationsView, run_glyph};
use crate::format;

/// The words for a skipped run.
pub fn skip_text(skip: &Skip) -> String {
    match skip {
        Skip::Missed { count: 1 } => "Missed while Orbit was closed or asleep".into(),
        Skip::Missed { count } => format!("Missed {count} runs while Orbit was closed or asleep"),
        Skip::StillRunning => "Skipped: the previous run was still going".into(),
        Skip::DailyLimit => "Skipped: the daily limit was reached".into(),
        Skip::NoChanges => "Skipped: nothing changed since the last run".into(),
        Skip::Dirty => "Skipped: the project had uncommitted changes".into(),
        Skip::CheckFailed(message) => format!("Skipped: {message}"),
        Skip::Unavailable(message) => format!("Skipped: {message}"),
    }
}

fn reason_text(reason: &RunReason) -> String {
    match reason {
        RunReason::Schedule => "Scheduled".into(),
        RunReason::Late => "Scheduled, started late".into(),
        RunReason::Manual => "Run now".into(),
        RunReason::Event(what) => what.clone(),
    }
}

/// `Today 09:00`, `Yesterday 09:00`, `Mon 09:00`, `12 Nov 09:00`.
fn day_time(ms: u64, now: u64) -> String {
    let text = format::timestamp(ms, now);
    if format::local_day(ms) == format::local_day(now) { format!("Today {text}") } else { text }
}

/// `12 runs · 2 failed · 48K tokens`.
pub fn stats(runs: &[AutomationRun], cx: &App) -> impl IntoElement {
    let ran: Vec<&AutomationRun> = runs.iter().filter(|r| !matches!(r.status, RunStatus::Skipped(_))).collect();
    let failed = ran.iter().filter(|r| r.status.is_failure()).count();
    let tokens: u64 = ran.iter().map(|r| r.tokens).sum();
    let mut parts = Vec::new();
    if !ran.is_empty() {
        parts.push(format!("{} run{}", ran.len(), if ran.len() == 1 { "" } else { "s" }));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if tokens > 0 {
        parts.push(format!("{} tokens", format::tokens(tokens)));
    }
    div().text_xs().text_color(cx.theme().muted_foreground).child(parts.join(" · "))
}

pub fn timeline(runs: &[AutomationRun], view: WeakEntity<AutomationsView>, state: &ClientState, cx: &App) -> AnyElement {
    let theme = cx.theme();
    if runs.is_empty() {
        return div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("No runs yet. Its first run appears here, linked to the thread it starts.")
            .into_any_element();
    }
    let now = now_ms();
    let last = runs.len() - 1;
    v_flex()
        .children(runs.iter().enumerate().map(|(i, run)| {
            let skipped = matches!(run.status, RunStatus::Skipped(_));
            let at = if run.started_ms > 0 { run.started_ms } else { run.finished_ms.max(run.due_ms) };
            let mut facts: Vec<String> = Vec::new();
            match &run.status {
                RunStatus::Running => facts.push(format!("Running for {}", format::duration(now.saturating_sub(run.started_ms)))),
                RunStatus::Succeeded => facts.push("Succeeded".into()),
                RunStatus::Failed(_) => facts.push("Failed".into()),
                RunStatus::TimedOut => facts.push("Stopped at its time limit".into()),
                RunStatus::Cancelled => facts.push("Cancelled".into()),
                RunStatus::Skipped(skip) => facts.push(skip_text(skip)),
            }
            if !skipped {
                facts.push(reason_text(&run.reason));
            }
            if run.duration_ms() > 0 && run.status != RunStatus::Running {
                facts.push(format::duration(run.duration_ms()));
            }
            if run.tokens > 0 {
                facts.push(format!("{} tokens", format::tokens(run.tokens)));
            }
            match run.changed {
                Some(true) => facts.push("changed files".into()),
                Some(false) if run.status == RunStatus::Succeeded => facts.push("no changes".into()),
                _ => {}
            }
            let repeats = (run.repeats > 0).then(|| format!("×{}", run.repeats + 1));
            let detail = match &run.status {
                RunStatus::Failed(message) => Some((message.clone(), theme.danger)),
                _ if !run.summary.is_empty() => Some((run.summary.clone(), theme.foreground)),
                _ => None,
            };
            let thread = run.thread.filter(|t| state.threads.contains_key(t));
            let open = thread.map(|thread| {
                let view = view.clone();
                Button::new(("automation-run-thread", run.id.0))
                    .ghost()
                    .xsmall()
                    .label("Open thread")
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_thread(thread, window, cx));
                    })
            });
            let pr = (!run.pr_url.is_empty()).then(|| run.pr_url.clone());
            h_flex()
                .items_start()
                .gap_3()
                .child(
                    v_flex()
                        .flex_none()
                        .items_center()
                        .h_full()
                        .child(div().pt_0p5().child(run_glyph(("automation-run-glyph", run.id.0), Some(run), false, cx)))
                        .when(i < last, |el| el.child(div().flex_1().min_h_3().w_px().mt_1().bg(theme.border))),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .pb_4()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(day_time(at, now)))
                                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(facts.join(" · ")))
                                .when_some(repeats, |el, r| el.child(div().text_xs().text_color(theme.muted_foreground).child(r)))
                                .children(open),
                        )
                        .when_some(detail, |el, (text, color)| {
                            el.child(div().text_sm().text_color(color).line_clamp(3).child(text))
                        })
                        .when_some(pr, |el, url| {
                            el.child(
                                Button::new(("automation-run-pr", run.id.0))
                                    .ghost()
                                    .xsmall()
                                    .icon(crate::assets::Lucide::GitPullRequest)
                                    .label("Pull request")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                )
        }))
        .into_any_element()
}
