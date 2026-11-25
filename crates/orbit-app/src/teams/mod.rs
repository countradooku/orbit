//! Agent teams and races in the window.
//!
//! What mounts where (the owners of those surfaces wire them in):
//!
//! | Component | Mount | Built with |
//! | --- | --- | --- |
//! | [`CrewStrip`] | Under an architect's thread, above its composer; renders nothing for other threads | `CrewStrip::new(app, workspace, thread, cx)` |
//! | [`NotesBoard`] | The thread inspector of any team member, or anywhere a team is shown | `NotesBoard::new(app, team, window, cx)` |
//! | [`RaceControl`] | The composer's control row, beside the model picker | `RaceControl::new(app, cx)`; at send, [`RaceControl::take`] then [`start_race`] |
//! | [`Constellation`] | Docked at the bottom of the sidebar (mini) | `Constellation::new(app, workspace, ConstellationMode::Mini, cx)` |
//! | [`TeamsPage`] | `Route::Teams` in the shell: teams and races, a race's compare view and the full constellation | `TeamsPage::new(app, workspace, window, cx)` |
//!
//! Every view reads the engine's records from [`AppState`]'s client state and
//! re-renders on `AppEvent::Teams` (a team or race changed) and on thread
//! changes; none polls or animates while nothing works.

mod compare;
mod constellation;
mod crew;
#[cfg(debug_assertions)]
pub mod demo;
mod notes;
mod race;

use std::collections::HashMap;

pub use compare::CompareView;
pub use constellation::{Constellation, ConstellationMode};
pub use crew::CrewStrip;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
pub use notes::NotesBoard;
use orbit_net::state::ClientState;
use orbit_proto::*;
pub use race::{RaceControl, start_race};

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::{format, ui};

// ── Shared vocabulary ───────────────────────────────────────────────────

/// One word for a worker's state.
pub fn worker_label(state: &WorkerState) -> &'static str {
    match state {
        WorkerState::Waiting(_) => "Waiting",
        WorkerState::Working => "Working",
        WorkerState::Done => "Done",
        WorkerState::Failed { .. } => "Failed",
        WorkerState::Merged => "Merged",
        WorkerState::Stopped => "Stopped",
    }
}

/// The color of a worker's state, from the status vocabulary: working has
/// no hue, done is ready for review, merged is merged, failed is failed.
pub fn worker_color(state: &WorkerState, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        WorkerState::Working => theme.muted_foreground,
        WorkerState::Waiting(_) | WorkerState::Stopped => theme.border,
        WorkerState::Done => theme.success,
        WorkerState::Merged => theme.magenta,
        WorkerState::Failed { .. } => theme.danger,
    }
}

