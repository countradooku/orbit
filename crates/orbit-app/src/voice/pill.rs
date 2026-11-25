//! The recording pill: the live waveform, the time, what has been heard so
//! far, and Cancel and Done.
//!
//! Inside the composer it sits above the text box; for other fields it
//! floats at the bottom of the window. [`Dictation`] renders it, so it
//! redraws with the session and the composer around it does not. Words Whisper is sure of read in the
//! foreground colour, the tentative tail muted. Near the five-minute cap the
//! time turns to a warning and says when recording stops.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::speech::{SpeechStatus, SpeechWaveform};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_voice::control::{MAX_DURATION, StopReason};

use super::{Dictation, Purpose};

/// Characters of live transcript shown; the start scrolls away.
const PREVIEW_CHARS: usize = 140;

fn clock(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The last `max` characters of `text`, with an ellipsis where it was cut.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let skip = count - max;
    let rest: String = text.chars().skip(skip).collect();
    // Start at a word.
    let rest = rest.split_once(' ').map_or(rest.as_str(), |(_, r)| r);
    format!("…{rest}")
}

/// The pill for `d`; `floating` draws its own surface, for the window
/// overlay.
pub fn render(d: &Dictation, floating: bool, cx: &App) -> impl IntoElement + use<> {
    {
        let theme = cx.theme().clone();
        let state = d.state().clone();
        let status = state.read(cx).status();
        let live = d.live();
        let elapsed = d.elapsed();
        let near_cap = d.near_cap();
        let capped = d.stopped_by() == Some(StopReason::Cap);
        let sending = d.sends_when_done();
        let purpose = d.purpose();

        let (stable, unstable) = (live.preview.stable.clone(), live.preview.unstable.clone());
        let heard = !stable.is_empty() || !unstable.is_empty();
        let text: AnyElement = if heard {
            let shown = tail(format!("{stable} {unstable}").trim(), PREVIEW_CHARS);
            // The muted tail is the last `unstable` characters of what shows.
            let tail_len = unstable.len().min(shown.len());
            let split = shown.len() - tail_len;
            let split = (0..=split).rev().find(|i| shown.is_char_boundary(*i)).unwrap_or(0);
            let tentative = shown[split..].trim();
            let highlights = (!tentative.is_empty()).then(|| {
                (shown.len() - tentative.len()..shown.len(), HighlightStyle { color: Some(theme.muted_foreground), ..Default::default() })
            });
            // Two lines at most; older words scroll away at the start.
            div()
                .min_w_0()
                .flex_1()
                .text_sm()
                .line_clamp(2)
                .child(StyledText::new(shown.clone()).with_highlights(highlights))
                .into_any_element()
        } else {
            let hint = match (status, live.loading, purpose) {
                (_, true, _) => "Loading the speech model…",
                (SpeechStatus::Stopping, _, _) => "Transcribing…",
                (_, _, Purpose::Steer) => "Say an agent’s name, then the instruction",
                _ => "Listening…",
            };
            div().min_w_0().flex_1().truncate().text_sm().text_color(theme.muted_foreground).child(hint).into_any_element()
        };

        let indicator: AnyElement = match status {
            SpeechStatus::Recording if !live.loading => {
                SpeechWaveform::new(&state).small().w_16().flex_shrink_0().text_color(theme.primary).into_any_element()
            }
            _ => div().w_16().flex().justify_center().child(Spinner::new().xsmall()).into_any_element(),
        };

        let time = if status == SpeechStatus::Stopping && sending {
            "Sending after transcription".to_owned()
        } else if status == SpeechStatus::Stopping {
            clock(elapsed)
        } else if near_cap {
            format!("{} · stops at {}", clock(elapsed), clock(MAX_DURATION))
        } else {
            clock(elapsed)
        };

        let capturing = status.is_capturing();
        let cancel = Button::new("dictation-cancel")
            .ghost()
            .xsmall()
            .label("Cancel")
            .when_some(Keystroke::parse("escape").ok(), |b, k| b.child(Kbd::new(k).appearance(false)))
            .accessibility_label("Cancel dictation")
            .on_click(|_, _, cx| super::cancel(cx));
        let done = Button::new("dictation-done")
            .small()
            .label("Done")
            .disabled(!capturing)
            .tooltip(if capturing { "Stop and insert the text" } else { "Transcribing…" })
            .on_click(|_, _, cx| super::confirm(cx));

        h_flex()
            .id("dictation-pill")
            .w_full()
            .gap_2()
            .min_w_0()
            .map(|el| {
                if floating {
                    el.px_3()
                        .py_2()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .shadow_lg()
                } else {
                    el.px_3().pt_2().pb_1()
                }
            })
            .child(indicator)
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(if near_cap || capped { theme.warning } else { theme.muted_foreground })
                    .child(time),
            )
            .child(text)
            .child(h_flex().flex_shrink_0().gap_1().child(cancel).child(done))
    }
}

#[cfg(test)]
mod tests {
    use super::{clock, tail};

    #[test]
    fn clock_and_tail() {
        assert_eq!(clock(std::time::Duration::from_secs(0)), "0:00");
        assert_eq!(clock(std::time::Duration::from_secs(272)), "4:32");
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("one two three four five", 10), "…four five");
    }
}
