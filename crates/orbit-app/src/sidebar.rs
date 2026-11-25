//! The sidebar: what needs you, what is working, and everything else.
//!
//! Threads live in sections by what they need from you, in this order:
//! Pinned, Needs you, Working, Recent, then the Snoozed and Settled shelves.
//! When a thread changes section (it starts working, it asks a question, it
//! finishes) its row arrives in the new section with a short slide and fade,
//! so the eye can follow it; rows that did not move never animate. While the
//! pointer is over the list nothing moves at all: rows keep their places
//! (their content stays live) and the list settles once the pointer leaves.
//! An alternative layout groups threads by project, each project row
//! carrying an activity strip: how many of its threads need you, failed,
//! work or have news.
//!
//! * Click opens a thread; Cmd/Ctrl-click opens it beside the open one as a
//!   tile; Shift-click selects a range and Cmd/Ctrl-Shift-click adds or
//!   removes one row. A selection gets a bar of bulk actions.
//! * Hovering a row a moment peeks into it: the latest reply, the pending
//!   question with its answers, and a field to reply without opening it.
//!
//! Rows are virtualized (a uniform list), so a thousand threads cost what is
//! visible. Every row has the same lanes: status, title, project, age.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::TimeZone as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::component::{VirtualListScrollHandle, v_virtual_list};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::ui::{self, Attention, Glyph, StatusGlyph};
use crate::{format, prefs, thread::display_title, thread_menu};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Pinned,
    NeedsYou,
    Working,
    Recent,
    Snoozed,
    Settled,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Pinned => "Pinned",
            Section::NeedsYou => "Needs you",
            Section::Working => "Working",
            Section::Recent => "Recent",
            Section::Snoozed => "Snoozed",
            Section::Settled => "Settled",
        }
    }

    fn of(thread: &Thread) -> Section {
        if thread.settled_ms.is_some() {
            return Section::Settled;
        }
        if thread.snoozed_until.is_some() {
            return Section::Snoozed;
        }
        match Attention::of(thread) {
            Attention::Waiting | Attention::Failed | Attention::RateLimited => Section::NeedsYou,
            _ if thread.pinned => Section::Pinned,
            Attention::Working => Section::Working,
            Attention::Unread | Attention::Idle => Section::Recent,
        }
    }

    /// Shelves start closed; they are reference, not work.
    fn open_by_default(self) -> bool {
        !matches!(self, Section::Snoozed | Section::Settled)
    }
}

#[derive(Clone, Debug)]
enum Entry {
    Section { section: Section, count: usize, open: bool },
    Project { id: Option<ProjectId>, name: String, count: usize, open: bool },
    /// A thread row; `compact` rows are one line (shelves).
    Thread { id: ThreadId, compact: bool },
}

impl Entry {
    /// Row height in rems; the virtual list needs every height up front.
    fn height(&self) -> f32 {
        match self {
            Entry::Section { .. } | Entry::Project { .. } => 2.0,
            Entry::Thread { compact: true, .. } => 2.0,
            Entry::Thread { compact: false, .. } => 3.25,
        }
    }
}

/// What a thread needs, in a few words, and the color that carries it.
fn status_line(thread: &Thread, state: &orbit_net::state::ClientState, theme: &gpui_kit::component::Theme) -> (String, Hsla) {
    let muted = theme.muted_foreground;
    if let Some(pending) = thread.pending.first() {
        let text = match &pending.request.kind {
            RequestKind::Question { .. } => "Has a question for you".to_owned(),
            kind => ui::request_line(kind),
        };
        return (text, theme.warning);
    }
    match &thread.status {
        ThreadStatus::Starting if thread.workspace.worktree.is_none() => ("Setting up the worktree…".into(), muted),
        ThreadStatus::Running | ThreadStatus::Starting => ("Working…".into(), muted),
        ThreadStatus::Failed { message } => (format!("Failed · {message}"), theme.danger),
        ThreadStatus::RateLimited { resets_ms } => (format::resets_at(*resets_ms, now_ms()), theme.warning),
        _ if thread.snoozed_until.is_some() => {
            let until = thread.snoozed_until.unwrap_or_default();
            let local = chrono::Local
                .timestamp_millis_opt(until as i64)
                .single()
                .map(|t| t.format("%a %H:%M").to_string())
                .unwrap_or_default();
            (format!("Snoozed until {local}"), muted)
        }
        _ if thread.unread => ("New result".into(), theme.primary),
        _ => {
            let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
            let model = ui::model_label(&thread.config, &state.providers);
            match project {
                Some(p) => (format!("{} · {}", p.name, model), muted),
                None => (model, muted),
            }
        }
    }
}

/// A row that just changed section: animate its arrival.
#[derive(Clone, Copy)]
struct Move {
    generation: u32,
    /// Arrived from a section above (slides down) or below (slides up).
    from_above: bool,
    at: Instant,
}

const MOVE_DURATION: Duration = Duration::from_millis(220);

/// Rows chosen for a bulk action.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Selection {
    pub ids: Vec<ThreadId>,
    /// Where a Shift-click range starts.
    pub anchor: Option<ThreadId>,
}