/// "3 working · 2 done · 1 waiting": the buckets that hold anyone.
pub fn tally_line(t: &Tally) -> String {
    let parts: Vec<String> = [(t.working, "working"), (t.done, "done"), (t.waiting, "waiting"), (t.failed, "failed"), (t.stopped, "stopped")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect();
    if parts.is_empty() { "No workers yet".into() } else { parts.join(" · ") }
}

/// The team `thread` leads.
pub fn team_led_by(state: &ClientState, thread: ThreadId) -> Option<&Team> {
    state.teams.values().find(|t| t.architect == thread)
}

/// A team's name: its architect's title.
pub fn team_title(state: &ClientState, team: &Team) -> String {
    state.threads.get(&team.architect).map(display_title).unwrap_or("Team").to_owned()
}

/// A candidate's agent, as the model picker names it.
pub fn candidate_agent(state: &ClientState, config: &ThreadConfig) -> String {
    ui::model_label(config, &state.providers)
}

/// Milliseconds a worker has worked (until now while it works).
pub fn worker_elapsed(w: &Worker, now: u64) -> Option<u64> {
    let start = w.started_ms?;
    Some(w.finished_ms.unwrap_or(now).saturating_sub(start))
}

/// Elapsed time at the clock's grain: `45s`, `12m`, `1h 05m`.
pub fn elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A segmented progress bar: one segment per state, sized by count, in the
/// order work flows (merged, done, working, failed, waiting).
pub fn crew_bar(team: &Team, id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let total = team.workers.len().max(1) as f32;
    let order = |s: &WorkerState| match s {
        WorkerState::Merged => 0,
        WorkerState::Done => 1,
        WorkerState::Working => 2,
        WorkerState::Failed { .. } => 3,
        WorkerState::Waiting(_) | WorkerState::Stopped => 4,
    };
    let mut buckets: Vec<(usize, &WorkerState, u32)> = Vec::new();
    for w in &team.workers {
        let k = order(&w.state);
        match buckets.iter_mut().find(|(o, ..)| *o == k) {
            Some(b) => b.2 += 1,
            None => buckets.push((k, &w.state, 1)),
        }
    }
    buckets.sort_by_key(|b| b.0);
    h_flex()
        .id(id)
        .w_full()
        .h_1()
        .gap(px(2.))
        .rounded_full()
        .overflow_hidden()
        .when(team.workers.is_empty(), |el| el.bg(theme.muted))
        .children(buckets.into_iter().map(|(_, state, n)| {
            div().h_full().rounded_full().w(relative(n as f32 / total)).flex_shrink(1.).bg(worker_color(state, cx))
        }))
}

// ── The routed page ─────────────────────────────────────────────────────

/// What the teams page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamsPageKind {
    /// Every team and race.
    Overview,
    Team(TeamId),
    /// A race's compare view.
    Race(RaceId),
    /// The full constellation.
    Galaxy,
}

/// `Route::Teams`: the overview of teams and races, one team with its crew
/// and notes, a race's compare view, or the full constellation.
pub struct TeamsPage {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    page: TeamsPageKind,
    crews: HashMap<TeamId, Entity<CrewStrip>>,
    notes: HashMap<TeamId, Entity<NotesBoard>>,
    compares: HashMap<RaceId, Entity<CompareView>>,
    galaxy: Entity<Constellation>,
    mini: Entity<Constellation>,
    /// The overview's own race composer.
    race_prompt: Entity<InputState>,
    race_control: Entity<RaceControl>,
    race_project: Option<ProjectId>,
    _subscriptions: Vec<Subscription>,
}

impl TeamsPage {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let galaxy = cx.new(|cx| Constellation::new(app.clone(), workspace.clone(), ConstellationMode::Galaxy, cx));
        let mini = cx.new(|cx| Constellation::new(app.clone(), workspace.clone(), ConstellationMode::Mini, cx));
        let race_prompt = cx.new(|cx| InputState::new(window, cx).placeholder("A prompt to race…"));
        let race_control = cx.new(|cx| RaceControl::new(app.clone(), cx).with_count(3));
        let subs = vec![
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| {
                if matches!(event, AppEvent::Teams) {
                    this.forget_gone(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&race_prompt, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.start_race(window, cx);
                }
            }),
            cx.observe(&race_control, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            workspace,
            page: TeamsPageKind::Overview,
            crews: HashMap::new(),
            notes: HashMap::new(),
            compares: HashMap::new(),
            galaxy,
            mini,
            race_prompt,
            race_control,
            race_project: None,
            _subscriptions: subs,
        }
    }

    /// The project a race from the overview runs in: the one picked, else
    /// the most recently used.
    fn race_project(&self, cx: &App) -> Option<ProjectId> {
        let state = self.app.read(cx).state();
        self.race_project.filter(|p| state.projects.iter().any(|x| x.id == *p)).or_else(|| {
            state.threads.values().filter(|t| t.project.is_some()).max_by_key(|t| t.updated_ms).and_then(|t| t.project).or_else(|| state.projects.first().map(|p| p.id))
        })
    }

    fn start_race(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.race_prompt.read(cx).value().trim().to_owned();
        let Some(project) = self.race_project(cx) else { return };
        if prompt.is_empty() || !self.race_control.read(cx).is_racing() {
            return;
        }
        let base = self.app.read(cx).state().defaults.clone();
        let Some(request) = self.race_control.update(cx, |c, cx| c.take(&base, cx)) else { return };
        self.race_control.update(cx, |c, cx| c.set_count(3, cx));
        self.race_prompt.update(cx, |input, cx| input.set_value("", window, cx));
        start_race(&self.app, self.workspace.clone(), project, prompt, request, window, cx);
    }

