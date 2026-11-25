//! The application's connection to the engine, on every host it shows.
//!
//! `AppState` owns one session per connected host: its [`Backend`] and
//! UI-agnostic [`ClientState`]. The window shows one host at a time, the
//! *active* session; the others stay connected in the background (their
//! thread records stay current, so the host switcher shows what waits on
//! the user everywhere, and switching back is instant). See
//! `docs/REMOTE.md` for why the window switches between hosts instead of
//! merging their threads.
//!
//! One foreground task per session drains its feed in batches; the active
//! session's batches are applied once and announced as narrowly as
//! possible:
//!
//! * `cx.notify()` only when shell records changed (sidebar, inbox, fleet
//!   observe this entity);
//! * [`AppEvent::Timeline`] for one thread's items (only that thread's view
//!   subscribes);
//! * [`AppEvent::Usage`] for the live context meter.
//!
//! A streamed token therefore re-renders one message, never the window.
//! A background session's batches update its records only.
//!
//! With a background host (`docs/HOST.md`), here or remote, it also
//! attaches this window to the host on every connection, learns which
//! terminals the host kept, and tells the window about the host
//! ([`AppEvent::Host`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;
use orbit_core::{EngineHandle, EngineOptions};
use orbit_net::host::{HostLink, LocalHost};
use orbit_net::remote::hosts::LOCAL_ID;
use orbit_net::remote::{HostEntry, HostKind, HostsFile, LinkPhase, RemoteLink, Tools};
use orbit_net::state::{ClientState, ItemChange};
use orbit_net::{Backend, ConnectionStatus, RemoteClient};
use orbit_proto::*;

use crate::host::HostNotice;
use crate::shell::WindowMemory;

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
    /// A plugin asked to show a notification (it holds that capability).
    PluginNotice { title: String, body: String },
}

/// How to reach one host: what [`connect`] returns.
pub struct Connection {
    pub entry: HostEntry,
    pub backend: Backend,
    /// This computer's background host, when the engine runs in one.
    pub host: Option<LocalHost>,
    /// An SSH or WSL host's link (its probe and install state).
    pub link: Option<Arc<RemoteLink>>,
    /// Why the engine runs in this process instead of the background host.
    pub fallback: Option<String>,
}

/// The engine runs in this process: asked for, for tests and benchmarks.
pub fn in_process_requested() -> bool {
    std::env::var("ORBIT_IN_PROCESS").is_ok_and(|v| !v.is_empty() && v != "0")
        || std::env::args().any(|a| a == "--in-process")
}

/// Starts reaching `entry` without waiting for it (blocking only to start
/// an engine in this process).
pub fn connect(entry: &HostEntry, data_dir: &Path, tools: &Tools) -> std::io::Result<Connection> {
    let only = |backend| Connection { entry: entry.clone(), backend, host: None, link: None, fallback: None };
    Ok(match &entry.kind {
        HostKind::Local => {
            let data = data_dir.to_owned();
            let in_process = move || EngineHandle::start(EngineOptions { data_dir: data, probe_providers: true });
            if in_process_requested() {
                only(Backend::Local(in_process()?))
            } else {
                let chosen = orbit_net::host::local_backend(data_dir.to_owned(), orbit_net::host::locate_binary(), in_process)?;
                Connection { host: chosen.host, fallback: chosen.fallback, ..only(chosen.backend) }
            }
        }
        HostKind::Url { url, token } => only(Backend::Remote(RemoteClient::connect(url.clone(), token.clone())?)),
        HostKind::Ssh { .. } | HostKind::Wsl { .. } => {
            let link = RemoteLink::new(entry.clone(), tools.clone());
            Connection { link: Some(link.clone()), ..only(Backend::Remote(link.connect()?)) }
        }
    })
}

