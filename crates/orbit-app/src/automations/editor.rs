//! The automation editor: a sheet with what the agent does, when, where,
//! on which agent, the checks before a run, its limits, and what happens
//! after. The schedule builder speaks plainly ("Every weekday at 09:00")
//! and previews the next five runs; cron and recurrence rules are there for
//! experts and are checked as they are typed.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_core::automations::{schedule, template};
use orbit_proto::*;

use super::builder::{Builder, DAY_LETTERS, DAY_NAMES, Repeat};
use crate::app_state::AppState;
use crate::assets::Lucide;

/// Called with the saved automation's id.
pub type OnSaved = Rc<dyn Fn(AutomationId, &mut Window, &mut App)>;

/// Where runs work, as the editor offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    Worktree,
    Project,
    PullRequest,
    Thread,
}

impl Where {
    const ALL: [Where; 4] = [Where::Worktree, Where::Project, Where::PullRequest, Where::Thread];

    fn label(self) -> &'static str {
        match self {
            Where::Worktree => "New worktree",
            Where::Project => "Project folder",
            Where::PullRequest => "Pull request branch",
            Where::Thread => "One thread",
        }
    }
}

pub struct AutomationEditor {
    app: Entity<AppState>,
    editing: Option<AutomationId>,
    recipe: String,
    name: Entity<InputState>,
    prompt: Entity<TextareaState>,
    project: Option<ProjectId>,
    builder: Builder,
    time: Entity<InputState>,
    minute: Entity<InputState>,
    month_day: Entity<InputState>,
    every: Entity<InputState>,
    cron: Entity<InputState>,
    rule: Entity<InputState>,
    tz: Entity<InputState>,
    /// Schedules past the first, and thread chains, made elsewhere (the
    /// CLI): kept as they are.
    kept: Vec<Trigger>,
    checks_mine: bool,
    checks_any: bool,
    review: bool,
    issue: bool,
    label: Entity<InputState>,
    push: bool,
    branch: Entity<InputState>,
    watch: bool,
    pr: Entity<InputState>,
    after: Option<AutomationId>,
    after_when: ChainWhen,
    place: Where,
    thread: Option<ThreadId>,
    config: ThreadConfig,
    only_changed: bool,
    only_clean: bool,
    skip_running: bool,
    command: Entity<InputState>,
    max_day: Entity<InputState>,
    timeout: Entity<InputState>,
    grace: Entity<InputState>,
    notify: Notify,
    settle: bool,
    open_pr: bool,
    preview: Result<Option<(Trigger, Vec<u64>, String)>, String>,
    error: Option<String>,
    saving: bool,
    on_saved: OnSaved,
    _subscriptions: Vec<Subscription>,
}

