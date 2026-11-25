//! One thread: its timeline, its header and its composer.
//!
//! Rendering cost follows what changes:
//! * the timeline is a virtualized `MessageScroller` (only visible rows
//!   exist), following the tail while the reader sits at the live edge;
//! * each assistant message owns a `TextViewState`, so a streamed token
//!   re-parses only the last Markdown block, on a background task;
//! * a token re-measures one row; a new or removed item re-lays out the
//!   rows and splices only the ones that changed, so measured heights and
//!   the reader's position are kept (opening a fold above the reader does
//!   not move what they are reading);
//! * the turn navigator is its own view, so scrolling re-renders the rail,
//!   not the thread.
//!
//! Keys (context `Thread`): Ctrl+F find, Enter / Shift+Enter or F3 /
//! Shift+F3 next and previous match, Esc closes it; Alt+Up / Alt+Down step
//! between your messages.

mod banners;
mod browser;
mod find;
mod inspector;
mod minimap;
mod questions;
pub(crate) mod rows;
mod terminal;
mod transcript;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::{RangeHighlight, TextViewState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::state::ItemChange;
use orbit_proto::*;

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::composer::{Composer, ComposerEvent};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use orbit_browser::{BrowserEvent, BrowserView, Url};
use minimap::{Mark, Minimap};
use questions::Questions;
use terminal::{TerminalPanel, TerminalPanelEvent};
use crate::actions::ToggleBrowser;
use crate::{format, ui};

pub use rows::Row;

gpui_kit::actions!(
    thread,
    [
        FindInThread,
        FindNext,
        FindPrevious,
        PreviousMessage,
        NextMessage,
        /// Puts the terminal's selected output in the thread's composer.
        AddSelectionToComposer,
    ]
);

/// Binds the thread's keys.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-f", FindInThread, Some("Thread")),
        KeyBinding::new("f3", FindNext, Some("Thread")),
        KeyBinding::new("shift-f3", FindPrevious, Some("Thread")),
        KeyBinding::new("alt-up", PreviousMessage, Some("Thread")),
        KeyBinding::new("alt-down", NextMessage, Some("Thread")),
        KeyBinding::new(crate::actions::platform("cmd-shift-l", "ctrl-shift-l"), AddSelectionToComposer, Some(orbit_terminal::KEY_CONTEXT)),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-f", FindInThread, Some("Thread")),
        KeyBinding::new("cmd-g", FindNext, Some("Thread")),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some("Thread")),
    ]);
}

/// The open find bar.
struct Find {
    input: Entity<InputState>,
    hits: Vec<find::Hit>,
    current: Option<usize>,
    /// Indices into `hits`, per item.
    by_item: HashMap<ItemId, Vec<usize>>,
    /// Replies whose text view carries highlights, to clear later.
    painted: HashSet<ItemId>,
    _subscription: Subscription,
}

