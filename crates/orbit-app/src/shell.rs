//! The window: title bar, sidebar, and the routed main area.
//!
//! Every command is an Action handled here, so a button, a menu item, the
//! palette and a key binding all run the same code.
//!
//! The workspace also owns what spans views: tiles (several threads side by
//! side), attention (calm notifications, the window title, the taskbar or
//! dock asking for attention), hand-offs from a second launch, and the
//! user's key bindings.

use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::fleet::FleetView;
use crate::home::HomeView;
use crate::inbox::InboxView;
use crate::instance::{Intent, Launch};
use crate::notify::{self, Note, NoteKind};
use crate::sidebar::{NewThreadIn, Sidebar};
use crate::thread::{ThreadView, ThreadViewEvent};
use crate::tiles::{self, Direction, MAX_TILES};
use crate::ui::{Attention, Glyph, StatusGlyph};
use crate::{format, keymap, palette, prefs, settings, shortcuts, thread_menu};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Thread(ThreadId),
    Inbox,
    Fleet,
    /// Full-window settings, with their own navigation in place of the
    /// sidebar.
    Settings,
}

/// Threads whose views stay alive for instant switching (tiles always do).
/// Older views are dropped and their timelines released.
const LIVE_VIEWS: usize = 6;

/// Marks a thread's toast, so opening the thread closes it.
struct ThreadNote;
/// Marks a burst's toast.
struct BurstNote;
/// Marks the key bindings report.
struct KeymapNote;

