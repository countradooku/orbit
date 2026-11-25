//! "Race ×N": the composer control that runs one prompt several times, or
//! across agents, each in its own worktree.
//!
//! The composer mounts [`RaceControl`] in its control row. At send, it asks
//! [`RaceControl::take`] for the candidates (given its own config); when that
//! returns some, it calls [`start_race`] instead of sending, which starts the
//! race and opens its compare view.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::Workspace;

/// Candidates in one race.
pub const MAX_CANDIDATES: usize = 8;

/// What a race starts with.
#[derive(Clone, Debug, PartialEq)]
pub struct RaceRequest {
    pub configs: Vec<ThreadConfig>,
    pub judge: bool,
}

pub struct RaceControl {
    app: Entity<AppState>,
    /// One means no race: the composer sends as usual.
    count: usize,
    /// Per candidate: an agent of its own, or the composer's (`None`).
    agents: Vec<Option<ThreadConfig>>,
    judge: bool,
    open: bool,
    /// Where the popover opens from its trigger.
    anchor: Anchor,
    /// What a candidate without an agent of its own runs on.
    base_label: SharedString,
}

impl RaceControl {
    pub fn new(app: Entity<AppState>, _: &mut Context<Self>) -> Self {
        Self {
            app,
            count: 1,
            agents: Vec::new(),
            judge: true,
            open: false,
            anchor: Anchor::BottomLeft,
            base_label: "Composer’s agent".into(),
        }
    }

    /// Opens the popover from `anchor` (default: above, for a composer at
    /// the bottom of the window) and names the agent candidates share.
    pub fn placed(mut self, anchor: Anchor, base_label: impl Into<SharedString>) -> Self {
        self.anchor = anchor;
        self.base_label = base_label.into();
        self
    }

    /// Starts with `count` candidates (2 or more races right away).
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = count.clamp(1, MAX_CANDIDATES);
        self
    }

    /// Debug builds: shows the popover (visual checks).
    pub fn debug_open(&mut self, cx: &mut Context<Self>) {
        self.open = true;
        cx.notify();
    }

    /// Whether the next send starts a race.
    pub fn is_racing(&self) -> bool {
        self.count >= 2
    }

    /// The candidates' configs for this send, from the composer's `base`
    /// config, and back to one run. `None` when not racing.
    pub fn take(&mut self, base: &ThreadConfig, cx: &mut Context<Self>) -> Option<RaceRequest> {
        if !self.is_racing() {
            return None;
        }
        let configs = candidate_configs(self.count, &self.agents, base);
        let request = RaceRequest { configs, judge: self.judge };
        self.count = 1;
        self.agents.clear();
        cx.notify();
        Some(request)
    }

    pub fn set_count(&mut self, count: usize, cx: &mut Context<Self>) {
        self.count = count.clamp(1, MAX_CANDIDATES);
        self.agents.truncate(self.count);
        cx.notify();
    }

    fn set_agent(&mut self, ix: usize, config: Option<ThreadConfig>, cx: &mut Context<Self>) {
        if self.agents.len() <= ix {
            self.agents.resize(ix + 1, None);
        }
        self.agents[ix] = config;
        cx.notify();
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let installed: Vec<ProviderInfo> = state.providers.iter().filter(|p| p.installed).cloned().collect();
        let count = self.count;
        let this = cx.weak_entity();
        let candidates = (0..count.max(2)).map(|ix| {
            let label = ((b'A' + ix as u8) as char).to_string();
            let chosen = self.agents.get(ix).cloned().flatten();
            let name = match &chosen {
                Some(config) => crate::ui::model_label(config, &state.providers),
                None => self.base_label.to_string(),
            };
            let (installed, this, base_label) = (installed.clone(), this.clone(), self.base_label.clone());
            h_flex()
                .gap_2()
                .when(count < 2, |el| el.opacity(0.5))
                .child(
                    h_flex()
                        .size_5()
                        .flex_none()
                        .justify_center()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(label),
                )
                .child(
                    Button::new(("race-agent", ix))
                        .ghost()
                        .small()
                        .disabled(count < 2)
                        .label(name)
                        .dropdown_menu(move |mut menu, _, _| {
                            let this2 = this.clone();
                            menu = menu.item(PopupMenuItem::new(base_label.clone()).checked(chosen.is_none()).on_click(move |_, _, cx| {
                                let _ = this2.update(cx, |this, cx| this.set_agent(ix, None, cx));
                            }));
                            for p in &installed {
                                let config = ThreadConfig {
                                    provider: p.kind,
                                    instance: if p.instance_id() == p.kind.id() { String::new() } else { p.instance.clone() },
                                    model: p.models.iter().find(|m| m.is_default).map(|m| m.slug.clone()).unwrap_or_default(),
                                    ..Default::default()
                                };
                                let checked = chosen.as_ref().is_some_and(|c| c.same_instance(&config));
                                let this = this.clone();
                                menu = menu.item(PopupMenuItem::new(p.display_name().to_owned()).checked(checked).on_click(move |_, _, cx| {
                                    let config = config.clone();
                                    let _ = this.update(cx, |this, cx| this.set_agent(ix, Some(config), cx));
                                }));
                            }
                            menu
                        }),
                )
        });
        v_flex()
            .w(rems(19.))
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Race"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Run the prompt several times, each in its own worktree, then compare the results."),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_sm().child("Candidates"))
                    .child(
                        Button::new("race-fewer")
                            .outline()
                            .xsmall()
                            .icon(IconName::Minus)
                            .disabled(count <= 1)
                            .tooltip("Fewer")
                            .on_click(cx.listener(|this, _, _, cx| this.set_count(this.count.saturating_sub(1), cx))),
                    )
                    .child(div().w(rems(1.5)).text_center().text_sm().child(if count < 2 { "Off".to_owned() } else { count.to_string() }))
                    .child(
                        Button::new("race-more")
                            .outline()
                            .xsmall()
                            .icon(IconName::Plus)
                            .disabled(count >= MAX_CANDIDATES)
                            .tooltip("More")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let next = if this.count < 2 { 2 } else { this.count + 1 };
                                this.set_count(next, cx)
                            })),
                    ),
            )
            .child(v_flex().gap_1().children(candidates.take(count.max(2))))
            .child(
                h_flex().gap_2().child(div().flex_1().text_sm().child("Judge the results")).child(
                    Switch::new("race-judge").small().checked(self.judge).on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.judge = *on;
                        cx.notify();
                    })),
                ),
            )
    }
}

