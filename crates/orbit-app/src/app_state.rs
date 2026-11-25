//! The application's connection to the engine.
//!
//! `AppState` owns the [`Backend`] and the UI-agnostic [`ClientState`]. One
//! foreground task drains the backend's feed in batches; each batch is applied
//! once and announced as narrowly as possible:
//!
//! * `cx.notify()` only when shell records changed (sidebar, inbox, fleet
//!   observe this entity);
//! * [`AppEvent::Timeline`] for one thread's items (only that thread's view
//!   subscribes);
//! * [`AppEvent::Usage`] for the live context meter.
//!
//! A streamed token therefore re-renders one message, never the window.
//!
//! With the background host (`docs/HOST.md`) it also attaches this window
//! to the host on every connection, learns which terminals the host kept,
//! and tells the window about the host ([`AppEvent::Host`]).

use std::collections::HashMap;

use gpui_kit::*;
use orbit_net::host::{HostLink, LocalHost};
use orbit_net::state::{ClientState, ItemChange};
use orbit_net::{Backend, ConnectionStatus};
use orbit_proto::*;

use crate::host::HostNotice;

pub enum AppEvent {
    /// A thread's timeline changed. `None` means it was (re)loaded whole.
    Timeline { thread: ThreadId, changes: Option<Vec<ItemChange>> },
    Usage(ThreadId),
    Workspace(ThreadId),
    TerminalOutput { term: TermId, data: Vec<u8> },
    TerminalExited { term: TermId, code: Option<i32> },
    /// A composer's saved draft changed (here or on another client).
    Draft(DraftTarget),
    /// A team or race changed (`crate::teams` views listen).
    Teams,
    /// A checkout's git status changed.
    Git(WorkspaceRoot),
    /// A thread's run targets started, ended or were stopped.
    Runs(ThreadId),
    /// A terminal's shell was stopped to save resources, or started again.
    TerminalSleep { term: TermId, sleeping: bool },
    /// The connection came back after it was lost (the host may have
    /// restarted): screens attached to the host attach again.
    Reconnected,
    /// The host's kept terminals were read ([`AppState::take_kept_terminal`]).
    HostTerminals,
    /// Something to tell the user about the background host.
    Host(HostNotice),
}

