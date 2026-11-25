//! Small, value-like components shared by every surface.
//!
//! They carry Orbit's domain language (thread status, context fill) so the
//! sidebar, inbox, fleet and thread views show the same state the same way.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_net::state::ClientState;
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

}

/// The one status vocabulary. Every state has its own shape as well as its
/// own color, so it reads in grayscale and for color-blind users, and it is
/// drawn the same way on rows, cards, tiles and the title bar.
///
/// | Glyph       | Shape                         | Color   |
/// | ----------- | ----------------------------- | ------- |
/// | Approval    | shield with `!`               | warning |
/// | Question    | filled disc with `?`          | warning |
/// | Failed      | octagon with `!`              | danger  |
/// | RateLimited | hourglass                     | warning |
/// | Working     | rotating open arc             | neutral |
/// | Quiet       | still open arc, faded         | neutral |
/// | Unverified  | dashed ring (no live signal)  | neutral |
/// | Unread      | small filled dot              | primary |
/// | Settled     | check                         | neutral |
/// | Idle        | nothing                       |         |
///
/// Working has no hue on purpose: color always means "look" or "act".
/// A state Orbit only guessed (an agent in a terminal read from its screen,
/// not its hooks) is drawn faded: it is a guess, and looks like one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Glyph {
    Approval,
    Question,
    Failed,
    RateLimited,
    Working,
    /// Working, but nothing heard for a while ("No output for 4m").
    Quiet,
    Unverified,
    Unread,
    Settled,
    Idle,
}

impl Glyph {
    /// The glyph for a thread, telling approvals from questions.
    pub fn of(thread: &Thread) -> Self {
        match Attention::of(thread) {
            Attention::Waiting => match thread.pending.first().map(|p| &p.request.kind) {
                Some(RequestKind::Question { .. }) | None => Glyph::Question,
                Some(_) => Glyph::Approval,
            },
            Attention::Idle if thread.settled_ms.is_some() => Glyph::Settled,
            other => Glyph::from(other),
        }
    }

    /// The glyph from the thread's record and its live evidence (see
    /// `docs/STATUS.md`): quiet or unverified sessions, and agents running
    /// in its terminals. A working thread whose host is not connected is
    /// unverified: Orbit never claims work it cannot see.
    pub fn live(thread: &Thread, state: &ClientState) -> Self {
        Look::of(thread, state).glyph
    }

    pub fn label(self) -> &'static str {
        match self {
            Glyph::Approval => "Needs approval",
            Glyph::Question => "Has a question",
            Glyph::Failed => "Failed",
            Glyph::RateLimited => "Waiting for quota",
            Glyph::Working => "Working",
            Glyph::Quiet => "No recent output",
            Glyph::Unverified => "Unverified",
            Glyph::Unread => "New result",
            Glyph::Settled => "Settled",
            Glyph::Idle => "Idle",
        }
    }

    /// Calls for the user: these pulse once when they appear.
    pub fn is_call(self) -> bool {
        matches!(self, Glyph::Approval | Glyph::Question | Glyph::Failed)
    }

    fn color(self, theme: &gpui_kit::component::Theme) -> Hsla {
        match self {
            Glyph::Approval | Glyph::Question | Glyph::RateLimited => theme.warning,
            Glyph::Failed => theme.danger,
            Glyph::Unread => theme.primary,
            Glyph::Working | Glyph::Quiet | Glyph::Unverified | Glyph::Settled | Glyph::Idle => theme.muted_foreground,
        }
    }
}

/// How a thread's status looks: its glyph, whether that is only a guess,
/// and the lines its hover explains it with.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub glyph: Glyph,
    /// The glyph comes from a terminal agent Orbit only read from its
    /// screen (no hook, no report).
    pub guess: bool,
    /// "No output for 4m · cargo test", "Claude Code · probably working".
    pub lines: Vec<String>,
}

impl Look {
    pub fn of(thread: &Thread, state: &ClientState) -> Self {
        let live = state.live.get(&thread.id);
        let connected = state.connection.as_ref().is_none_or(|c| *c == ConnectionStatus::Connected);
        Self::from_evidence(thread, live, connected, now_ms())
    }