pub struct Workspace {
    app: Entity<AppState>,
    route: Route,
    history: Vec<ThreadId>,
    sidebar: Entity<Sidebar>,
    sidebar_open: bool,
    split: Entity<ResizableState>,
    home: Entity<HomeView>,
    inbox: Entity<InboxView>,
    fleet: Entity<FleetView>,
    threads: Vec<(ThreadId, Entity<ThreadView>)>,
    /// Threads shown side by side, in grid order; empty or two or more.
    tiles: Vec<ThreadId>,
    /// Tiles from least to most recently focused, to pick one to replace.
    tile_recency: Vec<ThreadId>,
    maximized: bool,
    tile_focus: HashMap<ThreadId, Subscription>,
    focus: FocusHandle,
    /// Debug builds: a screen to open once data arrives
    /// (`ORBIT_DEBUG_ROUTE` = `latest`, `inbox`, `fleet`, `terminal`,
    /// `tiles`, `shortcuts`, `palette`), for screenshots and visual checks.
    debug_route: Option<String>,
    tracker: notify::Tracker,
    burst: Vec<Note>,
    burst_since: Option<Instant>,
    _flush: Option<Task<()>>,
    /// Threads waiting on the user, as the window title last said.
    title_count: usize,
    /// Where closing settings returns to.
    before_settings: Route,
    settings_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let sidebar = cx.new(|cx| Sidebar::new(app.clone(), weak.clone(), window, cx));
        let home = cx.new(|cx| HomeView::new(app.clone(), weak.clone(), window, cx));
        let inbox = cx.new(|cx| InboxView::new(app.clone(), weak.clone(), window, cx));
        let fleet = cx.new(|cx| FleetView::new(app.clone(), weak.clone(), cx));
        let split = cx.new(|_| ResizableState::default());
        let subs = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                if settings::get(cx).appearance.mode == ColorMode::System {
                    settings::reapply_appearance(window, cx);
                }
            }),
            // Field values, chat width and the page itself follow the model.
            cx.observe_global::<settings::SettingsModel>(|_, cx| cx.notify()),
            // Back in front: the user may have edited the key bindings.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active()
                    && let Some(report) = keymap::reload_if_changed(cx)
                {
                    this.show_keymap_report(&report, true, window, cx);
                }
            }),
            // Follow deletions: never leave the user on a thread that is gone.
            cx.observe_in(&app, window, |this, app, window, cx| {
                this.run_debug_route(window, cx);
                let connected = app.read(cx).connection().is_none_or(|c| *c == orbit_net::ConnectionStatus::Connected);
                if connected {
                    let state = app.read(cx).state();
                    let gone: Vec<ThreadId> =
                        this.tiles.iter().copied().filter(|id| !state.threads.contains_key(id)).collect();
                    for id in gone {
                        this.remove_tile(id, window, cx);
                    }
                    if let Route::Thread(id) = this.route
                        && !app.read(cx).state().threads.contains_key(&id)
                    {
                        this.threads.retain(|(t, _)| *t != id);
                        this.route = Route::Home;
                    }
                }
                this.observe_attention(window, cx);
                cx.notify();
            }),
        ];
        // Toasts sit bottom right, clear of the title bar and every view's
        // header controls; at most three at once.
        {
            let notification = &mut gpui_kit::component::Theme::global_mut(cx).notification;
            notification.placement = Anchor::BottomRight;
            notification.max_items = 3;
        }
        // Problems in the key bindings file are shown once the window is up.
        let report = keymap::last_report(cx);
        cx.defer_in(window, move |this, window, cx| this.show_keymap_report(&report, false, window, cx));
        Self {
            app,
            route: Route::Home,
            history: Vec::new(),
            sidebar,
            sidebar_open: true,
            split,
            home,
            inbox,
            fleet,
            threads: Vec::new(),
            tiles: Vec::new(),
            tile_recency: Vec::new(),
            maximized: false,
            tile_focus: HashMap::new(),
            focus: cx.focus_handle(),
            debug_route: if cfg!(debug_assertions) { std::env::var("ORBIT_DEBUG_ROUTE").ok() } else { None },
            tracker: notify::Tracker::default(),
            burst: Vec::new(),
            burst_since: None,
            _flush: None,
            title_count: 0,
            before_settings: Route::Home,
            settings_focus: cx.focus_handle(),
            _subscriptions: subs,
        }
    }

    fn run_debug_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(route) = self.debug_route.take() else { return };
        let state = self.app.read(cx).state();
        if state.threads.is_empty() {
            self.debug_route = Some(route);
            return;
        }
        // After a moment, so the requests arrive the way live ones do (and
        // notify).
        #[cfg(debug_assertions)]
        if std::env::var("ORBIT_DEBUG_DEMO").is_ok_and(|v| v == "attention") {
            let app = self.app.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                let _ = app.update(cx, |app, cx| app.debug_edit_threads(crate::demo::apply, cx));
            })
            .detach();
        }
        let state = self.app.read(cx).state();
        let mut recent: Vec<&Thread> = state.threads.values().filter(|t| !t.archived && t.settled_ms.is_none()).collect();
        recent.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let recent: Vec<ThreadId> = recent.iter().map(|t| t.id).collect();
        match route.as_str() {
            "inbox" => self.navigate(Route::Inbox, window, cx),
            "fleet" => self.navigate(Route::Fleet, window, cx),
            "latest" | "terminal" => {
                if let Some(id) = recent.first() {
                    self.navigate(Route::Thread(*id), window, cx);
                }
                if route == "terminal" {
                    self.toggle_terminal(&ToggleTerminal, window, cx);
                }
            }
            "tiles" => {
                if let Some(first) = recent.first() {
                    self.navigate(Route::Thread(*first), window, cx);
                }
                for id in recent.iter().skip(1).take(2) {
                    self.open_beside(*id, window, cx);
                }
            }
            "shortcuts" => window.defer(cx, |window, cx| shortcuts::open(window, cx)),
            "palette" => {
                let this = cx.entity();
                window.defer(cx, move |window, cx| palette::open(this, "", window, cx));
            }
            _ => {}
        }
    }

    // ── Attention ────────────────────────────────────────────────────────

    /// Threads on screen: the open one, or every visible tile.
    pub fn visible_threads(&self) -> Vec<ThreadId> {
        match self.route {
            Route::Thread(id) if self.tiles.contains(&id) && !self.maximized => self.tiles.clone(),
            Route::Thread(id) => vec![id],
            _ => Vec::new(),
        }
    }

    pub fn tiles(&self) -> &[ThreadId] {
        &self.tiles
    }

    /// Threads opened in this window, least recent first.
    pub fn history(&self) -> &[ThreadId] {
        &self.history
    }

    /// Notes what changed since the last look; a burst is shown once it
    /// goes quiet. Also keeps the window title's count current.
    fn observe_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = settings::get(cx).notifications.clone();
        let active = window.is_window_active();
        let visible = self.visible_threads();
        let state = self.app.read(cx).state();
        let notes = self.tracker.observe(state.threads.values(), &prefs, |id| active && visible.contains(&id));
        let waiting = InboxView::waiting(state);
        if waiting != self.title_count {
            self.title_count = waiting;
            window.set_window_title(&match waiting {
                0 => "Orbit".to_owned(),
                1 => "Orbit · 1 needs you".to_owned(),
                n => format!("Orbit · {n} need you"),
            });
        }
        if notes.is_empty() {
            return;
        }
        let since = *self.burst_since.get_or_insert_with(Instant::now);
        self.burst.extend(notes);
        let delay = notify::QUIET.min(notify::MAX_WAIT.saturating_sub(since.elapsed()));
        self._flush = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |this, window, cx| this.flush_notes(window, cx));
        }));
    }

    /// Shows the burst as one notification: a toast when the window is in
    /// front; otherwise a system notification, a request for attention and,
    /// if allowed, one short sound.
    fn flush_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.burst_since = None;
        let active = window.is_window_active();
        let visible = self.visible_threads();
        let notes: Vec<Note> =
            std::mem::take(&mut self.burst).into_iter().filter(|n| !(active && visible.contains(&n.thread))).collect();
        let Some(summary) = notify::summarize(&notes) else { return };
        let note = match summary.kind {
            NoteKind::NeedsYou => Notification::warning(summary.body.clone()),
            NoteKind::Failed => Notification::error(summary.body.clone()),
            NoteKind::Finished => Notification::success(summary.body.clone()),
        }
        .title(summary.title.clone());
        let note = match summary.open {
            Some(id) => note.id1::<ThreadNote>(("thread-note", id.0)),
            None => note.id::<BurstNote>(),
        };
        let weak = cx.weak_entity();
        let open = summary.open;
        let note = note.on_click(move |_, window, cx| {
            window.activate_window();
            let route = open.map_or(Route::Inbox, Route::Thread);
            let _ = weak.update(cx, |ws, cx| ws.navigate(route, window, cx));
        });
        if active {
            window.push_notification(note, cx);
        } else {
            window.push_notification(note.system(), cx);
            if summary.kind != NoteKind::Finished {
                window.request_attention();
                if prefs::get(cx).sounds {
                    window.play_system_bell();
                }
            }
        }
    }

    fn show_keymap_report(&self, report: &keymap::Report, reloaded: bool, window: &mut Window, cx: &mut App) {
        if report.problems.is_empty() {
            if reloaded {
                let rules = report.applied;
                window.push_notification(
                    Notification::success(format!("{rules} rule{} in effect.", if rules == 1 { "" } else { "s" }))
                        .title("Key bindings reloaded")
                        .id::<KeymapNote>(),
                    cx,
                );
            }
            return;
        }
        let mut body: Vec<String> = report.problems.iter().take(3).cloned().collect();
        if report.problems.len() > 3 {
            body.push(format!("And {} more.", report.problems.len() - 3));
        }
        let note = Notification::warning(body.join("\n"))
            .title(format!("Some key bindings in {} were not applied", keymap::FILE_NAME))
            .id::<KeymapNote>()
            .action(|_, _, cx| {
                Button::new("edit-keybindings").small().label("Edit…").on_click(cx.listener(|note, _, window, cx| {
                    window.dispatch_action(Box::new(OpenKeybindings), cx);
                    note.dismiss(window, cx);
                }))
            });
        window.push_notification(note, cx);
    }

    // ── Navigation ───────────────────────────────────────────────────────

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn app(&self) -> &Entity<AppState> {
        &self.app
    }

    pub fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if let Route::Thread(id) = route {
            // In tiles, a thread already tiled takes focus; another one takes
            // the focused tile's place.
            if self.tiles.len() >= 2 && !self.tiles.contains(&id) {
                let focused = match self.route {
                    Route::Thread(current) => self.tiles.iter().position(|t| *t == current),
                    _ => None,
                }
                .or_else(|| self.tiles.iter().position(|t| Some(t) == self.tile_recency.last()))
                .unwrap_or(0);
                let old = std::mem::replace(&mut self.tiles[focused], id);
                self.tile_focus.remove(&old);
                self.tile_recency.retain(|t| *t != old);
            }
            self.ensure_thread_view(id, window, cx);
            self.history.retain(|t| *t != id);
            self.history.push(id);
            if self.tiles.contains(&id) {
                self.tile_recency.retain(|t| *t != id);
                self.tile_recency.push(id);
            }
            window.remove_notification1::<ThreadNote>(("thread-note", id.0), cx);
            if let Some(view) = self.view(id).cloned() {
                window.defer(cx, move |window, cx| ThreadView::focus_composer(&view, window, cx));
            }
        }
        if route == Route::Settings && self.route != Route::Settings {
            self.before_settings = self.route.clone();
            settings::measure_storage(cx);
            self.settings_focus.focus(window, cx);
        } else if route != Route::Settings && self.route == Route::Settings {
            self.focus.focus(window, cx);
        }
        match route {
            Route::Home => {
                let home = self.home.clone();
                window.defer(cx, move |window, cx| HomeView::focus_composer(&home, window, cx));
            }
            Route::Inbox => {
                let inbox = self.inbox.clone();
                window.defer(cx, move |window, cx| InboxView::focus(&inbox, window, cx));
            }
            _ => {}
        }
        self.route = route;
        cx.notify();
    }

    fn view(&self, id: ThreadId) -> Option<&Entity<ThreadView>> {
        self.threads.iter().find(|(t, _)| *t == id).map(|(_, v)| v)
    }

    fn ensure_thread_view(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.threads.iter().position(|(t, _)| *t == id) {
            let entry = self.threads.remove(ix);
            self.threads.push(entry);
        } else {
            let app = self.app.clone();
            let view = cx.new(|cx| ThreadView::new(id, app, window, cx));
            let sub = cx.subscribe_in(&view, window, |this, _, event: &ThreadViewEvent, window, cx| match event {
                ThreadViewEvent::Review => this.toggle_review(&ToggleReview, window, cx),
                ThreadViewEvent::Open(id) => this.navigate(Route::Thread(*id), window, cx),
            });
            self._subscriptions.push(sub);
            self.threads.push((id, view));
        }
        // Drop the least recently used views, never a tile's.
        let keep = LIVE_VIEWS.max(self.tiles.len() + 1);
        while self.threads.len() > keep {
            match self.threads.iter().position(|(t, _)| !self.tiles.contains(t) && *t != id) {
                Some(ix) => {
                    self.threads.remove(ix);
                }
                None => break,
            }
        }
        if self.tiles.contains(&id) && !self.tile_focus.contains_key(&id) {
            self.watch_tile_focus(id, window, cx);
        }
    }

    /// Clicking or typing into a tile makes it the focused one.
    fn watch_tile_focus(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view(id).cloned() else { return };
        let handle = view.read(cx).focus_handle(cx);
        let sub = cx.on_focus_in(&handle, window, move |this, _, cx| {
            if this.tiles.contains(&id) && this.route != Route::Thread(id) {
                this.route = Route::Thread(id);
                this.tile_recency.retain(|t| *t != id);
                this.tile_recency.push(id);
                cx.notify();
            }
        });
        self.tile_focus.insert(id, sub);
    }

    fn current_thread_view(&self) -> Option<&Entity<ThreadView>> {
        match self.route {
            Route::Thread(id) => self.view(id),
            _ => None,
        }
    }

    fn current_thread(&self, cx: &App) -> Option<Thread> {
        match self.route {
            Route::Thread(id) => self.app.read(cx).state().threads.get(&id).cloned(),
            _ => None,
        }
    }

    // ── Tiles ────────────────────────────────────────────────────────────

    /// Shows `id` beside what is on screen. The first time, the open thread
    /// becomes the first tile; past six, the least recently used tile makes
    /// room.
    pub fn open_beside(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tiles.is_empty()
            && let Route::Thread(current) = self.route
            && current != id
        {
            self.tiles.push(current);
            self.tile_recency.push(current);
        }
        if !self.tiles.contains(&id) {
            if self.tiles.len() >= MAX_TILES {
                let current = match self.route {
                    Route::Thread(current) => Some(current),
                    _ => None,
                };
                let victim = self.tile_recency.iter().copied().find(|t| Some(*t) != current).unwrap_or(self.tiles[0]);
                self.remove_tile_quietly(victim);
                window.push_notification(
                    Notification::info(format!("Tiles hold {MAX_TILES} threads; the one you used least was closed.")),
                    cx,
                );
            }
            self.tiles.push(id);
        }
        if self.tiles.len() < 2 {
            self.tiles.clear();
            self.tile_recency.clear();
        } else {
            self.maximized = false;
            for tile in self.tiles.clone() {
                self.ensure_thread_view(tile, window, cx);
            }
        }
        self.navigate(Route::Thread(id), window, cx);
    }

    fn remove_tile_quietly(&mut self, id: ThreadId) {
        self.tiles.retain(|t| *t != id);
        self.tile_recency.retain(|t| *t != id);
        self.tile_focus.remove(&id);
    }

    /// Closes a tile; the neighbour takes focus. One tile left is just the
    /// thread again.
    fn remove_tile(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tiles.iter().position(|t| *t == id) else { return };
        let was_focused = self.route == Route::Thread(id);
        self.remove_tile_quietly(id);
        let next = self.tiles.get(ix.min(self.tiles.len().saturating_sub(1))).copied();
        if self.tiles.len() < 2 {
            self.tiles.clear();
            self.tile_recency.clear();
            self.tile_focus.clear();
            self.maximized = false;
        }
        if was_focused && let Some(next) = next {
            self.navigate(Route::Thread(next), window, cx);
        }
        cx.notify();
    }

    fn tiled(&self) -> bool {
        matches!(self.route, Route::Thread(id) if self.tiles.contains(&id))
    }

    fn close_tile(&mut self, _: &CloseTile, window: &mut Window, cx: &mut Context<Self>) {
        if let Route::Thread(id) = self.route
            && self.tiles.contains(&id)
        {
            self.remove_tile(id, window, cx);
        }
    }

    fn close_other_tiles(&mut self, _: &CloseOtherTiles, _: &mut Window, cx: &mut Context<Self>) {
        self.tiles.clear();
        self.tile_recency.clear();
        self.tile_focus.clear();
        self.maximized = false;
        cx.notify();
    }

    fn maximize_tile(&mut self, _: &MaximizeTile, _: &mut Window, cx: &mut Context<Self>) {
        if self.tiled() {
            self.maximized = !self.maximized;
            cx.notify();
        }
    }

    fn focus_tile_toward(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let Route::Thread(current) = self.route else { return };
        let Some(ix) = self.tiles.iter().position(|t| *t == current) else { return };
        let grid = tiles::rows(self.tiles.len(), tiles::is_wide(window));
        if let Some(next) = tiles::neighbor(&grid, ix, dir).and_then(|n| self.tiles.get(n).copied()) {
            self.navigate(Route::Thread(next), window, cx);
        }
    }

    fn open_thread_beside(&mut self, action: &OpenThreadBeside, window: &mut Window, cx: &mut Context<Self>) {
        self.open_beside(ThreadId(action.thread), window, cx);
    }

    fn open_beside_action(&mut self, _: &OpenBeside, window: &mut Window, cx: &mut Context<Self>) {
        // From the keyboard: the next thread in the sidebar that is not
        // tiled yet opens beside the current one.
        let Route::Thread(current) = self.route else { return };
        let order = self.sidebar.read(cx).thread_order(cx);
        let after = order.iter().position(|t| *t == current).map_or(0, |ix| ix + 1);
        let next = order[after.min(order.len())..]
            .iter()
            .chain(order.iter())
            .find(|t| !self.tiles.contains(t) && **t != current)
            .copied();
        if let Some(next) = next {
            self.open_beside(next, window, cx);
        }
    }

    // ── Hand-offs from a second launch ────────────────────────────────────

    /// A later launch handed over its arguments: come to the front and do
    /// what they ask.
    pub fn handle_launch(&mut self, launch: Launch, window: &mut Window, cx: &mut Context<Self>) {
        crate::instance::restore_window(window);
        window.activate_window();
        for intent in crate::instance::intents(&launch) {
            match intent {
                Intent::Thread(text) => {
                    let id = text.parse::<ThreadId>().ok().filter(|id| self.app.read(cx).state().threads.contains_key(id));
                    match id {
                        Some(id) => self.navigate(Route::Thread(id), window, cx),
                        None => window.push_notification(Notification::error(format!("There is no thread “{text}”.")), cx),
                    }
                }
                Intent::Folder(path) => self.add_project_path(path.display().to_string(), window, cx),
            }
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────

    fn new_thread(&mut self, _: &NewThread, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Home, window, cx);
    }

    fn new_thread_in(&mut self, action: &NewThreadIn, window: &mut Window, cx: &mut Context<Self>) {
        let project = ProjectId(action.project);
        self.home.update(cx, |home, cx| home.select_project(Some(project), cx));
        self.navigate(Route::Home, window, cx);
    }

    fn go_inbox(&mut self, _: &GoInbox, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Inbox, window, cx);
    }

    fn go_fleet(&mut self, _: &GoFleet, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Fleet, window, cx);
    }

    fn go_home(&mut self, _: &GoHome, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Home, window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn toggle_review(&mut self, _: &ToggleReview, window: &mut Window, cx: &mut Context<Self>) {
        let Route::Thread(id) = self.route else { return };
        crate::review::open(self.app.clone(), id, window, cx);
    }

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        palette::open(cx.entity(), "", window, cx);
    }

    fn run_command(&mut self, _: &RunCommand, window: &mut Window, cx: &mut Context<Self>) {
        palette::open(cx.entity(), ">", window, cx);
    }

    fn search_messages(&mut self, _: &SearchMessages, window: &mut Window, cx: &mut Context<Self>) {
        palette::open(cx.entity(), "#", window, cx);
    }

    fn show_shortcuts(&mut self, _: &ShowShortcuts, window: &mut Window, cx: &mut Context<Self>) {
        shortcuts::open(window, cx);
    }

    fn reload_keybindings(&mut self, _: &ReloadKeybindings, window: &mut Window, cx: &mut Context<Self>) {
        let report = keymap::reload(cx);
        self.show_keymap_report(&report, true, window, cx);
    }

    fn open_keybindings(&mut self, _: &OpenKeybindings, window: &mut Window, cx: &mut Context<Self>) {
        let path = keymap::path(cx);
        match keymap::ensure_file(&path) {
            Ok(()) => cx.open_with_system(&path),
            Err(err) => window.push_notification(
                Notification::error(format!("Couldn’t create {}: {err}", path.display())).title("Key bindings"),
                cx,
            ),
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Settings, window, cx);
    }

    fn close_settings(&mut self, _: &CloseSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.route == Route::Settings {
            let back = self.before_settings.clone();
            self.navigate(back, window, cx);
        } else {
            cx.propagate();
        }
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        let cleared = self.sidebar.update(cx, |sidebar, cx| sidebar.clear_selection(cx));
        if !cleared {
            cx.propagate();
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        settings::zoom(1, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        settings::zoom(-1, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        settings::zoom(0, cx);
    }

    pub fn add_project(&mut self, _: &AddProject, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.add_project_path(path.display().to_string(), window, cx));
        })
        .detach();
    }

    /// Adds a folder as a project (or finds it, if it is one) and starts a
    /// thread there.
    fn add_project_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.app.update(cx, |app, cx| app.call(Command::AddProject { path }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Reply::Project(id)) => {
                    this.home.update(cx, |home, cx| home.select_project(Some(id), cx));
                    this.navigate(Route::Home, window, cx);
                }
                Ok(_) => {}
                Err(message) => {
                    window.push_notification(Notification::error(message).title("Couldn’t add the project"), cx)
                }
            });
        })
        .detach();
    }

    fn stop_turn(&mut self, _: &StopTurn, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.current_thread_view().cloned() {
            view.update(cx, |view, cx| view.stop(window, cx));
        }
    }

    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.current_thread_view().cloned() {
            view.update(cx, |view, cx| view.toggle_terminal(window, cx));
        }
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        match self.current_thread_view() {
            Some(view) => ThreadView::focus_composer(&view.clone(), window, cx),
            None => HomeView::focus_composer(&self.home.clone(), window, cx),
        }
    }

    fn patch_current(&mut self, patch: impl FnOnce(&Thread) -> ThreadPatch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.current_thread(cx) else { return };
        let patch = patch(&thread);
        self.app.update(cx, |app, cx| app.run(Command::UpdateThread { id: thread.id, patch }, window, cx));
    }

    fn rename_thread(&mut self, _: &RenameThread, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current_thread(cx) {
            thread_menu::rename(self.app.clone(), &thread, window, cx);
        }
    }

    fn settle_thread(&mut self, _: &SettleThread, window: &mut Window, cx: &mut Context<Self>) {
        self.patch_current(|t| ThreadPatch { settled: Some(t.settled_ms.is_none()), ..Default::default() }, window, cx);
    }

    fn pin_thread(&mut self, _: &PinThread, window: &mut Window, cx: &mut Context<Self>) {
        self.patch_current(|t| ThreadPatch { pinned: Some(!t.pinned), ..Default::default() }, window, cx);
    }

    fn archive_thread(&mut self, _: &ArchiveThread, window: &mut Window, cx: &mut Context<Self>) {
        self.patch_current(|t| ThreadPatch { archived: Some(!t.archived), ..Default::default() }, window, cx);
    }

    /// Next or previous in the sidebar's own order, so the keys walk the
    /// list the user sees.
    fn cycle_thread(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ordered = self.sidebar.read(cx).thread_order(cx);
        if ordered.is_empty() {
            return;
        }
        let current = match self.route {
            Route::Thread(id) => ordered.iter().position(|t| *t == id),
            _ => None,
        };
        let next = match current {
            Some(ix) => (ix as isize + step).rem_euclid(ordered.len() as isize) as usize,
            None if step < 0 => ordered.len() - 1,
            None => 0,
        };
        self.navigate(Route::Thread(ordered[next]), window, cx);
    }

    fn next_thread(&mut self, _: &NextThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread(1, window, cx);
    }

    fn previous_thread(&mut self, _: &PreviousThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread(-1, window, cx);
    }

    fn jump_to_thread(&mut self, action: &JumpToThread, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.sidebar.read(cx).thread_order(cx);
        if let Some(id) = order.get(action.index).copied() {
            self.navigate(Route::Thread(id), window, cx);
        }
    }

    /// The next thread that needs the user, in triage order after the one
    /// on screen.
    fn next_attention(&mut self, _: &NextAttention, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let (cards, _) = crate::inbox::triage(state);
        let needs: Vec<ThreadId> = cards
            .into_iter()
            .filter(|id| state.threads.get(id).is_some_and(|t| Attention::of(t) == Attention::Waiting))
            .collect();
        let current = match self.route {
            Route::Thread(id) => needs.iter().position(|t| *t == id),
            _ => None,
        };
        let next = match current {
            Some(ix) => needs.get((ix + 1) % needs.len()).copied(),
            None => needs.first().copied(),
        };
        match next {
            Some(id) => self.navigate(Route::Thread(id), window, cx),
            None => window.push_notification(Notification::info("Nothing needs you right now.").id::<BurstNote>(), cx),
        }
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let in_settings = self.route == Route::Settings;
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let needs_you = InboxView::waiting(state);
        let working = state.threads.values().filter(|t| !t.archived && Attention::of(t) == Attention::Working).count();
        let quota = state
            .providers
            .iter()
            .flat_map(|p| p.quota.iter().map(move |q| (p.kind, q)))
            .filter(|(_, q)| q.utilization.is_some() || q.rejected)
            .max_by(|a, b| a.1.utilization.unwrap_or(1.0).total_cmp(&b.1.utilization.unwrap_or(1.0)));
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_3()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(Lucide::Orbit).small().text_color(theme.primary))
                        .child("Orbit"),
                )
                .when(in_settings, |el| {
                    el.child(
                        Button::new("close-settings")
                            .ghost()
                            .small()
                            .icon(IconName::ArrowLeft)
                            .label("Back")
                            .tooltip("Close settings (Esc)")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(CloseSettings), cx)),
                    )
                })
                .child(div().flex_1())
                .when(working > 0, |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(StatusGlyph::glyph("title-working-glyph", Glyph::Working))
                            .child(format!("{working} working")),
                    )
                })
                .when(needs_you > 0, |el| {
                    el.child(
                        h_flex()
                            .id("title-needs-you")
                            .h_6()
                            .gap_1p5()
                            .pl_1()
                            .pr_2()
                            .rounded_full()
                            .bg(theme.warning.opacity(0.12))
                            .hover(|s| s.bg(theme.warning.opacity(0.2)))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.warning)
                            .cursor_default()
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Open the Inbox").action(&GoInbox, None).build(window, cx)
                            })
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(GoInbox), cx))
                            .child(StatusGlyph::glyph("title-needs-glyph", Glyph::Question))
                            .child(format!("{needs_you} need{} you", if needs_you == 1 { "s" } else { "" })),
                    )
                })
                .when_some(quota, |el, (kind, q)| {
                    let pct = q.utilization.map(|u| (u * 100.0).round() as u32);
                    let label = match (q.rejected, pct) {
                        (true, _) => format!("{} quota used up · {}", kind.label(), format::resets_at(q.resets_ms, now_ms())),
                        (false, Some(p)) => format!("{} {}% of {} quota", kind.label(), p, humanize_window(&q.window)),
                        _ => String::new(),
                    };
                    el.child(div().text_xs().text_color(if q.rejected { theme.danger } else { theme.muted_foreground }).child(label))
                }),
        )
    }

    fn render_tiles(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app.read(cx).state();
        let focused = match self.route {
            Route::Thread(id) => id,
            _ => self.tiles[0],
        };
        let tile = |number: usize, id: ThreadId, cx: &App| -> Option<AnyElement> {
            let view = self.view(id)?.clone();
            let focus_view = view.clone();
            Some(
                tiles::tile(
                    number,
                    state.threads.get(&id),
                    &view,
                    id == focused,
                    self.maximized,
                    move |window, cx| ThreadView::focus_composer(&focus_view, window, cx),
                    cx,
                )
                .into_any_element(),
            )
        };
        let grid = v_flex().key_context("Tiles").size_full().p_1p5().gap_1p5();
        if self.maximized {
            let number = self.tiles.iter().position(|t| *t == focused).unwrap_or(0) + 1;
            return grid.children(tile(number, focused, cx)).into_any_element();
        }
        let shape = tiles::rows(self.tiles.len(), tiles::is_wide(window));
        let mut next = 0;
        let mut rows = Vec::new();
        for len in shape {
            let mut row = h_flex().flex_1().min_h_0().gap_1p5().items_stretch();
            for _ in 0..len {
                if let Some(id) = self.tiles.get(next).copied() {
                    row = row.children(tile(next + 1, id, cx));
                }
                next += 1;
            }
            rows.push(row);
        }
        grid.children(rows).into_any_element()
    }

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.route {
            Route::Home => self.home.clone().into_any_element(),
            Route::Inbox => self.inbox.clone().into_any_element(),
            Route::Fleet => self.fleet.clone().into_any_element(),
            Route::Settings => div().into_any_element(),
            Route::Thread(_) if self.tiled() => self.render_tiles(window, cx),
            Route::Thread(_) => match self.current_thread_view() {
                Some(view) => view.clone().into_any_element(),
                None => crate::ui::empty_state(
                    Icon::new(IconName::Inbox),
                    "That thread is gone",
                    "It was deleted, perhaps from another window.",
                    None,
                    cx,
                )
                .into_any_element(),
            },
        }
    }
}

