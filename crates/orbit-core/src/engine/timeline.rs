//! Timeline recovery: retrying a failed turn, forcing a stuck agent to stop,
//! resuming after a usage limit, and reading full tool output.
//!
//! A retry keeps the failed attempt in the timeline (the reader can still see
//! what went wrong) and runs the turn again under the same turn number, so the
//! client can fold the superseded attempt. The provider's own history is
//! rewound to before the turn when a cursor exists, so the agent does not see
//! the message twice.

use orbit_proto::*;

use super::orchestrator::{CommandResult, Deferred, Orchestrator};
use crate::provider::{SessionInput, TurnOutcome};

impl Orchestrator {
    /// Runs the last turn again after it failed or hit a usage limit.
    pub(crate) fn retry(&mut self, thread_id: ThreadId) -> CommandResult {
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        if thread.archived {
            return Err("This thread is archived. Unarchive it to continue.".into());
        }
        let runtime = self.runtimes.get(&thread_id);
        if runtime.is_some_and(|r| r.turn.is_some() || r.checkpointing || r.held_prompt.is_some()) {
            return Err("The agent is still working on this thread.".into());
        }
        if !matches!(thread.status, ThreadStatus::Failed { .. } | ThreadStatus::RateLimited { .. }) {
            return Err("Only a failed turn can be retried.".into());
        }
        let turn = thread.turns;
        let (message, cursor) = {
            let timeline = self.store.load_timeline(thread_id).map_err(|e| e.to_string())?;
            let items = timeline.items();
            let message = items
                .iter()
                .find(|i| {
                    i.turn == turn
                        && i.status == ItemStatus::Done
                        && matches!(&i.body, ItemBody::User(m) if !m.steer)
                })
                .map(|i| i.id);
            let cursor = items.iter().rev().filter(|i| i.turn < turn).find_map(|i| match &i.body {
                ItemBody::TurnEnd(summary) => summary.provider_cursor.clone(),
                _ => None,
            });
            (message, cursor)
        };
        let message = message.ok_or("This turn has no message to send again.")?;
        // Rewind the provider to before the failed turn. Without a cursor the
        // first turn starts a fresh session; a later turn keeps its history
        // rather than lose every earlier turn.
        let fresh = cursor.is_none() && turn <= 1;
        let runtime = self.runtimes.entry(thread_id).or_default();
        if cursor.is_some() || fresh {
            if let Some(session) = runtime.session.take() {
                session.send(SessionInput::Shutdown);
            }
            runtime.resume_at = cursor;
        }
        self.update_thread(thread_id, |t| {
            t.turns = turn.saturating_sub(1);
            t.status = ThreadStatus::Idle;
            if fresh {
                t.session = None;
            }
        });
        self.start_turn(thread_id, message).map(|_| Reply::Done)
    }

    /// Ends the agent's process when Stop did not take. The process tree is
    /// shut down, the turn ends as stopped and the thread is ready for the
    /// next message, which resumes the session.
    pub(crate) fn force_stop(&mut self, thread_id: ThreadId) -> CommandResult {
        let Some(runtime) = self.runtimes.get_mut(&thread_id) else { return Ok(Reply::Done) };
        let running = runtime.turn.is_some();
        runtime.held_prompt = None;
        if let Some(turn) = runtime.turn.as_mut() {
            turn.interrupted = true;
        }
        if let Some(session) = runtime.session.take() {
            session.send(SessionInput::Shutdown);
        }
        if running {
            self.finish_turn(thread_id, TurnOutcome::Interrupted, Usage::default(), 0);
            self.notice(thread_id, NoticeLevel::Info, "Stopped the agent’s process.".into());
        }
        Ok(Reply::Done)
    }

    /// The provider's usage window reset: run the limited turn again, then
    /// whatever was queued behind it.
    pub(crate) fn quota_reset(&mut self, thread_id: ThreadId) {
        let limited =
            matches!(self.store.thread(thread_id).map(|t| &t.status), Some(ThreadStatus::RateLimited { .. }));
        if !limited {
            return;
        }
        self.notice(thread_id, NoticeLevel::Info, "Your usage limit has reset.".into());
        if self.retry(thread_id).is_err() {
            self.update_thread(thread_id, |t| t.status = ThreadStatus::Idle);
            self.start_next_queued(thread_id);
        }
    }

    /// Reads a blob on the blocking pool. Only well-formed content hashes are
    /// accepted, so a remote client cannot name a path outside the store.
    pub(crate) fn read_blob(&self, hash: String) -> Deferred {
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        let path = valid.then(|| self.store.blob_path(&hash));
        Box::pin(async move {
            let Some(path) = path else { return Err("That output is not available.".into()) };
            tokio::task::spawn_blocking(move || std::fs::read(&path))
                .await
                .map_err(|e| e.to_string())?
                .map(Reply::Bytes)
                .map_err(|e| format!("Couldn’t read the full output: {e}"))
        })
    }
}
