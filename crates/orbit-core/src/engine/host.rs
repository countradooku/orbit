//! The background host, engine side: what keeps the host busy, and
//! interactive terminals that outlive the app.
//!
//! * **Activity.** [`HostActivity`] says what would stop if the host exited:
//!   running turns, terminals whose shell runs a command, an automation
//!   due soon. `orbitd --local` asks for it to decide when to exit.
//! * **Re-attach.** Every interactive terminal (opened for a screen, not a
//!   CLI command) is a session here. A screen that comes back, after the app
//!   restarted, attaches to its thread's terminal and replays the
//!   scrollback ring (`terminal::Scrollback`, kept by `engine/cli.rs` for
//!   every terminal). Output after the reply is held until the screen says
//!   it drew the scrollback, so nothing is lost or drawn twice.
//! * **Sleep.** A terminal with no input or output for [`SLEEP_AFTER`],
//!   whose shell runs nothing, with no agent in it and whose thread is not
//!   working, is put to sleep: its shell stops (no process, no PTY), its
//!   scrollback stays. Input, focus or an attach wakes it with a fresh shell
//!   in the same folder under the same id.
//!
//! Nothing here runs unless terminals exist: the tick returns at once when
//! there are no sessions, and checks for sleep every [`SLEEP_CHECK`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};
use crate::terminal::TermSize;

/// An idle interactive terminal is put to sleep after this long (override
/// with `ORBIT_TERMINAL_SLEEP_SECS`; 0 turns sleep off).
pub const SLEEP_AFTER: Duration = Duration::from_secs(30 * 60);
const SLEEP_CHECK: Duration = Duration::from_secs(30);
/// Output held for an attaching screen is sent after this long even if the
/// screen never says it is ready (it went away).
const HOLD_LIMIT: Duration = Duration::from_secs(5);
/// Written into a woken terminal: the old shell's state (its variables, its
/// folder) did not survive.
const WOKE: &[u8] = b"\r\n\x1b[2m\xe2\x94\x80\xe2\x94\x80 Woke from sleep: a new shell in the same folder \xe2\x94\x80\xe2\x94\x80\x1b[0m\r\n";

pub(crate) struct HostState {
    sessions: HashMap<TermId, Session>,
    next_check: Instant,
    sleep_after: Option<Duration>,
    started_ms: u64,
}

impl Default for HostState {
    fn default() -> Self {
        let sleep_after = match std::env::var("ORBIT_TERMINAL_SLEEP_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
            None => Some(SLEEP_AFTER),
        };
        let check = sleep_after.map_or(SLEEP_CHECK, |s| s.min(SLEEP_CHECK));
        Self { sessions: HashMap::new(), next_check: Instant::now() + check, sleep_after, started_ms: now_ms() }
    }
}

struct Session {
    cwd: String,
    last_io: Instant,
    state: Sleep,
    /// Output held for an attaching screen: the terminal's total output
    /// when the reply was sent, and when.
    held: Option<(u64, Instant)>,
}

enum Sleep {
    Awake,
    /// The shell was stopped and its exit has not arrived yet. A wake (and
    /// input typed meanwhile) waits for it.
    Stopping { thread: Option<ThreadId>, size: TermSize, wake: Option<TermSize>, input: Vec<u8> },
    Asleep { thread: Option<ThreadId>, size: TermSize },
}

impl Orchestrator {
    pub(crate) fn host_command(&mut self, command: HostCommand) -> CommandResult {
        match command {
            HostCommand::Activity => Ok(Reply::Host(HostReply::Activity(self.host_activity()))),
            // The host process handles these; in-process there is nothing
            // to keep alive.
            HostCommand::AttachUi | HostCommand::Hold { .. } => Ok(Reply::Done),
            HostCommand::Restart { .. } => Err("Orbit runs in this process: there is no background host to restart.".into()),
            HostCommand::AttachTerminal { thread, cwd, cols, rows } => {
                self.attach_terminal(thread, &cwd, TermSize::new(cols.max(2), rows.max(1)))
            }
            HostCommand::TerminalReady { term } => {
                self.release_held(term);
                Ok(Reply::Done)
            }
            HostCommand::WakeTerminal { term } => {
                let size = self.stored_size(term).unwrap_or_default();
                self.wake_terminal(term, size).map(|()| Reply::Done)
            }
            HostCommand::SleepTerminal { term } => {
                if !self.host.sessions.contains_key(&term) {
                    return Err("That terminal is closed.".into());
                }
                self.sleep_terminal(term);
                Ok(Reply::Done)
            }
            HostCommand::Terminals => {
                let mut ids: Vec<TermId> = self.host.sessions.keys().copied().collect();
                ids.sort();
                Ok(Reply::Host(HostReply::Terminals(ids.into_iter().map(|t| self.term_session(t)).collect())))
            }
        }
    }

