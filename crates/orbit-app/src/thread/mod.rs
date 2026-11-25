//! One thread: its timeline, its header and its composer.
//!
//! Rendering cost follows what changes:
//! * the timeline is a virtualized `MessageScroller` (only visible rows
//!   exist), following the tail while the reader sits at the live edge;
//! * each assistant message owns a `TextViewState`, so a streamed token
//!   re-parses only the last Markdown block, on a background task;
//! * a token re-measures one row; only a new or removed item rebuilds the
//!   row list, and an append keeps the reader's scroll position.

pub(crate) mod rows;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::state::ItemChange;
use orbit_proto::*;

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::composer::{Composer, ComposerEvent};
use gpui_kit::component::resizable::{ResizableState, resizable_panel, v_resizable};
use terminal::{TerminalPanel, TerminalPanelEvent};
use crate::{format, ui};

pub use rows::{COLUMN, Row};

pub struct ThreadView {
    id: ThreadId,
    app: Entity<AppState>,
    scroller: Entity<MessageScrollerState>,
    rows: Rc<Vec<Row>>,
    item_row: HashMap<ItemId, usize>,
    markdown: HashMap<ItemId, Entity<TextViewState>>,
    /// Rows the reader expanded (work logs, reasoning, tool output) by key.
    expanded: HashSet<ItemId>,
    composer: Entity<Composer>,
    focus: FocusHandle,
    loaded: bool,
    terminal: Option<Entity<TerminalPanel>>,
    split: Entity<ResizableState>,
    _subscriptions: Vec<Subscription>,
}

pub enum ThreadViewEvent {
    /// The reader asked to review this thread's changes.
    Review,
}

impl EventEmitter<ThreadViewEvent> for ThreadView {}

impl ThreadView {
    pub fn new(id: ThreadId, app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = app.read(cx).state().threads.get(&id).map(|t| t.config.clone()).unwrap_or_default();
        let composer = cx.new(|cx| Composer::new(app.clone(), config, window, cx));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let mut subscriptions = vec![
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| this.on_app_event(event, cx)),
            cx.observe(&app, |this, _, cx| this.sync_thread_record(cx)),
            cx.subscribe_in(&composer, window, |this, _, event: &ComposerEvent, window, cx| {
                this.on_composer(event, window, cx)
            }),
        ];
        subscriptions.push(cx.on_release(|this, cx| {
            let id = this.id;
            this.app.update(cx, |app, _| app.release_timeline(id));
        }));
        app.update(cx, |app, cx| app.retain_timeline(id, cx));
        let mut view = Self {
            id,
            app,
            scroller,
            rows: Rc::new(Vec::new()),
            item_row: HashMap::new(),
            markdown: HashMap::new(),
            expanded: HashSet::new(),
            composer,
            focus: cx.focus_handle(),
            loaded: false,
            terminal: None,
            split: cx.new(|_| ResizableState::default()),
            _subscriptions: subscriptions,
        };
        view.rebuild(true, cx);
        view.sync_thread_record(cx);
        view
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn focus_composer(view: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let composer = view.read(cx).composer.clone();
        Composer::focus(&composer, window, cx);
    }