impl Selection {
    /// Shift-click: everything between the anchor (or `from`, the open
    /// thread) and `to`, in list order.
    pub fn select_range(&mut self, order: &[ThreadId], from: Option<ThreadId>, to: ThreadId) {
        let anchor = self.anchor.or(from).unwrap_or(to);
        let (Some(a), Some(b)) = (order.iter().position(|t| *t == anchor), order.iter().position(|t| *t == to)) else {
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        self.ids = order[lo..=hi].to_vec();
        self.anchor = Some(anchor);
    }

    /// Cmd/Ctrl-Shift-click: one row in or out. Taking the anchor out moves
    /// the anchor to the nearest row still selected, so the next range
    /// starts from something the user can see is selected.
    pub fn toggle(&mut self, order: &[ThreadId], id: ThreadId) {
        if let Some(ix) = self.ids.iter().position(|t| *t == id) {
            self.ids.remove(ix);
            if self.anchor == Some(id) {
                let at = order.iter().position(|t| *t == id).unwrap_or(0);
                self.anchor = order
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| self.ids.contains(t))
                    .min_by_key(|(ix, _)| ix.abs_diff(at))
                    .map(|(_, t)| *t);
            }
        } else {
            self.ids.push(id);
            self.anchor = Some(id);
        }
    }

    /// Drops rows that are no longer listed.
    pub fn retain(&mut self, live: &HashSet<ThreadId>) {
        self.ids.retain(|t| live.contains(t));
        if self.anchor.is_some_and(|a| !live.contains(&a)) {
            self.anchor = self.ids.first().copied();
        }
    }
}

pub struct Sidebar {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    /// The live-agent mini-map above the footer.
    constellation: Entity<crate::teams::Constellation>,
    by_project: bool,
    closed_sections: HashSet<Section>,
    opened_shelves: HashSet<Section>,
    closed_projects: HashSet<Option<ProjectId>>,
    scroll: VirtualListScrollHandle,
    entries: Vec<Entry>,
    search: Entity<InputState>,
    query: String,
    sections: HashMap<ThreadId, Section>,
    moves: HashMap<ThreadId, Move>,
    /// The pointer is over the list: rows keep their places.
    hovered: bool,
    /// A rebuild waits for the pointer to leave.
    stale: bool,
    selection: Selection,
    /// The thread whose peek is open, with its timeline held.
    peeking: Option<ThreadId>,
    reply: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    _clock: Task<()>,
}