fn humanize_window(window: &str) -> String {
    match window {
        "five_hour" => "5-hour".into(),
        "seven_day" => "weekly".into(),
        "seven_day_opus" => "weekly Opus".into(),
        other => other.replace('_', " "),
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = if self.route == Route::Settings {
            div()
                .flex_1()
                .min_h_0()
                .key_context("SettingsPage")
                .track_focus(&self.settings_focus)
                // The search field hands back an Escape it has no use for.
                .on_action(|_: &gpui_kit::component::input::Escape, window, cx| {
                    window.dispatch_action(Box::new(CloseSettings), cx)
                })
                .child(settings::page(self.app.clone(), window, cx))
                .into_any_element()
        } else {
            let main = self.render_main(window, cx);
            div()
                .flex_1()
                .min_h_0()
                .child(
                    h_resizable("shell")
                        .with_state(&self.split)
                        .child(
                            resizable_panel()
                                .size(px(272.))
                                .size_range(px(208.)..px(440.))
                                .visible(self.sidebar_open)
                                .child(self.sidebar.clone()),
                        )
                        .child(resizable_panel().child(div().size_full().min_w_0().child(main))),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::new_thread))
            .on_action(cx.listener(Self::new_thread_in))
            .on_action(cx.listener(Self::go_inbox))
            .on_action(cx.listener(Self::go_fleet))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_review))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::run_command))
            .on_action(cx.listener(Self::search_messages))
            .on_action(cx.listener(Self::show_shortcuts))
            .on_action(cx.listener(Self::reload_keybindings))
            .on_action(cx.listener(Self::open_keybindings))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::close_settings))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::add_project))
            .on_action(cx.listener(Self::stop_turn))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::next_thread))
            .on_action(cx.listener(Self::previous_thread))
            .on_action(cx.listener(Self::jump_to_thread))
            .on_action(cx.listener(Self::next_attention))
            .on_action(cx.listener(Self::rename_thread))
            .on_action(cx.listener(Self::settle_thread))
            .on_action(cx.listener(Self::pin_thread))
            .on_action(cx.listener(Self::archive_thread))
            .on_action(cx.listener(Self::open_beside_action))
            .on_action(cx.listener(Self::open_thread_beside))
            .on_action(cx.listener(Self::close_tile))
            .on_action(cx.listener(Self::close_other_tiles))
            .on_action(cx.listener(Self::maximize_tile))
            .on_action(cx.listener(|this, _: &FocusTileLeft, window, cx| this.focus_tile_toward(Direction::Left, window, cx)))
            .on_action(cx.listener(|this, _: &FocusTileRight, window, cx| this.focus_tile_toward(Direction::Right, window, cx)))
            .on_action(cx.listener(|this, _: &FocusTileUp, window, cx| this.focus_tile_toward(Direction::Up, window, cx)))
            .on_action(cx.listener(|this, _: &FocusTileDown, window, cx| this.focus_tile_toward(Direction::Down, window, cx)))
            .on_action(cx.listener(Self::quit))
            .child(self.render_title_bar(cx))
            .child(body)
    }
}