    /// What would stop if the host exited now. The host process fills in
    /// who it is.
    pub fn host_activity(&self) -> HostActivity {
        let mut working: Vec<ThreadId> = self
            .runtimes
            .iter()
            .filter(|(_, r)| r.turn.is_some() || r.held_prompt.is_some())
            .map(|(id, _)| *id)
            .collect();
        working.sort();
        let (mut terminals, mut busy) = (0, 0);
        for term in self.terminals.ids() {
            terminals += 1;
            // A terminal that is not a session runs one command (the CLI's
            // `RunInTerminal`) until it exits.
            if !self.host.sessions.contains_key(&term) || self.terminals.busy(term) {
                busy += 1;
            }
        }
        let sleeping = self.host.sessions.values().filter(|s| !matches!(s.state, Sleep::Awake)).count() as u32;
        HostActivity {
            working,
            busy_terminals: busy,
            terminals,
            sleeping_terminals: sleeping,
            next_automation_ms: self.store.automations().iter().filter_map(|a| a.next_run_ms()).min(),
            protocol: PROTOCOL_VERSION,
            started_ms: self.host.started_ms,
            ..Default::default()
        }
    }

    // ── Hooks from the terminal paths ────────────────────────────────────

    /// An interactive terminal was opened (or woke) under `term`.
    pub(crate) fn host_terminal_opened(&mut self, term: TermId, cwd: &str) {
        let session = self.host.sessions.entry(term).or_insert_with(|| Session {
            cwd: cwd.to_owned(),
            last_io: Instant::now(),
            state: Sleep::Awake,
            held: None,
        });
        session.last_io = Instant::now();
    }

    /// Output from a terminal. Returns whether to send it to clients (not
    /// while it is held for an attaching screen).
    pub(crate) fn host_terminal_output(&mut self, term: TermId) -> bool {
        match self.host.sessions.get_mut(&term) {
            Some(session) => {
                session.last_io = Instant::now();
                session.held.is_none()
            }
            None => true,
        }
    }

    /// Input for a terminal. Returns `true` when it was handled here: the
    /// terminal sleeps, and wakes to receive it.
    pub(crate) fn host_terminal_input(&mut self, term: TermId, data: &[u8]) -> bool {
        let Some(session) = self.host.sessions.get_mut(&term) else { return false };
        session.last_io = Instant::now();
        match &mut session.state {
            Sleep::Awake => false,
            Sleep::Stopping { size, wake, input, .. } => {
                wake.get_or_insert(*size);
                input.extend_from_slice(data);
                true
            }
            Sleep::Asleep { size, .. } => {
                let size = *size;
                if self.wake_terminal(term, size).is_ok() {
                    self.status_terminal_input(term, data);
                    self.terminals.input(term, data);
                }
                true
            }
        }
    }

    /// A terminal's shell exited. Returns `true` when that was a terminal
    /// going to sleep: the exit is not reported.
    pub(crate) fn host_terminal_exited(&mut self, term: TermId) -> bool {
        let Some(session) = self.host.sessions.get_mut(&term) else { return false };
        match std::mem::replace(&mut session.state, Sleep::Awake) {
            Sleep::Awake => {
                self.host.sessions.remove(&term);
                false
            }
            Sleep::Stopping { thread, size, wake, input } => {
                session.state = Sleep::Asleep { thread, size };
                self.terminals.remove_exited(term);
                if let Some(size) = wake
                    && self.wake_terminal(term, size).is_ok()
                    && !input.is_empty()
                {
                    self.terminals.input(term, &input);
                }
                true
            }
            asleep @ Sleep::Asleep { .. } => {
                session.state = asleep;
                true
            }
        }
    }