    /// Opens the thread's terminal in its folder, or closes it.
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.take().is_some() {
            Composer::focus(&self.composer.clone(), window, cx);
            cx.notify();
            return;
        }
        let Some(thread) = self.thread(cx).cloned() else { return };
        let state = self.app.read(cx).state();
        let cwd = thread
            .workspace
            .worktree
            .clone()
            .or_else(|| thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.path.clone()))
            .unwrap_or_else(|| orbit_core::EngineOptions::default_data_dir().display().to_string());
        let panel = cx.new(|cx| TerminalPanel::new(self.app.clone(), cwd, window, cx));
        let sub = cx.subscribe_in(&panel, window, |this, _, event: &TerminalPanelEvent, window, cx| match event {
            TerminalPanelEvent::Close => this.toggle_terminal(window, cx),
        });
        self._subscriptions.push(sub);
        let focus = panel.clone();
        window.defer(cx, move |window, cx| TerminalPanel::focus(&focus, window, cx));
        self.terminal = Some(panel);
        cx.notify();
    }

    pub fn stop(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(Command::Interrupt { thread: self.id }, window, cx));
    }

    fn thread<'a>(&self, cx: &'a App) -> Option<&'a Thread> {
        self.app.read(cx).state().threads.get(&self.id)
    }

    fn on_app_event(&mut self, event: &AppEvent, cx: &mut Context<Self>) {
        match event {
            AppEvent::Timeline { thread, changes } if *thread == self.id => match changes {
                None => self.rebuild(true, cx),
                Some(changes) => self.apply_changes(changes, cx),
            },
            AppEvent::Usage(thread) if *thread == self.id => {
                let usage = self.app.read(cx).state().usage(self.id);
                self.composer.update(cx, |composer, cx| composer.set_usage(Some(usage), cx));
            }
            _ => {}
        }
    }

    /// Keeps the composer in step with the thread record (config, running).
    fn sync_thread_record(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx).cloned() else { return };
        let state = self.app.read(cx).state();
        let usage = state.usage(self.id);
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
        let context = Some(match (&thread.workspace.worktree, project) {
            (Some(_), _) => "Own worktree".to_owned(),
            (None, Some(p)) => p.name.clone(),
            (None, None) => "Scratch folder".to_owned(),
        });
        let branch = thread.workspace.branch.clone().or_else(|| project.and_then(|p| p.base_branch.clone()));
        self.composer.update(cx, |composer, cx| {
            composer.set_running(thread.status.is_busy(), cx);
            composer.set_config(thread.config.clone(), cx);
            composer.set_usage(Some(usage), cx);
            composer.set_context(context, branch, cx);
        });
        cx.notify();
    }

    fn apply_changes(&mut self, changes: &[ItemChange], cx: &mut Context<Self>) {
        let structural = changes.iter().any(|c| matches!(c, ItemChange::Inserted(_) | ItemChange::Removed(_)));
        if structural {
            self.rebuild(false, cx);
            return;
        }
        let mut touched = Vec::new();
        if let Some(timeline) = self.app.read(cx).state().timeline(self.id) {
            for change in changes {
                if let ItemChange::Updated(ix) = change
                    && let Some(item) = timeline.items().get(*ix)
                {
                    touched.push(item.clone());
                }
            }
        }
        let mut remeasure = HashSet::new();
        for item in &touched {
            self.sync_markdown(item, cx);
            if let Some(&row) = self.item_row.get(&item.id) {
                remeasure.insert(row);
            }
        }
        if !remeasure.is_empty() {
            self.scroller.update(cx, |scroller, cx| {
                for row in remeasure {
                    let _ = scroller.remeasure_items(row..row + 1, cx);
                }
            });
        }
        cx.notify();
    }

    fn sync_markdown(&mut self, item: &Item, cx: &mut Context<Self>) {
        let text = match &item.body {
            ItemBody::Assistant { text } | ItemBody::Reasoning { text } => text,
            _ => return,
        };
        match self.markdown.get(&item.id) {
            // A longer text with the old text as its prefix is parsed
            // incrementally (only the last block).
            Some(state) => state.update(cx, |state, cx| state.set_text(text, cx)),
            None => {
                let text = text.clone();
                let state = cx.new(|cx| TextViewState::markdown(&text, cx));
                self.markdown.insert(item.id, state);
            }
        }
    }

    /// Recomputes the row list. Appends keep the reader's position; other
    /// structural changes reset the list.
    fn rebuild(&mut self, full: bool, cx: &mut Context<Self>) {
        let Some(timeline) = self.app.read(cx).state().timeline(self.id) else { return };
        let items = timeline.items().to_vec();
        self.loaded = true;
        let rows = Row::build(&items);
        let mut item_row = HashMap::with_capacity(items.len());
        for (ix, row) in rows.iter().enumerate() {
            for id in row.items() {
                item_row.insert(id, ix);
            }
        }
        for item in &items {
            self.sync_markdown(item, cx);
        }
        let live: HashSet<ItemId> = items.iter().map(|i| i.id).collect();
        self.markdown.retain(|id, _| live.contains(id));

        let old = self.rows.clone();
        let (old_len, new_len) = (old.len(), rows.len());
        let appended = !full
            && old_len > 0
            && new_len >= old_len
            && old[..old_len - 1] == rows[..old_len - 1]
            && old[old_len - 1].key() == rows[old_len - 1].key();
        self.rows = Rc::new(rows);
        self.item_row = item_row;
        self.scroller.update(cx, |scroller, cx| {
            if appended {
                if new_len > old_len {
                    let _ = scroller.append(new_len - old_len, cx);
                }
                let _ = scroller.remeasure_items(old_len - 1..new_len, cx);
            } else {
                scroller.reset(new_len, cx);
            }
        });
        cx.notify();
    }

    fn on_composer(&mut self, event: &ComposerEvent, window: &mut Window, cx: &mut Context<Self>) {
        let command = match event {
            ComposerEvent::Submit { text, attachments, steer } => Command::Send {
                thread: self.id,
                text: text.clone(),
                attachments: attachments.clone(),
                steer: *steer,
            },
            ComposerEvent::Stop => Command::Interrupt { thread: self.id },
            ComposerEvent::WorktreeChosen(_) => return,
            ComposerEvent::ConfigChanged(config) => Command::UpdateThread {
                id: self.id,
                patch: ThreadPatch { config: Some(config.clone()), ..Default::default() },
            },
        };
        if matches!(event, ComposerEvent::Submit { .. }) {
            self.scroller.update(cx, |s, cx| s.scroll_to_end(cx));
        }
        self.app.update(cx, |app, cx| app.run(command, window, cx));
    }

    pub(crate) fn toggle_expanded(&mut self, key: ItemId, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        if let Some(&row) = self.item_row.get(&key) {
            self.scroller.update(cx, |s, cx| {
                let _ = s.remeasure_items(row..row + 1, cx);
            });
        }
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let Some(thread) = state.threads.get(&self.id) else { return h_flex() };
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
        let usage = state.usage(self.id);
        let model = ui::model_label(&thread.config, &state.providers);
        let id = self.id;
        h_flex()
            .h_11()
            .flex_none()
            .px_4()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(ui::StatusGlyph::new(("thread-status", id.0), thread))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(display_title(thread).to_owned()))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .when_some(project, |el, p| el.child(p.name.clone()))
                            .when_some(thread.workspace.branch.clone(), |el, branch| {
                                el.child("·").child(Icon::new(Lucide::GitBranch).xsmall()).child(branch)
                            })
                            .child("·")
                            .child(model),
                    ),
            )
            .child(ui::ContextMeter::new(("header-meter", id.0), usage))
            .when(!format::cost(usage.cost_micros).is_empty(), |el| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child(format::cost(usage.cost_micros)))
            })
            .child(
                Button::new("review")
                    .ghost()
                    .small()
                    .icon(Icon::new(Lucide::FileDiff))
                    .label("Review")
                    .tooltip("Review changes (Ctrl+R)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ThreadViewEvent::Review))),
            )
            .child({
                let (app, menu_thread) = (self.app.clone(), thread.clone());
                let path = project.map(|p| p.path.clone());
                Button::new("thread-menu").ghost().small().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, _| {
                    crate::thread_menu::build(menu, &app, &menu_thread, path.clone())
                })
            })
    }

    fn render_queue(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let timeline = self.app.read(cx).state().timeline(self.id)?;
        let queued: Vec<(ItemId, String)> = timeline
            .items()
            .iter()
            .filter(|i| i.status == ItemStatus::Queued)
            .filter_map(|i| match &i.body {
                ItemBody::User(m) => Some((i.id, m.text.clone())),
                _ => None,
            })
            .collect();
        if queued.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let mut list = v_flex().gap_1().mb_2();
        for (item, text) in queued {
            let app = self.app.clone();
            let thread = self.id;
            list = list.child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .text_sm()
                    .child(Icon::new(Lucide::Clock).xsmall().text_color(theme.muted_foreground))
                    .child(div().text_xs().text_color(theme.muted_foreground).child("Queued"))
                    .child(div().flex_1().min_w_0().truncate().child(text))
                    .child(
                        Button::new(("unqueue", item.0))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Remove from queue")
                            .on_click(move |_, window, cx| {
                                app.update(cx, |app, cx| {
                                    app.run(Command::EditQueued { thread, item, text: String::new() }, window, cx)
                                })
                            }),
                    ),
            );
        }
        Some(list)
    }
}