impl Sidebar {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&app, |this, _, cx| {
            if this.hovered {
                this.stale = true;
            } else {
                this.rebuild(cx);
            }
            cx.notify();
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search threads").clean_on_escape());
        let search_sub = cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = input.read(cx).value().trim().to_lowercase();
                this.rebuild(cx);
                cx.notify();
            }
        });
        let reply = cx.new(|cx| InputState::new(window, cx).placeholder("Reply…"));
        let reply_sub = cx.subscribe_in(&reply, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.send_reply(window, cx);
            }
        });
        // Relative ages ("4m") stay honest with one cheap render a minute.
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(60)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let prefs = prefs::get(cx).clone();
        let toggled = |s: Section| prefs.toggled_sections.iter().any(|name| name == s.label());
        let all = [Section::Pinned, Section::NeedsYou, Section::Working, Section::Recent, Section::Snoozed, Section::Settled];
        let constellation = {
            let (app, workspace) = (app.clone(), workspace.clone());
            cx.new(|cx| crate::teams::Constellation::new(app, workspace, crate::teams::ConstellationMode::Mini, cx))
        };
        let mut sidebar = Self {
            app,
            workspace,
            constellation,
            by_project: prefs.group_by_project,
            closed_sections: all.into_iter().filter(|s| s.open_by_default() && toggled(*s)).collect(),
            opened_shelves: all.into_iter().filter(|s| !s.open_by_default() && toggled(*s)).collect(),
            closed_projects: HashSet::new(),
            scroll: VirtualListScrollHandle::new(),
            entries: Vec::new(),
            search,
            query: String::new(),
            sections: HashMap::new(),
            moves: HashMap::new(),
            hovered: false,
            stale: false,
            selection: Selection::default(),
            peeking: None,
            reply,
            _subscriptions: vec![sub, search_sub, reply_sub],
            _clock: clock,
        };
        sidebar.rebuild(cx);
        sidebar
    }

    fn is_open(&self, section: Section) -> bool {
        if section.open_by_default() {
            !self.closed_sections.contains(&section)
        } else {
            self.opened_shelves.contains(&section)
        }
    }

    /// Thread rows in the order the list shows them: what next and previous
    /// thread, Cmd/Ctrl-1–9 and Shift-click ranges walk.
    pub fn thread_order(&self, _: &App) -> Vec<ThreadId> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Thread { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// Clears a multi-selection; false when there was none.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if self.selection.ids.is_empty() {
            return false;
        }
        self.selection = Selection::default();
        cx.notify();
        true
    }

    /// Debug builds: selects the first `n` rows, for screenshots.
    #[cfg(debug_assertions)]
    pub fn debug_select(&mut self, n: usize, cx: &mut Context<Self>) {
        let order = self.thread_order(cx);
        self.selection = Selection { ids: order.iter().take(n).copied().collect(), anchor: order.first().copied() };
        cx.notify();
    }

    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.hovered = hovered;
        if !hovered && self.stale {
            self.stale = false;
            self.rebuild(cx);
            cx.notify();
        }
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let mut threads: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));

        // Track section changes for arrival animations.
        let now = Instant::now();
        let first_build = self.sections.is_empty();
        for thread in &threads {
            let section = Section::of(thread);
            match self.sections.insert(thread.id, section) {
                Some(previous) if previous != section && !first_build => {
                    let generation = self.moves.get(&thread.id).map_or(0, |m| m.generation + 1);
                    self.moves.insert(thread.id, Move { generation, from_above: previous < section, at: now });
                }
                _ => {}
            }
        }
        let live: HashSet<ThreadId> = threads.iter().map(|t| t.id).collect();
        self.sections.retain(|id, _| live.contains(id));
        self.moves.retain(|id, m| live.contains(id) && now.duration_since(m.at) < MOVE_DURATION * 2);
        self.selection.retain(&live);

        let mut entries = Vec::new();
        if !self.query.is_empty() {
            let query = &self.query;
            let hits: Vec<&&Thread> = threads
                .iter()
                .filter(|t| {
                    let project = t.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
                    t.title.to_lowercase().contains(query)
                        || project.is_some_and(|p| p.name.to_lowercase().contains(query))
                        || t.workspace.branch.as_ref().is_some_and(|b| b.to_lowercase().contains(query))
                })
                .collect();
            entries.extend(hits.iter().map(|t| Entry::Thread { id: t.id, compact: false }));
        } else if self.by_project {
            let mut groups: Vec<(Option<ProjectId>, String)> =
                state.projects.iter().map(|p| (Some(p.id), p.name.clone())).collect();
            groups.push((None, "No project".into()));
            for (project, name) in groups {
                let in_group: Vec<&&Thread> =
                    threads.iter().filter(|t| t.project == project && t.settled_ms.is_none()).collect();
                if in_group.is_empty() && project.is_none() {
                    continue;
                }
                let open = !self.closed_projects.contains(&project);
                entries.push(Entry::Project { id: project, name, count: in_group.len(), open });
                if open {
                    let mut in_group = in_group;
                    in_group.sort_by_key(|t| (!t.pinned, std::cmp::Reverse(t.updated_ms)));
                    entries.extend(in_group.iter().map(|t| Entry::Thread { id: t.id, compact: false }));
                }
            }
        } else {
            for section in [Section::Pinned, Section::NeedsYou, Section::Working, Section::Recent, Section::Snoozed, Section::Settled] {
                let in_section: Vec<&&Thread> = threads.iter().filter(|t| Section::of(t) == section).collect();
                if in_section.is_empty() {
                    continue;
                }
                let open = self.is_open(section);
                entries.push(Entry::Section { section, count: in_section.len(), open });
                if open {
                    let compact = matches!(section, Section::Snoozed | Section::Settled);
                    entries.extend(in_section.iter().map(|t| Entry::Thread { id: t.id, compact }));
                }
            }
        }
        self.entries = entries;
    }

    fn save_layout(&self, cx: &mut App) {
        let toggled: Vec<String> = self
            .closed_sections
            .iter()
            .chain(self.opened_shelves.iter())
            .map(|s| s.label().to_owned())
            .collect();
        let by_project = self.by_project;
        prefs::update(cx, |p| {
            p.toggled_sections = toggled;
            p.toggled_sections.sort();
            p.group_by_project = by_project;
        });
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let set = if section.open_by_default() { &mut self.closed_sections } else { &mut self.opened_shelves };
        if !set.remove(&section) {
            set.insert(section);
        }
        self.rebuild(cx);
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        if !self.closed_projects.remove(&project) {
            self.closed_projects.insert(project);
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn navigate(&self, route: Route, window: &mut Window, cx: &mut App) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(route, window, cx));
    }

    fn current(&self, cx: &App) -> Option<Route> {
        self.workspace.upgrade().map(|ws| ws.read(cx).route().clone())
    }

    /// A click on a thread row, by its modifiers.
    fn click_thread(&mut self, id: ThreadId, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.modifiers();
        let command = if cfg!(target_os = "macos") { m.platform } else { m.control };
        let order = self.thread_order(cx);
        match (command, m.shift) {
            (true, true) => {
                self.selection.toggle(&order, id);
                cx.notify();
            }
            (false, true) => {
                let from = match self.current(cx) {
                    Some(Route::Thread(current)) => Some(current),
                    _ => None,
                };
                self.selection.select_range(&order, from, id);
                cx.notify();
            }
            (true, false) => {
                self.selection = Selection::default();
                let _ = self.workspace.update(cx, |ws, cx| ws.open_beside(id, window, cx));
            }
            (false, false) => {
                self.selection = Selection::default();
                self.navigate(Route::Thread(id), window, cx);
            }
        }
    }

    // ── Bulk actions ─────────────────────────────────────────────────────

    fn selected_threads(&self, cx: &App) -> Vec<Thread> {
        let state = self.app.read(cx).state();
        self.selection.ids.iter().filter_map(|id| state.threads.get(id).cloned()).collect()
    }

    fn bulk_patch(&mut self, patch: ThreadPatch, window: &mut Window, cx: &mut Context<Self>) {
        for thread in self.selected_threads(cx) {
            let patch = patch.clone();
            self.app.update(cx, |app, cx| app.run(Command::UpdateThread { id: thread.id, patch }, window, cx));
        }
        self.selection = Selection::default();
        cx.notify();
    }

    fn bulk_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<ThreadId> = self.selection.ids.clone();
        let n = ids.len();
        let app = self.app.clone();
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (app, ids, this) = (app.clone(), ids.clone(), this.clone());
            alert
                .title(format!("Delete {n} thread{}?", if n == 1 { "" } else { "s" }))
                .description("Their messages, attachments and checkpoints are removed from this computer. Files in your projects are not touched.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    for id in &ids {
                        app.update(cx, |app, cx| app.run(Command::DeleteThread { id: *id }, window, cx));
                    }
                    let _ = this.update(cx, |this, cx| {
                        this.selection = Selection::default();
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn open_selection_as_tiles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<ThreadId> = self.selection.ids.iter().take(crate::tiles::MAX_TILES).copied().collect();
        self.selection = Selection::default();
        let Some(first) = ids.first().copied() else { return };
        let _ = self.workspace.update(cx, |ws, cx| {
            ws.navigate(Route::Thread(first), window, cx);
            for id in ids.iter().skip(1) {
                ws.open_beside(*id, window, cx);
            }
        });
        cx.notify();
    }

    fn render_selection_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let n = self.selection.ids.len();
        let any_unread = self.selected_threads(cx).iter().any(|t| t.unread);
        let action = |id: &'static str, icon: Icon, label: &'static str| Button::new(id).ghost().xsmall().icon(icon).tooltip(label);
        v_flex()
            .flex_none()
            .mx_2()
            .mb_2()
            .p_1p5()
            .gap_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_md()
            .child(
                h_flex()
                    .px_1()
                    .gap_2()
                    .text_xs()
                    .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(format!("{n} selected")))
                    .child(
                        Button::new("bulk-clear")
                            .ghost()
                            .xsmall()
                            .label("Clear")
                            .tooltip_with_action("Clear the selection", &ClearSelection, None)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_selection(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        action("bulk-tiles", Icon::new(Lucide::LayoutGrid), "Open side by side")
                            .disabled(n < 2)
                            .on_click(cx.listener(|this, _, window, cx| this.open_selection_as_tiles(window, cx))),
                    )
                    .child(action("bulk-settle", Icon::new(Lucide::ListChecks), "Settle").on_click(cx.listener(|this, _, window, cx| {
                        this.bulk_patch(ThreadPatch { settled: Some(true), ..Default::default() }, window, cx)
                    })))
                    .child({
                        let this = cx.weak_entity();
                        action("bulk-snooze", Icon::new(Lucide::Clock), "Snooze").dropdown_menu(move |mut menu, _, _| {
                            for (label, until) in thread_menu::snooze_presets() {
                                let this = this.clone();
                                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                    let _ = this.update(cx, |this, cx| {
                                        this.bulk_patch(ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() }, window, cx)
                                    });
                                }));
                            }
                            menu
                        })
                    })
                    .child(
                        action("bulk-read", Icon::new(Lucide::CheckCheck), "Mark read").disabled(!any_unread).on_click(cx.listener(
                            |this, _, window, cx| this.bulk_patch(ThreadPatch { unread: Some(false), ..Default::default() }, window, cx),
                        )),
                    )
                    .child(action("bulk-archive", Icon::new(Lucide::Archive), "Archive").on_click(cx.listener(|this, _, window, cx| {
                        this.bulk_patch(ThreadPatch { archived: Some(true), ..Default::default() }, window, cx)
                    })))
                    .child(div().flex_1())
                    .child(
                        action("bulk-delete", Icon::new(Lucide::Trash), "Delete…")
                            .on_click(cx.listener(|this, _, window, cx| this.bulk_delete(window, cx))),
                    ),
            )
    }

    // ── Peek ─────────────────────────────────────────────────────────────

    /// A peek opened or closed: hold the thread's timeline while it shows.
    fn peek(&mut self, id: ThreadId, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open {
            if self.peeking == Some(id) {
                return;
            }
            if let Some(old) = self.peeking.replace(id) {
                self.app.update(cx, |app, _| app.release_timeline(old));
            }
            self.app.update(cx, |app, cx| app.retain_timeline(id, cx));
            let placeholder = match self.app.read(cx).state().threads.get(&id) {
                Some(t) => match t.pending.first().map(|p| &p.request.kind) {
                    Some(RequestKind::Question { .. }) => "Answer…",
                    Some(_) => "Tell the agent what to do instead…",
                    None if t.status.is_busy() => "Queue a message…",
                    None => "Reply…",
                },
                None => "Reply…",
            };
            self.reply.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.set_placeholder(placeholder, window, cx);
            });
        } else if self.peeking == Some(id) {
            self.peeking = None;
            self.app.update(cx, |app, _| app.release_timeline(id));
        }
        cx.notify();
    }

    fn send_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.peeking else { return };
        let text = self.reply.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(thread) = self.app.read(cx).state().threads.get(&id).cloned() else { return };
        let command = match thread.pending.first() {
            Some(p) if matches!(p.request.kind, RequestKind::Question { .. }) => {
                Command::Resolve { thread: id, request: p.request.id, decision: Decision::Answer { text } }
            }
            Some(p) => Command::Resolve { thread: id, request: p.request.id, decision: Decision::Deny { feedback: text } },
            None => Command::Send { thread: id, text, attachments: Vec::new(), steer: false },
        };
        self.app.update(cx, |app, cx| app.run(command, window, cx));
        self.reply.update(cx, |input, cx| input.set_value("", window, cx));
        window.push_notification(gpui_kit::component::notification::Notification::success("Sent to the agent."), cx);
    }

    fn render_peek(&self, id: ThreadId, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let state = self.app.read(cx).state();
        let Some(thread) = state.threads.get(&id) else { return div().into_any_element() };
        let (status, color) = status_line(thread, state, theme);
        let latest = state.timeline(id).and_then(|timeline| {
            timeline.items().iter().rev().find_map(|item| match &item.body {
                ItemBody::Assistant { text } if !text.trim().is_empty() => Some(tail(text, 6, 480)),
                _ => None,
            })
        });
        let pending = thread.pending.first();
        let app = self.app.clone();
        let answers: Vec<(String, Decision)> = match pending.map(|p| &p.request.kind) {
            Some(RequestKind::Question { options, multi_select: false, .. }) => {
                options.iter().take(6).map(|o| (o.label.clone(), Decision::Answer { text: o.label.clone() })).collect()
            }
            Some(RequestKind::Question { .. }) => Vec::new(),
            Some(RequestKind::Plan { .. }) => vec![
                ("Approve plan".into(), Decision::Allow),
                ("Keep planning".into(), Decision::Deny { feedback: String::new() }),
            ],
            Some(_) => vec![
                ("Allow".into(), Decision::Allow),
                ("Allow for this session".into(), Decision::AllowForSession),
                ("Deny".into(), Decision::Deny { feedback: String::new() }),
            ],
            None => Vec::new(),
        };
        let request = pending.map(|p| p.request.id);
        // The status line already names the request; this is its substance.
        let question = pending.map(|p| match &p.request.kind {
            RequestKind::Question { question, .. } => crate::inbox::clip(question, 240),
            RequestKind::Command { command, .. } => crate::inbox::clip(command, 200),
            RequestKind::Plan { plan } => crate::inbox::clip(plan, 240),
            RequestKind::FileChange { path, .. } => path.clone(),
            RequestKind::Tool { input, .. } => crate::inbox::clip(input, 200),
        });
        v_flex()
            .w(rems(22.))
            .gap_2()
            .p_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(StatusGlyph::new(("peek-glyph", id.0), thread, state))
                    .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(display_title(thread).to_owned())),
            )
            .child(div().text_xs().text_color(color).line_clamp(2).child(status))
            .when_some(latest, |el, text| {
                el.child(
                    div()
                        .px_2p5()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.foreground)
                        .line_clamp(6)
                        .child(text),
                )
            })
            .when_some(question, |el, q| el.child(div().text_sm().line_clamp(4).child(q)))
            .when(!answers.is_empty(), |el| {
                el.child(h_flex().flex_wrap().gap_1().children(answers.into_iter().enumerate().map(|(n, (label, decision))| {
                    let app = app.clone();
                    let button = Button::new(("peek-answer", id.0 ^ ((n as u64 + 1) << 56))).xsmall().label(label).on_click(
                        move |_, window, cx| {
                            if let Some(request) = request {
                                let decision = decision.clone();
                                app.update(cx, |app, cx| app.run(Command::Resolve { thread: id, request, decision }, window, cx));
                            }
                        },
                    );
                    if n == 0 { button.primary() } else { button.outline() }
                })))
            })
            .child(Input::new(&self.reply).small())
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Enter sends · click the row to open"),
            )
            .into_any_element()
    }

    // ── Rows ─────────────────────────────────────────────────────────────

    /// A project row's activity strip: how many of its threads need you,
    /// failed, work or have news, as glyphs with counts.
    fn activity_strip(&self, project: Option<ProjectId>, cx: &App) -> Div {
        let theme = cx.theme();
        let state = self.app.read(cx).state();
        let mut counts: Vec<(Glyph, usize)> = Vec::new();
        for thread in state.threads.values().filter(|t| t.project == project && !t.archived && t.settled_ms.is_none()) {
            let glyph = match Glyph::live(thread, state) {
                Glyph::Approval => Glyph::Question,
                Glyph::Idle | Glyph::Settled => continue,
                glyph => glyph,
            };
            match counts.iter_mut().find(|(g, _)| *g == glyph) {
                Some((_, n)) => *n += 1,
                None => counts.push((glyph, 1)),
            }
        }
        let rank = |g: &Glyph| match g {
            Glyph::Question => 0,
            Glyph::Failed => 1,
            Glyph::RateLimited => 2,
            Glyph::Working | Glyph::Quiet | Glyph::Unverified => 3,
            _ => 4,
        };
        counts.sort_by_key(|(g, _)| rank(g));
        let key = project.map_or(0, |p| p.0);
        h_flex().gap_1p5().children(counts.into_iter().map(|(glyph, n)| {
            h_flex()
                .gap_0p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(StatusGlyph::glyph(ElementId::NamedInteger(format!("strip-{glyph:?}").into(), key), glyph))
                .child(n.to_string())
        }))
    }

    fn render_entries(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let current = self.current(cx);
        let tiles: Vec<ThreadId> = self.workspace.upgrade().map(|ws| ws.read(cx).tiles().to_vec()).unwrap_or_default();
        let now = now_ms();
        let mut out = Vec::with_capacity(range.len());
        for ix in range {
            let Some(entry) = self.entries.get(ix).cloned() else { continue };
            let el: AnyElement = match entry {
                Entry::Section { section, count, open } => h_flex()
                    .id(("section", section as usize))
                    .h_8()
                    .px_2()
                    .gap_1p5()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if section == Section::NeedsYou { theme.warning } else { theme.muted_foreground })
                    .cursor_default()
                    .hover(|s| s.text_color(theme.sidebar_foreground))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
                    .child(div().flex_1().child(section.label()))
                    .child(count.to_string())
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                    .into_any_element(),
                Entry::Project { id, name, count, open } => {
                    let new_in = id;
                    let seed = id.map(|p| p.0).unwrap_or_default();
                    h_flex()
                        .id(("project", seed))
                        .h_8()
                        .px_2()
                        .gap_2()
                        .rounded(theme.radius)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.sidebar_foreground)
                        .cursor_default()
                        .hover(|s| s.bg(theme.sidebar_accent.opacity(0.55)))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(id, cx)))
                        .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall().text_color(theme.muted_foreground))
                        .child(ui::Monogram::new(name.clone(), seed))
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .child(self.activity_strip(id, cx))
                        // Open, the rows below are the count; closed, it says what is inside.
                        .when(!open && count > 0, |el| el.child(div().text_color(theme.muted_foreground).child(count.to_string())))
                        .when_some(new_in, |el, project| {
                            el.child(
                                Button::new(("new-in", project.0))
                                    .ghost()
                                    .xsmall()
                                    .icon(Lucide::SquarePen)
                                    .tooltip("New thread in this project")
                                    .on_click(move |_, window, cx| {
                                        window.dispatch_action(Box::new(NewThreadIn { project: project.0 }), cx)
                                    }),
                            )
                        })
                        .into_any_element()
                }
                Entry::Thread { id, compact } => {
                    let Some(thread) = state.threads.get(&id) else {
                        out.push(div().h(rems(entry.height())).into_any_element());
                        continue;
                    };
                    let open = current == Some(Route::Thread(id));
                    let picked = self.selection.ids.contains(&id);
                    let tiled = tiles.contains(&id);
                    let attention = Attention::of(thread);
                    let title = display_title(thread).to_owned();
                    let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
                    let project_path = project.map(|p| p.path.clone());
                    let (mark_name, mark_seed) = match project {
                        Some(p) => (p.name.clone(), p.id.0),
                        None => ("No project".to_owned(), 0),
                    };
                    let settled = thread.settled_ms.is_some();
                    let has_draft = state.drafts.get(&DraftTarget::Thread(id)).is_some_and(|d| !d.text.trim().is_empty() || !d.attachments.is_empty());
                    let app = self.app.clone();
                    let menu_thread = thread.clone();
                    let age = format::relative_time(thread.updated_ms, now);
                    let base = h_flex()
                        .id(("thread-row", id.0))
                        .h(rems(entry.height()))
                        .px_2()
                        .gap_2p5()
                        .rounded(theme.radius_lg)
                        .cursor_default()
                        .when(open, |el| el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                        .when(picked && !open, |el| el.bg(theme.primary.opacity(0.12)))
                        .when(!open && !picked, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.55))))
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| this.click_thread(id, event, window, cx)))
                        .on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(move |this, _, window, cx| {
                                let _ = this.workspace.update(cx, |ws, cx| ws.open_beside(id, window, cx));
                            }),
                        )
                        .context_menu(move |menu, _, cx| thread_menu::build(menu, &app, &menu_thread, project_path.clone(), cx));
                    let row = if compact {
                        base.text_sm()
                            .when(!open, |el| el.text_color(theme.muted_foreground))
                            .child(ui::Monogram::new(mark_name, mark_seed))
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(age))
                    } else {
                        let (status, status_color) = status_line(thread, state, &theme);
                        base.child(ui::Monogram::new(mark_name, mark_seed)).child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_sm()
                                                .when(attention == Attention::Unread || attention == Attention::Waiting, |el| {
                                                    el.font_weight(FontWeight::SEMIBOLD)
                                                })
                                                .when(settled && !open, |el| el.text_color(theme.muted_foreground))
                                                .child(title),
                                        )
                                        .when(tiled, |el| {
                                            el.child(Icon::new(Lucide::LayoutGrid).xsmall().text_color(theme.muted_foreground))
                                        })
                                        .when(has_draft, |el| {
                                            el.child(
                                                div()
                                                    .id(("draft", id.0))
                                                    .tooltip(|window, cx| {
                                                        gpui_kit::component::tooltip::Tooltip::new("Unsent draft").build(window, cx)
                                                    })
                                                    .child(Icon::new(Lucide::Pencil).xsmall().text_color(theme.muted_foreground)),
                                            )
                                        })
                                        .when(thread.pinned, |el| {
                                            el.child(Icon::new(Lucide::Pin).xsmall().text_color(theme.muted_foreground))
                                        })
                                        .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(age)),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .text_xs()
                                        .child(StatusGlyph::new(("glyph", id.0), thread, state))
                                        .child(div().flex_1().min_w_0().truncate().text_color(status_color).child(status))
                                        .when(thread.woke, |el| {
                                            el.child(
                                                div()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(theme.primary.opacity(0.15))
                                                    .text_color(theme.primary)
                                                    .child("Woke"),
                                            )
                                        }),
                                ),
                        )
                    };
                    let row = match self.moves.get(&id).copied() {
                        Some(mv) => {
                            let from = if mv.from_above { -1.0 } else { 1.0 };
                            row.relative()
                                .with_animation(
                                    ElementId::NamedInteger("moved".into(), id.0 ^ ((mv.generation as u64) << 56)),
                                    Animation::new(MOVE_DURATION).with_easing(ease_out_quint()),
                                    move |el, t| el.opacity(0.35 + 0.65 * t).top(rems(0.6 * from * (1.0 - t))),
                                )
                                .into_any_element()
                        }
                        None => row.into_any_element(),
                    };
                    if compact {
                        row
                    } else {
                        let this = cx.weak_entity();
                        HoverCard::new(("peek", id.0))
                            .anchor(Anchor::LeftCenter)
                            .open_delay(Duration::from_millis(550))
                            .close_delay(Duration::from_millis(200))
                            .trigger_style(StyleRefinement::default().w_full())
                            .trigger(row)
                            .on_open_change(move |open, window, cx| {
                                let open = *open;
                                let _ = this.update(cx, |this, cx| this.peek(id, open, window, cx));
                            })
                            .content({
                                let this = cx.weak_entity();
                                move |_, _, cx| match this.upgrade() {
                                    Some(sidebar) => sidebar.read(cx).render_peek(id, cx),
                                    None => div().into_any_element(),
                                }
                            })
                            .into_any_element()
                    }
                }
            };
            out.push(div().px_2().child(el).into_any_element());
        }
        out
    }
}

