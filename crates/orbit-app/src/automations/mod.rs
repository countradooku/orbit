//! Automations: agents that start on a schedule or when something happens.
//!
//! A master–detail page. The list keeps every automation scannable: its
//! last run's outcome, when it runs next ("in 3h · weekdays 9:00") and a
//! switch to pause it. The detail shows what starts it, the next runs, and
//! the history of runs, each linked to its thread. With nothing selected the
//! detail is the recipe gallery; with no automations at all the page
//! explains what an automation is and offers the recipes.
//!
//! The editor is a sheet (`editor.rs`) over the page; the schedule builder
//! behind it is plain data (`builder.rs`).

pub mod builder;
mod editor;
mod gallery;
mod history;

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_core::automations::schedule::Schedule;
use orbit_net::state::ClientState;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::{format, ui};

gpui_kit::actions!(orbit_automations, [NewAutomation, EditAutomation, SelectNextAutomation, SelectPreviousAutomation]);

pub struct AutomationsView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    selected: Option<AutomationId>,
    /// The detail shows the recipes instead of the selection.
    gallery: bool,
    /// "Run now" requests in flight.
    starting: HashSet<AutomationId>,
    split: Entity<ResizableState>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl AutomationsView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        cx.bind_keys([
            KeyBinding::new("down", SelectNextAutomation, Some("Automations && !Input")),
            KeyBinding::new("up", SelectPreviousAutomation, Some("Automations && !Input")),
            KeyBinding::new("enter", EditAutomation, Some("Automations && !Input")),
        ]);
        let subscriptions = vec![cx.observe(&app, |this, app, cx| {
            // Follow deletions.
            if let Some(id) = this.selected
                && app.read(cx).state().automation(id).is_none()
            {
                this.selected = None;
            }
            cx.notify();
        })];
        let debug = if cfg!(debug_assertions) { std::env::var("ORBIT_DEBUG_AUTOMATIONS").unwrap_or_default() } else { String::new() };
        Self {
            app,
            workspace,
            focus: cx.focus_handle(),
            selected: None,
            gallery: debug == "gallery",
            starting: HashSet::new(),
            split: cx.new(|_| ResizableState::default()),
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let handle = this.read(cx).focus.clone();
        handle.focus(window, cx);
    }

    /// Shown: select something sensible when nothing is.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_none() && !self.gallery {
            self.selected = self.ordered(cx).first().copied();
        }
        cx.notify();
    }

    fn ordered(&self, cx: &App) -> Vec<AutomationId> {
        self.app.read(cx).state().automations.iter().map(|a| a.id).collect()
    }

    pub fn select(&mut self, id: AutomationId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.gallery = false;
        if let Some(ix) = self.ordered(cx).iter().position(|a| *a == id) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.ordered(cx);
        if order.is_empty() {
            return;
        }
        let current = self.selected.and_then(|id| order.iter().position(|a| *a == id));
        let next = match current {
            Some(ix) => (ix as isize + delta).clamp(0, order.len() as isize - 1) as usize,
            None => 0,
        };
        self.select(order[next], cx);
    }

    fn set_paused(&mut self, id: AutomationId, paused: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(Command::Automation(AutomationCommand::SetPaused { id, paused }), window, cx));
    }

    fn run_now(&mut self, id: AutomationId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.starting.insert(id) {
            return;
        }
        cx.notify();
        let task = self.app.update(cx, |app, cx| app.call(Command::Automation(AutomationCommand::RunNow { id }), cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting.remove(&id);
                if let Err(message) = result {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(message).title("Couldn’t start the run"),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_delete(&mut self, id: AutomationId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.app.read(cx).state().automation(id).map(|a| a.spec.name.clone()) else { return };
        let app = self.app.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app = app.clone();
            alert
                .title(format!("Delete “{name}”?"))
                .description("Its schedule and run history are removed. Threads its runs started stay.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    app.update(cx, |app, cx| app.run(Command::Automation(AutomationCommand::Delete { id }), window, cx));
                    true
                })
        });
    }

    fn on_saved(&self, cx: &Context<Self>) -> editor::OnSaved {
        let view = cx.weak_entity();
        Rc::new(move |id, _, cx| {
            let _ = view.update(cx, |this, cx| this.select(id, cx));
        })
    }

    /// The editor for a new automation, on the user's usual agent.
    pub fn open_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.blank_spec(cx);
        editor::open(self.app.clone(), None, spec, self.on_saved(cx), window, cx);
    }

    fn blank_spec(&self, cx: &App) -> AutomationSpec {
        let state = self.app.read(cx).state();
        AutomationSpec {
            project: self.default_project(cx),
            config: ThreadConfig { access: Access::AutoEdit, ..state.defaults.clone() },
            triggers: vec![Trigger::Rule { rule: "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0".into(), tz: String::new() }],
            ..Default::default()
        }
    }

    /// The selected automation's project, else the only or first project.
    fn default_project(&self, cx: &App) -> Option<ProjectId> {
        let state = self.app.read(cx).state();
        self.selected.and_then(|id| state.automation(id)).and_then(|a| a.spec.project).or_else(|| state.projects.first().map(|p| p.id))
    }

    pub fn open_recipe(&mut self, recipe: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recipe) = orbit_core::automations::recipes::find(recipe) else { return };
        let defaults = self.app.read(cx).state().defaults.clone();
        let spec = recipe.spec(self.default_project(cx), &defaults);
        editor::open(self.app.clone(), None, spec, self.on_saved(cx), window, cx);
    }

    /// Debug builds: opens the editor for screenshots (`ORBIT_DEBUG_ROUTE`
    /// = `automation-editor` edits the selection, `automation-new` starts
    /// one; `ORBIT_DEBUG_RECIPE` names a recipe to start from).
    pub fn debug_open_editor(&mut self, editing: bool, window: &mut Window, cx: &mut Context<Self>) {
        if editing && let Some(id) = self.selected.or_else(|| self.ordered(cx).first().copied()) {
            return self.edit(id, window, cx);
        }
        match std::env::var("ORBIT_DEBUG_RECIPE") {
            Ok(recipe) if cfg!(debug_assertions) => self.open_recipe(&recipe, window, cx),
            _ => self.open_new(window, cx),
        }
    }

    fn edit(&mut self, id: AutomationId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = self.app.read(cx).state().automation(id).map(|a| a.spec.clone()) else { return };
        editor::open(self.app.clone(), Some(id), spec, self.on_saved(cx), window, cx);
    }

    fn duplicate(&mut self, id: AutomationId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut spec) = self.app.read(cx).state().automation(id).map(|a| a.spec.clone()) else { return };
        spec.name = format!("{} (copy)", spec.name);
        editor::open(self.app.clone(), None, spec, self.on_saved(cx), window, cx);
    }

    fn open_thread(&self, thread: ThreadId, window: &mut Window, cx: &mut App) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let total = state.automations.len();
        let paused = state.automations.iter().filter(|a| a.paused).count();
        let running = state.automations.iter().filter(|a| matches!(last_run(state, a.id).map(|r| &r.status), Some(RunStatus::Running))).count();
        let mut counts = vec![format!("{} on", total - paused)];
        if paused > 0 {
            counts.push(format!("{paused} paused"));
        }
        if running > 0 {
            counts.push(format!("{running} running"));
        }
        h_flex()
            .flex_none()
            .h_11()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(Lucide::CalendarClock).small().text_color(theme.muted_foreground))
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Automations"))
            .when(total > 0, |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(counts.join(" · "))))
            .child(div().flex_1())
            .when(total > 0, |el| {
                el.child(
                    Button::new("automations-recipes")
                        .ghost()
                        .small()
                        .icon(Icon::new(IconName::LayoutDashboard))
                        .label("Recipes")
                        .selected(self.gallery)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.gallery = !this.gallery;
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("automations-new")
                    .small()
                    .icon(Icon::new(IconName::Plus))
                    .label("New automation…")
                    .on_click(cx.listener(|this, _, window, cx| this.open_new(window, cx))),
            )
    }

    fn render_rows(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let now = now_ms();
        let state = self.app.read(cx).state();
        let list = &state.automations;
        list[range.start.min(list.len())..range.end.min(list.len())]
            .iter()
            .map(|a| {
                let id = a.id;
                let selected = self.selected == Some(id) && !self.gallery;
                let line = status_line(a, state, now);
                div()
                    .px_1p5()
                    .py_0p5()
                    .child(
                        h_flex()
                            .id(("automation-row", id.0))
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .rounded(theme.radius)
                            .cursor_default()
                            .when(selected, |el| el.bg(theme.list_active))
                            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)))
                            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                            .child(run_glyph(("automation-glyph", id.0), last_run(state, id), a.paused, cx))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .when(selected, |el| el.font_weight(FontWeight::MEDIUM))
                                            .when(a.paused, |el| el.text_color(theme.muted_foreground))
                                            .child(a.spec.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(line),
                                    ),
                            )
                            .child(
                                Switch::new(("automation-on", id.0))
                                    .small()
                                    .checked(!a.paused)
                                    .tooltip(if a.paused { "Resume" } else { "Pause" })
                                    .on_click(cx.listener(move |this, on: &bool, window, cx| this.set_paused(id, !*on, window, cx))),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_detail(&self, id: AutomationId, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app.read(cx).state();
        let Some(a) = state.automation(id).cloned() else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let now = now_ms();
        let runs: Vec<AutomationRun> = state.runs(id).to_vec();
        let project = a.spec.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.name.clone());
        let agent = format!("{} · {}", ui::model_label(&a.spec.config, &state.providers), a.spec.config.access.label());
        let target = match a.spec.target {
            RunTarget::Worktree => "a new worktree each run".to_owned(),
            RunTarget::Project => "the project folder".to_owned(),
            RunTarget::PullRequest => "the pull request’s branch".to_owned(),
            RunTarget::Thread(t) => format!("“{}”", state.threads.get(&t).map(thread_title).unwrap_or_else(|| "a deleted thread".into())),
        };
        let mut meta = Vec::new();
        if let Some(project) = project {
            meta.push(project);
        }
        meta.push(agent);
        meta.push(format!("in {target}"));
        let paused = a.paused;
        let starting = self.starting.contains(&id) || !a.state.pending.is_empty();
        let view = cx.weak_entity();
        let (v_pause, v_dup, v_del) = (view.clone(), view.clone(), view.clone());
        let menu = Button::new("automation-more").ghost().small().icon(IconName::Ellipsis).tooltip("More").dropdown_menu(move |menu, _, _| {
            let (v_pause, v_dup, v_del) = (v_pause.clone(), v_dup.clone(), v_del.clone());
            menu.item(PopupMenuItem::new(if paused { "Resume" } else { "Pause" }).icon(if paused { IconName::Play } else { IconName::Pause }).on_click(
                move |_, window, cx| {
                    let _ = v_pause.update(cx, |this, cx| this.set_paused(id, !paused, window, cx));
                },
            ))
            .item(PopupMenuItem::new("Duplicate…").icon(IconName::Copy).on_click(move |_, window, cx| {
                let _ = v_dup.update(cx, |this, cx| this.duplicate(id, window, cx));
            }))
            .separator()
            .item(PopupMenuItem::new("Delete…").icon(Lucide::Trash).on_click(move |_, window, cx| {
                let _ = v_del.update(cx, |this, cx| this.confirm_delete(id, window, cx));
            }))
        });
        let header = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().truncate().text_lg().font_weight(FontWeight::SEMIBOLD).child(a.spec.name.clone()))
                    .child(
                        Button::new("automation-run")
                            .small()
                            .icon(IconName::Play)
                            .label("Run now")
                            .loading(starting)
                            .tooltip("Starts a run now, without its checks or daily limit")
                            .on_click(cx.listener(move |this, _, window, cx| this.run_now(id, window, cx))),
                    )
                    .child(
                        Button::new("automation-edit")
                            .small()
                            .outline()
                            .icon(Lucide::Pencil)
                            .label("Edit…")
                            .on_click(cx.listener(move |this, _, window, cx| this.edit(id, window, cx))),
                    )
                    .child(menu),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(meta.join(" · ")));
        let note = (!a.state.note.is_empty() || paused).then(|| {
            let text = if a.state.note.is_empty() { "Paused: no runs start until you resume it.".to_owned() } else { a.state.note.clone() };
            h_flex()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_sm()
                .child(Icon::new(IconName::Pause).small().text_color(theme.muted_foreground))
                .child(div().flex_1().child(text))
                .when(paused, |el| {
                    el.child(
                        Button::new("automation-resume")
                            .small()
                            .label("Resume")
                            .on_click(cx.listener(move |this, _, window, cx| this.set_paused(id, false, window, cx))),
                    )
                })
        });
        let triggers = v_flex().gap_1p5().children(a.spec.triggers.iter().map(|t| {
            let icon = if t.is_schedule() { Icon::new(Lucide::CalendarClock) } else { Icon::new(Lucide::Zap) };
            h_flex()
                .gap_2()
                .text_sm()
                .child(icon.small().text_color(theme.muted_foreground))
                .child(capitalize(&trigger_text(t, state)))
        }));
        let upcoming: Vec<u64> = if paused { Vec::new() } else { next_runs(&a, now, 5) };
        let schedule = v_flex()
            .gap_3()
            .child(triggers)
            .when(!upcoming.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(ui::section_label("Next runs", cx).px_0().pt_0())
                        .children(upcoming.iter().enumerate().map(|(i, t)| {
                            h_flex()
                                .gap_2()
                                .text_sm()
                                .when(i > 0, |el| el.text_color(theme.muted_foreground))
                                .child(div().w(rems(10.)).child(when_text(*t, now)))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(in_text(*t, now)))
                        })),
                )
            });
        let stats = history::stats(&runs, cx);
        let prompt = v_flex()
            .gap_1()
            .child(ui::section_label("Prompt", cx).px_0().pt_0())
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(a.spec.prompt.clone()),
            );
        v_flex()
            .id("automation-detail")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(48.))
                    .px_6()
                    .py_5()
                    .gap_6()
                    .child(header)
                    .children(note)
                    .child(schedule)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(h_flex().gap_3().child(ui::section_label("Runs", cx).px_0().pt_0()).child(stats))
                            .child(history::timeline(&runs, cx.weak_entity(), state, cx)),
                    )
                    .child(prompt),
            )
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let point = |icon: Icon, title: &'static str, text: &'static str| {
            h_flex()
                .w_full()
                .min_w_0()
                .gap_3()
                .items_start()
                .child(div().p_2().rounded(theme.radius).bg(theme.muted).child(icon.small().text_color(theme.muted_foreground)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(text)),
                )
        };
        v_flex()
            .id("automations-empty")
            .size_full()
            .overflow_y_scroll()
            .items_center()
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(60.))
                    .px_6()
                    .pt_10()
                    .pb_8()
                    .gap_8()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Put agents to work while you’re away"))
                            .child(div().max_w(rems(38.)).text_sm().text_color(theme.muted_foreground).child(
                                "An automation starts an agent for you: every weekday at 9:00, or when checks fail on your pull \
                                 request. Each run is a thread you can open, review and continue.",
                            ))
                            .child(
                                h_flex().pt_2().child(
                                    Button::new("automations-empty-new")
                                        .primary()
                                        .icon(Icon::new(IconName::Plus))
                                        .label("New automation…")
                                        .on_click(cx.listener(|this, _, window, cx| this.open_new(window, cx))),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap_6()
                            .child(point(
                                Icon::new(Lucide::CalendarClock),
                                "On a schedule",
                                "In plain words, cron or an interval, in any time zone. Runs missed while asleep catch up.",
                            ))
                            .child(point(
                                Icon::new(Lucide::Zap),
                                "When something happens",
                                "Failing checks, a review request, a new issue, a push, or another automation finishing.",
                            ))
                            .child(point(
                                Icon::new(Lucide::ShieldCheck),
                                "On your terms",
                                "Checks before a run, daily limits and time limits. Pull requests only when you ask.",
                            )),
                    )
                    .child(gallery::render(cx.weak_entity(), 3, cx)),
            )
            .into_any_element()
    }
}

