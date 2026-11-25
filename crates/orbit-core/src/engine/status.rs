//! The honest status engine, engine side: liveness of thread sessions,
//! repairs of states that can no longer be true, sleep detection, and the
//! status of agents running in terminals. See `docs/STATUS.md`.
//!
//! Everything here piggybacks on work the engine already does: agent
//! events, PTY output and the 500 ms tick. Nothing polls on its own, and an
//! idle engine (no turn, no terminal agent) does a few map lookups per tick.

use std::path::Path;
use std::time::Instant;

use crossfire::BlockingTxTrait as _;
use orbit_proto::*;

use super::orchestrator::{Internal, Orchestrator};
use crate::provider::{AgentEvent, TurnOutcome};
use crate::status::hook::{self, HookMessage};
use crate::status::term::TermWatch;
use crate::status::{ClockJump, TurnWatch, new_token, silence_threshold};
use crate::terminal::{Shell, TermSize};

impl Orchestrator {
    // ── Thread sessions ──────────────────────────────────────────────────

    /// Every agent event is a sign of life. Called before the event is
    /// applied.
    pub(crate) fn status_on_agent(&mut self, thread: ThreadId, event: &AgentEvent) {
        let (now, ms) = self.clock();
        if !self.status.lost.is_empty() {
            self.status.lost.remove(&thread);
        }
        let in_turn = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
        match event {
            AgentEvent::TurnFinished { .. } | AgentEvent::Exited { .. } => {
                if self.status.turns.remove(&thread).is_some() {
                    self.publish_live(thread);
                }
                return;
            }
            AgentEvent::TurnStarted { .. } => {}
            _ if !in_turn => return,
            _ => {}
        }
        let watch = self.status.turns.entry(thread).or_insert_with(|| TurnWatch::new(now, ms));
        match event {
            AgentEvent::ToolStarted(call) => {
                let title = if call.title.is_empty() { call.name.clone() } else { call.title.clone() };
                match watch.open_tools.iter_mut().find(|(id, _)| *id == call.call_id) {
                    Some(slot) => slot.1 = title,
                    None => watch.open_tools.push((call.call_id.clone(), title)),
                }
            }
            AgentEvent::ToolFinished { call_id, .. } => watch.open_tools.retain(|(id, _)| id != call_id),
            _ => {}
        }
        if watch.signal(now, ms) {
            self.publish_live(thread);
        }
    }

    /// The session's process is gone. Requests it raised can no longer be
    /// answered; a thread must not keep waiting on them. Called after the
    /// exit was applied.
    pub(crate) fn status_after_exit(&mut self, thread: ThreadId) {
        let busy = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
        if busy {
            return; // the turn's own end settles them
        }
        let pending: Vec<RequestId> =
            self.store.thread(thread).map(|t| t.pending.iter().map(|p| p.request.id).collect()).unwrap_or_default();
        if pending.is_empty() {
            return;
        }
        for request in pending {
            self.settle_request(thread, request, Decision::Deny { feedback: "The agent stopped before this was answered.".into() });
        }
        self.notice(thread, NoticeLevel::Warning, "The agent stopped while it was waiting for you. Send a message to continue.".into());
    }

    /// A turn is ending: requests still open in it are cancelled, so no
    /// approval card stays live for an agent that is no longer asking.
    pub(crate) fn status_close_requests(&mut self, thread: ThreadId) {
        let items: Vec<ItemId> = self.store.thread(thread).map(|t| t.pending.iter().map(|p| p.item).collect()).unwrap_or_default();
        for id in items {
            if let Some(mut item) = self.item(thread, id)
                && item.status == ItemStatus::Streaming
            {
                item.status = ItemStatus::Cancelled;
                self.put_item(thread, item);
            }
        }
    }

    /// The periodic part, on the engine's existing tick.
    pub(crate) fn status_tick(&mut self) {
        self.status_tick_at(Instant::now(), now_ms());
    }

    pub(crate) fn status_tick_at(&mut self, now: Instant, ms: u64) {
        if let Some(jump) = self.status.clock.observe(now, ms) {
            self.reverify(jump, now, ms);
        }
        self.find_lost_sessions();
        self.age_turns(now);
        self.repair_statuses();
        let mut changed_terms = Vec::new();
        for (term, watch) in self.status.terminals.iter_mut() {
            if watch.tick(now, ms) {
                changed_terms.push(*term);
            }
        }
        for term in changed_terms {
            self.terminal_agent_changed(term);
        }
    }

