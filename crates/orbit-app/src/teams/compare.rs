//! A race's compare view: one column per candidate (diff stats, tests,
//! time, tokens and cost, the judge's verdict), then any two candidates
//! compared file by file, where a hunk can be taken from one into the other.
//! "Keep" merges a candidate into the base branch and archives the rest.

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::diff::{FileDiff, LineKind};
use orbit_proto::*;

use super::candidate_agent;
use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::{format, ui};

/// What the file-by-file comparison holds.
enum Comparison {
    Loading,
    Ready(Vec<FileDiff>),
    Failed(String),
}

pub struct CompareView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    race: RaceId,
    /// The candidate whose working tree a taken hunk changes, and the one
    /// it comes from.
    into: Option<ThreadId>,
    from: Option<ThreadId>,
    comparison: HashMap<(ThreadId, ThreadId), Comparison>,
    file: Option<String>,
    busy: Option<&'static str>,
    _subscriptions: Vec<Subscription>,
}

impl CompareView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, race: RaceId, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| {
                if matches!(event, AppEvent::Teams) {
                    this.refresh_if_changed(cx);
                    cx.notify();
                }
            }),
            cx.observe(&app, |_, _, cx| cx.notify()),
        ];
        let mut view = Self {
            app,
            workspace,
            race,
            into: None,
            from: None,
            comparison: HashMap::new(),
            file: None,
            busy: None,
            _subscriptions: subs,
        };
        view.pick_default_pair(cx);
        view
    }

    fn race<'a>(&self, cx: &'a App) -> Option<&'a Race> {
        self.app.read(cx).state().races.get(&self.race)
    }

    /// A against the judge's pick (or B), once there is something to see.
    fn pick_default_pair(&mut self, cx: &mut Context<Self>) {
        if self.into.is_some() {
            return;
        }
        let Some(race) = self.race(cx) else { return };
        let threads: Vec<ThreadId> = race.candidates.iter().map(|c| c.thread).collect();
        if threads.len() < 2 {
            return;
        }
        let pick = match &race.judge {
            JudgeState::Verdict { winner, .. } => Some(*winner),
            _ => None,
        };
        let from = pick.unwrap_or(threads[1]);
        let into = threads.iter().copied().find(|t| *t != from).unwrap_or(threads[0]);
        self.into = Some(into);
        self.from = Some(from);
        self.load(cx);
    }

    /// Candidates' work moved (a finished turn, a taken hunk): compare again.
    fn refresh_if_changed(&mut self, cx: &mut Context<Self>) {
        if self.into.is_none() {
            self.pick_default_pair(cx);
            return;
        }
        if let (Some(a), Some(b)) = (self.into, self.from)
            && !self.comparison.contains_key(&(a, b))
        {
            self.load(cx);
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let (Some(a), Some(b)) = (self.into, self.from) else { return };
        self.comparison.insert((a, b), Comparison::Loading);
        let task = self.app.update(cx, |app, cx| app.call(Command::from(TeamCommand::CompareCandidates { a, b }), cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let value = match result {
                    Ok(Reply::Diff(raw)) => Comparison::Ready(orbit_proto::diff::parse(&raw)),
                    Ok(_) => Comparison::Failed("Unexpected reply.".into()),
                    Err(e) => Comparison::Failed(e),
                };
                if let Comparison::Ready(files) = &value
                    && !this.file.as_ref().is_some_and(|f| files.iter().any(|x| x.path() == f))
                {
                    this.file = files.first().map(|f| f.path().to_owned());
                }
                this.comparison.insert((a, b), value);
                cx.notify();
            });
        })
        .detach();
    }

    fn choose(&mut self, into: ThreadId, from: ThreadId, cx: &mut Context<Self>) {
        if into == from {
            return;
        }
        self.into = Some(into);
        self.from = Some(from);
        if !matches!(self.comparison.get(&(into, from)), Some(Comparison::Ready(_))) {
            self.load(cx);
        }
        cx.notify();
    }

    fn take_hunk(&mut self, file: &FileDiff, hunk: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(into) = self.into else { return };
        let patch = hunk_patch(file, hunk);
        let task = self.app.update(cx, |app, cx| app.call(Command::from(TeamCommand::ApplyPatch { thread: into, patch }), cx));
        self.busy = Some("hunk");
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if let Err(message) = result {
                    use gpui_kit::component::notification::Notification;
                    window.push_notification(Notification::error(format!("Couldn’t take the hunk: {message}")), cx);
                }
                // The comparison changed with it.
                this.comparison.clear();
                this.load(cx);
            });
        })
        .detach();
    }

    fn run(&mut self, label: &'static str, command: TeamCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = Some(label);
        cx.notify();
        let task = self.app.update(cx, |app, cx| app.call(Command::from(command), cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Reply::Merge(info)) => {
                        use gpui_kit::component::notification::Notification;
                        window.push_notification(Notification::success(format!("Merged into {}", info.target)), cx);
                    }
                    Ok(_) => {}
                    Err(message) => {
                        use gpui_kit::component::notification::Notification;
                        window.push_notification(Notification::error(message), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_keep(&mut self, candidate: &Candidate, window: &mut Window, cx: &mut Context<Self>) {
        let Some(race) = self.race(cx).cloned() else { return };
        let others: Vec<String> = race.candidates.iter().filter(|c| c.thread != candidate.thread).map(|c| c.label.clone()).collect();
        let title = format!("Keep {}?", candidate.label);
        let description = format!(
            "{}’s work merges into {}. {} {} archived; their worktrees stay, so they can be restored.",
            candidate.label,
            race.base,
            join_labels(&others),
            if others.len() == 1 { "is" } else { "are" }
        );
        let ok = format!("Keep {}", candidate.label);
        let (view, winner, race_id) = (cx.weak_entity(), candidate.thread, race.id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert.title(title.clone()).description(description.clone()).show_cancel(true).ok_text(ok.clone()).on_ok(move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.run("keep", TeamCommand::KeepWinner { race: race_id, winner }, window, cx));
                true
            })
        });
    }

    fn open_thread(&self, thread: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
    }

    fn render_column(&self, race: &Race, c: &Candidate, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let thread = state.threads.get(&c.thread);
        let pick = matches!(&race.judge, JudgeState::Verdict { winner, .. } if *winner == c.thread);
        let kept = matches!(race.state, RaceState::Decided { winner } if winner == c.thread);
        let decided = matches!(race.state, RaceState::Decided { .. });
        // A running candidate shows its live usage.
        let usage = if c.finished.is_some() { c.usage } else { state.usage(c.thread) };
        let stat = |label: &'static str, value: AnyElement| {
            h_flex()
                .h_6()
                .gap_2()
                .text_xs()
                .child(div().w(rems(4.)).flex_none().text_color(theme.muted_foreground).child(label))
                .child(div().flex_1().min_w_0().child(value))
        };
        let changes = if c.files.is_empty() && c.finished.is_none() {
            div().text_color(theme.muted_foreground).child("—").into_any_element()
        } else {
            h_flex()
                .gap_1p5()
                .child(div().text_color(theme.success).child(format!("+{}", c.additions())))
                .child(div().text_color(theme.danger).child(format!("−{}", c.deletions())))
                .child(div().text_color(theme.muted_foreground).child(match c.files.len() {
                    1 => "· 1 file".to_owned(),
                    n => format!("· {n} files"),
                }))
                .into_any_element()
        };
        let tests = match &c.test {
            TestState::None => div().text_color(theme.muted_foreground).child("No test command").into_any_element(),
            TestState::Pending => div().text_color(theme.muted_foreground).child("After it finishes").into_any_element(),
            TestState::Running => h_flex().gap_1p5().child(Spinner::new().xsmall()).child("Running…").into_any_element(),
            TestState::Passed { ms } => h_flex()
                .gap_1()
                .text_color(theme.success)
                .child(Icon::new(IconName::CircleCheck).xsmall())
                .child(format!("Passed · {}", format::duration(*ms)))
                .into_any_element(),
            TestState::Failed { code, tail, .. } => {
                let tail: SharedString = tail.trim().to_owned().into();
                h_flex()
                    .id(("test-failed", c.thread.0))
                    .gap_1()
                    .text_color(theme.danger)
                    .child(Icon::new(IconName::CircleX).xsmall())
                    .child(match code {
                        Some(code) => format!("Failed · exit {code}"),
                        None => "Failed".to_owned(),
                    })
                    .when(!tail.is_empty(), |el| el.tooltip(move |window, cx| Tooltip::new(tail.clone()).build(window, cx)))
                    .into_any_element()
            }
        };
        let outcome = match &c.finished {
            Some(CandidateEnd::Failed { message }) => Some((message.clone(), theme.danger)),
            _ => None,
        };
        let thread_id = c.thread;
        let candidate = c.clone();
        v_flex()
            .id(("candidate", c.thread.0))
            .min_w(rems(15.))
            .flex_1()
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if pick || kept { theme.success.opacity(0.6) } else { theme.border })
            .bg(if pick || kept { theme.success.opacity(0.04) } else { theme.background })
            .when(decided && !kept, |el| el.opacity(0.6))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .size_6()
                            .flex_none()
                            .justify_center()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(c.label.clone()),
                    )
                    .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(candidate_agent(state, &c.config)))
                    .when_some(thread, |el, t| el.child(ui::StatusGlyph::new(("candidate-glyph", c.thread.0), t, state))),
            )
            .child(
                v_flex()
                    .child(stat("Changes", changes))
                    .child(stat("Tests", tests))
                    .child(stat(
                        "Time",
                        div()
                            .text_color(if c.duration_ms == 0 { theme.muted_foreground } else { theme.foreground })
                            .child(if c.duration_ms == 0 { "—".to_owned() } else { format::duration(c.duration_ms) })
                            .into_any_element(),
                    ))
                    .child(stat(
                        "Tokens",
                        div().child(format::tokens(usage.input_tokens + usage.output_tokens)).into_any_element(),
                    ))
                    .child(stat(
                        "Cost",
                        match usage.cost_micros {
                            0 => div().text_color(theme.muted_foreground).child("—").into_any_element(),
                            n => div().child(format::cost(n)).into_any_element(),
                        },
                    )),
            )
            .when(pick, |el| {
                let reason = match &race.judge {
                    JudgeState::Verdict { reason, .. } => reason.clone(),
                    _ => String::new(),
                };
                el.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.success)
                                .child(Icon::new(IconName::Star).xsmall())
                                .child("Judge’s pick"),
                        )
                        .when(!reason.is_empty(), |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(reason))),
                )
            })
            .when_some(outcome, |el, (message, color)| el.child(div().text_xs().text_color(color).child(message)))
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(("candidate-open", c.thread.0))
                            .ghost()
                            .small()
                            .label("Open")
                            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(thread_id, window, cx))),
                    )
                    .child(div().flex_1())
                    .map(|el| match (&race.state, kept) {
                        (RaceState::Decided { .. }, true) => el.child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.success)
                                .child(Icon::new(Lucide::GitMerge).xsmall())
                                .child(format!("Kept on {}", race.base)),
                        ),
                        (RaceState::Decided { .. }, false) => {
                            el.child(div().text_xs().text_color(theme.muted_foreground).child("Archived"))
                        }
                        _ => el.child(
                            Button::new(("candidate-keep", c.thread.0))
                                .small()
                                .when(pick, |b| b.primary())
                                .label(format!("Keep {}…", c.label))
                                .disabled(c.finished.is_none() || self.busy.is_some())
                                .loading(self.busy == Some("keep"))
                                .on_click(cx.listener(move |this, _, window, cx| this.confirm_keep(&candidate, window, cx))),
                        ),
                    }),
            )
            .into_any_element()
    }

    fn render_pair_picker(&self, race: &Race, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let side = |which: &'static str, current: Option<ThreadId>, cx: &mut Context<Self>| {
            h_flex().gap_0p5().children(race.candidates.iter().map(|c| {
                let thread = c.thread;
                Button::new(SharedString::from(format!("pair-{which}-{}", c.label)))
                    .xsmall()
                    .ghost()
                    .selected(current == Some(thread))
                    .label(c.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let (into, from) = (this.into.unwrap_or(thread), this.from.unwrap_or(thread));
                        match which {
                            "into" if thread != from => this.choose(thread, from, cx),
                            "from" if thread != into => this.choose(into, thread, cx),
                            _ => {}
                        }
                    }))
            }))
        };
        h_flex()
            .gap_2()
            .text_sm()
            .child(div().font_weight(FontWeight::MEDIUM).child("Compare"))
            .child(side("into", self.into, cx))
            .child(div().text_color(theme.muted_foreground).child("with"))
            .child(side("from", self.from, cx))
    }

    fn render_diff(&self, race: &Race, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (Some(into), Some(from)) = (self.into, self.from) else {
            return div().into_any_element();
        };
        let label = |t: ThreadId| race.candidate(t).map(|c| c.label.clone()).unwrap_or_default();
        let (into_label, from_label) = (label(into), label(from));
        let files = match self.comparison.get(&(into, from)) {
            None | Some(Comparison::Loading) => {
                return h_flex().p_4().gap_2().text_sm().text_color(theme.muted_foreground).child(Spinner::new().small()).child("Comparing…").into_any_element();
            }
            Some(Comparison::Failed(e)) => {
                return div().p_4().text_sm().text_color(theme.danger).child(format!("Couldn’t compare: {e}")).into_any_element();
            }
            Some(Comparison::Ready(files)) => files,
        };
        if files.is_empty() {
            return div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(format!("{into_label} and {from_label} made the same changes."))
                .into_any_element();
        }
        let selected = self.file.clone().unwrap_or_else(|| files[0].path().to_owned());
        let list = v_flex().w(rems(16.)).flex_none().py_1().border_r_1().border_color(theme.border).children(files.iter().map(|f| {
            let path = f.path().to_owned();
            let active = path == selected;
            let (name, dir) = match path.rsplit_once('/') {
                Some((dir, name)) => (name.to_owned(), dir.to_owned()),
                None => (path.clone(), String::new()),
            };
            h_flex()
                .id(SharedString::from(format!("cmp-file-{path}")))
                .h_7()
                .mx_1()
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .text_xs()
                .cursor_default()
                .when(active, |el| el.bg(theme.list_active))
                .when(!active, |el| el.hover(|s| s.bg(theme.list_hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.file = Some(path.clone());
                    cx.notify();
                }))
                .child(div().flex_none().child(name))
                .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(dir))
                .child(div().flex_none().text_color(theme.success).child(format!("+{}", f.additions)))
                .child(div().flex_none().text_color(theme.danger).child(format!("−{}", f.deletions)))
        }));
        let file = files.iter().find(|f| f.path() == selected).unwrap_or(&files[0]);
        let mono = theme.mono_font_family.clone();
        let hunks = file.hunks.iter().enumerate().map(|(ix, hunk)| {
            let file_for_take = file.clone();
            v_flex()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .child(
                    h_flex()
                        .h_8()
                        .pl_3()
                        .pr_1()
                        .gap_2()
                        .bg(theme.muted)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .font_family(mono.clone())
                                .text_color(theme.muted_foreground)
                                .child(format!("@@ −{},{} +{},{} @@ {}", hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len, file.text(hunk.header).trim())),
                        )
                        .child(
                            Button::new(SharedString::from(format!("take-{}-{ix}", file.path())))
                                .ghost()
                                .xsmall()
                                .icon(IconName::ArrowLeft)
                                .label(format!("Take {from_label}’s hunk into {into_label}"))
                                .disabled(self.busy.is_some() || matches!(race.state, RaceState::Decided { .. }))
                                .on_click(cx.listener(move |this, _, window, cx| this.take_hunk(&file_for_take, ix, window, cx))),
                        ),
                )
                .child(v_flex().py_1().font_family(mono.clone()).text_xs().children(hunk.lines.iter().map(|line| {
                    let (sign, bg, fg) = match line.kind {
                        LineKind::Added => ("+", theme.success.opacity(0.12), theme.foreground),
                        LineKind::Removed => ("−", theme.danger.opacity(0.12), theme.foreground),
                        LineKind::Context => (" ", theme.transparent, theme.muted_foreground),
                        LineKind::NoNewline => (" ", theme.transparent, theme.muted_foreground),
                    };
                    h_flex()
                        .min_h_5()
                        .px_3()
                        .gap_3()
                        .bg(bg)
                        .text_color(fg)
                        .child(div().w(rems(0.75)).flex_none().child(sign))
                        .child(div().flex_1().min_w_0().whitespace_nowrap().overflow_hidden().child(file.line_text(line).to_owned()))
                })))
        });
        h_flex()
            .flex_1()
            .min_h_0()
            .items_start()
            .child(list)
            .child(
                v_flex()
                    .id("cmp-hunks")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_3()
                    .gap_3()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("− lines are {into_label}’s, + lines are {from_label}’s. Taking a hunk changes {into_label}’s worktree.")),
                    )
                    .children(hunks),
            )
            .into_any_element()
    }
}