    fn render_new_race(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let project = self.race_project(cx);
        let project_name = project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.name.clone()).unwrap_or_else(|| "No project".into());
        let projects: Vec<(ProjectId, String)> = state.projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let this = cx.weak_entity();
        let ready = project.is_some() && self.race_control.read(cx).is_racing() && !self.race_prompt.read(cx).value().trim().is_empty();
        v_flex()
            .flex_1()
            .min_w_0()
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("New race"))
            .child(Input::new(&self.race_prompt))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("race-project")
                            .ghost()
                            .small()
                            .icon(IconName::Folder)
                            .label(project_name)
                            .dropdown_menu(move |mut menu, _, _| {
                                for (id, name) in &projects {
                                    let (id, this) = (*id, this.clone());
                                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(Some(id) == project).on_click(move |_, _, cx| {
                                        let _ = this.update(cx, |this, cx| {
                                            this.race_project = Some(id);
                                            cx.notify();
                                        });
                                    }));
                                }
                                menu
                            }),
                    )
                    .child(self.race_control.clone())
                    .child(div().flex_1())
                    .child(
                        Button::new("race-start")
                            .primary()
                            .small()
                            .label("Start race")
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| this.start_race(window, cx))),
                    ),
            )
    }

    /// Shows `page` (the caller navigates to `Route::Teams`).
    pub fn show(&mut self, page: TeamsPageKind, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    /// Debug builds: the page for `ORBIT_DEBUG_ROUTE` (`teams`, `team`,
    /// `compare`, `galaxy`), once the records arrived.
    pub fn debug_show(&mut self, route: &str, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let page = match route {
            "team" => state.teams.values().max_by_key(|t| t.workers.len()).map(|t| TeamsPageKind::Team(t.id)),
            "compare" => state.races.values().next().map(|r| TeamsPageKind::Race(r.id)),
            "galaxy" => Some(TeamsPageKind::Galaxy),
            _ => Some(TeamsPageKind::Overview),
        };
        self.show(page.unwrap_or(TeamsPageKind::Overview), cx);
    }

    fn forget_gone(&mut self, cx: &App) {
        let state = self.app.read(cx).state();
        self.crews.retain(|id, _| state.teams.contains_key(id));
        self.notes.retain(|id, _| state.teams.contains_key(id));
        self.compares.retain(|id, _| state.races.contains_key(id));
        let gone = match self.page {
            TeamsPageKind::Team(id) => !state.teams.contains_key(&id),
            TeamsPageKind::Race(id) => !state.races.contains_key(&id),
            _ => false,
        };
        if gone {
            self.page = TeamsPageKind::Overview;
        }
    }

    fn go(&mut self, page: TeamsPageKind, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(Route::Teams, window, cx));
        cx.notify();
    }

    fn crew(&mut self, team: &Team, cx: &mut Context<Self>) -> Entity<CrewStrip> {
        let (app, ws, architect) = (self.app.clone(), self.workspace.clone(), team.architect);
        self.crews.entry(team.id).or_insert_with(|| cx.new(|cx| CrewStrip::new(app, ws, architect, cx).unbounded())).clone()
    }

    fn notes_board(&mut self, team: TeamId, window: &mut Window, cx: &mut Context<Self>) -> Entity<NotesBoard> {
        let app = self.app.clone();
        self.notes.entry(team).or_insert_with(|| cx.new(|cx| NotesBoard::new(app, team, window, cx))).clone()
    }

    fn compare(&mut self, race: RaceId, cx: &mut Context<Self>) -> Entity<CompareView> {
        let (app, ws) = (self.app.clone(), self.workspace.clone());
        self.compares.entry(race).or_insert_with(|| cx.new(|cx| CompareView::new(app, ws, race, cx))).clone()
    }

    fn render_header(&self, title: SharedString, back: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .h_9()
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .when(back, |el| {
                el.child(
                    Button::new("teams-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .tooltip("Teams and races")
                        .on_click(cx.listener(|this, _, window, cx| this.go(TeamsPageKind::Overview, window, cx))),
                )
            })
            .when(!back, |el| el.child(div().pl_1().child(Icon::new(IconName::Network).small().text_color(theme.muted_foreground))))
            .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
            .when(self.page != TeamsPageKind::Galaxy, |el| {
                el.child(
                    Button::new("teams-galaxy")
                        .ghost()
                        .small()
                        .icon(Icon::new(Lucide::Orbit))
                        .label("Constellation")
                        .on_click(cx.listener(|this, _, window, cx| this.go(TeamsPageKind::Galaxy, window, cx))),
                )
            })
    }

    fn render_overview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let top = h_flex()
            .items_stretch()
            .gap_3()
            .child(self.render_new_race(cx))
            .child(
                v_flex()
                    .w(rems(16.))
                    .flex_none()
                    .p_3()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Live now"))
                    .child(self.mini.clone()),
            );
        let state = self.app.read(cx).state();
        let mut teams: Vec<&Team> = state.teams.values().collect();
        teams.sort_by_key(|t| std::cmp::Reverse(t.created_ms));
        let mut races: Vec<&Race> = state.races.values().collect();
        races.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
        if teams.is_empty() && races.is_empty() {
            return v_flex()
                .flex_1()
                .w_full()
                .max_w(rems(56.))
                .mx_auto()
                .p_6()
                .gap_6()
                .child(top)
                .child(ui::empty_state(
                    Icon::new(IconName::Network),
                    "No teams or races yet",
                    "Race a prompt above, or start a thread as a team’s architect with `orbit team start`.",
                    None,
                    cx,
                ))
                .into_any_element();
        }
        let now = now_ms();
        let team_cards: Vec<AnyElement> = teams
            .iter()
            .map(|team| {
                let id = team.id;
                let title = team_title(state, team);
                let tally = team.tally();
                v_flex()
                    .id(("team-card", id.0))
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_default()
                    .on_click(cx.listener(move |this, _, window, cx| this.go(TeamsPageKind::Team(id), window, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Network).xsmall().text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                            .when_some(team.breaker.as_ref(), |el, _| {
                                el.child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.warning))
                            })
                            .child(div().text_xs().text_color(theme.muted_foreground).child(format::relative_time(team.created_ms, now))),
                    )
                    .child(crew_bar(team, ("team-card-bar", id.0), cx))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(tally_line(&tally)))
                    .into_any_element()
            })
            .collect();
        let race_cards: Vec<AnyElement> = races
            .iter()
            .map(|race| {
                let id = race.id;
                let state_line = match &race.state {
                    RaceState::Running if race.settled() => "Ready to compare".to_owned(),
                    RaceState::Running => {
                        let done = race.candidates.iter().filter(|c| c.finished.is_some()).count();
                        format!("{done} of {} finished", race.candidates.len())
                    }
                    RaceState::Decided { winner } => {
                        let label = race.candidate(*winner).map(|c| c.label.clone()).unwrap_or_default();
                        format!("Kept {label}")
                    }
                    RaceState::Cancelled => "Cancelled".into(),
                };
                v_flex()
                    .id(("race-card", id.0))
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_default()
                    .on_click(cx.listener(move |this, _, window, cx| this.go(TeamsPageKind::Race(id), window, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(Lucide::Layers).xsmall().text_color(theme.muted_foreground))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(race.prompt.lines().next().unwrap_or_default().to_owned()),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(format::relative_time(race.created_ms, now))),
                    )
                    .child(
                        h_flex().gap_1().children(race.candidates.iter().map(|c| {
                            let color = match (&c.finished, &c.test) {
                                (_, TestState::Failed { .. }) | (Some(CandidateEnd::Failed { .. }), _) => theme.danger,
                                (Some(_), _) => theme.success,
                                (None, _) => theme.muted_foreground,
                            };
                            h_flex()
                                .gap_1()
                                .px_1p5()
                                .h_5()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_xs()
                                .child(div().size_1p5().rounded_full().bg(color))
                                .child(c.label.clone())
                        })),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(state_line))
                    .into_any_element()
            })
            .collect();
        let section = |label: &'static str, cards: Vec<AnyElement>| {
            v_flex()
                .gap_2()
                .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(label))
                .child(div().grid().grid_cols(2).gap_3().children(cards))
        };
        div()
            .id("teams-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(56.))
                    .mx_auto()
                    .p_6()
                    .gap_6()
                    .child(top)
                    .when(!team_cards.is_empty(), |el| el.child(section("Teams", team_cards)))
                    .when(!race_cards.is_empty(), |el| el.child(section("Races", race_cards))),
            )
            .into_any_element()
    }

    fn render_team(&mut self, id: TeamId, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(team) = self.app.read(cx).state().teams.get(&id).cloned() else {
            return div().into_any_element();
        };
        let crew = self.crew(&team, cx);
        let notes = self.notes_board(id, window, cx);
        let theme = cx.theme();
        h_flex()
            .flex_1()
            .min_h_0()
            .items_start()
            .p_4()
            .gap_4()
            .child(v_flex().flex_1().min_w_0().max_h_full().child(crew))
            .child(
                v_flex()
                    .w(rems(22.))
                    .flex_none()
                    .max_h_full()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .child(notes),
            )
            .into_any_element()
    }
}