/// The last `lines` lines of `text`, at most `max` characters.
fn tail(text: &str, lines: usize, max: usize) -> String {
    let all: Vec<&str> = text.trim().lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = all[all.len().saturating_sub(lines)..].join("\n");
    match tail.char_indices().rev().nth(max) {
        Some((at, _)) => format!("…{}", &tail[at..]),
        None => tail,
    }
}

/// Starts a thread in a given project (from a project row or the palette).
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct NewThreadIn {
    pub project: u64,
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let waiting = crate::inbox::InboxView::waiting(state);
        let current = self.current(cx);
        let connection = state.connection.clone();
        let has_projects = !state.projects.is_empty();
        let nav_item = |id: &'static str, icon: Icon, label: &'static str, route: Route, badge: Option<usize>, action: &dyn Action| {
            let action = action.boxed_clone();
            let selected = current.as_ref() == Some(&route);
            h_flex()
                .id(id)
                .h_8()
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .text_sm()
                .cursor_default()
                .when(selected, |el| el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                .when(!selected, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.6))))
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(route.clone(), window, cx)))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(label).action(action.as_ref(), None).build(window, cx)
                })
                .child(icon.small().text_color(theme.muted_foreground))
                .child(div().flex_1().child(label))
                .when_some(badge.filter(|n| *n > 0), |el, n| {
                    el.child(
                        h_flex()
                            .h_5()
                            .min_w_5()
                            .px_1p5()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.warning.opacity(0.16))
                            .text_color(theme.warning)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(n.to_string()),
                    )
                })
        };
        let count = self.entries.len();
        let by_project = self.by_project;
        let selecting = !self.selection.ids.is_empty();
        v_flex()
            .size_full()
            .key_context("Sidebar")
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        Button::new("new-thread")
                            .flex_1()
                            .small()
                            .icon(Icon::new(Lucide::SquarePen))
                            .label("New thread")
                            .tooltip_with_action("New thread", &NewThread, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewThread), cx)),
                    )
                    .child(
                        Button::new("palette")
                            .ghost()
                            .small()
                            .icon(Lucide::Command)
                            .tooltip_with_action("Search and commands", &OpenPalette, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx)),
                    ),
            )
            .child(
                div().px_2().pb_1().child(
                    Input::new(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(theme.muted_foreground))
                        .cleanable(true),
                ),
            )
            .child(
                v_flex()
                    .px_2()
                    .gap_0p5()
                    .child(nav_item("nav-inbox", Icon::new(IconName::Inbox), "Inbox", Route::Inbox, Some(waiting), &GoInbox))
                    .child(nav_item("nav-fleet", Icon::new(IconName::LayoutDashboard), "Fleet", Route::Fleet, None, &GoFleet)),
            )
            .child(
                h_flex()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .child(ui::section_label("Threads", cx).px_2().pt_0().pb_0().flex_1())
                    .child(
                        Button::new("group-toggle")
                            .ghost()
                            .xsmall()
                            .icon(if by_project { IconName::Folder } else { IconName::Menu })
                            .tooltip(if by_project { "Group by status" } else { "Group by project" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.by_project = !this.by_project;
                                this.rebuild(cx);
                                this.save_layout(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("add-project")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FolderPlus)
                            .tooltip_with_action("Add project…", &AddProject, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(AddProject), cx)),
                    ),
            )
            .child(if count == 0 {
                v_flex()
                    .flex_1()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if !self.query.is_empty() {
                        "No thread matches. Search messages too with # in the palette."
                    } else if has_projects {
                        "No threads yet. Start one from New thread."
                    } else {
                        "Add a folder to start working in it. Orbit uses your existing checkout."
                    })
                    .when(!has_projects, |el| {
                        el.child(
                            Button::new("add-first-project")
                                .small()
                                .outline()
                                .icon(Lucide::FolderPlus)
                                .label("Add project…")
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(AddProject), cx)),
                        )
                    })
                    .into_any_element()
            } else {
                let rem = window.rem_size();
                let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(self.entries.iter().map(|e| size(px(0.), rem * e.height())).collect());
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.set_hovered(*hovered, cx)))
                    .child(
                        v_virtual_list(cx.entity(), "sidebar-entries", sizes, |this, range, window, cx| {
                            this.render_entries(range, window, cx)
                        })
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .into_any_element()
            })
            .when(selecting, |el| el.child(self.render_selection_bar(cx)))
            .child(div().px_1().py_1p5().border_t_1().border_color(theme.sidebar_border).child(self.constellation.clone()))
            .child(
                h_flex()
                    .h_10()
                    .px_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(connection_pill(connection.as_ref(), &theme))
                    .child(div().flex_1())
                    .child(
                        Button::new("shortcuts")
                            .ghost()
                            .small()
                            .icon(Lucide::Keyboard)
                            .tooltip_with_action("Keyboard shortcuts", &ShowShortcuts, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(ShowShortcuts), cx)),
                    )
                    .child(
                        Button::new("settings")
                            .ghost()
                            .small()
                            .icon(IconName::Settings)
                            .tooltip_with_action("Settings", &OpenSettings, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
                    ),
            )
    }
}