    fn from_evidence(thread: &Thread, live: Option<&ThreadLive>, connected: bool, now: u64) -> Self {
        let status = LiveStatus::of(thread, live);
        let own = LiveStatus::of(thread, live.map(|l| ThreadLive { terminals: Vec::new(), ..l.clone() }).as_ref());
        // A terminal agent decided the glyph: a guess when every agent in
        // that state was only read from the screen.
        let deciding: Vec<&TerminalAgent> = live
            .into_iter()
            .flat_map(|l| l.terminals.iter())
            .filter(|a| LiveStatus::of_agent(a.state) == status)
            .collect();
        let guess = status != own && !deciding.is_empty() && deciding.iter().all(|a| a.evidence.is_guess());
        let glyph = match status {
            LiveStatus::Permission => Glyph::Approval,
            LiveStatus::NeedsYou => Glyph::Question,
            LiveStatus::Failed => Glyph::Failed,
            LiveStatus::RateLimited => Glyph::RateLimited,
            LiveStatus::Working | LiveStatus::Quiet | LiveStatus::Unverified if !connected && status == own => {
                Glyph::Unverified
            }
            LiveStatus::Working => Glyph::Working,
            LiveStatus::Quiet => Glyph::Quiet,
            LiveStatus::Unverified => Glyph::Unverified,
            LiveStatus::DoneUnread => Glyph::Unread,
            LiveStatus::Idle if thread.settled_ms.is_some() => Glyph::Settled,
            LiveStatus::Idle => Glyph::Idle,
        };
        let mut lines = Vec::new();
        if !connected && glyph == Glyph::Unverified && status == own {
            lines.push("No signal from the host".to_owned());
        }
        if let Some(line) = live.and_then(|l| liveness_line(&l.liveness, now)) {
            lines.push(line);
        }
        lines.extend(live.into_iter().flat_map(|l| l.terminals.iter()).map(agent_line));
        Self { glyph, guess, lines }
    }
}

impl From<Attention> for Glyph {
    fn from(attention: Attention) -> Self {
        match attention {
            Attention::Waiting => Glyph::Question,
            Attention::Failed => Glyph::Failed,
            Attention::RateLimited => Glyph::RateLimited,
            Attention::Working => Glyph::Working,
            Attention::Unread => Glyph::Unread,
            Attention::Idle => Glyph::Idle,
        }
    }
}

/// One spin of the working arc.
const SPIN: Duration = Duration::from_millis(1200);
/// A glyph that changes eases in over this long.
const GLYPH_ENTER: Duration = Duration::from_millis(180);
/// A call for attention rings once, over this long.
const GLYPH_PULSE: Duration = Duration::from_millis(600);

/// What a glyph slot remembers between frames: the glyph it last drew and
/// when that changed, so a change animates and a first sight does not.
struct GlyphMemory {
    glyph: Glyph,
    generation: u32,
    changed_at: Option<Instant>,
}

/// A fixed-width status slot. Every row reserves it, so titles never shift
/// when a status appears or clears.
#[derive(IntoElement)]
pub struct StatusGlyph {
    id: ElementId,
    glyph: Glyph,
    guess: bool,
    tooltip: Option<SharedString>,
}

impl StatusGlyph {
    /// The glyph for `thread` from its record and live evidence, with a
    /// tooltip naming the state, why (quiet, unverified, terminal agents),
    /// its age and the tokens spent.
    pub fn new(id: impl Into<ElementId>, thread: &Thread, state: &ClientState) -> Self {
        let look = Look::of(thread, state);
        let tooltip = (look.glyph != Glyph::Idle || !look.lines.is_empty()).then(|| glyph_tooltip(&look, thread));
        Self { id: id.into(), glyph: look.glyph, guess: look.guess, tooltip }
    }

    pub fn attention(id: impl Into<ElementId>, attention: Attention) -> Self {
        Self { id: id.into(), glyph: attention.into(), guess: false, tooltip: None }
    }

    pub fn glyph(id: impl Into<ElementId>, glyph: Glyph) -> Self {
        Self { id: id.into(), glyph, guess: false, tooltip: None }
    }
}

fn glyph_tooltip(look: &Look, thread: &Thread) -> SharedString {
    let age = format::relative_time(thread.updated_ms, now_ms());
    let age = if age == "now" { "just now".to_owned() } else { format!("{age} ago") };
    let tokens = thread.usage.input_tokens + thread.usage.output_tokens;
    let label = if look.guess { format!("Probably: {}", look.glyph.label().to_lowercase()) } else { look.glyph.label().to_owned() };
    let mut text = if tokens > 0 { format!("{label} · {age} · {} tokens", format::tokens(tokens)) } else { format!("{label} · {age}") };
    for line in &look.lines {
        text.push('\n');
        text.push_str(line);
    }
    text.into()
}