    /// Sessions whose task ended without a word (it panicked, or a bug
    /// dropped its exit) are found here: their input channel is closed. Two
    /// sightings a tick apart, so an exit still in the event queue wins.
    fn find_lost_sessions(&mut self) {
        let closed: Vec<ThreadId> = self
            .runtimes
            .iter()
            .filter(|(_, r)| r.session.as_ref().is_some_and(|s| s.input.is_disconnected()))
            .map(|(id, _)| *id)
            .collect();
        let previously = std::mem::take(&mut self.status.lost);
        for thread in closed {
            if previously.contains(&thread) {
                self.lost_contact(thread);
            } else {
                self.status.lost.insert(thread);
            }
        }
    }

    fn lost_contact(&mut self, thread: ThreadId) {
        log::warn!("lost contact with the agent of {thread}");
        if let Some(runtime) = self.runtimes.get_mut(&thread) {
            runtime.session = None;
        }
        let busy = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
        if busy {
            let message = "Orbit lost contact with the agent: its session ended without reporting why.".to_owned();
            self.finish_turn(thread, TurnOutcome::Failed { message }, Usage::default(), 0);
        } else {
            self.status_after_exit(thread);
        }
    }

    /// Marks running turns quiet after their provider's silence threshold.
    fn age_turns(&mut self, now: Instant) {
        let running: Vec<(ThreadId, Instant, bool, ProviderKind)> = self
            .runtimes
            .iter()
            .filter_map(|(id, r)| {
                let turn = r.turn.as_ref()?;
                let thread = self.store.thread(*id)?;
                // Blocked on the user, or on Orbit's own snapshot: silence
                // is expected and says nothing about the agent.
                let waiting = thread.status == ThreadStatus::Waiting || !thread.pending.is_empty() || r.held_prompt.is_some();
                Some((*id, turn.started, waiting, thread.config.provider))
            })
            .collect();
        let live: std::collections::HashSet<ThreadId> = running.iter().map(|(id, ..)| *id).collect();
        let stale: Vec<ThreadId> = self.status.turns.keys().filter(|id| !live.contains(id)).copied().collect();
        for thread in stale {
            self.status.turns.remove(&thread);
            self.publish_live(thread);
        }
        for (thread, started, waiting, provider) in running {
            let started_ms = now_ms().saturating_sub(now.saturating_duration_since(started).as_millis() as u64);
            let watch = self.status.turns.entry(thread).or_insert_with(|| TurnWatch::new(started, started_ms));
            if waiting {
                // The clock restarts when the user answers.
                watch.last_signal = now;
                watch.last_signal_ms = now_ms();
            }
            if watch.tick(now, silence_threshold(provider), waiting) {
                self.publish_live(thread);
            }
        }
    }

    /// States that can no longer be true are put right: a thread "running"
    /// with no turn, or "waiting" with nothing to answer.
    fn repair_statuses(&mut self) {
        let mut fixes: Vec<(ThreadId, ThreadStatus)> = Vec::new();
        for thread in self.store.threads() {
            let runtime = self.runtimes.get(&thread.id);
            let in_turn = runtime.is_some_and(|r| r.turn.is_some() || r.held_prompt.is_some());
            match thread.status {
                ThreadStatus::Running if !in_turn && !runtime.is_some_and(|r| r.checkpointing) => {
                    fixes.push((thread.id, ThreadStatus::Idle));
                }
                ThreadStatus::Waiting if thread.pending.is_empty() => {
                    fixes.push((thread.id, if in_turn { ThreadStatus::Running } else { ThreadStatus::Idle }));
                }
                ThreadStatus::Idle if runtime.is_some_and(|r| r.turn.is_some()) => {
                    fixes.push((thread.id, ThreadStatus::Running));
                }
                _ => {}
            }
        }
        for (thread, status) in fixes {
            log::warn!("{thread}: status repaired to {status:?}");
            self.update_thread(thread, |t| t.status = status);
        }
    }