    /// The user closed a terminal. A sleeping one has no shell to exit, so
    /// its exit is reported here.
    pub(crate) fn host_terminal_closed(&mut self, term: TermId) {
        let Some(session) = self.host.sessions.get_mut(&term) else { return };
        match session.state {
            Sleep::Awake => {}
            // The exit is on its way; let it through as a real one.
            Sleep::Stopping { .. } => session.state = Sleep::Awake,
            Sleep::Asleep { .. } => {
                self.host.sessions.remove(&term);
                self.cli.exited(term, None);
                self.emit(Event::TerminalExited { term, code: None });
            }
        }
    }

    // ── Attach, sleep, wake ──────────────────────────────────────────────

    fn attach_terminal(&mut self, thread: ThreadId, cwd: &str, size: TermSize) -> CommandResult {
        if self.store.thread(thread).is_none() {
            return Err("That thread no longer exists.".into());
        }
        let existing = self.host.sessions.keys().copied().filter(|t| self.session_thread(*t) == Some(thread)).max();
        let term = match existing {
            Some(term) => {
                let awake = self.host.sessions.get(&term).is_some_and(|s| matches!(s.state, Sleep::Awake));
                if awake { self.resize_for_attach(term, size) } else { self.wake_terminal(term, size)? }
                term
            }
            None => {
                let term = self.open_terminal_as(None, cwd, size)?;
                self.bind_terminal(term, Some(thread))?;
                term
            }
        };
        let total = self.output_total(term);
        let scrollback = self.cli.terminals.get(&term).map(|r| r.tail.replay()).unwrap_or_default();
        if let Some(session) = self.host.sessions.get_mut(&term) {
            session.held = Some((total, Instant::now()));
        }
        Ok(Reply::Host(HostReply::Terminal { session: self.term_session(term), scrollback }))
    }

    /// A screen of `size` takes the terminal over. A program drawing the
    /// whole screen is asked to redraw (a size change does that), since the
    /// scrollback may have lost the frame it is showing.
    fn resize_for_attach(&mut self, term: TermId, size: TermSize) {
        let same = self.terminals.size(term) == Some(size);
        if same && self.terminals.busy(term) && size.rows > 1 {
            let _ = self.terminals.resize(term, TermSize::new(size.cols, size.rows - 1));
        }
        let _ = self.terminals.resize(term, size);
    }

    /// Sends the output held for an attaching screen.
    fn release_held(&mut self, term: TermId) {
        let Some((at, _)) = self.host.sessions.get_mut(&term).and_then(|s| s.held.take()) else { return };
        let since = self.output_total(term).saturating_sub(at) as usize;
        if since == 0 {
            return;
        }
        if let Some(record) = self.cli.terminals.get(&term) {
            let data = record.tail.tail(since);
            self.emit(Event::TerminalOutput { term, data });
        }
    }

    /// Stops the shell, keeping the session and its scrollback.
    fn sleep_terminal(&mut self, term: TermId) {
        let Some(session) = self.host.sessions.get(&term) else { return };
        if !matches!(session.state, Sleep::Awake) {
            return;
        }
        let size = self.terminals.size(term).unwrap_or_default();
        let thread = self.session_thread(term);
        self.release_held(term);
        if let Some(session) = self.host.sessions.get_mut(&term) {
            session.state = Sleep::Stopping { thread, size, wake: None, input: Vec::new() };
        }
        if !self.terminals.close(term) {
            // Its shell already ended; no exit will come to finish this.
            if let Some(session) = self.host.sessions.get_mut(&term) {
                session.state = Sleep::Asleep { thread, size };
            }
        }
        self.status_terminal_closed(term);
        self.emit(Event::TerminalSleep { term, sleeping: true });
    }