/// One connected host.
struct Session {
    entry: HostEntry,
    /// Tells this session's pump from an earlier one for the same host.
    serial: u64,
    backend: Backend,
    state: ClientState,
    /// How many views show each thread; the timeline is released at zero.
    open: HashMap<ThreadId, usize>,
    host: Option<LocalHost>,
    link: Option<Arc<RemoteLink>>,
    /// Terminals the host kept for threads, until a view shows them.
    kept_terminals: HashMap<ThreadId, TermSession>,
    was_connected: bool,
    /// The last measured round trip (remote hosts).
    latency: Option<Duration>,
    _pump: Task<()>,
    _ping: Option<Task<()>>,
}

/// What the host switcher shows about one host.
#[derive(Clone, Debug, Default)]
pub struct HostSummary {
    /// `None` when this window has not connected to it.
    pub connection: Option<ConnectionStatus>,
    pub phase: Option<LinkPhase>,
    pub latency: Option<Duration>,
    /// Threads with a running turn, and threads waiting on the user.
    pub working: usize,
    pub waiting: usize,
}

pub struct AppState {
    active: Session,
    /// Connected hosts the window does not show now.
    parked: Vec<Session>,
    hosts: HostsFile,
    /// A host from `ORBIT_HOST`: shown, never saved.
    ephemeral: Option<String>,
    data_dir: PathBuf,
    tools: Tools,
    /// The window is closing: terminals stay running in the host.
    detaching: bool,
    /// Views of the host being left are closing: terminals stay too.
    switching: bool,
    /// Where each host's window was, for coming back to it.
    memories: HashMap<String, WindowMemory>,
    /// The outdated-host notice was shown for this host version.
    outdated_shown: Option<String>,
    next_serial: u64,
}

impl EventEmitter<AppEvent> for AppState {}

/// Round trips are measured this often on the host shown, and less often
/// on the others.
const PING_ACTIVE: Duration = Duration::from_secs(15);
const PING_PARKED: Duration = Duration::from_secs(60);

impl AppState {
    pub fn new(first: Connection, mut hosts: HostsFile, data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let ephemeral = hosts.get(&first.entry.id).is_none().then(|| first.entry.id.clone());
        if ephemeral.is_some() {
            hosts.hosts.push(first.entry.clone());
        }
        hosts.active = first.entry.id.clone();
        let active = Self::session(first, 1, cx);
        Self {
            active,
            parked: Vec::new(),
            hosts,
            ephemeral,
            data_dir,
            tools: Tools::default(),
            detaching: false,
            switching: false,
            memories: HashMap::new(),
            outdated_shown: None,
            next_serial: 2,
        }
    }