    /// Time jumped: the machine slept or the clock changed. Nothing heard
    /// before can be taken as still true, and timers set by duration may
    /// have missed their moment.
    fn reverify(&mut self, jump: ClockJump, now: Instant, ms: u64) {
        let reason = match jump {
            ClockJump::Slept { missed } => {
                format!("Orbit was asleep for {}; waiting for the agent to report", short_duration(missed.as_millis() as u64))
            }
            ClockJump::Jumped { .. } => "the system clock changed; waiting for the agent to report".to_owned(),
        };
        log::info!("re-verifying live sessions: {jump:?}");
        let threads: Vec<ThreadId> = self.status.turns.keys().copied().collect();
        for thread in threads {
            let waiting = self.store.thread(thread).is_some_and(|t| t.status == ThreadStatus::Waiting);
            if !waiting && let Some(watch) = self.status.turns.get_mut(&thread) {
                watch.last_signal = now;
                watch.last_signal_ms = ms;
                if watch.distrust(ms, reason.clone()) {
                    self.publish_live(thread);
                }
            }
        }
        let terms: Vec<TermId> = self.status.terminals.keys().copied().collect();
        for term in terms {
            if self.status.terminals.get_mut(&term).is_some_and(|w| w.distrust(ms)) {
                self.terminal_agent_changed(term);
            }
        }
        // Timers count monotonic time, which stops during sleep on some
        // systems: catch up on snoozes and quota resets that are due.
        let due_snoozes: Vec<ThreadId> =
            self.store.threads().filter(|t| t.snoozed_until.is_some_and(|until| until <= ms)).map(|t| t.id).collect();
        for thread in due_snoozes {
            self.wake(thread);
        }
        let due_quota: Vec<ThreadId> = self
            .store
            .threads()
            .filter(|t| matches!(t.status, ThreadStatus::RateLimited { resets_ms } if resets_ms > 0 && resets_ms <= ms))
            .map(|t| t.id)
            .collect();
        for thread in due_quota {
            let _ = self.internal_tx.send(Internal::QuotaReset(thread));
        }
    }

    // ── Terminals ────────────────────────────────────────────────────────

    /// Opens a terminal whose agents can report to Orbit, and binds it to
    /// the thread whose folder it opens in, if exactly one open thread
    /// works there.
    pub(crate) fn open_terminal(&mut self, cwd: &str, size: TermSize) -> Result<Reply, String> {
        self.open_terminal_as(None, cwd, size).map(Reply::Terminal)
    }

    /// Opens an interactive shell, under `id` when given (a sleeping
    /// terminal waking up keeps its id).
    pub(crate) fn open_terminal_as(&mut self, id: Option<TermId>, cwd: &str, size: TermSize) -> Result<TermId, String> {
        let endpoint = self.hook_endpoint();
        let token = new_token();
        let mut shell = Shell::user_default();
        if let Some(endpoint) = &endpoint {
            let injection = hook::injection(self.store.dir(), endpoint, &token, shell.program());
            shell = shell.args(injection.args);
            for (key, value) in injection.env {
                shell = shell.env(key, value);
            }
        }
        let term = self
            .terminals
            .open_as(id.unwrap_or_else(TermId::new), Path::new(cwd), size, Some(shell))
            .map_err(|e| format!("Couldn’t open a terminal in {cwd}: {e}"))?;
        let thread = self.thread_for_folder(cwd);
        self.status.terminals.insert(term, TermWatch::new(term, token, thread));
        self.host_terminal_opened(term, cwd);
        Ok(term)
    }

    /// The hook endpoint, started on first use.
    fn hook_endpoint(&mut self) -> Option<String> {
        if self.status.endpoint.is_none() && !self.status.endpoint_failed {
            let endpoint = hook::endpoint(self.store.dir());
            let tx = self.internal_tx.clone();
            match hook::serve(&endpoint, move |msg| {
                let _ = tx.send(Internal::Hook(msg));
            }) {
                Ok(()) => self.status.endpoint = Some(endpoint),
                Err(err) => {
                    // Terminals still report through OSC and heuristics.
                    log::warn!("hook endpoint {endpoint}: {err}");
                    self.status.endpoint_failed = true;
                }
            }
        }
        self.status.endpoint.clone()
    }

    /// The one open thread working in `folder`, if there is exactly one.
    fn thread_for_folder(&self, folder: &str) -> Option<ThreadId> {
        let want = normalize(folder);
        let candidates: Vec<ThreadId> = self
            .store
            .threads()
            .filter(|t| !t.archived)
            .filter(|t| {
                let dir = match (&t.workspace.worktree, t.project.and_then(|p| self.store.project(p))) {
                    (Some(worktree), _) => worktree.clone(),
                    (None, Some(project)) => project.path.clone(),
                    (None, None) => return false,
                };
                normalize(&dir) == want
            })
            .map(|t| t.id)
            .collect();
        let open: Vec<ThreadId> = candidates.iter().copied().filter(|id| self.open.contains(id)).collect();
        match (open.as_slice(), candidates.as_slice()) {
            ([one], _) | ([], [one]) => Some(*one),
            _ => None,
        }
    }