    /// Starts a fresh shell for a sleeping terminal, under the same id.
    fn wake_terminal(&mut self, term: TermId, size: TermSize) -> Result<(), String> {
        let Some(session) = self.host.sessions.get_mut(&term) else { return Err("That terminal is closed.".into()) };
        let thread = match &mut session.state {
            Sleep::Awake => return Ok(()),
            Sleep::Stopping { wake, .. } => {
                *wake = Some(size);
                return Ok(());
            }
            Sleep::Asleep { thread, .. } => *thread,
        };
        session.state = Sleep::Awake;
        let cwd = session.cwd.clone();
        if let Err(err) = self.open_terminal_as(Some(term), &cwd, size) {
            self.host.sessions.remove(&term);
            self.cli.exited(term, None);
            self.emit(Event::TerminalExited { term, code: None });
            return Err(err);
        }
        if let Some(thread) = thread
            && self.store.thread(thread).is_some()
        {
            let _ = self.bind_terminal(term, Some(thread));
        }
        if let Some(record) = self.cli.terminals.get_mut(&term) {
            record.info.running = true;
        }
        self.terminal_output(term, WOKE.to_vec());
        self.emit(Event::TerminalSleep { term, sleeping: false });
        Ok(())
    }

    /// Periodic work: release stale holds, put idle terminals to sleep.
    pub(crate) fn host_tick(&mut self) {
        if self.host.sessions.is_empty() {
            return;
        }
        let now = Instant::now();
        let stale: Vec<TermId> = self
            .host
            .sessions
            .iter()
            .filter(|(_, s)| s.held.is_some_and(|(_, at)| now - at >= HOLD_LIMIT))
            .map(|(t, _)| *t)
            .collect();
        for term in stale {
            self.release_held(term);
        }
        if now < self.host.next_check {
            return;
        }
        let Some(sleep_after) = self.host.sleep_after else { return };
        self.host.next_check = now + sleep_after.min(SLEEP_CHECK);
        // Sleeping terminals of deleted threads go for good.
        let orphans: Vec<TermId> = self
            .host
            .sessions
            .iter()
            .filter(|(_, s)| matches!(s.state, Sleep::Asleep { thread: Some(t), .. } if self.store.thread(t).is_none()))
            .map(|(t, _)| *t)
            .collect();
        for term in orphans {
            self.host_terminal_closed(term);
        }
        let idle: Vec<TermId> = self
            .host
            .sessions
            .iter()
            .filter(|(_, s)| matches!(s.state, Sleep::Awake) && s.held.is_none() && now - s.last_io >= sleep_after)
            .map(|(t, _)| *t)
            .collect();
        for term in idle {
            let agent = self.status.terminals.get(&term).is_some_and(|w| w.agent().is_some());
            let working = self
                .session_thread(term)
                .and_then(|t| self.runtimes.get(&t))
                .is_some_and(|r| r.turn.is_some() || r.held_prompt.is_some());
            if !agent && !working && !self.terminals.busy(term) {
                self.sleep_terminal(term);
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn session_thread(&self, term: TermId) -> Option<ThreadId> {
        match &self.host.sessions.get(&term)?.state {
            Sleep::Awake => self.status.terminals.get(&term).and_then(|w| w.thread),
            Sleep::Stopping { thread, .. } | Sleep::Asleep { thread, .. } => *thread,
        }
    }

    fn stored_size(&self, term: TermId) -> Option<TermSize> {
        match &self.host.sessions.get(&term)?.state {
            Sleep::Awake => self.terminals.size(term),
            Sleep::Stopping { size, .. } | Sleep::Asleep { size, .. } => Some(*size),
        }
    }

    /// Bytes the terminal ever wrote (what the scrollback kept or dropped).
    fn output_total(&self, term: TermId) -> u64 {
        self.cli.terminals.get(&term).map_or(0, |r| r.tail.dropped() + r.tail.len() as u64)
    }

    fn term_session(&self, term: TermId) -> TermSession {
        let session = self.host.sessions.get(&term);
        let size = self.stored_size(term).unwrap_or_default();
        TermSession {
            term,
            thread: self.session_thread(term),
            cwd: session.map(|s| s.cwd.clone()).unwrap_or_default(),
            cols: size.cols,
            rows: size.rows,
            sleeping: session.is_some_and(|s| !matches!(s.state, Sleep::Awake)),
            busy: self.terminals.busy(term),
        }
    }
}