/// The glyph's shape, at rest.
fn glyph_shape(glyph: Glyph, id: &ElementId, window: &Window, theme: &gpui_kit::component::Theme) -> AnyElement {
    let color = glyph.color(theme);
    match glyph {
        Glyph::Working => {
            // All arcs turn in phase, and slowly when the window is in the
            // background, so many working rows never read as noise.
            let mut spin = Animation::new(SPIN).repeat_synced();
            if !window.is_window_active() {
                spin = spin.with_max_fps(10.);
            }
            Icon::new(IconName::LoaderCircle)
                .size_3p5()
                .text_color(color)
                .with_animation(id.clone(), spin, |icon, t| icon.transform(Transformation::rotate(percentage(t))))
                .into_any_element()
        }
        // Still and faded: work that has gone silent, not work in motion.
        Glyph::Quiet => Icon::new(IconName::LoaderCircle).size_3p5().text_color(color.opacity(0.6)).into_any_element(),
        Glyph::Unverified => Icon::new(Lucide::CircleDashed).size_3p5().text_color(color).into_any_element(),
        Glyph::Question => h_flex()
            .size_3p5()
            .justify_center()
            .rounded_full()
            .bg(color)
            .text_color(theme.warning_foreground)
            .text_size(rems(0.6875))
            .line_height(rems(0.6875))
            .font_weight(FontWeight::BLACK)
            .child("?")
            .into_any_element(),
        Glyph::Approval => Icon::new(Lucide::ShieldAlert).size_3p5().text_color(color).into_any_element(),
        Glyph::Failed => Icon::new(Lucide::OctagonAlert).size_3p5().text_color(color).into_any_element(),
        Glyph::RateLimited => Icon::new(Lucide::Hourglass).size_3p5().text_color(color).into_any_element(),
        Glyph::Unread => div().size_2().rounded_full().bg(color).into_any_element(),
        Glyph::Settled => Icon::new(IconName::Check).size_3().text_color(color).into_any_element(),
        Glyph::Idle => div().into_any_element(),
    }
}

impl RenderOnce for StatusGlyph {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let memory_id = ElementId::NamedChild(Arc::new(self.id.clone()), "memory".into());
        let glyph = self.glyph;
        let memory = window.use_keyed_state(memory_id, cx, |_, _| GlyphMemory { glyph, generation: 0, changed_at: None });
        let (generation, changed_at) = memory.update(cx, |memory, _| {
            if memory.glyph != glyph {
                memory.glyph = glyph;
                memory.generation += 1;
                memory.changed_at = Some(Instant::now());
            }
            (memory.generation, memory.changed_at)
        });
        let theme = cx.theme();
        let shape = glyph_shape(glyph, &self.id, window, theme);
        // A guess is drawn faded, so it never reads like a report.
        let shape = if self.guess { div().flex().opacity(0.5).child(shape).into_any_element() } else { shape };
        let fresh = changed_at.filter(|at| at.elapsed() < GLYPH_PULSE);
        let color = glyph.color(theme);

        let slot = h_flex().id(self.id.clone()).relative().size_4().flex_none().justify_center();
        let slot = match fresh {
            // A change eases in (scale and fade); a call also rings once.
            Some(_) => {
                let key = |part: &'static str| {
                    ElementId::NamedChild(Arc::new(self.id.clone()), SharedString::from(format!("{part}-{generation}")))
                };
                let entering = div()
                    .flex()
                    .justify_center()
                    .items_center()
                    .size_4()
                    .child(shape)
                    .with_animation(key("enter"), Animation::new(GLYPH_ENTER).with_easing(ease_out_quint()), |el, t| {
                        el.opacity(t)
                    });
                slot.when(glyph.is_call(), |slot| {
                    slot.child(
                        div()
                            .absolute()
                            .rounded_full()
                            .border_1()
                            .border_color(color)
                            .with_animation(
                                key("pulse"),
                                Animation::new(GLYPH_PULSE).with_easing(ease_out_quint()),
                                |ring, t| {
                                    let size = 0.75 + 0.75 * t;
                                    let inset = (1.0 - size) / 2.0;
                                    ring.size(rems(size)).top(rems(inset)).left(rems(inset)).opacity(0.7 * (1.0 - t))
                                },
                            ),
                    )
                })
                .child(entering)
            }
            None => slot.child(shape),
        };
        slot.when_some(self.tooltip, |slot, text| {
            slot.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx))
        })
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
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let fill = self.usage.context_fill();
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
        // The same ring and color ramp as the quota rings in the title bar.
        div()
            .id(self.id.clone())
            .px_1()
            .rounded_md()
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx))
            .child(crate::usage::ring::Meter::new(ElementId::NamedChild(std::sync::Arc::new(self.id), "meter".into()), fill))
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