fn connection_pill(status: Option<&ConnectionStatus>, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let (color, label) = match status {
        None | Some(ConnectionStatus::Connected) => (theme.success, "This computer".to_string()),
        Some(ConnectionStatus::Connecting) => (theme.warning, "Connecting…".to_string()),
        Some(ConnectionStatus::Disconnected { reason }) => (theme.danger, reason.clone()),
    };
    h_flex()
        .min_w_0()
        .gap_1p5()
        .px_2()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().flex_none().size_1p5().rounded_full().bg(color))
        .child(div().truncate().child(label))
}

#[cfg(test)]
mod tests {
    use super::{Selection, tail};
    use orbit_proto::ThreadId;

    #[test]
    fn shift_click_selects_a_range_from_the_anchor() {
        let ids: Vec<ThreadId> = (0..6).map(|_| ThreadId::new()).collect();
        let mut sel = Selection::default();
        // No anchor yet: the range starts at the open thread.
        sel.select_range(&ids, Some(ids[1]), ids[3]);
        assert_eq!(sel.ids, ids[1..=3].to_vec());
        // Upward from the same anchor.
        sel.select_range(&ids, Some(ids[5]), ids[0]);
        assert_eq!(sel.ids, ids[0..=1].to_vec());
    }

    #[test]
    fn deselecting_the_anchor_moves_it_to_a_selected_row() {
        let ids: Vec<ThreadId> = (0..5).map(|_| ThreadId::new()).collect();
        let mut sel = Selection::default();
        sel.toggle(&ids, ids[1]);
        sel.toggle(&ids, ids[3]);
        assert_eq!(sel.anchor, Some(ids[3]));
        sel.toggle(&ids, ids[3]);
        assert_eq!(sel.anchor, Some(ids[1]), "T3's bug left the anchor on the unselected row");
        sel.select_range(&ids, None, ids[2]);
        assert_eq!(sel.ids, ids[1..=2].to_vec());
    }

    #[test]
    fn peeks_show_the_end_of_a_reply() {
        assert_eq!(tail("a\n\nb\nc\nd", 2, 100), "c\nd");
        assert_eq!(tail("abcdef", 1, 3), "…cdef");
    }
}