impl Render for RaceControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let racing = self.is_racing();
        let count = self.count;
        let content = cx.weak_entity();
        Popover::new("race-control")
            .anchor(self.anchor)
            .open(self.open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.open = *open;
                cx.notify();
            }))
            .trigger(
                Button::new("race-pill")
                    .ghost()
                    .small()
                    .selected(racing)
                    .icon(Icon::new(Lucide::Layers))
                    .label(if racing { format!("Race ×{count}") } else { "Race".to_owned() })
                    .tooltip("Race this prompt across several candidates"),
            )
            .content(move |_, _, cx| {
                content.upgrade().map(|c| c.update(cx, |c, cx| c.render_content(cx).into_any_element())).unwrap_or_else(|| div().into_any_element())
            })
    }
}

/// Each candidate's config: its own agent or the composer's `base`, with
/// the composer's mode and access (a candidate never gets more access than
/// the composer asked for).
pub fn candidate_configs(count: usize, agents: &[Option<ThreadConfig>], base: &ThreadConfig) -> Vec<ThreadConfig> {
    (0..count)
        .map(|i| {
            let mut config = agents.get(i).cloned().flatten().unwrap_or_else(|| base.clone());
            config.access = base.access;
            config.mode = base.mode;
            config
        })
        .collect()
}

/// Starts a race of `prompt` in `project` and opens its compare view.
pub fn start_race(
    app: &Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    project: ProjectId,
    prompt: String,
    request: RaceRequest,
    window: &mut Window,
    cx: &mut App,
) {
    let command = Command::Team(TeamCommand::StartRace {
        project,
        prompt,
        configs: request.configs,
        test_command: None,
        judge: request.judge,
        base: String::new(),
    });
    let task = app.update(cx, |app, cx| app.call(command, cx));
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = task.await;
        let _ = handle.update(cx, |_, window, cx| match result {
            Ok(Reply::Team(TeamReply::Race(race))) => super::open_race(&workspace, race, window, cx),
            Ok(_) => {}
            Err(message) => {
                use gpui_kit::component::WindowExt as _;
                use gpui_kit::component::notification::Notification;
                window.push_notification(Notification::error(message), cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::candidate_configs;
    use orbit_proto::*;

    #[test]
    fn candidates_follow_their_agent_but_never_widen_access() {
        let base = ThreadConfig { model: "opus".into(), access: Access::Ask, ..Default::default() };
        let codex = ThreadConfig { provider: ProviderKind::Codex, access: Access::Full, ..Default::default() };
        let configs = candidate_configs(3, &[None, Some(codex)], &base);
        assert_eq!(configs.len(), 3);
        assert_eq!(configs[0], base);
        assert_eq!((configs[1].provider, configs[1].access), (ProviderKind::Codex, Access::Ask));
        assert_eq!(configs[2], base);
    }
}
