//! Reordering queued messages and sending one ahead of its turn.
//!
//! Queue order is item order: clients keep a timeline sorted by item id
//! (ids are time-ordered) and a restart rebuilds the queue from the
//! timeline. Moving a message therefore re-issues the messages that must
//! come after it under fresh, larger ids, so every client and every restart
//! sees the same order without a separate ordering record.

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};

impl Orchestrator {
    fn queued_ids(&self, thread: ThreadId) -> Vec<ItemId> {
        self.runtimes.get(&thread).map(|r| r.queue.iter().copied().collect()).unwrap_or_default()
    }

    pub(crate) fn move_queued(&mut self, thread: ThreadId, item: ItemId, to: u32) -> CommandResult {
        self.store.load_timeline(thread).map_err(|e| e.to_string())?;
        let old = self.queued_ids(thread);
        let from = old.iter().position(|id| *id == item).ok_or("That message was already sent.")?;
        let mut order = old.clone();
        order.remove(from);
        order.insert((to as usize).min(order.len()), item);
        if order == old {
            return Ok(Reply::Done);
        }
        // Keep each message whose id is still in order; re-issue the rest.
        let mut last: Option<ItemId> = None;
        let mut next_queue = Vec::with_capacity(order.len());
        for id in order {
            if last.is_none_or(|last| id > last) {
                next_queue.push(id);
                last = Some(id);
                continue;
            }
            let Some(mut message) = self.item(thread, id) else { continue };
            self.emit(Event::ItemRemoved { thread, item: id });
            message.id = ItemId::new();
            last = Some(message.id);
            next_queue.push(message.id);
            self.put_item(thread, message);
        }
        if let Some(runtime) = self.runtimes.get_mut(&thread) {
            runtime.queue = next_queue.into();
        }
        Ok(Reply::Done)
    }

    pub(crate) fn steer_queued(&mut self, thread: ThreadId, item_id: ItemId) -> CommandResult {
        let item = self.item(thread, item_id).ok_or("That message no longer exists.")?;
        if item.status != ItemStatus::Queued {
            return Err("That message was already sent.".into());
        }
        let ItemBody::User(mut message) = item.body.clone() else {
            return Err("Only messages can be sent.".into());
        };
        let runtime = self.runtimes.get(&thread);
        let in_turn = runtime.is_some_and(|r| r.turn.is_some());
        let checkpointing = runtime.is_some_and(|r| r.checkpointing);
        let status = self.store.thread(thread).map(|t| t.status.clone()).unwrap_or_default();
        let steerable = in_turn && !checkpointing && status != ThreadStatus::Starting;
        let idle = !in_turn && !checkpointing && !status.is_busy() && !matches!(status, ThreadStatus::RateLimited { .. });
        if !steerable && !idle {
            // Nothing can take it this instant (a snapshot or a quota
            // wait): it goes first instead.
            return self.move_queued(thread, item_id, 0);
        }
        if let Some(runtime) = self.runtimes.get_mut(&thread) {
            runtime.queue.retain(|id| *id != item_id);
        }
        self.update_thread(thread, |t| t.queued = t.queued.saturating_sub(1));
        if idle {
            return self.start_turn(thread, item_id).map(|_| Reply::Done);
        }
        let turn = self.active_turn_number(thread).unwrap_or(item.turn);
        message.steer = true;
        let (text, attachments) = (message.text.clone(), message.attachments.clone());
        // It joins the conversation now, so it takes its place at the end.
        self.emit(Event::ItemRemoved { thread, item: item_id });
        let id = ItemId::new();
        self.put_item(thread, Item { id, status: ItemStatus::Done, turn, body: ItemBody::User(message) });
        let prompt = self.prompt_input(thread, id, text, &attachments, true);
        if let Some(session) = self.runtimes.get(&thread).and_then(|r| r.session.as_ref()) {
            session.send(prompt);
        }
        Ok(Reply::Done)
    }
}
