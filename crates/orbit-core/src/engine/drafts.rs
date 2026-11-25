//! Unsent messages, kept on the host.
//!
//! A draft is a durable record in the meta log ([`Event::DraftSaved`]), so it
//! survives restarts, replays to every client of the host and is part of the
//! snapshot a new client receives. Composers save after a short pause in
//! typing; saving the same content again is free (no event), and an empty
//! draft removes the record. A thread's draft goes with the thread, and the
//! blobs a draft attaches are kept by garbage collection.

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};

/// Largest draft text kept, in bytes. Far above what can be sent; it only
/// stops a runaway paste from bloating the meta log.
const MAX_DRAFT_BYTES: usize = 2 * 1024 * 1024;

impl Orchestrator {
    pub(crate) fn save_draft(&mut self, target: DraftTarget, mut draft: Draft) -> CommandResult {
        if let DraftTarget::Thread(id) = target
            && self.store.thread(id).is_none()
        {
            return Err("That thread no longer exists.".into());
        }
        if draft.text.len() > MAX_DRAFT_BYTES {
            return Err("This draft is too long to keep. Attach the text as a file instead.".into());
        }
        let unchanged = match self.store.draft(target) {
            Some(current) => current.same_content(&draft),
            None => draft.is_empty(),
        };
        if unchanged {
            return Ok(Reply::Done);
        }
        draft.updated_ms = if draft.is_empty() { 0 } else { now_ms() };
        self.emit(Event::DraftSaved { target, draft: Box::new(draft) });
        Ok(Reply::Done)
    }
}