/// What an agent asks for, in one short line: rows, cards, peeks and
/// notifications all say it the same way.
pub fn request_line(kind: &RequestKind) -> String {
    match kind {
        RequestKind::Command { command, .. } => {
            format!("Wants to run {}", command.lines().next().unwrap_or_default().trim())
        }
        RequestKind::FileChange { path, .. } => {
            format!("Wants to change {}", path.rsplit(['/', '\\']).next().unwrap_or(path))
        }
        RequestKind::Tool { name, .. } => format!("Wants to use {name}"),
        RequestKind::Question { question, .. } => {
            let line = question.lines().find(|l| !l.trim().is_empty()).unwrap_or("Has a question").trim();
            line.to_owned()
        }
        RequestKind::Plan { .. } => "Has a plan for you to approve".into(),
    }
}

/// The kinds of request. "Approve all of this kind" groups by these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestClass {
    Command,
    FileChange,
    Tool,
    Question,
    Plan,
}

impl RequestClass {
    pub fn of(kind: &RequestKind) -> Self {
        match kind {
            RequestKind::Command { .. } => RequestClass::Command,
            RequestKind::FileChange { .. } => RequestClass::FileChange,
            RequestKind::Tool { .. } => RequestClass::Tool,
            RequestKind::Question { .. } => RequestClass::Question,
            RequestKind::Plan { .. } => RequestClass::Plan,
        }
    }

    /// A yes or no: allow, allow for the session, or deny.
    pub fn is_approval(self) -> bool {
        matches!(self, RequestClass::Command | RequestClass::FileChange | RequestClass::Tool)
    }

    /// "commands", "file changes": for "Approve all 3 commands".
    pub fn plural(self) -> &'static str {
        match self {
            RequestClass::Command => "commands",
            RequestClass::FileChange => "file changes",
            RequestClass::Tool => "tool uses",
            RequestClass::Question => "questions",
            RequestClass::Plan => "plans",
        }
    }
}

/// The keys that run `action`, as keycaps, read from the live keymap so the
/// user's own bindings show everywhere.
pub fn keycaps(action: &dyn Action, cx: &App) -> Option<AnyElement> {
    let strokes = crate::keymap::keystrokes_for(action, cx)?;
    Some(
        h_flex()
            .gap_0p5()
            .children(strokes.into_iter().map(|s| gpui_kit::component::kbd::Kbd::new(s).into_any_element()))
            .into_any_element(),
    )
}

/// A literal keycap (`"j"`, `"shift-a"`), for keys that are not one
/// action's binding, such as 1–9.
pub fn keycap(keys: &'static str) -> AnyElement {
    match Keystroke::parse(keys) {
        Ok(stroke) => gpui_kit::component::kbd::Kbd::new(stroke).into_any_element(),
        Err(_) => div().child(keys.to_owned()).into_any_element(),
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

#[cfg(test)]
mod look_tests {
    use super::{Glyph, Look};
    use orbit_proto::*;

    fn running() -> Thread {
        Thread { id: ThreadId(1), status: ThreadStatus::Running, ..Default::default() }
    }

    fn agent(state: AgentState, evidence: Evidence) -> TerminalAgent {
        TerminalAgent { term: TermId(2), agent: "claude".into(), state, evidence, since_ms: 0, last_signal_ms: 0, detail: String::new() }
    }

    #[test]
    fn a_quiet_turn_says_what_it_is_waiting_on() {
        let live = ThreadLive { liveness: Liveness::Quiet { since_ms: 0, tool: Some("cargo test".into()) }, ..Default::default() };
        let look = Look::from_evidence(&running(), Some(&live), true, 4 * 60_000);
        assert_eq!(look.glyph, Glyph::Quiet);
        assert!(!look.guess);
        assert_eq!(look.lines, vec!["No output for 4m · cargo test".to_owned()]);
    }

    #[test]
    fn terminal_agents_read_from_the_screen_look_like_guesses() {
        let idle = Thread { id: ThreadId(1), ..Default::default() };
        let guessed = ThreadLive { terminals: vec![agent(AgentState::Working, Evidence::Heuristic)], ..Default::default() };
        let look = Look::from_evidence(&idle, Some(&guessed), true, 0);
        assert_eq!((look.glyph, look.guess), (Glyph::Working, true));
        assert_eq!(look.lines, vec!["Claude Code · probably working".to_owned()]);
        let hooked =
            ThreadLive { terminals: vec![agent(AgentState::NeedsYou(NeedKind::Permission), Evidence::Hook)], ..Default::default() };
        let look = Look::from_evidence(&idle, Some(&hooked), true, 0);
        assert_eq!((look.glyph, look.guess), (Glyph::Approval, false));
    }

    #[test]
    fn work_on_a_host_out_of_reach_is_unverified() {
        let look = Look::from_evidence(&running(), None, false, 0);
        assert_eq!(look.glyph, Glyph::Unverified);
        assert_eq!(look.lines, vec!["No signal from the host".to_owned()]);
        assert_eq!(Look::from_evidence(&running(), None, true, 0).glyph, Glyph::Working);
    }
}