impl AutomationEditor {
    pub fn new(
        app: Entity<AppState>,
        editing: Option<AutomationId>,
        spec: AutomationSpec,
        on_saved: OnSaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |value: String, placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let schedules: Vec<&Trigger> = spec.triggers.iter().filter(|t| t.is_schedule()).collect();
        let builder = Builder::of(schedules.first().copied());
        let mut kept: Vec<Trigger> = schedules.iter().skip(1).map(|t| (*t).clone()).collect();
        let (mut checks_mine, mut checks_any, mut review, mut issue, mut push, mut watch) = (false, false, false, false, false, false);
        let (mut label, mut branch, mut pr) = (String::new(), String::new(), String::new());
        let (mut after, mut after_when) = (None, ChainWhen::Succeeded);
        for trigger in &spec.triggers {
            let Trigger::Event(event) = trigger else { continue };
            match event {
                EventTrigger::ChecksFailed { mine: true } => checks_mine = true,
                EventTrigger::ChecksFailed { mine: false } => checks_any = true,
                EventTrigger::ReviewRequested => review = true,
                EventTrigger::IssueLabeled { label: l } => {
                    issue = true;
                    label = l.clone();
                }
                EventTrigger::BranchPushed { branch: b } => {
                    push = true;
                    branch = b.clone();
                }
                EventTrigger::WatchPr { number } => {
                    watch = true;
                    pr = if *number == 0 { String::new() } else { number.to_string() };
                }
                EventTrigger::Finished { source: ChainSource::Automation(id), when } => {
                    after = Some(*id);
                    after_when = *when;
                }
                EventTrigger::Finished { .. } => kept.push(trigger.clone()),
            }
        }
        let (place, thread) = match spec.target {
            RunTarget::Worktree => (Where::Worktree, None),
            RunTarget::Project => (Where::Project, None),
            RunTarget::PullRequest => (Where::PullRequest, None),
            RunTarget::Thread(t) => (Where::Thread, Some(t)),
        };
        let name = input(spec.name.clone(), "Nightly dependency updates".into(), window, cx);
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .placeholder("What should the agent do each run? Use {date}, {pr} or {last_run_summary} to fill in details.")
                .default_value(spec.prompt.clone())
        });
        let time = input(builder.time.clone(), "09:00".into(), window, cx);
        let minute = input(builder.minute.clone(), "0".into(), window, cx);
        let month_day = input(builder.month_day.clone(), "1".into(), window, cx);
        let every = input(builder.every.clone(), "30".into(), window, cx);
        let cron = input(builder.cron.clone(), "0 9 * * 1-5".into(), window, cx);
        let rule = input(builder.rule.clone(), "FREQ=WEEKLY;BYDAY=MO;BYHOUR=9".into(), window, cx);
        let tz = input(builder.tz.clone(), format!("This computer’s ({})", schedule::system_zone_name()).into(), window, cx);
        let max_day = input(if spec.limits.max_runs_per_day == 0 { String::new() } else { spec.limits.max_runs_per_day.to_string() }, "No limit".into(), window, cx);
        let timeout = input(if spec.limits.timeout_minutes == 0 { String::new() } else { spec.limits.timeout_minutes.to_string() }, "Never".into(), window, cx);
        let grace = input(spec.limits.grace_minutes.to_string(), "0".into(), window, cx);
        let schedule_inputs = [&time, &minute, &month_day, &every, &cron, &rule, &tz];
        let mut subscriptions: Vec<Subscription> = schedule_inputs
            .iter()
            .map(|state| {
                cx.subscribe_in(*state, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.refresh_preview(cx);
                    }
                })
            })
            .collect();
        // The error under the footer is about the last attempt; typing
        // starts over.
        subscriptions.push(cx.subscribe_in(&name, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) && this.error.take().is_some() {
                cx.notify();
            }
        }));
        let mut this = Self {
            app,
            editing,
            recipe: spec.recipe.clone(),
            name,
            prompt,
            project: spec.project,
            builder,
            time,
            minute,
            month_day,
            every,
            cron,
            rule,
            tz,
            kept,
            checks_mine,
            checks_any,
            review,
            issue,
            label: input(label, "needs-triage".into(), window, cx),
            push,
            branch: input(branch, "main".into(), window, cx),
            watch,
            pr: input(pr, "42".into(), window, cx),
            after,
            after_when,
            place,
            thread,
            config: spec.config.clone(),
            only_changed: spec.checks.changes_since_last_run,
            only_clean: spec.checks.clean_repo,
            skip_running: spec.checks.skip_if_running,
            command: input(spec.checks.command.clone(), "Optional, such as cargo check".into(), window, cx),
            max_day,
            timeout,
            grace,
            notify: spec.on_finish.notify,
            settle: spec.on_finish.settle_if_unchanged,
            open_pr: spec.on_finish.open_pr,
            preview: Ok(None),
            error: None,
            saving: false,
            on_saved,
            _subscriptions: subscriptions,
        };
        this.refresh_preview(cx);
        this
    }

    pub fn is_new(&self) -> bool {
        self.editing.is_none()
    }

    fn read_builder(&mut self, cx: &App) {
        let value = |s: &Entity<InputState>| s.read(cx).value().to_string();
        self.builder.time = value(&self.time);
        self.builder.minute = value(&self.minute);
        self.builder.month_day = value(&self.month_day);
        self.builder.every = value(&self.every);
        self.builder.cron = value(&self.cron);
        self.builder.rule = value(&self.rule);
        self.builder.tz = value(&self.tz);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.read_builder(cx);
        self.preview = self.builder.preview(now_ms(), 5);
        cx.notify();
    }

    fn set_repeat(&mut self, repeat: Repeat, cx: &mut Context<Self>) {
        self.builder.repeat = repeat;
        self.refresh_preview(cx);
    }

    fn toggle_day(&mut self, day: usize, cx: &mut Context<Self>) {
        self.builder.days ^= 1 << day;
        self.refresh_preview(cx);
    }

    /// The spec the form describes, or what to fix.
    fn spec(&mut self, cx: &App) -> Result<AutomationSpec, String> {
        self.read_builder(cx);
        let value = |s: &Entity<InputState>| s.read(cx).value().trim().to_owned();
        let mut triggers = Vec::new();
        if let Some(trigger) = self.builder.trigger()? {
            triggers.push(trigger);
        }
        if self.checks_mine {
            triggers.push(Trigger::Event(EventTrigger::ChecksFailed { mine: true }));
        }
        if self.checks_any {
            triggers.push(Trigger::Event(EventTrigger::ChecksFailed { mine: false }));
        }
        if self.review {
            triggers.push(Trigger::Event(EventTrigger::ReviewRequested));
        }
        if self.issue {
            triggers.push(Trigger::Event(EventTrigger::IssueLabeled { label: value(&self.label) }));
        }
        if self.push {
            triggers.push(Trigger::Event(EventTrigger::BranchPushed { branch: value(&self.branch) }));
        }
        if self.watch {
            let number = value(&self.pr).trim_start_matches('#').parse::<u64>().map_err(|_| "Give the pull request’s number to watch.")?;
            triggers.push(Trigger::Event(EventTrigger::WatchPr { number }));
        }
        if let Some(id) = self.after {
            triggers.push(Trigger::Event(EventTrigger::Finished { source: ChainSource::Automation(id), when: self.after_when }));
        }
        triggers.extend(self.kept.iter().cloned());
        let minutes = |s: &Entity<InputState>, what: &str| -> Result<u32, String> {
            let text = value(s);
            if text.is_empty() {
                return Ok(0);
            }
            text.parse::<u32>().map_err(|_| format!("{what} is a whole number of minutes."))
        };
        let target = match self.place {
            Where::Worktree => RunTarget::Worktree,
            Where::Project => RunTarget::Project,
            Where::PullRequest => RunTarget::PullRequest,
            Where::Thread => RunTarget::Thread(self.thread.ok_or("Pick the thread runs continue.")?),
        };
        let name = value(&self.name);
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        Ok(AutomationSpec {
            name: if name.is_empty() { prompt.lines().next().unwrap_or_default().chars().take(48).collect() } else { name },
            project: self.project,
            target,
            config: self.config.clone(),
            prompt,
            triggers,
            checks: PreChecks {
                changes_since_last_run: self.only_changed,
                clean_repo: self.only_clean,
                command: value(&self.command),
                skip_if_running: self.skip_running,
            },
            limits: Limits {
                max_runs_per_day: value(&self.max_day).parse().unwrap_or(0),
                timeout_minutes: minutes(&self.timeout, "The time limit")?,
                grace_minutes: minutes(&self.grace, "The catch-up window")?,
            },
            on_finish: OnFinish { notify: self.notify, settle_if_unchanged: self.settle, open_pr: self.open_pr },
            recipe: self.recipe.clone(),
        })
    }

    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let spec = match self.spec(cx) {
            Ok(spec) => spec,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.error = None;
        cx.notify();
        let command = Command::Automation(AutomationCommand::Save { id: self.editing, spec: Box::new(spec) });
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(Reply::Automation(AutomationReply::Saved(id))) => {
                        window.close_sheet(cx);
                        (this.on_saved)(id, window, cx);
                    }
                    Ok(_) => this.error = Some("Orbit didn’t confirm the save.".into()),
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn insert_variable(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("{{{name}}}");
        self.prompt.update(cx, |prompt, cx| {
            prompt.focus(window, cx);
            prompt.insert(text, window, cx);
        });
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_what(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let theme = cx.theme().clone();
        let projects: Vec<(ProjectId, String)> = state.projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let current = self.project;
        let project_name = current
            .and_then(|id| projects.iter().find(|(p, _)| *p == id))
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "No project".into());
        let view = cx.weak_entity();
        let picker = Button::new("automation-project")
            .outline()
            .small()
            .icon(Icon::new(IconName::Folder))
            .label(project_name)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, name) in &projects {
                    let (view, id) = (view.clone(), *id);
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(current == Some(id)).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.project = Some(id);
                            cx.notify();
                        });
                    }));
                }
                let none = view.clone();
                menu.separator().item(PopupMenuItem::new("No project").checked(current.is_none()).on_click(move |_, _, cx| {
                    let _ = none.update(cx, |this, cx| {
                        this.project = None;
                        cx.notify();
                    });
                }))
            });
        let chips = h_flex().gap_1().flex_wrap().children(template::VARIABLES.iter().map(|(name, help)| {
            let name: &'static str = name;
            Button::new(SharedString::from(format!("var-{name}")))
                .ghost()
                .xsmall()
                .label(format!("{{{name}}}"))
                .tooltip(*help)
                .on_click(cx.listener(move |this, _, window, cx| this.insert_variable(name, window, cx)))
        }));
        v_flex()
            .gap_3()
            .child(field("Name", None, Input::new(&self.name).small(), cx))
            .child(field("Project", None, h_flex().child(picker), cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Prompt", cx))
                    .child(Textarea::new(&self.prompt))
                    .child(h_flex().gap_1().items_start().child(div().pt_0p5().text_xs().text_color(theme.muted_foreground).child("Insert")).child(chips)),
            )
    }

    fn render_when(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let repeat = self.builder.repeat;
        let view = cx.weak_entity();
        let repeat_button = Button::new("automation-repeat")
            .outline()
            .small()
            .icon(Icon::new(Lucide::CalendarClock))
            .label(repeat.label())
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (i, r) in Repeat::MENU.into_iter().enumerate() {
                    if i == 6 || i == 8 {
                        menu = menu.separator();
                    }
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(r.label()).checked(r == repeat).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.set_repeat(r, cx));
                    }));
                }
                menu
            });
        let small_input = |state: &Entity<InputState>, width: f32| div().w(rems(width)).child(Input::new(state).small());
        let mut row = h_flex().gap_2().items_center().child(repeat_button);
        match repeat {
            Repeat::Hourly => {
                row = row.child(muted("at", cx)).child(small_input(&self.minute, 3.5)).child(muted("minutes past", cx));
            }
            Repeat::Daily | Repeat::Weekdays => row = row.child(muted("at", cx)).child(small_input(&self.time, 4.5)),
            Repeat::Weekly => {
                let days = self.builder.days;
                row = row
                    .child(h_flex().gap_0p5().children((0..7).map(|d| {
                        let button = Button::new(("automation-day", d as u64)).small();
                        let button = if days & (1 << d) != 0 { button.primary() } else { button.outline() };
                        button
                            .label(DAY_LETTERS[d])
                            .tooltip(DAY_NAMES[d])
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_day(d, cx)))
                    })))
                    .child(muted("at", cx))
                    .child(small_input(&self.time, 4.5));
            }
            Repeat::Monthly => {
                row = row
                    .child(muted("on day", cx))
                    .child(small_input(&self.month_day, 3.5))
                    .child(muted("at", cx))
                    .child(small_input(&self.time, 4.5));
            }
            Repeat::Interval => {
                let hours = self.builder.unit_hours;
                row = row.child(muted("every", cx)).child(small_input(&self.every, 3.5)).child(
                    TabBar::new("automation-unit")
                        .segmented()
                        .xsmall()
                        .selected_index(if hours { 1 } else { 0 })
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            this.builder.unit_hours = *ix == 1;
                            this.refresh_preview(cx);
                        }))
                        .child(Tab::new().label("minutes"))
                        .child(Tab::new().label("hours")),
                );
            }
            Repeat::Cron | Repeat::Rule | Repeat::Never => {}
        }
        let expert = match repeat {
            Repeat::Cron => Some(field(
                "Cron expression",
                Some("Minute, hour, day of month, month, day of week. Names (mon, jan), ranges (1-5) and steps (*/15) work."),
                Input::new(&self.cron).small(),
                cx,
            )),
            Repeat::Rule => Some(field(
                "Recurrence rule",
                Some("FREQ (HOURLY, DAILY, WEEKLY, MONTHLY, YEARLY), INTERVAL, BYDAY (MO, 1MO, -1FR), BYHOUR, BYMINUTE, BYMONTHDAY, DTSTART, UNTIL."),
                Input::new(&self.rule).small(),
                cx,
            )),
            _ => None,
        };
        let has_schedule = repeat != Repeat::Never;
        let preview = match &self.preview {
            Err(message) => h_flex()
                .gap_1p5()
                .text_xs()
                .text_color(theme.danger)
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(message.clone())
                .into_any_element(),
            Ok(Some((_, times, description))) => {
                let now = now_ms();
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(theme.success)
                            .child(Icon::new(IconName::CircleCheck).xsmall())
                            .child(format!("Runs {description}")),
                    )
                    .child(
                        v_flex().pl_5().gap_0p5().text_xs().text_color(theme.muted_foreground).children(
                            times.iter().map(|t| div().child(super::when_text(*t, now))),
                        ),
                    )
                    .into_any_element()
            }
            Ok(None) => div().text_xs().text_color(theme.muted_foreground).child("Only the events below start runs.").into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(row)
            .children(expert)
            .when(has_schedule && repeat != Repeat::Interval, |el| {
                el.child(field("Time zone", None, small_input(&self.tz, 16.), cx))
            })
            .child(preview)
            .when(!self.kept.is_empty(), |el| {
                let state = self.app.read(cx).state();
                el.child(v_flex().gap_1().children(self.kept.iter().enumerate().map(|(i, t)| {
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(Lucide::CalendarClock).xsmall())
                        .child(format!("Also {}", super::trigger_text(t, state)))
                        .child(
                            Button::new(("automation-kept-remove", i as u64))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.kept.remove(i);
                                    cx.notify();
                                })),
                        )
                })))
            })
            .child(self.render_events(cx))
    }

    fn render_events(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let others: Vec<(AutomationId, String)> = state
            .automations
            .iter()
            .filter(|a| Some(a.id) != self.editing)
            .map(|a| (a.id, a.spec.name.clone()))
            .collect();
        let check = |id: &'static str, text: &'static str, on: bool, set: fn(&mut Self, bool), cx: &mut Context<Self>| {
            Checkbox::new(id).label(text).checked(on).on_click(cx.listener(move |this, checked: &bool, _, cx| {
                set(this, *checked);
                cx.notify();
            }))
        };
        let with = |checkbox: Checkbox, extra: Option<AnyElement>| h_flex().gap_2().min_h_7().child(checkbox).children(extra);
        let input = |state: &Entity<InputState>, width: f32| div().w(rems(width)).child(Input::new(state).small()).into_any_element();
        let after_name = self
            .after
            .and_then(|id| others.iter().find(|(a, _)| *a == id))
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "Pick an automation".into());
        let current_after = self.after;
        let view = cx.weak_entity();
        let after_picker = Button::new("automation-after")
            .outline()
            .small()
            .label(after_name)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, name) in &others {
                    let (view, id) = (view.clone(), *id);
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(current_after == Some(id)).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.after = Some(id);
                            cx.notify();
                        });
                    }));
                }
                let none = view.clone();
                menu.separator().item(PopupMenuItem::new("None").checked(current_after.is_none()).on_click(move |_, _, cx| {
                    let _ = none.update(cx, |this, cx| {
                        this.after = None;
                        cx.notify();
                    });
                }))
            });
        let when_ix = match self.after_when {
            ChainWhen::Succeeded => 0,
            ChainWhen::Failed => 1,
            ChainWhen::Either => 2,
        };
        v_flex()
            .gap_1()
            .child(div().pt_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child("Or when"))
            .child(with(check("event-checks-mine", "Checks fail on my pull requests", self.checks_mine, |t, v| t.checks_mine = v, cx), None))
            .child(with(check("event-checks-any", "Checks fail on any pull request", self.checks_any, |t, v| t.checks_any = v, cx), None))
            .child(with(check("event-review", "Someone asks for my review", self.review, |t, v| t.review = v, cx), None))
            .child(with(
                check("event-issue", "A new issue is labeled", self.issue, |t, v| t.issue = v, cx),
                self.issue.then(|| input(&self.label, 10.)),
            ))
            .child(with(
                check("event-push", "A branch is pushed", self.push, |t, v| t.push = v, cx),
                self.push.then(|| input(&self.branch, 10.)),
            ))
            .child(with(
                check("event-watch", "Babysit pull request until it merges", self.watch, |t, v| t.watch = v, cx),
                self.watch.then(|| input(&self.pr, 5.)),
            ))
            .child(with(
                Checkbox::new("event-after").label("Another automation finishes").checked(self.after.is_some()).on_click(cx.listener(
                    |this, checked: &bool, _, cx| {
                        if !*checked {
                            this.after = None;
                        } else if this.after.is_none() {
                            let state = this.app.read(cx).state();
                            this.after = state.automations.iter().map(|a| a.id).find(|id| Some(*id) != this.editing);
                        }
                        cx.notify();
                    },
                )),
                self.after.is_some().then(|| {
                    h_flex()
                        .gap_2()
                        .child(after_picker)
                        .child(
                            TabBar::new("automation-after-when")
                                .segmented()
                                .xsmall()
                                .selected_index(when_ix)
                                .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                    this.after_when = [ChainWhen::Succeeded, ChainWhen::Failed, ChainWhen::Either][*ix];
                                    cx.notify();
                                }))
                                .child(Tab::new().label("succeeds"))
                                .child(Tab::new().label("fails"))
                                .child(Tab::new().label("either")),
                        )
                        .into_any_element()
                }),
            ))
    }

    fn render_where(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let theme = cx.theme().clone();
        let index = Where::ALL.iter().position(|w| *w == self.place).unwrap_or(0);
        let help = match self.place {
            Where::Worktree => "Each run gets its own branch and folder, so runs never touch your checkout.",
            Where::Project => "Runs work in the project folder itself, beside your own changes.",
            Where::PullRequest => "Runs started by a pull request event work on its branch; others get a new worktree.",
            Where::Thread => "Every run is a new message in the same thread, so the agent remembers earlier runs.",
        };
        let threads: Vec<(ThreadId, String)> = {
            let mut list: Vec<&Thread> = state.threads.values().filter(|t| !t.archived && (self.project.is_none() || t.project == self.project)).collect();
            list.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
            list.into_iter().take(30).map(|t| (t.id, super::thread_title(t))).collect()
        };
        let current = self.thread;
        let thread_name = current
            .and_then(|id| state.threads.get(&id))
            .map(super::thread_title)
            .unwrap_or_else(|| "Pick a thread".into());
        let view = cx.weak_entity();
        v_flex()
            .gap_2()
            .child(
                TabBar::new("automation-where")
                    .segmented()
                    .small()
                    .selected_index(index)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.place = Where::ALL[*ix];
                        cx.notify();
                    }))
                    .children(Where::ALL.iter().map(|w| Tab::new().label(w.label()))),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(help))
            .when(self.place == Where::Thread, |el| {
                el.child(
                    h_flex().child(
                        Button::new("automation-thread").outline().small().label(thread_name).dropdown_caret(true).dropdown_menu(
                            move |mut menu, _, _| {
                                for (id, title) in &threads {
                                    let (view, id) = (view.clone(), *id);
                                    menu = menu.item(PopupMenuItem::new(title.clone()).checked(current == Some(id)).on_click(
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.thread = Some(id);
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                menu
                            },
                        ),
                    ),
                )
            })
    }

    fn render_agent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let providers: Vec<(String, String, ProviderKind)> =
            state.providers.iter().filter(|p| p.installed).map(|p| (p.instance_id().to_owned(), p.display_name().to_owned(), p.kind)).collect();
        let provider = state.provider_for(&self.config).cloned();
        let provider_name = provider.as_ref().map(|p| p.display_name().to_owned()).unwrap_or_else(|| self.config.provider.label().to_owned());
        let models: Vec<(String, String)> = provider.as_ref().map(|p| p.models.iter().map(|m| (m.slug.clone(), m.name.clone())).collect()).unwrap_or_default();
        let model_name = crate::ui::model_label(&self.config, &state.providers);
        let effort = provider
            .as_ref()
            .and_then(|p| p.models.iter().find(|m| m.slug == self.config.model).or_else(|| p.models.iter().find(|m| m.is_default)))
            .and_then(|m| m.options.iter().find(|o| o.id == "effort").cloned());
        let current_effort = self.config.options.iter().find(|o| o.id == "effort").and_then(|o| match &o.value {
            OptionSetting::Choice(c) => Some(c.clone()),
            OptionSetting::Toggle(_) => None,
        });
        let config = self.config.clone();
        let view = cx.weak_entity();
        let v1 = view.clone();
        let agent = Button::new("automation-agent").outline().small().label(provider_name).dropdown_caret(true).dropdown_menu(
            move |mut menu, _, _| {
                for (id, name, kind) in &providers {
                    let (view, id, kind) = (v1.clone(), id.clone(), *kind);
                    let on = config.instance_id() == id;
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(on).on_click(move |_, _, cx| {
                        let id = id.clone();
                        let _ = view.update(cx, |this, cx| {
                            let builtin = id == kind.id();
                            this.config = ThreadConfig {
                                provider: kind,
                                instance: if builtin { String::new() } else { id },
                                mode: this.config.mode,
                                access: this.config.access,
                                ..Default::default()
                            };
                            cx.notify();
                        });
                    }));
                }
                menu
            },
        );
        let current_model = self.config.model.clone();
        let v2 = view.clone();
        let model = Button::new("automation-model").outline().small().label(model_name).dropdown_caret(true).dropdown_menu(
            move |mut menu, _, _| {
                for (slug, name) in &models {
                    let (view, slug) = (v2.clone(), slug.clone());
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(*slug == current_model).on_click(move |_, _, cx| {
                        let slug = slug.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.config.model = slug;
                            this.config.options.clear();
                            cx.notify();
                        });
                    }));
                }
                menu
            },
        );
        let effort_button = effort.and_then(|option| {
            let OptionKind::Select { choices } = option.kind else { return None };
            let label = current_effort
                .as_ref()
                .and_then(|c| choices.iter().find(|x| x.id == *c))
                .map(|c| c.label.clone())
                .unwrap_or_else(|| "Default effort".into());
            let v3 = view.clone();
            let current = current_effort.clone();
            Some(Button::new("automation-effort").outline().small().label(label).dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for choice in &choices {
                        let (view, id) = (v3.clone(), choice.id.clone());
                        menu = menu.item(PopupMenuItem::new(choice.label.clone()).checked(current.as_deref() == Some(&choice.id)).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                let _ = view.update(cx, |this, cx| {
                                    this.config.options.retain(|o| o.id != "effort");
                                    this.config.options.push(OptionValue { id: "effort".into(), value: OptionSetting::Choice(id) });
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                },
            ))
        });
        let access = self.config.access;
        let v4 = view.clone();
        let access_button = Button::new("automation-access").outline().small().label(access.label()).dropdown_caret(true).dropdown_menu(
            move |mut menu, _, _| {
                for a in Access::ALL {
                    let view = v4.clone();
                    menu = menu.item(PopupMenuItem::new(a.label()).checked(a == access).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.config.access = a;
                            cx.notify();
                        });
                    }));
                }
                menu
            },
        );
        let waits = self.config.access == Access::Ask && self.config.mode == Mode::Build;
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(agent)
                    .child(model)
                    .children(effort_button)
                    .child(
                        TabBar::new("automation-mode")
                            .segmented()
                            .small()
                            .selected_index(if self.config.mode == Mode::Plan { 1 } else { 0 })
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.config.mode = if *ix == 1 { Mode::Plan } else { Mode::Build };
                                cx.notify();
                            }))
                            .child(Tab::new().label("Build"))
                            .child(Tab::new().label("Plan")),
                    )
                    .child(access_button),
            )
            .when(waits, |el| {
                el.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(IconName::TriangleAlert).xsmall())
                        .child("With “Ask first”, a run stops at its first edit until you approve it."),
                )
            })
    }

    fn render_checks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let check = |id: &'static str, text: &'static str, on: bool, set: fn(&mut Self, bool), cx: &mut Context<Self>| {
            Checkbox::new(id).label(text).checked(on).on_click(cx.listener(move |this, checked: &bool, _, cx| {
                set(this, *checked);
                cx.notify();
            }))
        };
        let number = |state: &Entity<InputState>| div().w(rems(4.5)).child(Input::new(state).small());
        v_flex()
            .gap_1()
            .child(check("check-changed", "Only when the project changed since the last run", self.only_changed, |t, v| t.only_changed = v, cx))
            .child(check("check-clean", "Only when the project has no uncommitted changes", self.only_clean, |t, v| t.only_clean = v, cx))
            .child(check("check-running", "Skip a run while the previous one is still going", self.skip_running, |t, v| t.skip_running = v, cx))
            .child(
                v_flex()
                    .pt_2()
                    .gap_1()
                    .child(label("Run this check first", cx))
                    .child(Input::new(&self.command).small())
                    .child(help("A shell command in the project folder. The run is skipped unless it succeeds within a minute.", cx)),
            )
            .child(
                h_flex()
                    .pt_2()
                    .gap_4()
                    .flex_wrap()
                    .child(v_flex().gap_1().child(label("Runs a day", cx)).child(number(&self.max_day)))
                    .child(v_flex().gap_1().child(label("Stop after (min)", cx)).child(number(&self.timeout)))
                    .child(v_flex().gap_1().child(label("Catch up within (min)", cx)).child(number(&self.grace))),
            )
            .child(help("A run missed while the computer slept still starts if it is at most this late; otherwise it is recorded as missed.", cx))
    }

    fn render_after(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let index = match self.notify {
            Notify::Always => 0,
            Notify::Failures => 1,
            Notify::Never => 2,
        };
        let worktree = self.place == Where::Worktree;
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child("Ask for attention"))
                    .child(
                        TabBar::new("automation-notify")
                            .segmented()
                            .small()
                            .selected_index(index)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.notify = [Notify::Always, Notify::Failures, Notify::Never][*ix];
                                cx.notify();
                            }))
                            .child(Tab::new().label("Always"))
                            .child(Tab::new().label("On failure"))
                            .child(Tab::new().label("Never")),
                    ),
            )
            .child(Checkbox::new("after-settle").label("Settle the thread when the run changed nothing").checked(self.settle).on_click(
                cx.listener(|this, checked: &bool, _, cx| {
                    this.settle = *checked;
                    cx.notify();
                }),
            ))
            .child(
                Checkbox::new("after-pr")
                    .label("Open a pull request with the run’s changes")
                    .checked(self.open_pr && worktree)
                    .disabled(!worktree)
                    .tooltip(if worktree { "Commits, pushes and opens a pull request on your forge." } else { "Only runs in a new worktree can open a pull request." })
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.open_pr = *checked;
                        cx.notify();
                    })),
            )
    }

    /// The sheet's footer: the last error, Cancel, and Create or Save.
    pub fn render_footer(editor: &Entity<Self>, cx: &App) -> impl IntoElement {
        let this = editor.read(cx);
        let theme = cx.theme();
        let (save, cancel) = (editor.clone(), editor.clone());
        h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.danger)
                    .when_some(this.error.clone(), |el, error| el.child(error)),
            )
            .child(Button::new("automation-cancel").label("Cancel").on_click(move |_, window, cx| {
                let _ = &cancel;
                window.close_sheet(cx);
            }))
            .child(
                Button::new("automation-save")
                    .primary()
                    .label(if this.is_new() { "Create" } else { "Save" })
                    .loading(this.saving)
                    .on_click(move |_, window, cx| save.update(cx, |editor, cx| editor.save(window, cx))),
            )
    }
}

