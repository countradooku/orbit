//! The crew strip: an architect's workers at a glance.
//!
//! A header with the tally ("3 working · 2 done · 1 waiting") over a
//! segmented progress bar, then one row per worker sorted by state, with
//! sub-workers indented under the worker that spawned them. A waiting row
//! says why it waits. Clicking a row opens the worker's thread; its trailing
//! button opens it beside the current one as a tile.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::{crew_bar, tally_line, team_led_by, worker_elapsed, worker_label};
use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::{format, ui};

/// How often the elapsed times move while someone works. Coarse on purpose:
/// the clock reads in minutes, so a faster tick would only spend frames.
const CLOCK: Duration = Duration::from_secs(10);
/// Rows shown before the list scrolls.
const VISIBLE_ROWS: f32 = 8.;

pub struct CrewStrip {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    architect: ThreadId,
    collapsed: bool,
    /// Rows shown before the list scrolls; `None` shows every row.
    max_rows: Option<f32>,
    merging: Option<String>,
    _clock: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CrewStrip {
    /// The crew of the team `architect` leads. Renders nothing while the
    /// thread leads no team, so it can be mounted under every thread.
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, architect: ThreadId, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| {
                if matches!(event, AppEvent::Teams) {
                    this.sync_clock(cx);
                    cx.notify();
                }
            }),
            // Worker glyphs follow their threads.
            cx.observe(&app, |_, _, cx| cx.notify()),
        ];
        let mut strip = Self {
            app,
            workspace,
            architect,
            collapsed: false,
            max_rows: Some(VISIBLE_ROWS),
            merging: None,
            _clock: None,
            _subscriptions: subs,
        };
        strip.sync_clock(cx);
        strip
    }

    /// Shows every row, for a page with room for the whole crew.
    pub fn unbounded(mut self) -> Self {
        self.max_rows = None;
        self
    }

    /// Ticks the elapsed times only while a worker works.
    fn sync_clock(&mut self, cx: &mut Context<Self>) {
        let working = team_led_by(self.app.read(cx).state(), self.architect)
            .is_some_and(|t| t.workers.iter().any(|w| w.state == WorkerState::Working));
        match (working, self._clock.is_some()) {
            (true, false) => {
                self._clock = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(CLOCK).await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                }));
            }
            (false, true) => self._clock = None,
            _ => {}
        }
    }

    fn open(&self, thread: ThreadId, beside: bool, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |ws, cx| {
            if beside {
                ws.open_beside(thread, window, cx);
            } else {
                ws.navigate(Route::Thread(thread), window, cx);
            }
        });
    }

    fn merge(&mut self, team: TeamId, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.merging = Some(key.clone());
        cx.notify();
        let command = Command::from(TeamCommand::MergeWorker { actor: None, team: Some(team), worker: key.clone(), dry_run: false });
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.merging = None;
                cx.notify();
            });
            let message = match result {
                Ok(Reply::Merge(info)) if !info.conflicts.is_empty() => {
                    Some(Err(format!("{key} conflicts in {}. Nothing changed.", info.conflicts.join(", "))))
                }
                Ok(Reply::Merge(info)) => Some(Ok(format!("Merged {key} into {}", info.target))),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            };
            if let Some(message) = message {
                let _ = handle.update(cx, |_, window, cx| {
                    use gpui_kit::component::WindowExt as _;
                    use gpui_kit::component::notification::Notification;
                    let note = match message {
                        Ok(text) => Notification::success(text),
                        Err(text) => Notification::warning(text),
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    fn resume(&mut self, team: TeamId, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(Command::from(TeamCommand::ResetBreaker { team, limits: None }), window, cx));
    }

    fn render_row(&self, team: &Team, w: &Worker, depth: usize, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let thread = w.thread.and_then(|t| state.threads.get(&t));
        let key = w.key.clone();
        let row_id = SharedString::from(format!("crew-{}-{}", team.id, w.key));
        // The status slot: the thread's own glyph once it runs; before that,
        // a still hourglass (waiting is not working, and not a call).
        let glyph = match (thread, &w.state) {
            (Some(t), WorkerState::Working | WorkerState::Failed { .. }) => {
                ui::StatusGlyph::new(SharedString::from(format!("{row_id}-glyph")), t, state).into_any_element()
            }
            (_, WorkerState::Done) => Icon::new(IconName::CircleCheck).size_3p5().text_color(theme.success).into_any_element(),
            (_, WorkerState::Merged) => Icon::new(Lucide::GitMerge).size_3p5().text_color(theme.magenta).into_any_element(),
            (_, WorkerState::Failed { .. }) => Icon::new(Lucide::OctagonAlert).size_3p5().text_color(theme.danger).into_any_element(),
            (_, WorkerState::Stopped) => Icon::new(IconName::CircleX).size_3p5().text_color(theme.muted_foreground).into_any_element(),
            _ => Icon::new(Lucide::Hourglass).size_3p5().text_color(theme.muted_foreground).into_any_element(),
        };
        // One line of secondary text: why it waits, what failed, or what
        // it reported.
        let (detail, detail_color): (String, Hsla) = match &w.state {
            WorkerState::Waiting(reason) => (reason.describe(), theme.muted_foreground),
            WorkerState::Failed { message } => (message.clone(), theme.danger),
            WorkerState::Done | WorkerState::Merged if w.merge.as_ref().is_some_and(|m| !m.conflicts.is_empty()) => {
                let m = w.merge.as_ref().unwrap();
                (format!("Conflicts in {}", m.conflicts.join(", ")), theme.warning)
            }
            WorkerState::Done | WorkerState::Merged => {
                (w.summary.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_owned(), theme.muted_foreground)
            }
            _ if !w.scope.is_empty() => (w.scope.join(", "), theme.muted_foreground),
            _ => (String::new(), theme.muted_foreground),
        };
        let usage = w.thread.map(|t| state.usage(t)).unwrap_or_default();
        let tokens = usage.input_tokens + usage.output_tokens;
        let elapsed = worker_elapsed(w, now).map(super::elapsed).unwrap_or_default();
        let tooltip: SharedString = format!("{} · {}{}", w.key, worker_label(&w.state), if detail.is_empty() { String::new() } else { format!("\n{detail}") }).into();
        let can_merge = w.state == WorkerState::Done && w.thread.is_some();
        let merging = self.merging.as_deref() == Some(w.key.as_str());
        let team_id = team.id;
        let target = w.thread;
        h_flex()
            .id(row_id.clone())
            .h_8()
            .pl(rems(0.75 + 1.25 * depth as f32))
            .pr_1p5()
            .gap_2()
            .cursor_default()
            .hover(|s| s.bg(theme.list_hover))
            .when_some(target, |el, thread| {
                el.on_click(cx.listener(move |this, _, window, cx| this.open(thread, false, window, cx)))
            })
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(h_flex().size_4().flex_none().justify_center().child(glyph))
            .child(
                div()
                    .w(rems(1.75))
                    .flex_none()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(w.key.clone()),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(div().flex_shrink_0().max_w(relative(0.6)).truncate().text_sm().child(w.title.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(detail_color).child(detail)),
            )
            .child(
                div()
                    .w(rems(3.5))
                    .flex_none()
                    .text_right()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if tokens > 0 { format::tokens(tokens) } else { String::new() }),
            )
            .child(div().w(rems(3.)).flex_none().text_right().text_xs().text_color(theme.muted_foreground).child(elapsed))
            // Fixed lanes, so rows line up whatever each one offers.
            .child(
                h_flex()
                    .w(rems(4.25))
                    .flex_none()
                    .justify_end()
                    .gap_0p5()
                    .when(can_merge, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("{row_id}-merge")))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(Lucide::GitMerge))
                                .loading(merging)
                                .tooltip("Merge into the architect’s branch")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.merge(team_id, key.clone(), window, cx)
                                })),
                        )
                    })
                    .when_some(target, |el, thread| {
                        el.child(
                            Button::new(SharedString::from(format!("{row_id}-beside")))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(Lucide::Columns2))
                                .tooltip("Open beside")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open(thread, true, window, cx)
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// Workers in display order with their depth: by state (working, waiting,
/// failed, done, merged), each followed by the sub-workers it spawned.
pub fn ordered(team: &Team) -> Vec<(&Worker, usize)> {
    let rank = |s: &WorkerState| match s {
        WorkerState::Working => 0,
        WorkerState::Waiting(_) => 1,
        WorkerState::Failed { .. } => 2,
        WorkerState::Done => 3,
        WorkerState::Merged => 4,
        WorkerState::Stopped => 5,
    };
    let mut out = Vec::with_capacity(team.workers.len());
    fn visit<'a>(team: &'a Team, parent: ThreadId, depth: usize, rank: &dyn Fn(&WorkerState) -> u8, out: &mut Vec<(&'a Worker, usize)>) {
        let mut children: Vec<&Worker> = team.workers.iter().filter(|w| w.parent == parent).collect();
        children.sort_by_key(|w| (rank(&w.state), w.created_ms));
        for w in children {
            out.push((w, depth));
            if let Some(thread) = w.thread
                && depth < 8
            {
                visit(team, thread, depth + 1, rank, out);
            }
        }
    }
    visit(team, team.architect, 0, &rank, &mut out);
    // Workers whose parent left the team still show, at the top level.
    for w in &team.workers {
        if !out.iter().any(|(x, _)| x.key == w.key) {
            out.push((w, 0));
        }
    }
    out
}

impl Render for CrewStrip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(team) = team_led_by(self.app.read(cx).state(), self.architect).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let tally = team.tally();
        let now = now_ms();
        let rows: Vec<AnyElement> = ordered(&team).into_iter().map(|(w, depth)| self.render_row(&team, w, depth, now, cx)).collect();
        let team_id = team.id;
        let header = h_flex()
            .h_9()
            .px_3()
            .gap_2()
            .child(Icon::new(IconName::Network).xsmall().text_color(theme.muted_foreground))
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Crew"))
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(tally_line(&tally)))
            .when(!team.notes.is_empty(), |el| {
                el.child(
                    Button::new(("crew-notes", team_id.0))
                        .ghost()
                        .xsmall()
                        .icon(IconName::FileText)
                        .label(format!("Notes · {}", team.notes.len()))
                        .on_click(cx.listener(move |this, _, window, cx| super::open_team(&this.workspace, team_id, window, cx))),
                )
            })
            .child(
                Button::new(("crew-collapse", team_id.0))
                    .ghost()
                    .xsmall()
                    .icon(if self.collapsed { IconName::ChevronDown } else { IconName::ChevronUp })
                    .tooltip(if self.collapsed { "Show workers" } else { "Hide workers" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    })),
            );
        let breaker = team.breaker.as_ref().map(|b| {
            h_flex()
                .mx_3()
                .mb_2()
                .px_2()
                .py_1p5()
                .gap_2()
                .rounded(theme.radius)
                .bg(theme.warning.opacity(0.1))
                .text_xs()
                .child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.warning))
                .child(div().flex_1().min_w_0().child(format!("Spawning paused: {}.", b.reason)))
                .child(
                    Button::new(("crew-resume", team_id.0))
                        .outline()
                        .xsmall()
                        .label("Resume")
                        .on_click(cx.listener(move |this, _, window, cx| this.resume(team_id, window, cx))),
                )
        });
        let overlap = team.overlaps.first().map(|o| {
            let state = self.app.read(cx).state();
            let names: Vec<String> = o
                .threads
                .iter()
                .map(|t| team.worker_of(*t).map(|w| w.key.clone()).unwrap_or_else(|| state.threads.get(t).map(|x| crate::thread::display_title(x).to_owned()).unwrap_or_default()))
                .collect();
            let more = team.overlaps.len() - 1;
            h_flex()
                .px_3()
                .pb_2()
                .gap_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.warning))
                .child(
                    div().flex_1().min_w_0().truncate().child(format!(
                        "{} both changed {}{}",
                        names.join(" and "),
                        o.path,
                        if more > 0 { format!(" and {more} more") } else { String::new() }
                    )),
                )
        });
        v_flex()
            .id(("crew", team_id.0))
            .w_full()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(header)
            .child(div().px_3().pb_2().child(crew_bar(&team, ("crew-bar", team_id.0), cx)))
            .children(breaker)
            .children(overlap)
            .when(!self.collapsed && !rows.is_empty(), |el| {
                el.child(
                    div()
                        .id(("crew-rows", team_id.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .py_1()
                        .when_some(self.max_rows, |el, rows| el.max_h(rems(2. * rows + 0.5)).overflow_y_scroll())
                        .children(rows),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::ordered;
    use orbit_proto::*;

    #[test]
    fn rows_sort_by_state_and_nest_sub_workers() {
        let w = |key: &str, parent: u64, thread: u64, state: WorkerState, at: u64| Worker {
            key: key.into(),
            parent: ThreadId(parent),
            thread: Some(ThreadId(thread)),
            state,
            created_ms: at,
            ..Default::default()
        };
        let team = Team {
            architect: ThreadId(1),
            workers: vec![
                w("w1", 1, 11, WorkerState::Done, 1),
                w("w2", 1, 12, WorkerState::Working, 2),
                w("w3", 12, 13, WorkerState::Done, 3),
                w("w4", 1, 14, WorkerState::Waiting(WaitReason::Breaker), 4),
                w("w5", 12, 15, WorkerState::Working, 5),
            ],
            ..Default::default()
        };
        let order: Vec<(&str, usize)> = ordered(&team).into_iter().map(|(w, d)| (w.key.as_str(), d)).collect();
        assert_eq!(order, [("w2", 0), ("w5", 1), ("w3", 1), ("w4", 0), ("w1", 0)]);
    }
}