    pub(crate) fn bind_terminal(&mut self, term: TermId, thread: Option<ThreadId>) -> Result<Reply, String> {
        if thread.is_some_and(|t| self.store.thread(t).is_none()) {
            return Err("That thread no longer exists.".into());
        }
        let Some(watch) = self.status.terminals.get_mut(&term) else {
            return Err("That terminal is closed.".into());
        };
        let previous = std::mem::replace(&mut watch.thread, thread);
        watch.bound_explicitly = true;
        for thread in [previous, thread].into_iter().flatten() {
            self.publish_live(thread);
        }
        Ok(Reply::Done)
    }

    /// PTY output: OSC reports, titles and liveness. Cheap when nothing
    /// agent-like runs: one `memchr` pass for ESC.
    pub(crate) fn status_terminal_output(&mut self, term: TermId, data: &[u8]) {
        let Some(watch) = self.status.terminals.get_mut(&term) else { return };
        if watch.on_output(data, Instant::now(), now_ms()) {
            self.terminal_agent_changed(term);
        }
    }

    pub(crate) fn status_terminal_input(&mut self, term: TermId, data: &[u8]) {
        let Some(watch) = self.status.terminals.get_mut(&term) else { return };
        if watch.on_input(data, Instant::now(), now_ms()) {
            self.terminal_agent_changed(term);
        }
    }

    pub(crate) fn status_terminal_closed(&mut self, term: TermId) {
        if let Some(watch) = self.status.terminals.remove(&term) {
            let had_agent = watch.agent().is_some();
            if had_agent {
                self.emit(Event::TerminalAgent { term, agent: None });
            }
            if let Some(thread) = watch.thread {
                self.publish_live(thread);
            }
        }
    }

    /// A hook call arrived on the endpoint.
    pub(crate) fn on_hook(&mut self, msg: HookMessage) {
        let Some((term, watch)) = self.status.terminals.iter_mut().find(|(_, w)| w.token == msg.token) else {
            log::debug!("hook call with an unknown token");
            return;
        };
        let term = *term;
        if watch.on_hook(&msg.call, Instant::now(), now_ms()) {
            self.terminal_agent_changed(term);
        }
    }

    fn terminal_agent_changed(&mut self, term: TermId) {
        let Some(watch) = self.status.terminals.get(&term) else { return };
        let (agent, thread) = (watch.agent(), watch.thread);
        self.emit(Event::TerminalAgent { term, agent: agent.map(Box::new) });
        if let Some(thread) = thread {
            self.publish_live(thread);
        }
    }

    // ── Publishing ───────────────────────────────────────────────────────

    pub(crate) fn thread_live(&self, thread: ThreadId) -> ThreadLive {
        let (liveness, last_signal_ms) = match self.status.turns.get(&thread) {
            Some(w) => (w.liveness.clone(), w.last_signal_ms),
            None => (Liveness::Live, 0),
        };
        let mut terminals: Vec<TerminalAgent> = self
            .status
            .terminals
            .values()
            .filter(|w| w.thread == Some(thread))
            .filter_map(TermWatch::agent)
            .collect();
        terminals.sort_by_key(|a| a.term);
        ThreadLive { liveness, last_signal_ms, terminals }
    }

    /// Broadcasts a thread's live status if it changed in a way worth
    /// drawing. Signal timestamps alone never cause an event.
    pub(crate) fn publish_live(&mut self, thread: ThreadId) {
        if self.store.thread(thread).is_none() {
            self.status.published.remove(&thread);
            return;
        }
        let live = self.thread_live(thread);
        let previous = self.status.published.get(&thread).cloned().unwrap_or_default();
        let same = previous.liveness == live.liveness && previous.terminals == live.terminals;
        // A turn starting or ending is worth an event; a new signal is not.
        let same_turn = (previous.last_signal_ms == 0) == (live.last_signal_ms == 0);
        if same && same_turn {
            return;
        }
        if live.is_default() {
            self.status.published.remove(&thread);
        } else {
            self.status.published.insert(thread, live.clone());
        }
        self.emit(Event::ThreadLive { thread, live: Box::new(live) });
    }

    /// Live status for a fresh client's snapshot.
    pub(crate) fn live_snapshot(&self) -> Vec<(ThreadId, ThreadLive)> {
        let mut threads: Vec<ThreadId> = self.status.turns.keys().copied().collect();
        threads.extend(self.status.terminals.values().filter_map(|w| w.thread));
        threads.sort();
        threads.dedup();
        threads
            .into_iter()
            .filter(|t| self.store.thread(*t).is_some())
            .map(|t| (t, self.thread_live(t)))
            .filter(|(_, live)| !live.is_default())
            .collect()
    }
}

/// Folder paths compared loosely: separators, a trailing slash and (on
/// Windows) case do not matter.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if cfg!(windows) { path.to_lowercase() } else { path.to_owned() }
}