fn join_labels(labels: &[String]) -> String {
    match labels {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let Some(race) = self.race(cx).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let running = race.state == RaceState::Running;
        let finished = race.candidates.iter().filter(|c| c.finished.is_some()).count();
        let status = match (&race.state, &race.judge) {
            (RaceState::Decided { winner }, _) => {
                format!("Kept {}", race.candidate(*winner).map(|c| c.label.clone()).unwrap_or_default())
            }
            (RaceState::Cancelled, _) => "Cancelled".to_owned(),
            (_, JudgeState::Running) => "The judge is comparing…".to_owned(),
            (_, JudgeState::Failed { message }) => format!("No verdict: {message}"),
            _ if race.settled() => "Every candidate finished".to_owned(),
            _ => format!("{finished} of {} finished", race.candidates.len()),
        };
        let judge_running = race.judge == JudgeState::Running;
        let toolbar = h_flex()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_sm().line_clamp(2).child(race.prompt.trim().to_owned()))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(Lucide::GitBranch).xsmall())
                            .child(race.base.clone())
                            .child("·")
                            .child(status)
                            .when_some(race.test_command.clone(), |el, cmd| {
                                el.child("·").child(div().font_family(theme.mono_font_family.clone()).child(cmd))
                            }),
                    ),
            )
            .when(running && race.test_command.is_some(), |el| {
                el.child(
                    Button::new("race-tests")
                        .outline()
                        .small()
                        .icon(IconName::RefreshCw)
                        .label("Run tests")
                        .disabled(self.busy.is_some())
                        .on_click(cx.listener(|this, _, window, cx| {
                            let race = this.race;
                            this.run("tests", TeamCommand::RunTests { race }, window, cx)
                        })),
                )
            })
            .when(running, |el| {
                el.child(
                    Button::new("race-judge")
                        .outline()
                        .small()
                        .icon(IconName::Star)
                        .label(if matches!(race.judge, JudgeState::Verdict { .. }) { "Judge again" } else { "Ask a judge" })
                        .loading(judge_running)
                        .disabled(judge_running || finished == 0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let race = this.race;
                            this.run("judge", TeamCommand::Judge { race }, window, cx)
                        })),
                )
                .child(
                    Button::new("race-cancel")
                        .ghost()
                        .small()
                        .label("Stop race")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let race = this.race;
                            this.run("cancel", TeamCommand::CancelRace { race }, window, cx)
                        })),
                )
            });
        let columns = h_flex().px_4().gap_3().items_stretch().children(race.candidates.iter().map(|c| self.render_column(&race, c, cx)));
        v_flex()
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(div().id("race-columns").flex_none().overflow_x_scroll().pb_3().child(columns))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .mx_4()
                    .mb_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .child(h_flex().h_10().flex_none().px_3().border_b_1().border_color(theme.border).child(self.render_pair_picker(&race, cx)))
                    .child(self.render_diff(&race, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::join_labels;

    #[test]
    fn labels_read_as_a_list() {
        assert_eq!(join_labels(&["A".into()]), "A");
        assert_eq!(join_labels(&["A".into(), "C".into()]), "A and C");
        assert_eq!(join_labels(&["A".into(), "C".into(), "D".into()]), "A, C and D");
    }
}