pub struct ThreadView {
    id: ThreadId,
    app: Entity<AppState>,
    scroller: Entity<MessageScrollerState>,
    rows: Rc<Vec<Row>>,
    item_row: HashMap<ItemId, usize>,
    markdown: HashMap<ItemId, Entity<TextViewState>>,
    /// Rows the reader expanded (work logs, reasoning, tool output, long
    /// messages) by key.
    expanded: HashSet<ItemId>,
    /// Folds (a turn's work, an earlier attempt) the reader opened.
    folds: HashSet<ItemId>,
    /// Items inside a closed fold → its key.
    folded: HashMap<ItemId, ItemId>,
    composer: Entity<Composer>,
    minimap: Entity<Minimap>,
    find: Option<Find>,
    questions: Option<Questions>,
    focus: FocusHandle,
    loaded: bool,
    /// Settings that change rows (heights, clock times); rows are
    /// re-measured when they do.
    layout: (ChatWidth, u16, u16, ClockFormat),
    terminal: Option<Entity<TerminalPanel>>,
    split: Entity<ResizableState>,
    /// The preview browser, created the first time it opens.
    browser: Option<Entity<BrowserView>>,
    browser_open: bool,
    browser_split: Entity<ResizableState>,
    /// Dev-server addresses seen in the thread and its terminal.
    urls: browser::Suggestions,
    /// Whether the shell shows this thread (it keeps a few views alive).
    shown: bool,
    /// Releases a hidden page after a while.
    browser_suspend: Task<()>,
    /// When the running turn started (as seen here), for the elapsed time.
    turn_started: Option<Instant>,
    /// When the reader pressed Stop on the running turn.
    stop_requested: Option<Instant>,
    /// The banner the reader dismissed (by key), until it changes.
    dismissed: Option<String>,
    /// Debug builds: steps to run once the timeline loads, from
    /// `ORBIT_DEBUG_THREAD` (for screenshots), e.g. `find:token;inspect`.
    debug_steps: Option<String>,
    /// Debug builds: the question panel shows demo questions; keep it.
    debug_questions: bool,
    busy: bool,
    _ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub enum ThreadViewEvent {
    /// The reader asked to review this thread's changes.
    Review,
    /// Open another thread (a fork just created from this one).
    Open(ThreadId),
}

impl EventEmitter<ThreadViewEvent> for ThreadView {}

impl ThreadView {
    pub fn new(id: ThreadId, app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = app.read(cx).state().threads.get(&id).map(|t| t.config.clone()).unwrap_or_default();
        let composer = cx.new(|cx| Composer::for_thread(id, app.clone(), config, window, cx));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let minimap = cx.new(|cx| Minimap::new(scroller.clone(), cx));
        let mut subscriptions = vec![
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| this.on_app_event(event, cx)),
            cx.observe_in(&app, window, |this, _, window, cx| this.sync_thread_record(window, cx)),
            cx.subscribe_in(&composer, window, |this, _, event: &ComposerEvent, window, cx| {
                this.on_composer(event, window, cx)
            }),
        ];
        subscriptions.push(cx.observe_global::<crate::settings::SettingsModel>(|this, cx| {
            let layout = Self::layout_key(cx);
            if layout != this.layout {
                this.layout = layout;
                this.scroller.update(cx, |scroller, cx| scroller.remeasure(cx));
                cx.notify();
            }
        }));
        subscriptions.push(cx.on_release(|this, cx| {
            let id = this.id;
            this.app.update(cx, |app, _| app.release_timeline(id));
            let url = this.browser.as_ref().and_then(|b| b.read(cx).url().cloned());
            browser::remember(id, browser::Memory { url, open: this.browser_open }, cx);
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
            folds: HashSet::new(),
            folded: HashMap::new(),
            composer,
            minimap,
            find: None,
            questions: None,
            focus: cx.focus_handle(),
            loaded: false,
            layout: Self::layout_key(cx),
            terminal: None,
            split: cx.new(|_| ResizableState::default()),
            browser: None,
            browser_open: false,
            browser_split: cx.new(|_| ResizableState::default()),
            urls: browser::Suggestions::default(),
            shown: true,
            browser_suspend: Task::ready(()),
            turn_started: None,
            stop_requested: None,
            dismissed: None,
            debug_steps: if cfg!(debug_assertions) { std::env::var("ORBIT_DEBUG_THREAD").ok() } else { None },
            debug_questions: false,
            busy: false,
            _ticker: None,
            _subscriptions: subscriptions,
        };
        view.rebuild(true, cx);
        view.sync_thread_record(window, cx);
        if browser::remembered(id, cx).open {
            view.set_browser_open(true, window, cx);
        }
        view
    }

    fn layout_key(cx: &App) -> (ChatWidth, u16, u16, ClockFormat) {
        let a = &crate::settings::get(cx).appearance;
        (a.chat_width, a.ui_scale, a.code_font_size, a.clock)
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
            TerminalPanelEvent::Urls(urls) => this.offer_urls(urls.clone(), cx),
            TerminalPanelEvent::AddToComposer { text, cwd } => {
                let chip = crate::composer::ContextChip::TerminalSelection { text: text.clone(), cwd: Some(cwd.clone()) };
                let composer = this.composer.clone();
                composer.update(cx, |composer, cx| composer.add_context(chip, window, cx));
                Composer::focus(&composer, window, cx);
            }
        });
        self._subscriptions.push(sub);
        let focus = panel.clone();
        window.defer(cx, move |window, cx| TerminalPanel::focus(&focus, window, cx));
        self.terminal = Some(panel);
        cx.notify();
    }

    // ── Preview browser ──────────────────────────────────────────────────