impl Render for AutomationEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Debug builds: `ORBIT_DEBUG_EDITOR_SKIP=n` leaves out the first n
        // sections, so screenshots reach the lower ones without scrolling.
        let skip: usize = if cfg!(debug_assertions) {
            std::env::var("ORBIT_DEBUG_EDITOR_SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
        } else {
            0
        };
        let sections = [
            ("What", self.render_what(cx).into_any_element()),
            ("When", self.render_when(cx).into_any_element()),
            ("Where", self.render_where(cx).into_any_element()),
            ("Agent", self.render_agent(cx).into_any_element()),
            ("Before a run", self.render_checks(cx).into_any_element()),
            ("After a run", self.render_after(cx).into_any_element()),
        ];
        v_flex().gap_6().pb_4().children(sections.into_iter().skip(skip).map(|(title, body)| section(title, body, cx)))
    }
}

fn section(title: &'static str, body: AnyElement, cx: &App) -> Div {
    v_flex()
        .gap_3()
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().foreground).child(title))
        .child(body)
}

fn label(text: &'static str, cx: &App) -> Div {
    div().text_xs().font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(text)
}

fn help(text: &'static str, cx: &App) -> Div {
    div().text_xs().text_color(cx.theme().muted_foreground).child(text)
}

fn muted(text: &'static str, cx: &App) -> Div {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text)
}

fn field(title: &'static str, note: Option<&'static str>, control: impl IntoElement, cx: &App) -> Div {
    v_flex().gap_1().child(label(title, cx)).child(control).when_some(note, |el, note| el.child(help(note, cx)))
}

/// Opens the editor in a sheet: for `id` (its current spec), or a new one
/// from `spec`.
pub fn open(app: Entity<AppState>, editing: Option<AutomationId>, spec: AutomationSpec, on_saved: OnSaved, window: &mut Window, cx: &mut App) {
    let editor = cx.new(|cx| AutomationEditor::new(app, editing, spec, on_saved, window, cx));
    let title = if editing.is_some() { "Edit automation" } else { "New automation" };
    let focus = editor.clone();
    window.open_sheet(cx, move |sheet, _, cx| {
        sheet
            .title(div().text_base().font_weight(FontWeight::SEMIBOLD).child(title))
            .size(px(600.))
            .child(div().px_1().child(editor.clone()))
            .footer(AutomationEditor::render_footer(&editor, cx))
    });
    let name = focus.read(cx).name.clone();
    window.defer(cx, move |window, cx| name.update(cx, |name, cx| name.focus(window, cx)));
}