impl Render for AutomationsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let columns = if window.viewport_size().width > px(1100.) { 2 } else { 1 };
        let count = self.app.read(cx).state().automations.len();
        let body = if count == 0 {
            self.render_empty(cx)
        } else {
            let list = uniform_list("automation-rows", count, cx.processor(Self::render_rows)).track_scroll(&self.scroll).flex_1().py_1();
            let detail = match (self.gallery, self.selected) {
                (false, Some(id)) => self.render_detail(id, cx),
                _ => v_flex()
                    .id("automations-gallery")
                    .size_full()
                    .overflow_y_scroll()
                    .child(div().px_6().py_5().child(gallery::render(cx.weak_entity(), columns, cx)))
                    .into_any_element(),
            };
            div()
                .flex_1()
                .min_h_0()
                .child(
                    h_resizable("automations")
                        .with_state(&self.split)
                        .child(
                            resizable_panel()
                                .size(px(340.))
                                .size_range(px(260.)..px(560.))
                                .child(v_flex().size_full().border_r_1().border_color(theme.border).child(list)),
                        )
                        .child(resizable_panel().child(div().size_full().min_w_0().child(detail))),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .key_context("Automations")
            .track_focus(&self.focus)
            .bg(theme.background)
            .on_action(cx.listener(|this, _: &SelectNextAutomation, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPreviousAutomation, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &EditAutomation, window, cx| {
                if let Some(id) = this.selected {
                    this.edit(id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewAutomation, window, cx| this.open_new(window, cx)))
            .child(self.render_toolbar(cx))
            .child(body)
    }
}

// ── Words ────────────────────────────────────────────────────────────────

pub fn thread_title(thread: &Thread) -> String {
    if thread.title.trim().is_empty() { "Untitled thread".into() } else { thread.title.clone() }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A trigger in words: `weekdays 9:00 (Asia/Tokyo)`, `when my review is
/// requested`, `after “Nightly deps” succeeds`.
pub fn trigger_text(trigger: &Trigger, state: &ClientState) -> String {
    match trigger {
        Trigger::Event(EventTrigger::Finished { source, when }) => {
            let name = match source {
                ChainSource::Automation(id) => state.automation(*id).map(|a| format!("“{}”", a.spec.name)).unwrap_or_else(|| "a deleted automation".into()),
                ChainSource::Thread(id) => state.threads.get(id).map(|t| format!("thread “{}”", thread_title(t))).unwrap_or_else(|| "a deleted thread".into()),
            };
            let verb = match when {
                ChainWhen::Succeeded => "succeeds",
                ChainWhen::Failed => "fails",
                ChainWhen::Either => "finishes",
            };
            format!("after {name} {verb}")
        }
        Trigger::Cron { tz, .. } | Trigger::Rule { tz, .. } if !tz.is_empty() => {
            format!("{} ({tz})", orbit_core::automations::describe(trigger))
        }
        other => orbit_core::automations::describe(other),
    }
}

/// A future time as a day and a time: `Today 09:00`, `Tomorrow 09:00`,
/// `Thu 26 Nov 09:00`.
pub fn when_text(ms: u64, now: u64) -> String {
    use chrono::TimeZone as _;
    let time = format::timestamp(ms, ms);
    match format::local_day(ms) - format::local_day(now) {
        0 => format!("Today {time}"),
        1 => format!("Tomorrow {time}"),
        _ => {
            let day = chrono::Local.timestamp_millis_opt(ms as i64).single().map(|d| d.format("%a %-d %b").to_string());
            format!("{} {time}", day.unwrap_or_default())
        }
    }
}

/// How far ahead, short: `in 12 min`, `in 10h`, `in 3d`.
pub fn in_text(ms: u64, now: u64) -> String {
    let mins = ms.saturating_sub(now) / 60_000;
    match mins {
        0 => "now".into(),
        1..=59 => format!("in {mins} min"),
        60..=1_439 => format!("in {}h", (mins + 30) / 60),
        _ => format!("in {}d", (mins + 720) / 1_440),
    }
}

/// The next runs of every schedule trigger, merged.
fn next_runs(a: &Automation, now: u64, count: usize) -> Vec<u64> {
    let mut times: Vec<u64> = Vec::new();
    for (i, trigger) in a.spec.triggers.iter().enumerate() {
        let Ok(Some(schedule)) = Schedule::of(trigger) else { continue };
        match a.state.next_ms.get(i).copied().filter(|t| *t > 0) {
            Some(first) => {
                times.push(first);
                times.extend(schedule.upcoming(first, count.saturating_sub(1)));
            }
            None => times.extend(schedule.upcoming(now, count)),
        }
    }
    times.sort_unstable();
    times.dedup();
    times.truncate(count);
    times
}

fn last_run(state: &ClientState, id: AutomationId) -> Option<&AutomationRun> {
    state.runs(id).first()
}

/// The list row's second line: what it waits for, or what it is doing.
fn status_line(a: &Automation, state: &ClientState, now: u64) -> String {
    let when: Vec<String> = a.spec.triggers.iter().take(1).map(|t| trigger_text(t, state)).collect();
    let when = when.join("");
    if a.paused {
        return if a.state.note.is_empty() { format!("Paused · {when}") } else { a.state.note.clone() };
    }
    if let Some(run) = last_run(state, a.id)
        && run.status == RunStatus::Running
    {
        return format!("Running for {}", format::duration(now.saturating_sub(run.started_ms)));
    }
    if !a.state.pending.is_empty() {
        return "Starting…".into();
    }
    match a.next_run_ms() {
        Some(next) => format!("{} · {when}", in_text(next, now)),
        None => capitalize(&when),
    }
}

/// The last run's outcome as a glyph: running, succeeded, failed, skipped,
/// never ran.
fn run_glyph(id: impl Into<ElementId>, run: Option<&AutomationRun>, paused: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (icon, color, tip): (Icon, Hsla, &str) = match run.map(|r| &r.status) {
        Some(RunStatus::Running) => return ui::StatusGlyph::glyph(id, ui::Glyph::Working).into_any_element(),
        Some(RunStatus::Succeeded) => (Icon::new(IconName::CircleCheck), theme.success, "Last run succeeded"),
        Some(RunStatus::Failed(_)) | Some(RunStatus::TimedOut) => (Icon::new(IconName::CircleX), theme.danger, "Last run failed"),
        Some(RunStatus::Skipped(_)) | Some(RunStatus::Cancelled) => (Icon::new(Lucide::CircleMinus), theme.muted_foreground, "Last run was skipped"),
        None if paused => (Icon::new(IconName::Pause), theme.muted_foreground, "Paused"),
        None => (Icon::new(Lucide::CircleDashed), theme.muted_foreground, "Never ran"),
    };
    div()
        .id(id)
        .flex_none()
        .size_4()
        .flex()
        .items_center()
        .justify_center()
        .child(icon.small().text_color(color))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
        .into_any_element()
}