    /// Opens the preview browser beside the conversation, or closes it.
    pub fn toggle_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.browser_open;
        self.set_browser_open(open, window, cx);
        let Some(browser) = self.browser.clone() else { return };
        if open {
            // Nothing to show yet: the address field takes the keyboard.
            if browser.read(cx).url().is_none() {
                window.defer(cx, move |window, cx| browser.update(cx, |b, cx| b.focus_address(window, cx)));
            }
        } else if browser.read(cx).focus_handle(cx).contains_focused(window, cx) {
            Composer::focus(&self.composer.clone(), window, cx);
        }
    }

    /// Opens the preview on `url` (an address the agent or the terminal printed).
    fn open_preview(&mut self, url: Url, window: &mut Window, cx: &mut Context<Self>) {
        self.set_browser_open(true, window, cx);
        if let Some(browser) = &self.browser {
            browser.update(cx, |b, cx| b.navigate(url, window, cx));
        }
    }

    fn set_browser_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open && self.browser.is_none() {
            let browser = cx.new(|cx| BrowserView::new(window, cx));
            if let Some(url) = browser::remembered(self.id, cx).url {
                browser.update(cx, |b, cx| b.navigate(url, window, cx));
            }
            self._subscriptions.push(cx.subscribe(&browser, |this, _, event: &BrowserEvent, cx| {
                if let BrowserEvent::UrlChanged(url) = event {
                    browser::remember(this.id, browser::Memory { url: url.clone(), open: this.browser_open }, cx);
                }
            }));
            self.browser = Some(browser);
            self.push_suggestions(cx);
        }
        self.browser_open = open;
        let url = self.browser.as_ref().and_then(|b| b.read(cx).url().cloned());
        browser::remember(self.id, browser::Memory { url, open }, cx);
        self.sync_browser_visibility(cx);
        cx.notify();
    }

    /// The shell shows this thread (`true`) or another view (`false`). The
    /// page is a native surface: it must be hidden explicitly.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            self.sync_browser_visibility(cx);
        }
    }

    /// Shows the page while the panel is open on screen; a hidden page is
    /// released after [`browser::SUSPEND_AFTER`].
    fn sync_browser_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = self.browser.clone() else { return };
        let visible = self.shown && self.browser_open;
        browser.update(cx, |b, cx| b.set_visible(visible, cx));
        self.browser_suspend = if visible || browser.read(cx).is_suspended() {
            Task::ready(())
        } else {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(browser::SUSPEND_AFTER).await;
                let _ = this.update(cx, |this, cx| {
                    if !(this.shown && this.browser_open)
                        && let Some(browser) = &this.browser
                    {
                        browser.update(cx, |b, cx| b.suspend(cx));
                    }
                });
            })
        };
    }

    /// Remembers dev-server addresses seen in the thread or its terminal.
    fn offer_urls(&mut self, urls: Vec<Url>, cx: &mut Context<Self>) {
        if self.urls.add(urls) {
            self.push_suggestions(cx);
            cx.notify();
        }
    }

    fn push_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = self.browser.clone() else { return };
        let port = self.thread(cx).and_then(|t| t.workspace.port_base);
        let offered = self.urls.offered(port);
        browser.update(cx, |b, cx| b.set_suggested_urls(offered, cx));
    }

    /// Asks the agent to stop; if it has not stopped after a few seconds,
    /// the banner offers Force stop.
    pub fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            return;
        }
        self.app.update(cx, |app, cx| app.run(Command::Interrupt { thread: self.id }, window, cx));
        if self.stop_requested.is_none() {
            self.stop_requested = Some(Instant::now());
        }
    }

    fn force_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(Command::ForceStop { thread: self.id }, window, cx));
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismissed = None;
        self.app.update(cx, |app, cx| app.run(Command::Retry { thread: self.id }, window, cx));
        self.scroller.update(cx, |s, cx| s.scroll_to_end(cx));
    }

    fn snooze_until(&mut self, until: u64, window: &mut Window, cx: &mut Context<Self>) {
        let patch = ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() };
        self.app.update(cx, |app, cx| app.run(Command::UpdateThread { id: self.id, patch }, window, cx));
    }

    fn dismiss_banner(&mut self, cx: &mut Context<Self>) {
        let status = self.thread(cx).map(|t| t.status.clone()).unwrap_or_default();
        self.dismissed = banners::banner(&status, None).map(|b| b.key());
        cx.notify();
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

    /// Keeps the composer, the question panel and the live state in step
    /// with the thread record.
    fn sync_thread_record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let busy = thread.status.is_busy();
        if busy != self.busy {
            self.busy = busy;
            self.turn_started = busy.then(Instant::now);
            self.stop_requested = None;
            // A once-a-second tick only while a turn runs, for the elapsed
            // time and the Stop banner; idle threads cost nothing.
            self._ticker = busy.then(|| {
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                        let alive = this.update(cx, |this, cx| {
                            let last = this.rows.len().saturating_sub(1);
                            this.scroller.update(cx, |s, cx| {
                                let _ = s.remeasure_items(last..last + 1, cx);
                            });
                            cx.notify();
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                })
            });
            self.rebuild(false, cx);
        }
        let pending = questions::pending(&thread);
        if !self.debug_questions && self.questions.as_ref().map(|q| &q.questions) != Some(&pending) {
            let had = self.questions.is_some();
            self.questions = if pending.is_empty() { None } else { Questions::new(pending, window, cx) };
            if !had && let Some(questions) = &self.questions {
                questions.focus(window, cx);
            }
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_running(thread.status.is_busy(), cx);
            composer.set_config(thread.config.clone(), cx);
            composer.set_usage(Some(usage), cx);
            composer.set_context(context, branch, cx);
        });
        cx.notify();
    }

    fn apply_changes(&mut self, changes: &[ItemChange], cx: &mut Context<Self>) {
        let found: Vec<Url> = self
            .app
            .read(cx)
            .state()
            .timeline(self.id)
            .map(|t| {
                changes
                    .iter()
                    .filter_map(|c| match c {
                        ItemChange::Inserted(ix) | ItemChange::Updated(ix) => t.items().get(*ix),
                        ItemChange::Removed(_) => None,
                    })
                    .flat_map(browser::item_urls)
                    .collect()
            })
            .unwrap_or_default();
        if !found.is_empty() {
            self.offer_urls(found, cx);
        }
        let structural = changes.iter().any(|c| matches!(c, ItemChange::Inserted(_) | ItemChange::Removed(_)));
        if structural {
            // Positions shift in a structural batch; the re-layout also
            // brings streaming text views up to date.
            self.rebuild(false, cx);
            return;
        }
        // What changed, without copying the items (a step's output can be
        // 64 KiB; a token must not clone it).
        let mut touched: Vec<(ItemId, Option<String>)> = Vec::new();
        if let Some(timeline) = self.app.read(cx).state().timeline(self.id) {
            for change in changes {
                if let ItemChange::Updated(ix) = change
                    && let Some(item) = timeline.items().get(*ix)
                    && !touched.iter().any(|(id, _)| *id == item.id)
                {
                    let text = match &item.body {
                        ItemBody::Assistant { text } | ItemBody::Reasoning { text } => Some(text.clone()),
                        _ => None,
                    };
                    touched.push((item.id, text));
                }
            }
        }
        let mut remeasure = HashSet::new();
        for (id, text) in &touched {
            if let Some(text) = text {
                self.sync_markdown(*id, text, cx);
            }
            if let Some(&row) = self.item_row.get(id) {
                remeasure.insert(row);
            }
        }
        if self.find.is_some() && !touched.is_empty() {
            self.search(false, cx);
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

    /// Brings a reply's (or thought's) text view up to date, creating it on
    /// first use. A longer text with the old text as its prefix is parsed
    /// incrementally (only the last block).
    fn sync_markdown(&mut self, id: ItemId, text: &str, cx: &mut Context<Self>) {
        match self.markdown.get(&id) {
            Some(state) => state.update(cx, |state, cx| state.set_text(text, cx)),
            None => {
                let state = cx.new(|cx| TextViewState::markdown(text, cx));
                self.markdown.insert(id, state);
            }
        }
    }

    /// Lays the rows out again. The first load resets the list; after that
    /// only the rows that changed are spliced, so heights already measured
    /// and the reader's position are kept.
    fn rebuild(&mut self, full: bool, cx: &mut Context<Self>) {
        // Read the timeline in place (no copy of every item per new step);
        // collect only what needs a mutable context afterwards.
        let (layout, item_row, marks, texts, live, urls) = {
            let state = self.app.read(cx).state();
            let Some(timeline) = state.timeline(self.id) else { return };
            let items = timeline.items();
            let busy = state.threads.get(&self.id).is_some_and(|t| t.status.is_busy());
            let layout = rows::build(items, busy, &self.folds, format::local_day);
            let mut item_row = HashMap::with_capacity(items.len());
            let mut marks = Vec::new();
            for (ix, row) in layout.rows.iter().enumerate() {
                for id in row.items() {
                    item_row.insert(id, ix);
                }
                if let Row::User(id) = row
                    && let Some(Item { body: ItemBody::User(m), .. }) = timeline.get(*id)
                    && !m.steer
                {
                    marks.push(Mark { row: ix, item: *id, preview: minimap::preview(&m.text) });
                }
            }
            // Text views to create, or (after a reload) to bring up to date;
            // streamed updates reach them through `apply_changes`.
            let texts: Vec<(ItemId, String)> = items
                .iter()
                .filter_map(|i| match &i.body {
                    ItemBody::Assistant { text } | ItemBody::Reasoning { text }
                        if full || i.status == ItemStatus::Streaming || !self.markdown.contains_key(&i.id) =>
                    {
                        Some((i.id, text.clone()))
                    }
                    _ => None,
                })
                .collect();
            let live: HashSet<ItemId> = items.iter().map(|i| i.id).collect();
            let urls: Vec<Url> = if full { items.iter().flat_map(browser::item_urls).collect() } else { Vec::new() };
            (layout, item_row, marks, texts, live, urls)
        };
        if !urls.is_empty() {
            self.offer_urls(urls, cx);
        }
        self.loaded = true;
        for (id, text) in texts {
            self.sync_markdown(id, &text, cx);
        }
        let rows = layout.rows;
        self.markdown.retain(|id, _| live.contains(id));
        self.folds.retain(|id| live.contains(id));

        let old = self.rows.clone();
        let reset = full || old.is_empty();
        let change = rows::splice(&old, &rows);
        self.rows = Rc::new(rows);
        self.item_row = item_row;
        self.folded = layout.folded;
        let new_len = self.rows.len();
        self.scroller.update(cx, |scroller, cx| {
            if reset {
                scroller.reset(new_len, cx);
            } else if let Some((range, count)) = change {
                let _ = scroller.splice(range, count, cx);
            }
        });
        self.minimap.update(cx, |minimap, cx| minimap.set_marks(marks, cx));
        if self.find.is_some() {
            self.search(false, cx);
        }
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
            ComposerEvent::Stop => {
                self.stop(window, cx);
                return;
            }
            ComposerEvent::WorktreeChosen(_) => return,
            ComposerEvent::ConfigChanged(config) => Command::UpdateThread {
                id: self.id,
                patch: ThreadPatch { config: Some(config.clone()), ..Default::default() },
            },
        };
        if matches!(event, ComposerEvent::Submit { .. }) {
            self.dismissed = None;
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

    pub(crate) fn toggle_fold(&mut self, key: ItemId, cx: &mut Context<Self>) {
        if !self.folds.remove(&key) {
            self.folds.insert(key);
        }
        self.rebuild(false, cx);
    }

    /// Opens a step in the inspector; `diff` opens it on its changes.
    pub(crate) fn inspect(&mut self, item: ItemId, diff: bool, window: &mut Window, cx: &mut Context<Self>) {
        inspector::open(self.app.clone(), self.id, item, diff, window, cx);
    }

    /// Quotes the selected part of a reply (or all of it) into the composer.
    pub(crate) fn quote_reply(&mut self, item: ItemId, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.markdown.get(&item).map(|md| md.read(cx).selected_text()).unwrap_or_default();
        let text = if selected.trim().is_empty() {
            match self.app.read(cx).state().timeline(self.id).and_then(|t| t.get(item)).map(|i| &i.body) {
                Some(ItemBody::Assistant { text }) => text.clone(),
                _ => return,
            }
        } else {
            selected
        };
        Composer::append_to_draft(&self.composer, &transcript::quote(&text), window, cx);
    }

    /// A new thread with the history up to `item`, opened when ready.
    pub(crate) fn fork_at(&mut self, item: ItemId, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.app.update(cx, |app, cx| app.call(Command::ForkThread { id: self.id, at: item }, cx));
        cx.spawn_in(window, async move |this, cx| match task.await {
            Ok(Reply::Thread(fork)) => {
                let _ = this.update(cx, |_, cx| cx.emit(ThreadViewEvent::Open(fork)));
            }
            Err(message) => {
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(message).title("Couldn’t fork"),
                        cx,
                    )
                });
            }
            Ok(_) => {}
        })
        .detach();
    }

    /// The whole conversation as Markdown, on the clipboard.
    fn copy_markdown(&self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let Some(timeline) = state.timeline(self.id) else { return };
        let Some(thread) = state.threads.get(&self.id) else { return };
        let items = rows::display_order(timeline.items());
        let text = transcript::thread_markdown(display_title(thread), thread.config.provider.label(), &items);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification("Copied the thread as Markdown", cx);
    }

    fn answer_questions(
        &mut self,
        submission: &gpui_kit::component::questionnaire::QuestionnaireSubmission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(questions) = &self.questions else { return };
        for (request, text) in questions.answers(submission) {
            let decision = Decision::Answer { text };
            self.app.update(cx, |app, cx| app.run(Command::Resolve { thread: self.id, request, decision }, window, cx));
        }
        Composer::focus(&self.composer.clone(), window, cx);
    }

    fn skip_questions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(questions) = &self.questions else { return };
        let requests: Vec<RequestId> = questions.questions.iter().map(|q| q.request).collect();
        for request in requests {
            let decision = Decision::Deny { feedback: "The user chose not to answer.".into() };
            self.app.update(cx, |app, cx| app.run(Command::Resolve { thread: self.id, request, decision }, window, cx));
        }
        Composer::focus(&self.composer.clone(), window, cx);
    }

    /// Runs `ORBIT_DEBUG_THREAD` steps: `find:<query>`, `next`, `open-folds`,
    /// `inspect` (the first step with output), `quote` (the last reply),
    /// `top` (scroll to the top), `questions` (the question panel).
    fn run_debug_steps(&mut self, steps: &str, window: &mut Window, cx: &mut Context<Self>) {
        for step in steps.split(';') {
            let items: Vec<Item> = self.app.read(cx).state().timeline(self.id).map(|t| t.items().to_vec()).unwrap_or_default();
            match step.split_once(':') {
                Some(("find", query)) => {
                    self.open_find(window, cx);
                    if let Some(find) = &self.find {
                        find.input.update(cx, |input, cx| input.set_value(query, window, cx));
                    }
                    self.search(true, cx);
                }
                _ if step == "next" => self.step_find(true, cx),
                _ if step == "top" => self.scroller.update(cx, |s, cx| {
                    s.scroll_to_item(0, cx);
                }),
                _ if step == "open-folds" => {
                    self.folds.extend(self.folded.values().copied());
                    self.rebuild(false, cx);
                }
                _ if step == "inspect" => {
                    if let Some(item) = items.iter().find(|i| matches!(&i.body, ItemBody::Tool(c) if c.output.len() > 200)) {
                        self.inspect(item.id, false, window, cx);
                    }
                }
                // The question panel for the thread's questions, answered or
                // not (a restart closes pending ones), to look at it.
                _ if step == "questions" => {
                    let questions: Vec<questions::Question> = items
                        .iter()
                        .filter_map(|i| match &i.body {
                            ItemBody::Request(AgentRequest { id, kind: RequestKind::Question { question, options, multi_select }, .. }) => {
                                Some(questions::Question { request: *id, text: question.clone(), options: options.clone(), multi: *multi_select })
                            }
                            _ => None,
                        })
                        .collect();
                    self.questions = Questions::new(questions, window, cx);
                    self.debug_questions = true;
                    if let Some(q) = &self.questions {
                        q.focus(window, cx);
                    }
                }
                _ if step == "quote" => {
                    if let Some(item) = items.iter().rev().find(|i| matches!(i.body, ItemBody::Assistant { .. })) {
                        self.quote_reply(item.id, window, cx);
                    }
                }
                _ => {}
            }
        }
    }

    // ── Find ─────────────────────────────────────────────────────────────

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            find.input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.focus(window, cx);
            });
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in thread"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::Change => this.search(true, cx),
            InputEvent::PressEnter { shift, .. } => this.step_find(!*shift, cx),
            _ => {}
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(Find {
            input,
            hits: Vec::new(),
            current: None,
            by_item: HashMap::new(),
            painted: HashSet::new(),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.take() {
            for id in find.painted {
                if let Some(md) = self.markdown.get(&id) {
                    md.update(cx, |md, cx| md.clear_range_highlights(cx));
                }
            }
        }
        Composer::focus(&self.composer.clone(), window, cx);
        cx.notify();
    }

    /// Runs the query over the thread. `fresh` (the query changed) picks the
    /// match nearest to where the reader is and shows it; otherwise the
    /// current match is kept by position.
    fn search(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let Some(find) = &self.find else { return };
        let query = find.input.read(cx).value().to_string();
        let mut hits = Vec::new();
        if let Some(needle) = find::Needle::new(&query)
            && let Some(timeline) = self.app.read(cx).state().timeline(self.id)
        {
            for item in rows::display_order(timeline.items()) {
                let rendered = match &item.body {
                    ItemBody::Assistant { .. } => self.markdown.get(&item.id).map(|md| md.read(cx).rendered_text()),
                    _ => None,
                };
                hits.extend(find::search_item(&needle, item, rendered.as_ref().map(|r| r.as_str())));
            }
        }
        let top = self.scroller.read(cx).logical_scroll_top().item_ix;
        let current = if fresh {
            find::nearest(&hits, |id| self.row_of(id), top)
        } else {
            find.current.filter(|&ix| ix < hits.len()).or(if hits.is_empty() { None } else { Some(0) })
        };
        let find = self.find.as_mut().expect("checked above");
        find.by_item.clear();
        for (ix, hit) in hits.iter().enumerate() {
            find.by_item.entry(hit.item).or_default().push(ix);
        }
        find.hits = hits;
        find.current = current;
        self.paint_replies(cx);
        if fresh && let Some(ix) = current {
            self.reveal(ix, cx);
        }
        cx.notify();
    }

    /// The row an item is drawn in, or its fold's row while folded.
    fn row_of(&self, item: ItemId) -> usize {
        self.item_row
            .get(&item)
            .or_else(|| self.folded.get(&item).and_then(|key| self.item_row.get(key)))
            .copied()
            .unwrap_or(0)
    }

    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else { return };
        find.current = find::step(find.current, find.hits.len(), forward);
        let Some(ix) = find.current else { return };
        self.paint_replies(cx);
        self.reveal(ix, cx);
        cx.notify();
    }

    /// Paints the matches inside replies (their text views draw them).
    fn paint_replies(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else { return };
        let theme = cx.theme().clone();
        let mut painted = HashSet::new();
        for (item, ixs) in &find.by_item {
            let Some(md) = self.markdown.get(item) else { continue };
            let highlights: Vec<RangeHighlight> = ixs
                .iter()
                .filter(|&&ix| find.hits[ix].part == find::Part::Body)
                .map(|&ix| {
                    let current = Some(ix) == find.current;
                    RangeHighlight::new(find.hits[ix].range.clone(), theme.warning.opacity(if current { 0.55 } else { 0.22 }))
                })
                .collect();
            if highlights.is_empty() {
                continue;
            }
            if md.update(cx, |md, cx| md.set_range_highlights(highlights, cx)).is_ok() {
                painted.insert(*item);
            }
        }
        for stale in find.painted.difference(&painted) {
            if let Some(md) = self.markdown.get(stale) {
                md.update(cx, |md, cx| md.clear_range_highlights(cx));
            }
        }
        find.painted = painted;
    }

    /// Brings a match into view: opens the fold, work log or long message
    /// it is hidden in, scrolls to its row and, inside a reply, to its line.
    fn reveal(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(hit) = self.find.as_ref().and_then(|f| f.hits.get(ix)).cloned() else { return };
        if let Some(key) = self.folded.get(&hit.item).copied() {
            self.folds.insert(key);
            self.rebuild(false, cx);
        }
        let Some(&row) = self.item_row.get(&hit.item) else { return };
        let remeasure = match self.rows.get(row) {
            // Open the log (inside an opened fold it is always open).
            Some(Row::Work { items, nested }) => {
                let key = items[0];
                !*nested && self.expanded.insert(key)
            }
            Some(Row::User(_)) => self.expanded.insert(hit.item),
            _ => false,
        };
        if remeasure {
            self.scroller.update(cx, |s, cx| {
                let _ = s.remeasure_items(row..row + 1, cx);
            });
        }
        // Inside a laid-out reply, place the match's line near the top: its
        // height is known, and the match sits about as far down as it is
        // into the text. Elsewhere the row comes to the top.
        let within = self
            .markdown
            .get(&hit.item)
            .filter(|_| matches!(self.rows.get(row), Some(Row::Assistant(_))))
            .and_then(|md| {
                let md = md.read(cx);
                let height = md.bounds().size.height;
                let len = md.rendered_text().len().max(1);
                (height > px(0.)).then(|| height * (hit.range.start as f32 / len as f32))
            });
        let top = self.scroller.read(cx).logical_scroll_top();
        let offset = within.map(|y| (y - px(72.)).max(px(0.))).unwrap_or(px(0.));
        // Already in view (same row, within the first screenful below the
        // top edge): leave the page still.
        let visible = top.item_ix == row
            && within.is_some_and(|y| y >= top.offset_in_item && y < top.offset_in_item + px(360.));
        if !visible {
            self.scroller.update(cx, |s, cx| {
                s.scroll_to_offset(ListOffset { item_ix: row, offset_in_item: offset }, cx);
            });
        }
    }

    /// Find matches inside one part of an item, with the current one marked.
    pub(crate) fn hits(&self, item: ItemId, part: find::Part) -> Vec<(std::ops::Range<usize>, bool)> {
        let Some(find) = &self.find else { return Vec::new() };
        let Some(ixs) = find.by_item.get(&item) else { return Vec::new() };
        ixs.iter()
            .filter(|&&ix| find.hits[ix].part == part)
            .map(|&ix| (find.hits[ix].range.clone(), Some(ix) == find.current))
            .collect()
    }

    fn render_find(&self, find: &Find, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let has_query = !find.input.read(cx).value().trim().is_empty();
        let count = match (has_query, find.hits.len(), find.current) {
            (false, _, _) => String::new(),
            (true, 0, _) => "No matches".to_owned(),
            (true, n, Some(ix)) => format!("{} of {n}", ix + 1),
            (true, n, None) => format!("{n}"),
        };
        let none = has_query && find.hits.is_empty();
        let bar = h_flex()
            .id("find-bar")
            .w(rems(24.))
            .p_1()
            .gap_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_find(window, cx);
                }
            }))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&find.input)
                        .small()
                        .appearance(false)
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(theme.muted_foreground)),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(if none { theme.danger } else { theme.muted_foreground })
                    .child(count),
            )
            .child(
                Button::new("find-previous")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .tooltip("Previous match (Shift+Enter)")
                    .disabled(find.hits.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(false, cx))),
            )
            .child(
                Button::new("find-next")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip("Next match (Enter)")
                    .disabled(find.hits.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(true, cx))),
            )
            .child(
                Button::new("find-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            );
        // In the flow above the timeline, not over it: a match is never
        // hidden under the bar.
        h_flex().flex_none().w_full().justify_end().px_4().pt_2().pb_1().child(bar).into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let browser_button = self.render_browser_button(cx);
        let state = self.app.read(cx).state();
        let Some(thread) = state.threads.get(&self.id) else { return h_flex() };
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
        let usage = state.usage(self.id);
        let model = ui::model_label(&thread.config, &state.providers);
        let id = self.id;
        let view = cx.entity().downgrade();
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
                Button::new("find")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip_with_action("Find in thread", &FindInThread, Some("Thread"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_find(window, cx))),
            )
            .child(browser_button)
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
                Button::new("thread-menu").ghost().small().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, cx| {
                    let view = view.clone();
                    crate::thread_menu::build(menu, &app, &menu_thread, path.clone(), cx).separator().item(
                        PopupMenuItem::new("Copy thread as Markdown").on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.copy_markdown(window, cx));
                        }),
                    )
                })
            })
    }

    /// The header's browser control: "Open preview localhost:3000" once the
    /// agent or the terminal printed an address and the panel is closed;
    /// otherwise a toggle.
    fn render_browser_button(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.urls.latest().filter(|_| !self.browser_open).cloned() {
            Some(url) => {
                let host = orbit_browser::host_label(&url);
                Button::new("open-preview")
                    .ghost()
                    .small()
                    .icon(IconName::Globe)
                    .label(format!("Open preview {host}"))
                    .tooltip(format!("Show {url} beside the conversation"))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_preview(url.clone(), window, cx)))
                    .into_any_element()
            }
            None => Button::new("toggle-browser")
                .ghost()
                .small()
                .icon(IconName::Globe)
                .selected(self.browser_open)
                .tooltip_with_action(
                    if self.browser_open { "Close the preview browser" } else { "Preview browser" },
                    &ToggleBrowser,
                    Some("Thread"),
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_browser(window, cx)))
                .into_any_element(),
        }
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.thread(cx)?.status.clone();
        let banner = banners::banner(&status, self.stop_requested.map(|at| at.elapsed()))?;
        if self.dismissed.as_deref() == Some(banner.key().as_str()) {
            return None;
        }
        Some(banners::render(&banner, cx))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loaded && !self.rows.is_empty() && let Some(steps) = self.debug_steps.take() {
            let view = cx.entity();
            window.defer(cx, move |window, cx| view.update(cx, |this, cx| this.run_debug_steps(&steps, window, cx)));
        }
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
        // Rows carry their own spacing (a reply sits closer to its steps
        // than to the next message), so the list adds only a hairline.
        .with_row_style(StyleRefinement::default().pb_1())
        .flex_1()
        .min_h_0();

        let column = crate::settings::chat_column(cx);
        let find_bar = self.find.as_ref().map(|find| self.render_find(find, cx));
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
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .children(find_bar)
                    .child(timeline)
                    .child(self.minimap.clone())
                    .into_any_element()
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
                            .max_w(column)
                            .children(self.questions.as_ref().map(|q| q.render(cx)))
                            .children(self.render_banner(cx))
                            .child(crate::composer::QueuePanel::new(self.id, &self.composer))
                            .child(self.composer.clone()),
                    ),
            );
        v_flex()
            .size_full()
            .key_context("Thread")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FindInThread, window, cx| this.open_find(window, cx)))
            // Ctrl+F inside a text field without its own search (the
            // composer) arrives as the field's Search action.
            .on_action(cx.listener(|this, _: &gpui_kit::component::input::Search, window, cx| this.open_find(window, cx)))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step_find(true, cx)))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| this.step_find(false, cx)))
            .on_action(cx.listener(|this, _: &PreviousMessage, _, cx| {
                this.minimap.update(cx, |m, cx| m.step(false, cx));
            }))
            .on_action(cx.listener(|this, _: &NextMessage, _, cx| {
                this.minimap.update(cx, |m, cx| m.step(true, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleBrowser, window, cx| this.toggle_browser(window, cx)))
            .child(self.render_header(cx))
            .child({
                let main = match &self.terminal {
                    None => conversation.into_any_element(),
                    Some(terminal) => v_resizable("thread-split")
                        .with_state(&self.split)
                        .child(resizable_panel().child(conversation))
                        .child(resizable_panel().size(px(300.)).size_range(px(120.)..px(900.)).child(terminal.clone()))
                        .into_any_element(),
                };
                let body = div().flex_1().min_h_0();
                match self.browser.as_ref().filter(|_| self.browser_open) {
                    None => body.child(main),
                    // The page beside the thread, so the reply and its
                    // result are seen together.
                    Some(browser) => body.child(
                        h_resizable("thread-browser-split")
                            .with_state(&self.browser_split)
                            .child(resizable_panel().size_range(px(360.)..Pixels::MAX).child(main))
                            .child(
                                resizable_panel()
                                    .size(px(560.))
                                    .size_range(px(320.)..px(1600.))
                                    .child(div().size_full().border_l_1().border_color(theme.border).child(browser.clone())),
                            ),
                    ),
                }
            })
    }
}
