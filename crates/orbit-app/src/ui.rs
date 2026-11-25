//! Small, value-like components shared by every surface.
//!
//! They carry Orbit's domain language (thread status, context fill) so the
//! sidebar, inbox, fleet and thread views show the same state the same way.

use std::time::Duration;

use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::assets::Lucide;
use crate::format;

/// What a thread's status lane shows, in priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// Blocked on the user. Never drawn like `Working`.
    Waiting,
    Failed,
    RateLimited,
    Working,
    /// Finished with a result the user has not seen.
    Unread,
    Idle,
}

impl Attention {
    pub fn of(thread: &Thread) -> Self {
        match &thread.status {
            ThreadStatus::Waiting => Attention::Waiting,
            _ if !thread.pending.is_empty() => Attention::Waiting,
            ThreadStatus::Failed { .. } => Attention::Failed,
            ThreadStatus::RateLimited { .. } => Attention::RateLimited,
            ThreadStatus::Running | ThreadStatus::Starting => Attention::Working,
            ThreadStatus::Idle if thread.unread => Attention::Unread,
            ThreadStatus::Idle => Attention::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Attention::Waiting => "Needs you",
            Attention::Failed => "Failed",
            Attention::RateLimited => "Waiting for quota",
            Attention::Working => "Working",
            Attention::Unread => "New result",
            Attention::Idle => "Idle",
        }
    }
}

/// A fixed-width status slot. Every row reserves it, so titles never shift
/// when a status appears or clears.
#[derive(IntoElement)]
pub struct StatusGlyph {
    id: ElementId,
    attention: Attention,
}

impl StatusGlyph {
    pub fn new(id: impl Into<ElementId>, thread: &Thread) -> Self {
        Self { id: id.into(), attention: Attention::of(thread) }
    }

    pub fn attention(id: impl Into<ElementId>, attention: Attention) -> Self {
        Self { id: id.into(), attention }
    }
}

impl RenderOnce for StatusGlyph {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let slot = h_flex().size_4().flex_none().justify_center();
        match self.attention {
            Attention::Working => slot.child(
                Icon::new(IconName::LoaderCircle)
                    .xsmall()
                    .text_color(theme.muted_foreground)
                    .with_animation(
                        self.id,
                        Animation::new(Duration::from_millis(900)).repeat(),
                        |icon, t| icon.transform(Transformation::rotate(percentage(t))),
                    ),
            ),
            // A slow pulse: waiting on you is a call to act, not progress.
            Attention::Waiting => slot.child(
                Icon::new(Lucide::Hand).xsmall().text_color(theme.warning).with_animation(
                    self.id,
                    Animation::new(Duration::from_millis(1600)).repeat().with_easing(pulsating_between(0.45, 1.0)),
                    |icon, t| icon.opacity(t),
                ),
            ),
            Attention::Failed => slot.child(Icon::new(IconName::CircleAlert).xsmall().text_color(theme.danger)),
            Attention::RateLimited => slot.child(Icon::new(Lucide::Hourglass).xsmall().text_color(theme.warning)),
            Attention::Unread => slot.child(div().size_1p5().rounded_full().bg(theme.primary)),
            Attention::Idle => slot,
        }
    }
}

/// Context-window fill as a ring and a percentage, with token counts in the
/// tooltip. Always visible; never hidden behind a setting.
#[derive(IntoElement)]
pub struct ContextMeter {
    id: ElementId,
    usage: Usage,
}

impl ContextMeter {
    pub fn new(id: impl Into<ElementId>, usage: Usage) -> Self {
        Self { id: id.into(), usage }
    }
}

impl RenderOnce for ContextMeter {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let fill = self.usage.context_fill();
        let pct = fill.map(|f| (f * 100.0).round() as u32);
        let color = match pct {
            Some(p) if p >= 90 => theme.danger,
            Some(p) if p >= 75 => theme.warning,
            _ => theme.muted_foreground,
        };
        let tooltip: SharedString = match fill {
            Some(_) => format!(
                "Context {} of {} tokens{}",
                format::tokens(self.usage.context_tokens),
                format::tokens(self.usage.context_window),
                match format::cost(self.usage.cost_micros) {
                    c if c.is_empty() => String::new(),
                    c => format!(" · {c} so far"),
                }
            )
            .into(),
            None => "Context size appears after the first reply".into(),
        };
        h_flex()
            .id(self.id.clone())
            .gap_1p5()
            .px_1()
            .rounded_md()
            .text_xs()
            .text_color(color)
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx))
            .child(
                ProgressCircle::new(ElementId::NamedChild(std::sync::Arc::new(self.id), "ring".into()))
                    .xsmall()
                    .color(color)
                    .value(pct.unwrap_or(0) as f32),
            )
            .child(match pct {
                Some(p) => format!("{p}%"),
                None => "–".into(),
            })
    }
}

