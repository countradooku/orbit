//! Fan-out of events to subscribers with replay.
//!
//! Every subscriber has its own bounded channel. Publishing never blocks: a
//! subscriber whose channel is full is dropped, sees its channel close, and
//! resubscribes with the last sequence it applied. A ring of recent durable
//! envelopes serves those replays; a gap older than the ring gets a snapshot.

use std::collections::VecDeque;

use crossfire::{AsyncRx, BlockingTxTrait, MTx, mpsc};
use orbit_proto::Envelope;

/// Envelopes kept for `since` replay.
const REPLAY_RING: usize = 16 * 1024;
/// Per-subscriber queue depth. Deep enough for a burst of streaming deltas
/// between two frames of a busy UI, small enough to bound memory.
const SUBSCRIBER_QUEUE: usize = 8 * 1024;

pub type EventRx = AsyncRx<mpsc::Array<Envelope>>;

pub struct Bus {
    subscribers: Vec<MTx<mpsc::Array<Envelope>>>,
    ring: VecDeque<Envelope>,
}

impl Bus {
    pub fn new() -> Self {
        Self { subscribers: Vec::new(), ring: VecDeque::with_capacity(1024) }
    }

    /// Adds a subscriber. Returns its receiver and, when `since` is still in
    /// the ring, the envelopes it missed (`None` means it needs a snapshot).
    pub fn subscribe(&mut self, since: Option<u64>) -> (EventRx, Option<Vec<Envelope>>) {
        let (tx, rx) = mpsc::bounded_blocking_async(SUBSCRIBER_QUEUE);
        self.subscribers.push(tx);
        let replay = since.and_then(|since| {
            let oldest = self.ring.front().map(|e| e.seq).unwrap_or(u64::MAX);
            if since + 1 < oldest && !self.ring.is_empty() {
                return None;
            }
            Some(self.ring.iter().filter(|e| e.seq > since).cloned().collect())
        });
        (rx, replay)
    }

    pub fn publish(&mut self, env: Envelope) {
        if env.seq != 0 {
            if self.ring.len() == REPLAY_RING {
                self.ring.pop_front();
            }
            self.ring.push_back(env.clone());
        }
        self.subscribers.retain(|tx| tx.try_send(env.clone()).is_ok());
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_proto::{Event, ThreadId};

    fn env(seq: u64) -> Envelope {
        Envelope { seq, event: Event::ThreadRemoved(ThreadId(seq)) }
    }

    #[test]
    fn replays_what_a_subscriber_missed() {
        let mut bus = Bus::new();
        for seq in 1..=5 {
            bus.publish(env(seq));
        }
        let (_rx, replay) = bus.subscribe(Some(3));
        let seqs: Vec<u64> = replay.unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4, 5]);
    }

    #[test]
    fn slow_subscribers_are_dropped_not_waited_for() {
        let mut bus = Bus::new();
        let (rx, _) = bus.subscribe(None);
        for seq in 1..=(SUBSCRIBER_QUEUE as u64 + 10) {
            bus.publish(env(seq));
        }
        assert_eq!(bus.subscriber_count(), 0);
        drop(rx);
    }
}
