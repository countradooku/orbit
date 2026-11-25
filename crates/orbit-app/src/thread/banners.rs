//! The banner above the composer: one at a time, the most urgent first, each
//! with the action that resolves it.
//!
//! * Stop did not take: after a few seconds, say so and offer Force stop
//!   (ends the agent's process; the thread resumes on the next message).
//! * A usage limit: when it resets, that Orbit retries the turn then, and
//!   Retry now or Snooze until then.
//! * A failed turn: the error, Retry and Copy.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::ThreadView;
use crate::assets::Lucide;
use crate::format;

/// How long Stop may take before Force stop is offered.
pub const STOP_GRACE: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, PartialEq)]
pub enum Banner {
    /// Stop was sent this long ago and the turn is still running.
    Unresponsive(Duration),
    /// The provider's usage limit; resets at this Unix time (0 = unknown).
    Limit(u64),
    Failed(String),
}

impl Banner {
    /// A key that changes when the banner means something new, so a
    /// dismissed banner comes back for a new failure.
    pub fn key(&self) -> String {
        match self {
            Banner::Unresponsive(_) => "stop".into(),
            Banner::Limit(at) => format!("limit-{at}"),
            Banner::Failed(message) => format!("failed-{message}"),
        }
    }
}

/// The banner a thread needs now, if any.
pub fn banner(status: &ThreadStatus, stop_waited: Option<Duration>) -> Option<Banner> {
    match status {
        s if s.is_busy() => stop_waited.filter(|w| *w >= STOP_GRACE).map(Banner::Unresponsive),
        ThreadStatus::RateLimited { resets_ms } => Some(Banner::Limit(*resets_ms)),
        ThreadStatus::Failed { message } => Some(Banner::Failed(message.clone())),
        _ => None,
    }
}

/// `offer` holds extra actions for a usage limit: continuing on another
/// account (`crate::usage::failover`).
pub fn render(banner: &Banner, offer: Option<AnyElement>, cx: &mut Context<ThreadView>) -> AnyElement {
    let theme = cx.theme().clone();
    let (icon, color, title, detail): (Icon, Hsla, String, Option<String>) = match banner {
        Banner::Unresponsive(waited) => (
            Icon::new(Lucide::OctagonX),
            theme.warning,
            "The agent hasn’t stopped yet".into(),
            Some(format!(
                "Stop was sent {} ago. Force stop ends the agent’s process; your next message resumes the thread.",
                format::duration(waited.as_millis() as u64 / 1000 * 1000)
            )),
        ),
        Banner::Limit(resets) => (
            Icon::new(Lucide::Hourglass),
            theme.warning,
            "Usage limit reached".into(),
            Some(if *resets == 0 {
                "The provider didn’t say when it resets. Retry when you’re ready.".into()
            } else {
                format!(
                    "{} ({}). Orbit retries this turn then, and sends anything you queue after it.",
                    format::resets_at(*resets, now_ms()),
                    format::timestamp(*resets, now_ms())
                )
            }),
        ),
        // The error itself is in the timeline; the banner says what Retry does.
        Banner::Failed(message) => (
            Icon::new(IconName::CircleAlert),
            theme.danger,
            format!("This turn failed: {}", message.lines().next().unwrap_or_default()),
            Some("Retry sends your message again from the same point; this attempt stays above, folded.".into()),
        ),
    };
    let actions = match banner {
        Banner::Unresponsive(_) => h_flex().gap_1().child(
            Button::new("force-stop")
                .small()
                .danger()
                .icon(Lucide::OctagonX)
                .label("Force stop")
                .on_click(cx.listener(|this, _, window, cx| this.force_stop(window, cx))),
        ),
        Banner::Limit(resets) => {
            let resets = *resets;
            h_flex()
                .gap_1()
                .when(resets > now_ms(), |el| {
                    el.child(
                        Button::new("snooze-reset")
                            .small()
                            .ghost()
                            .label("Snooze until then")
                            .tooltip("Hide this thread until the limit resets; it comes back when it runs")
                            .on_click(cx.listener(move |this, _, window, cx| this.snooze_until(resets, window, cx))),
                    )
                })
                .child(
                    Button::new("retry-now")
                        .small()
                        .outline()
                        .icon(IconName::RefreshCw)
                        .label("Retry now")
                        .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx))),
                )
                .children(offer)
        }
        Banner::Failed(message) => h_flex()
            .gap_1()
            .child(Clipboard::new("copy-error").value(message.clone()))
            .child(
                Button::new("dismiss-banner")
                    .small()
                    .ghost()
                    .label("Dismiss")
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss_banner(cx))),
            )
            .child(
                Button::new("retry")
                    .small()
                    .primary()
                    .icon(IconName::RefreshCw)
                    .label("Retry")
                    .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx))),
            ),
    };
    h_flex()
        .mb_2()
        .px_3()
        .py_2()
        .gap_3()
        .items_start()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.07))
        .child(div().pt_0p5().child(icon.small().text_color(color)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).line_clamp(2).child(title))
                .when_some(detail, |el, detail| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).line_clamp(3).child(detail))
                }),
        )
        .child(actions)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Banner, banner};
    use orbit_proto::*;
    use std::time::Duration;

    #[test]
    fn the_most_urgent_banner_wins_and_stop_waits_its_grace() {
        let running = ThreadStatus::Running;
        assert_eq!(banner(&running, None), None);
        assert_eq!(banner(&running, Some(Duration::from_secs(1))), None, "Stop gets a moment first");
        assert_eq!(banner(&running, Some(Duration::from_secs(5))), Some(Banner::Unresponsive(Duration::from_secs(5))));
        assert_eq!(banner(&ThreadStatus::RateLimited { resets_ms: 9 }, None), Some(Banner::Limit(9)));
        assert_eq!(banner(&ThreadStatus::Failed { message: "boom".into() }, None), Some(Banner::Failed("boom".into())));
        assert_eq!(banner(&ThreadStatus::Idle, Some(Duration::from_secs(9))), None, "a stopped turn needs no banner");
    }
}