    fn session(connection: Connection, serial: u64, cx: &mut Context<Self>) -> Session {
        // The pump applies every update, so a remote reply can wait for the
        // events before it: what a command did is shown when it returns.
        if let Backend::Remote(client) = &connection.backend {
            client.order_replies(true);
        }
        let mut feed = connection.backend.feed();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(batch) = feed.next_batch().await {
                if this.update(cx, |this, cx| this.deliver(serial, batch, cx)).is_err() {
                    break;
                }
            }
        });
        let ping = (!connection.entry.is_local()).then(|| Self::pinger(serial, cx));
        Session {
            entry: connection.entry,
            serial,
            backend: connection.backend,
            state: ClientState::default(),
            open: HashMap::new(),
            host: connection.host,
            link: connection.link,
            kept_terminals: HashMap::new(),
            was_connected: false,
            latency: None,
            _pump: pump,
            _ping: ping,
        }
    }

    /// Measures a remote session's round trip now and then.
    fn pinger(serial: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut first = true;
            loop {
                let wait = match this.read_with(cx, |this, _| this.active.serial == serial) {
                    Ok(true) if first => Duration::from_secs(2),
                    Ok(true) => PING_ACTIVE,
                    Ok(false) => PING_PARKED,
                    Err(_) => return,
                };
                first = false;
                cx.background_executor().timer(wait).await;
                let Ok(Some(reply)) = this.update(cx, |this, _| {
                    this.find(serial)
                        .filter(|s| matches!(s.state.connection, Some(ConnectionStatus::Connected)))
                        .map(|s| s.backend.call(Command::Host(HostCommand::Ping)))
                }) else {
                    continue;
                };
                let started = Instant::now();
                let answered = reply.await.is_ok_and(|r| r.is_ok());
                let took = started.elapsed();
                let _ = this.update(cx, |this, cx| {
                    let Some(session) = this.find_mut(serial) else { return };
                    let next = answered.then_some(took);
                    // Re-render only for a change one would see.
                    let shown = |d: Option<Duration>| d.map(|d| d.as_millis() / 5);
                    if shown(session.latency) != shown(next) {
                        session.latency = next;
                        cx.notify();
                    }
                });
            }
        })
    }

    fn sessions(&self) -> impl Iterator<Item = &Session> {
        std::iter::once(&self.active).chain(&self.parked)
    }

    fn find(&self, serial: u64) -> Option<&Session> {
        self.sessions().find(|s| s.serial == serial)
    }

    fn find_mut(&mut self, serial: u64) -> Option<&mut Session> {
        std::iter::once(&mut self.active).chain(&mut self.parked).find(|s| s.serial == serial)
    }

    /// This computer's background host, when the window shows it and the
    /// engine runs in one.
    pub fn host(&self) -> Option<&LocalHost> {
        self.active.host.as_ref()
    }

    /// The window is closing or Orbit quits: leave terminals running in the
    /// host (their panels close without closing them) and start no host.
    pub fn detach(&mut self) {
        self.detaching = true;
        for session in self.sessions() {
            if let Some(host) = &session.host {
                host.close();
            }
        }
    }

    /// The window's first engine runs in this process (no background host).
    pub fn runs_in_process(&self) -> bool {
        self.sessions().all(|s| matches!(s.backend, Backend::Local(_)))
    }

    /// Sends `command` to every connected host (holding them all while the
    /// window restarts).
    pub fn send_all(&self, command: Command) {
        for session in self.sessions() {
            session.backend.send(command.clone());
        }
    }

    /// Every host's backend, for shutting down at quit.
    pub fn backends(&self) -> Vec<Backend> {
        self.sessions().map(|s| s.backend.clone()).collect()
    }

    /// Views are closing without closing what they show on the host: the
    /// window closes, or it switches to another host.
    pub fn detaching(&self) -> bool {
        self.detaching || self.switching
    }

    /// The terminal the host kept for `thread`, once: the view that shows
    /// it takes it.
    pub fn take_kept_terminal(&mut self, thread: ThreadId) -> Option<TermSession> {
        self.active.kept_terminals.remove(&thread)
    }

    pub fn state(&self) -> &ClientState {
        &self.active.state
    }

    pub fn connection(&self) -> Option<&ConnectionStatus> {
        self.active.state.connection.as_ref()
    }

    // ── Hosts ────────────────────────────────────────────────────────────

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn hosts(&self) -> &HostsFile {
        &self.hosts
    }

    /// The host the window shows.
    pub fn active_entry(&self) -> &HostEntry {
        &self.active.entry
    }

    /// The host came from `ORBIT_HOST` (not saved, not removable).
    pub fn is_ephemeral(&self, id: &str) -> bool {
        self.ephemeral.as_deref() == Some(id)
    }

    /// The shown host's SSH or WSL link.
    pub fn link(&self) -> Option<&Arc<RemoteLink>> {
        self.active.link.as_ref()
    }

    /// Whether the shown host ever delivered its state to this window.
    pub fn has_snapshot(&self) -> bool {
        self.active.was_connected
    }

    /// What the switcher shows for `id`.
    pub fn summary(&self, id: &str) -> HostSummary {
        let Some(session) = self.sessions().find(|s| s.entry.id == id) else { return HostSummary::default() };
        let working = session
            .state
            .threads
            .values()
            .filter(|t| !t.archived && crate::ui::Attention::of(t) == crate::ui::Attention::Working)
            .count();
        // An engine in this process has no connection to lose.
        let unset = if matches!(session.backend, Backend::Local(_)) { ConnectionStatus::Connected } else { ConnectionStatus::Connecting };
        HostSummary {
            connection: Some(session.state.connection.clone().unwrap_or(unset)),
            phase: session.link.as_ref().map(|l| l.phase()),
            latency: session.latency,
            working,
            waiting: crate::inbox::InboxView::waiting(&session.state),
        }
    }

    /// A host's state, shown or not (tests).
    #[cfg(test)]
    fn state_of(&self, id: &str) -> Option<&ClientState> {
        self.sessions().find(|s| s.entry.id == id).map(|s| &s.state)
    }

    fn save_hosts(&self) {
        let mut file = self.hosts.clone();
        if let Some(id) = &self.ephemeral {
            file.hosts.retain(|h| &h.id != id);
            if &file.active == id {
                file.active = LOCAL_ID.into();
            }
        }
        if let Err(err) = file.save(&self.data_dir) {
            log::warn!("couldn’t save the hosts: {err}");
        }
    }

    /// Adds a host (or replaces the one with its id) and saves the list.
    pub fn add_host(&mut self, entry: HostEntry, cx: &mut Context<Self>) -> String {
        let id = self.hosts.upsert(entry);
        self.save_hosts();
        cx.notify();
        id
    }

    /// Renames a host ("" goes back to its default name).
    pub fn rename_host(&mut self, id: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim().to_owned();
        let Some(entry) = self.hosts.hosts.iter_mut().find(|h| h.id == id) else { return };
        entry.name = name.clone();
        for session in std::iter::once(&mut self.active).chain(&mut self.parked) {
            if session.entry.id == id {
                session.entry.name = name.clone();
            }
        }
        self.save_hosts();
        cx.notify();
    }

    /// Forgets a host and closes its connection. Not the host shown
    /// (switch first), nor this computer.
    pub fn remove_host(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if id == self.active.entry.id || !self.hosts.remove(id) {
            return false;
        }
        if let Some(ix) = self.parked.iter().position(|s| s.entry.id == id) {
            self.parked.remove(ix).backend.shutdown();
        }
        self.memories.remove(id);
        self.save_hosts();
        cx.notify();
        true
    }

    /// Try the shown host again now: after the user fixed what its last
    /// failure asked for, or installed `orbitd`.
    pub fn retry(&self) {
        if let Some(link) = &self.active.link {
            link.retry();
        }
        if let Backend::Remote(client) = &self.active.backend {
            client.reconnect_now();
        }
    }

    /// Keeps where `id`'s window was, for [`Self::take_memory`].
    pub fn remember(&mut self, id: String, memory: WindowMemory) {
        self.memories.insert(id, memory);
    }

    pub fn take_memory(&mut self, id: &str) -> Option<WindowMemory> {
        self.memories.remove(id)
    }

    /// The window starts leaving the shown host: its views close next,
    /// without closing terminals. [`Self::activate`] follows.
    pub fn begin_switch(&mut self) {
        self.switching = true;
    }

    /// Shows host `id`: the one connected in the background, or a new
    /// connection. The host left stays connected in the background.
    pub fn activate(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        self.switching = false;
        if id == self.active.entry.id {
            return Ok(());
        }
        let entry = self.hosts.get(id).cloned().ok_or("That host is no longer in the list.")?;
        let next = match self.parked.iter().position(|s| s.entry.id == id) {
            Some(ix) => self.parked.remove(ix),
            None => {
                let connection =
                    connect(&entry, &self.data_dir, &self.tools).map_err(|e| format!("Couldn’t connect to {}: {e}", entry.label()))?;
                let serial = self.next_serial;
                self.next_serial += 1;
                Self::session(connection, serial, cx)
            }
        };
        let mut left = std::mem::replace(&mut self.active, next);
        // The host left keeps no timeline open: its views are gone.
        for (thread, _) in left.open.drain() {
            left.state.drop_timeline(thread);
            left.state.cancel_fetch(thread);
            left.backend.send(Command::CloseThread { id: thread });
        }
        left.kept_terminals.clear();
        self.parked.push(left);
        self.hosts.active = id.to_owned();
        self.save_hosts();
        // As after a snapshot: settings, bots and teams are this host's now
        // (a host not reached yet sends its own when it connects).
        if self.active.was_connected {
            let state = &self.active.state;
            crate::settings::receive(state.settings.clone(), cx);
            crate::bots::receive(state.bots.clone(), state.guard.clone(), true, cx);
            crate::plugins::receive(&state.plugins, cx);
            self.fetch_kept_terminals(cx);
        }
        cx.emit(AppEvent::Teams);
        cx.notify();
        Ok(())
    }

    // ── Updates ──────────────────────────────────────────────────────────

    fn deliver(&mut self, serial: u64, batch: Vec<orbit_net::Update>, cx: &mut Context<Self>) {
        if self.active.serial == serial {
            self.apply(batch, cx);
            return;
        }
        let Some(session) = self.parked.iter_mut().find(|s| s.serial == serial) else { return };
        let changes = session.state.apply(batch);
        if changes.connection && matches!(session.state.connection, Some(ConnectionStatus::Connected)) {
            session.was_connected = true;
            if session.host.is_some() || session.link.is_some() {
                session.backend.send(Command::Host(HostCommand::AttachUi));
            }
        }
        // The switcher's counts and states.
        if changes.reset || changes.connection || !changes.threads.is_empty() || !changes.removed_threads.is_empty() {
            cx.notify();
        }
    }

    fn apply(&mut self, batch: Vec<orbit_net::Update>, cx: &mut Context<Self>) {
        let changes = self.active.state.apply(batch);
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
        let state = &self.active.state;
        if changes.settings || changes.reset {
            crate::settings::receive(state.settings.clone(), cx);
        }
        if changes.bots || changes.guard || changes.reset {
            let bots = changes.bots.then(|| state.bots.clone()).flatten();
            let guard = changes.guard.then(|| state.guard.clone()).flatten();
            crate::bots::receive(bots, guard, changes.reset, cx);
        }
        if changes.plugins || changes.reset {
            crate::plugins::receive(&state.plugins, cx);
        }
        for (plugin, title, body) in changes.plugin_notices {
            let name = state.plugins.plugin(&plugin).map(|p| p.name.clone()).unwrap_or(plugin);
            let title = if title.is_empty() { name } else { title };
            cx.emit(AppEvent::PluginNotice { title, body });
        }
        if changes.reset {
            // Everything was replaced: reload every timeline a view shows.
            let open: Vec<ThreadId> = self.active.open.keys().copied().collect();
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
        match &self.active.state.connection {
            Some(ConnectionStatus::Connected) => {
                if self.active.host.is_some() || self.active.link.is_some() {
                    self.send(Command::Host(HostCommand::AttachUi));
                }
                // A host from another build that still speaks this
                // protocol (an update that left it running): it restarts
                // into this build once nothing is working.
                if let Some(host) = &self.active.host
                    && let HostLink::Ready { version, .. } = host.link()
                    && version != env!("CARGO_PKG_VERSION")
                {
                    self.send(Command::Host(HostCommand::Restart { when: RestartWhen::WhenIdle }));
                }
                // Usage limits are read only while a window was in front
                // lately: say so again to this (possibly new) host. The
                // first connection is the window opening, in front.
                let reconnect = self.active.was_connected;
                if !reconnect || cx.active_window().is_some() {
                    self.send(Command::Usage(UsageCommand::Seen));
                }
                if std::mem::replace(&mut self.active.was_connected, true) {
                    cx.emit(AppEvent::Reconnected);
                } else if self.active.host.as_ref().is_some_and(LocalHost::found_running) {
                    self.tell_kept_work(cx);
                }
                self.fetch_kept_terminals(cx);
            }
            Some(ConnectionStatus::Disconnected { .. }) => {
                if let Some(HostLink::Outdated { version, status, restart }) = self.active.host.as_ref().map(LocalHost::link)
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
        let serial = self.active.serial;
        let reply = self.call(Command::Host(HostCommand::Terminals), cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Reply::Host(HostReply::Terminals(sessions))) = reply.await {
                let _ = this.update(cx, |this, cx| {
                    if this.active.serial != serial {
                        return;
                    }
                    this.active.kept_terminals = sessions.into_iter().filter_map(|s| Some((s.thread?, s))).collect();
                    cx.emit(AppEvent::HostTerminals);
                });
            }
        })
        .detach();
    }

    // ── Timelines ────────────────────────────────────────────────────────

    /// A view starts showing `thread`. Loads its timeline on first use.
    pub fn retain_timeline(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        let count = self.active.open.entry(thread).or_default();
        *count += 1;
        if *count == 1 && self.active.state.timeline(thread).is_none() {
            self.fetch_timeline(thread, cx);
        } else if *count == 1 {
            self.mark_read(thread);
        }
    }

    /// A view stops showing `thread`.
    pub fn release_timeline(&mut self, thread: ThreadId) {
        if let Some(count) = self.active.open.get_mut(&thread) {
            *count -= 1;
            if *count == 0 {
                self.active.open.remove(&thread);
                self.active.state.drop_timeline(thread);
                self.active.state.cancel_fetch(thread);
                self.active.backend.send(Command::CloseThread { id: thread });
            }
        }
    }

    fn fetch_timeline(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        self.active.state.begin_fetch(thread);
        let serial = self.active.serial;
        let reply = self.active.backend.call(Command::OpenThread { id: thread });
        cx.spawn(async move |this, cx| {
            let result = reply.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.serial != serial || !this.active.open.contains_key(&thread) {
                    return;
                }
                if let Ok(Ok(Reply::Timeline { items, mark })) = result
                    && this.active.state.finish_fetch(thread, items, mark)
                {
                    cx.emit(AppEvent::Timeline { thread, changes: None });
                }
            });
        })
        .detach();
    }

    fn mark_read(&self, thread: ThreadId) {
        if self.active.state.threads.get(&thread).is_some_and(|t| t.unread) {
            self.active.backend.send(Command::UpdateThread {
                id: thread,
                patch: ThreadPatch { unread: Some(false), ..Default::default() },
            });
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// Runs a command on the shown host and resolves with its reply.
    pub fn call(&self, command: Command, cx: &mut Context<Self>) -> Task<Result<Reply, String>> {
        let reply = self.active.backend.call(command);
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
        self.active.backend.send(command);
    }

    /// Debug builds: edits thread records in place, to show states the
    /// engine only reaches with a live agent (screenshots, visual checks).
    #[cfg(debug_assertions)]
    pub fn debug_edit_threads(&mut self, edit: impl FnOnce(&mut HashMap<ThreadId, Thread>), cx: &mut Context<Self>) {
        edit(&mut self.active.state.threads);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    //! Two hosts in one window: an engine here and one reached over the
    //! network, each in its own session.
    use std::time::{Duration, Instant};

    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use orbit_core::{EngineHandle, EngineOptions};
    use orbit_net::Backend;
    use orbit_net::remote::{HostEntry, HostsFile};
    use orbit_proto::*;

    use super::{AppState, Connection};

    fn engine(dir: &std::path::Path) -> EngineHandle {
        EngineHandle::start(EngineOptions { data_dir: dir.to_owned(), probe_providers: false }).unwrap()
    }

    /// Runs the app until `done` holds (the engines answer on their own
    /// threads).
    fn wait(cx: &mut TestAppContext, app: &Entity<AppState>, what: &str, done: impl Fn(&AppState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if cx.read(|cx| done(app.read(cx))) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn call(cx: &mut TestAppContext, app: &Entity<AppState>, command: Command) -> Reply {
        let task = app.update(cx, |app, cx| app.call(command, cx));
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = result.clone();
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await)).detach();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if let Some(result) = result.borrow_mut().take() {
                return result.unwrap();
            }
            assert!(Instant::now() < deadline, "no reply");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn new_thread(cx: &mut TestAppContext, app: &Entity<AppState>, title: &str) -> ThreadId {
        let start = ThreadStart { title: title.into(), ..Default::default() };
        match call(cx, app, Command::StartThread(start)) {
            Reply::Thread(id) => id,
            other => panic!("{other:?}"),
        }
    }

    #[gpui_kit::test]
    fn the_window_switches_hosts_and_keeps_the_other_connected(cx: &mut TestAppContext) {
        // The engines and the client run on threads of their own.
        cx.executor().allow_parking();
        let (here, there, client) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let local = engine(here.path());
        let remote = engine(there.path());
        let endpoint = orbit_net::endpoint::start(remote.clone(), there.path(), "test").unwrap();
        let mut hosts = HostsFile::default();
        let other = hosts.upsert(HostEntry::url(&endpoint.info().url, &endpoint.info().token));
        let first = Connection { entry: HostEntry::local(), backend: Backend::Local(local.clone()), host: None, link: None, fallback: None };
        let data_dir = client.path().to_owned();
        let app = cx.new(|cx| AppState::new(first, hosts, data_dir, cx));
        let mine = new_thread(cx, &app, "here");
        wait(cx, &app, "the thread here", |a| a.state().threads.contains_key(&mine));
        app.update(cx, |app, cx| app.retain_timeline(mine, cx));

        // Show the other host: its threads, none of these.
        app.update(cx, |app, cx| {
            app.begin_switch();
            app.activate(&other, cx).unwrap();
        });
        assert!(!cx.read(|cx| app.read(cx).detaching()), "the switch is over");
        wait(cx, &app, "the other host to connect", |a| a.has_snapshot());
        assert_eq!(cx.read(|cx| app.read(cx).active_entry().id.clone()), other);
        assert!(cx.read(|cx| app.read(cx).state().threads.is_empty()), "the other host's threads only");
        let theirs = new_thread(cx, &app, "there");
        wait(cx, &app, "the thread there", |a| a.state().threads.contains_key(&theirs));
        // The saved list follows.
        assert_eq!(HostsFile::load(client.path()).active, other);

        // This computer stays connected in the background, its records
        // current, its timeline released.
        let renamed = ThreadPatch { title: Some("renamed here".into()), ..Default::default() };
        local.call(Command::UpdateThread { id: mine, patch: renamed }).recv().unwrap().unwrap();
        wait(cx, &app, "the background host's record to follow", |a| {
            a.state_of("local").and_then(|s| s.threads.get(&mine)).is_some_and(|t| t.title == "renamed here")
        });
        assert_eq!(cx.read(|cx| app.read(cx).summary("local").connection.clone()), Some(orbit_net::ConnectionStatus::Connected));

        // Back: this computer's threads, at once (no reconnect).
        app.update(cx, |app, cx| app.activate("local", cx).unwrap());
        cx.read(|cx| {
            let state = app.read(cx).state();
            assert!(state.threads.contains_key(&mine) && !state.threads.contains_key(&theirs));
            assert!(state.timeline(mine).is_none(), "a host left closes its timelines");
        });
        // Neither the shown host nor this computer can be removed; the
        // other can, which closes its connection.
        app.update(cx, |app, cx| {
            assert!(!app.remove_host("local", cx));
            assert!(app.remove_host(&other, cx));
            assert_eq!(app.summary(&other).connection, None);
        });
        assert_eq!(HostsFile::load(client.path()).hosts.len(), 1);
        for backend in cx.read(|cx| app.read(cx).backends()) {
            backend.shutdown();
        }
        endpoint.close();
        remote.shutdown();
    }
}