/// `Claude Code · Opus 5.5`, from the provider's own catalog.
pub fn model_label(config: &ThreadConfig, providers: &[ProviderInfo]) -> String {
    let provider = providers.iter().find(|p| p.serves(config));
    let model = provider.and_then(|p| {
        p.models
            .iter()
            .find(|m| m.slug == config.model)
            .or_else(|| if config.model.is_empty() { p.models.iter().find(|m| m.is_default) } else { None })
    });
    match model {
        // A provider's "default" entry names the model it resolves to in its
        // description ("Opus 5.5 · Best for…"); show that model.
        Some(model) if model.slug == "default" => model
            .description
            .as_deref()
            .and_then(|d| d.split(" · ").next())
            .filter(|n| !n.is_empty() && n.len() < 40)
            .map(str::to_owned)
            .unwrap_or_else(|| model.name.clone()),
        Some(model) => model.name.clone(),
        None if !config.model.is_empty() => config.model.clone(),
        None => provider.map(|p| p.display_name()).unwrap_or(config.provider.label()).to_owned(),
    }
}

/// A section label in a sidebar or list: small, muted, sentence case.
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .px_2()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// An empty state: icon, title, one line of help, and an optional action.
pub fn empty_state(
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    help: impl Into<SharedString>,
    action: Option<AnyElement>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .p_8()
        .child(
            div()
                .p_3()
                .mb_1()
                .rounded_lg()
                .bg(theme.muted)
                .child(icon.into().with_size(Size::Large).text_color(theme.muted_foreground)),
        )
        .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .child(div().max_w(rems(26.)).text_sm().text_center().text_color(theme.muted_foreground).child(help.into()))
        .when_some(action, |el, action| el.child(div().mt_2().child(action)))
}

use gpui_kit::component::Size;

/// A project's two-letter mark on a soft tint. The color is derived from the
/// project id, so each project keeps one recognizable color everywhere.
#[derive(IntoElement)]
pub struct Monogram {
    name: SharedString,
    seed: u64,
}

impl Monogram {
    pub fn new(name: impl Into<SharedString>, seed: u64) -> Self {
        Self { name: name.into(), seed }
    }
}

pub fn initials(name: &str) -> String {
    let mut words = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    let first = words.next().unwrap_or("?");
    match words.next() {
        Some(second) => first.chars().take(1).chain(second.chars().take(1)).collect::<String>(),
        None => first.chars().take(2).collect(),
    }
    .to_uppercase()
}

impl RenderOnce for Monogram {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let palette = [theme.blue, theme.magenta, theme.cyan, theme.green, theme.yellow, theme.red, theme.chart_2, theme.chart_4];
        let color = palette[(self.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 61) as usize % palette.len()];
        h_flex()
            .flex_none()
            .size_5()
            .justify_center()
            .rounded(theme.radius)
            .bg(color.opacity(0.16))
            .text_color(color)
            .text_size(rems(0.6))
            .font_weight(FontWeight::BOLD)
            .child(initials(&self.name))
    }
}

/// The mark of an agent runtime.
pub fn provider_icon(kind: ProviderKind, cx: &App) -> Icon {
    let theme = cx.theme();
    match kind {
        ProviderKind::Claude => Icon::new(IconName::Asterisk).text_color(theme.warning),
        ProviderKind::Codex => Icon::new(IconName::Bot).text_color(theme.foreground),
        ProviderKind::Acp => Icon::new(Lucide::Plug).text_color(theme.info),
        ProviderKind::OpenCode => Icon::new(Lucide::Terminal).text_color(theme.foreground),
        ProviderKind::Pi => Icon::new(Lucide::Sparkle).text_color(theme.success),
    }
}

#[cfg(test)]
mod monogram_tests {
    use super::initials;

    #[test]
    fn initials_read_like_names() {
        assert_eq!(initials("Silver Orchard"), "SO");
        assert_eq!(initials("storefront-web"), "SW");
        assert_eq!(initials("orbit"), "OR");
    }
}