pub struct AppState {
    backend: Backend,
    state: ClientState,
    /// How many views show each thread; the timeline is released at zero.
    open: HashMap<ThreadId, usize>,
    /// The background host, when the engine runs in one.
    host: Option<LocalHost>,
    /// The window is closing: terminals stay running in the host.
    detaching: bool,
    /// Terminals the host kept for threads, until a view shows them.
    kept_terminals: HashMap<ThreadId, TermSession>,
    was_connected: bool,
    /// The outdated-host notice was shown for this host version.
    outdated_shown: Option<String>,
    _pump: Task<()>,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new(backend: Backend, host: Option<LocalHost>, cx: &mut Context<Self>) -> Self {
        // The pump applies every update, so a remote reply can wait for the
        // events before it: what a command did is shown when it returns.
        if let Backend::Remote(client) = &backend {
            client.order_replies(true);
        }
        let mut feed = backend.feed();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(batch) = feed.next_batch().await {
                if this.update(cx, |this, cx| this.apply(batch, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            backend,
            state: ClientState::default(),
            open: HashMap::new(),
            host,
            detaching: false,
            kept_terminals: HashMap::new(),
            was_connected: false,
            outdated_shown: None,
            _pump: pump,
        }
    }

    /// The background host, when the engine runs in one.
    pub fn host(&self) -> Option<&LocalHost> {
        self.host.as_ref()
    }

    /// The window is closing or Orbit quits: leave terminals running in the
    /// host (their panels close without closing them) and start no host.
    pub fn detach(&mut self) {
        self.detaching = true;
        if let Some(host) = &self.host {
            host.close();
        }
    }

    pub fn detaching(&self) -> bool {
        self.detaching
    }

    /// The terminal the host kept for `thread`, once: the view that shows
    /// it takes it.
    pub fn take_kept_terminal(&mut self, thread: ThreadId) -> Option<TermSession> {
        self.kept_terminals.remove(&thread)
    }

    pub fn state(&self) -> &ClientState {
        &self.state
    }

    pub fn connection(&self) -> Option<&ConnectionStatus> {
        self.state.connection.as_ref()
    }

    fn apply(&mut self, batch: Vec<orbit_net::Update>, cx: &mut Context<Self>) {
        let changes = self.state.apply(batch);
        if changes.is_empty() {
            return;
        }
        let shell = changes.affects_shell() || changes.reset;
        if changes.connection {
            self.on_connection(cx);
        }
        for &(term, sleeping) in &changes.sleeping_terminals {
            cx.emit(AppEvent::TerminalSleep { term, sleeping });
        }
        if changes.settings || changes.reset {
            crate::settings::receive(self.state.settings.clone(), cx);
        }
        if changes.bots || changes.guard || changes.reset {
            let bots = changes.bots.then(|| self.state.bots.clone()).flatten();
            let guard = changes.guard.then(|| self.state.guard.clone()).flatten();
            crate::bots::receive(bots, guard, changes.reset, cx);
        }
        if changes.reset {
            // Everything was replaced: reload every timeline a view shows.
            let open: Vec<ThreadId> = self.open.keys().copied().collect();
            for thread in open {
                self.fetch_timeline(thread, cx);
            }
        }
        for thread in changes.loaded {
            cx.emit(AppEvent::Timeline { thread, changes: None });
        }
        for (thread, item_changes) in changes.timelines {
            cx.emit(AppEvent::Timeline { thread, changes: Some(item_changes) });
        }
        for thread in changes.usage {
            cx.emit(AppEvent::Usage(thread));
        }
        for thread in changes.workspace {
            cx.emit(AppEvent::Workspace(thread));
        }
        for root in changes.git {
            cx.emit(AppEvent::Git(root));
        }
        // Before their output, so a view can attach to a new run's terminal.
        for thread in changes.runs {
            cx.emit(AppEvent::Runs(thread));
        }
        for (term, data) in changes.terminals {
            cx.emit(AppEvent::TerminalOutput { term, data });
        }
        for (term, code) in changes.exited_terminals {
            cx.emit(AppEvent::TerminalExited { term, code });
        }
        for target in changes.drafts {
            cx.emit(AppEvent::Draft(target));
        }
        if changes.teams || changes.reset {
            cx.emit(AppEvent::Teams);
        }
        if shell {
            cx.notify();
        }
    }

    // ── The background host ──────────────────────────────────────────────

    fn on_connection(&mut self, cx: &mut Context<Self>) {
        match &self.state.connection {
            Some(ConnectionStatus::Connected) => {
                if let Some(host) = &self.host {
                    self.send(Command::Host(HostCommand::AttachUi));
                    // A host from another build that still speaks this
                    // protocol (an update that left it running): it restarts
                    // into this build once nothing is working.
                    if let HostLink::Ready { version, .. } = host.link()
                        && version != env!("CARGO_PKG_VERSION")
                    {
                        self.send(Command::Host(HostCommand::Restart { when: RestartWhen::WhenIdle }));
                    }
                }
                // Usage limits are read only while a window was in front
                // lately: say so again to this (possibly new) host. The
                // first connection is the window opening, in front.
                let reconnect = self.was_connected;
                if !reconnect || cx.active_window().is_some() {
                    self.send(Command::Usage(UsageCommand::Seen));
                }
                if std::mem::replace(&mut self.was_connected, true) {
                    cx.emit(AppEvent::Reconnected);
                } else if self.host.as_ref().is_some_and(LocalHost::found_running) {
                    self.tell_kept_work(cx);
                }
                self.fetch_kept_terminals(cx);
            }
            Some(ConnectionStatus::Disconnected { .. }) => {
                if let Some(HostLink::Outdated { version, status, restart }) = self.host.as_ref().map(LocalHost::link)
                    && restart.is_none()
                    && self.outdated_shown.as_ref() != Some(&version)
                {
                    self.outdated_shown = Some(version.clone());
                    cx.emit(AppEvent::Host(HostNotice::Outdated { version, status }));
                }
            }
            _ => {}
        }
    }

    /// On the first connection to a host that was already running: what it
    /// kept doing while Orbit was closed.
    fn tell_kept_work(&self, cx: &mut Context<Self>) {
        let reply = self.call(Command::Host(HostCommand::Activity), cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Reply::Host(HostReply::Activity(activity))) = reply.await
                && let Some(summary) = activity.summary()
            {
                let _ = this.update(cx, |_, cx| cx.emit(AppEvent::Host(HostNotice::KeptWorking(summary))));
            }
        })
        .detach();
    }

