//! The meter quota rings and the context meter share: one ring, one color
//! ramp, so 85 % looks the same wherever it is shown.
//!
//! The ramp is the theme's own: muted while there is room, warning from
//! [`WARN`], danger from [`FULL`] or when requests are refused. Color never
//! carries the meaning alone; the percentage sits beside the ring.

use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex};
use gpui_kit::*;

/// From here a meter warns.
pub const WARN: f32 = 0.80;
/// From here a meter reads as out.
pub const FULL: f32 = 0.95;

/// How full a meter is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Room,
    Warn,
    Full,
}

impl Level {
    pub fn of(fraction: f32, refused: bool) -> Self {
        match fraction {
            _ if refused => Level::Full,
            f if f >= FULL => Level::Full,
            f if f >= WARN => Level::Warn,
            _ => Level::Room,
        }
    }

    pub fn color(self, theme: &Theme) -> Hsla {
        match self {
            Level::Room => theme.muted_foreground,
            Level::Warn => theme.warning,
            Level::Full => theme.danger,
        }
    }
}

/// A ring and its percentage. `fraction` is `None` while unknown (the ring
/// stays empty and the label reads `–`).
#[derive(IntoElement)]
pub struct Meter {
    id: ElementId,
    fraction: Option<f32>,
    refused: bool,
    on_pace_to_run_out: bool,
}

impl Meter {
    pub fn new(id: impl Into<ElementId>, fraction: Option<f32>) -> Self {
        Self { id: id.into(), fraction, refused: false, on_pace_to_run_out: false }
    }

    /// Requests are refused: the ring reads full, whatever the figure.
    pub fn refused(mut self, refused: bool) -> Self {
        self.refused = refused;
        self
    }

    /// At the recent pace it runs out before it resets: it warns at any
    /// share.
    pub fn running_out(mut self, running_out: bool) -> Self {
        self.on_pace_to_run_out = running_out;
        self
    }
}

impl RenderOnce for Meter {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let fraction = if self.refused { Some(1.0) } else { self.fraction.map(|f| f.clamp(0.0, 1.0)) };
        let level = match Level::of(fraction.unwrap_or(0.0), self.refused) {
            Level::Room if self.on_pace_to_run_out => Level::Warn,
            level => level,
        };
        let color = level.color(theme);
        let pct = fraction.map(|f| (f * 100.0).round() as u32);
        let label: SharedString = match pct {
            Some(p) => format!("{p}%").into(),
            None => "–".into(),
        };
        h_flex()
            .gap_1()
            .text_xs()
            .text_color(color)
            .child(
                ProgressCircle::new(ElementId::NamedChild(std::sync::Arc::new(self.id), "ring".into()))
                    .xsmall()
                    .color(color)
                    .value(pct.unwrap_or(0) as f32),
            )
            .child(label)
    }
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn the_ramp_has_three_steps_and_refusal_is_full() {
        assert_eq!(Level::of(0.0, false), Level::Room);
        assert_eq!(Level::of(0.79, false), Level::Room);
        assert_eq!(Level::of(0.80, false), Level::Warn);
        assert_eq!(Level::of(0.949, false), Level::Warn);
        assert_eq!(Level::of(0.95, false), Level::Full);
        assert_eq!(Level::of(0.10, true), Level::Full);
    }
}