pub fn display_title(thread: &Thread) -> &str {
    if thread.title.is_empty() { "New thread" } else { &thread.title }
}

impl Focusable for ThreadView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.weak_entity();
        let rows = self.rows.clone();
        let empty = self.loaded && rows.is_empty();
        let timeline = MessageScroller::new("timeline", self.scroller.clone(), move |ix, window, cx| {
            let Some(view) = view.upgrade() else { return div().into_any_element() };
            let Some(row) = rows.get(ix).cloned() else { return div().into_any_element() };
            rows::render_row(&view, &row, window, cx)
        })
        .with_bottom_fade(theme.background)
        .with_list_style(StyleRefinement::default().pt_4().pb_2())
        .flex_1()
        .min_h_0();

        let conversation = v_flex()
            .size_full()
            .child(if empty {
                ui::empty_state(
                    Icon::new(Lucide::MessageSquare),
                    "Start the conversation",
                    "Describe the change you want. The agent works in this project and shows every step here.",
                    None,
                    cx,
                )
                .into_any_element()
            } else if !self.loaded {
                div().flex_1().into_any_element()
            } else {
                timeline.into_any_element()
            })
            .child(
                v_flex()
                    .flex_none()
                    .w_full()
                    .px_4()
                    .pb_4()
                    .items_center()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(rows::COLUMN)
                            .children(self.render_queue(cx))
                            .child(self.composer.clone()),
                    ),
            );
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .child(self.render_header(cx))
            .child(match &self.terminal {
                None => div().flex_1().min_h_0().child(conversation).into_any_element(),
                Some(terminal) => div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_resizable("thread-split")
                            .with_state(&self.split)
                            .child(resizable_panel().child(conversation))
                            .child(
                                resizable_panel()
                                    .size(px(300.))
                                    .size_range(px(120.)..px(900.))
                                    .child(terminal.clone()),
                            ),
                    )
                    .into_any_element(),
            })
    }
}