    fn fetch_kept_terminals(&self, cx: &mut Context<Self>) {
        let reply = self.call(Command::Host(HostCommand::Terminals), cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Reply::Host(HostReply::Terminals(sessions))) = reply.await {
                let _ = this.update(cx, |this, cx| {
                    this.kept_terminals = sessions.into_iter().filter_map(|s| Some((s.thread?, s))).collect();
                    cx.emit(AppEvent::HostTerminals);
                });
            }
        })
        .detach();
    }

    // ── Timelines ────────────────────────────────────────────────────────

    /// A view starts showing `thread`. Loads its timeline on first use.
    pub fn retain_timeline(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        let count = self.open.entry(thread).or_default();
        *count += 1;
        if *count == 1 && self.state.timeline(thread).is_none() {
            self.fetch_timeline(thread, cx);
        } else if *count == 1 {
            self.mark_read(thread);
        }
    }

    /// A view stops showing `thread`.
    pub fn release_timeline(&mut self, thread: ThreadId) {
        if let Some(count) = self.open.get_mut(&thread) {
            *count -= 1;
            if *count == 0 {
                self.open.remove(&thread);
                self.state.drop_timeline(thread);
                self.state.cancel_fetch(thread);
                self.backend.send(Command::CloseThread { id: thread });
            }
        }
    }

    fn fetch_timeline(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        self.state.begin_fetch(thread);
        let reply = self.backend.call(Command::OpenThread { id: thread });
        cx.spawn(async move |this, cx| {
            let result = reply.await;
            let _ = this.update(cx, |this, cx| {
                if !this.open.contains_key(&thread) {
                    return;
                }
                if let Ok(Ok(Reply::Timeline { items, mark })) = result
                    && this.state.finish_fetch(thread, items, mark)
                {
                    cx.emit(AppEvent::Timeline { thread, changes: None });
                }
            });
        })
        .detach();
    }

    fn mark_read(&self, thread: ThreadId) {
        if self.state.threads.get(&thread).is_some_and(|t| t.unread) {
            self.backend.send(Command::UpdateThread {
                id: thread,
                patch: ThreadPatch { unread: Some(false), ..Default::default() },
            });
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// Runs a command and resolves with its reply.
    pub fn call(&self, command: Command, cx: &mut Context<Self>) -> Task<Result<Reply, String>> {
        let reply = self.backend.call(command);
        cx.background_spawn(async move {
            reply.await.unwrap_or_else(|_| Err("The engine stopped before it answered.".into()))
        })
    }

    /// Runs a command whose only interesting result is failure; failures are
    /// shown as a notification in `window`.
    pub fn run(&self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.call(command, cx);
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(message) = task.await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    use gpui_kit::component::WindowExt as _;
                    use gpui_kit::component::notification::Notification;
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
    }

    pub fn send(&self, command: Command) {
        self.backend.send(command);
    }

    /// Debug builds: edits thread records in place, to show states the
    /// engine only reaches with a live agent (screenshots, visual checks).
    #[cfg(debug_assertions)]
    pub fn debug_edit_threads(&mut self, edit: impl FnOnce(&mut HashMap<ThreadId, Thread>), cx: &mut Context<Self>) {
        edit(&mut self.state.threads);
        cx.notify();
    }
}