impl Render for TeamsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let (title, back): (SharedString, bool) = match self.page {
            TeamsPageKind::Overview => ("Teams and races".into(), false),
            TeamsPageKind::Team(id) => {
                (state.teams.get(&id).map(|t| team_title(state, t)).unwrap_or_else(|| "Team".into()).into(), true)
            }
            TeamsPageKind::Race(id) => (
                state
                    .races
                    .get(&id)
                    .map(|r| format!("Race: {}", r.prompt.lines().next().unwrap_or_default()))
                    .unwrap_or_else(|| "Race".into())
                    .into(),
                true,
            ),
            TeamsPageKind::Galaxy => ("Constellation".into(), true),
        };
        let body = match self.page {
            TeamsPageKind::Overview => self.render_overview(cx),
            TeamsPageKind::Team(id) => self.render_team(id, window, cx),
            TeamsPageKind::Race(id) => self.compare(id, cx).into_any_element(),
            TeamsPageKind::Galaxy => self.galaxy.clone().into_any_element(),
        };
        v_flex().size_full().bg(cx.theme().background).child(self.render_header(title, back, cx)).child(body)
    }
}

/// Opens a race's compare view from anywhere (a candidate's thread, a
/// notification).
pub fn open_race(workspace: &WeakEntity<Workspace>, race: RaceId, window: &mut Window, cx: &mut App) {
    let _ = workspace.update(cx, |ws, cx| {
        ws.teams_page().update(cx, |page, cx| page.show(TeamsPageKind::Race(race), cx));
        ws.navigate(Route::Teams, window, cx);
    });
}

/// Opens a team's page.
pub fn open_team(workspace: &WeakEntity<Workspace>, team: TeamId, window: &mut Window, cx: &mut App) {
    let _ = workspace.update(cx, |ws, cx| {
        ws.teams_page().update(cx, |page, cx| page.show(TeamsPageKind::Team(team), cx));
        ws.navigate(Route::Teams, window, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::tally_line;
    use orbit_proto::Tally;

    #[test]
    fn elapsed_reads_at_the_clocks_grain() {
        assert_eq!(super::elapsed(45_000), "45s");
        assert_eq!(super::elapsed(20 * 60_000 + 20_000), "20m");
        assert_eq!(super::elapsed(65 * 60_000), "1h 05m");
    }

    #[test]
    fn the_tally_names_only_buckets_with_someone_in_them() {
        assert_eq!(tally_line(&Tally { working: 3, done: 2, waiting: 1, ..Default::default() }), "3 working · 2 done · 1 waiting");
        assert_eq!(tally_line(&Tally { done: 4, failed: 1, ..Default::default() }), "4 done · 1 failed");
        assert_eq!(tally_line(&Tally::default()), "No workers yet");
    }
}
