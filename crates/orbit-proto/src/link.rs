//! The host link's message framing: the hot messages get compact frames.
//!
//! Streamed tokens are most of the traffic, and as `Event::ItemDelta`s each
//! would repeat its thread and item ids (time-ordered `u64`s, nine bytes
//! each as varints) for a few bytes of text. On the link the two hot
//! messages get frames of their own, told apart by the first byte:
//!
//! * [`DELTAS`]: streamed tokens, grouped into [`DeltaRun`]s that name their
//!   thread and item once;
//! * [`DONE`]: `Ok(Reply::Done)` for call `id`, four bytes;
//! * [`MSG`]: any other [`ServerMsg`].
//!
//! Each side keeps one [`ServerEncoder`] or [`ServerDecoder`] per
//! connection, so buffers are reused. Clients see plain [`ServerMsg`]s.

use crate::codec::{Codec, FrameError};
use crate::ids::{ItemId, ThreadId};
use crate::wire::{ClientMsg, Delta, Envelope, Event, Reply, ServerMsg};

/// First byte: a [`ServerMsg`] follows.
pub const MSG: u8 = 0;
/// First byte: a `Vec<DeltaRun>` follows.
pub const DELTAS: u8 = 1;
/// First byte: a call id (`u32` LE) whose reply is `Ok(Reply::Done)`.
pub const DONE: u8 = 2;

/// Consecutive streamed tokens into one item, without the [`Event`]s
/// around them.
#[derive(Clone, Debug, PartialEq, wincode::SchemaWrite, wincode::SchemaRead)]
pub struct DeltaRun {
    pub thread: ThreadId,
    pub item: ItemId,
    pub deltas: Vec<Delta>,
}

/// The host's side: frames server messages for the link.
#[derive(Default)]
pub struct ServerEncoder {
    msg: Codec,
    deltas: Codec,
    runs: Vec<DeltaRun>,
}

impl ServerEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames any message.
    pub fn message(&mut self, msg: &ServerMsg) -> Vec<u8> {
        if let ServerMsg::Reply { id, result: Ok(Reply::Done) } = msg {
            let mut out = Vec::with_capacity(5);
            out.push(DONE);
            out.extend_from_slice(&id.to_le_bytes());
            return out;
        }
        frame(MSG, self.msg.encode(msg))
    }

    /// Frames `events` in order: runs of streamed tokens as [`DELTAS`]
    /// frames, the rest as [`ServerMsg::Events`]. Usually one frame.
    pub fn events(&mut self, events: Vec<Envelope>) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut other: Vec<Envelope> = Vec::new();
        for env in events {
            match env {
                Envelope { seq: 0, event: Event::ItemDelta { thread, item, delta } } => {
                    if !other.is_empty() {
                        frames.push(frame(MSG, self.msg.encode(&ServerMsg::Events(std::mem::take(&mut other)))));
                    }
                    match self.runs.last_mut() {
                        Some(run) if run.thread == thread && run.item == item => run.deltas.push(delta),
                        _ => self.runs.push(DeltaRun { thread, item, deltas: vec![delta] }),
                    }
                }
                env => {
                    if !self.runs.is_empty() {
                        frames.push(frame(DELTAS, self.deltas.encode(&self.runs)));
                        self.runs.clear();
                    }
                    other.push(env);
                }
            }
        }
        if !self.runs.is_empty() {
            frames.push(frame(DELTAS, self.deltas.encode(&self.runs)));
            self.runs.clear();
        }
        if !other.is_empty() {
            frames.push(frame(MSG, self.msg.encode(&ServerMsg::Events(other))));
        }
        frames
    }

    /// Reads a client message.
    pub fn client(&mut self, bytes: &[u8]) -> Result<ClientMsg, FrameError> {
        self.msg.decode(bytes)
    }
}

/// The client's side: reads the host's frames, writes calls.
#[derive(Default)]
pub struct ServerDecoder {
    msg: Codec,
    deltas: Codec,
    calls: Codec,
}

impl ServerDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn message(&mut self, bytes: &[u8]) -> Result<ServerMsg, FrameError> {
        let (&kind, body) = bytes.split_first().ok_or(FrameError::Incomplete)?;
        match kind {
            MSG => self.msg.decode(body),
            DELTAS => {
                let runs: Vec<DeltaRun> = self.deltas.decode(body)?;
                let mut events = Vec::with_capacity(runs.iter().map(|r| r.deltas.len()).sum());
                for DeltaRun { thread, item, deltas } in runs {
                    events.extend(deltas.into_iter().map(|delta| Envelope { seq: 0, event: Event::ItemDelta { thread, item, delta } }));
                }
                Ok(ServerMsg::Events(events))
            }
            DONE => {
                let id = body.get(..4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).ok_or(FrameError::Incomplete)?;
                Ok(ServerMsg::Reply { id, result: Ok(Reply::Done) })
            }
            _ => Err(FrameError::Checksum),
        }
    }

    /// Encodes a call or hello for the host.
    pub fn client(&mut self, msg: &ClientMsg) -> Vec<u8> {
        self.calls.encode(msg).to_vec()
    }
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(kind);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Command;

    fn delta(text: &str) -> Envelope {
        Envelope { seq: 0, event: Event::ItemDelta { thread: ThreadId(1), item: ItemId(2), delta: Delta::Text(text.into()) } }
    }

    #[test]
    fn events_keep_their_order_across_frame_kinds() {
        let events = vec![
            delta("a"),
            delta("b"),
            Envelope { seq: 5, event: Event::ThreadRemoved(ThreadId(9)) },
            delta("c"),
            // A durable delta (never sent today) keeps its seq on the slow path.
            Envelope { seq: 6, event: Event::ItemDelta { thread: ThreadId(1), item: ItemId(2), delta: Delta::Text("d".into()) } },
        ];
        let mut enc = ServerEncoder::new();
        let mut dec = ServerDecoder::new();
        let frames = enc.events(events.clone());
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), [DELTAS, MSG, DELTAS, MSG]);
        let mut back = Vec::new();
        for f in &frames {
            let ServerMsg::Events(envs) = dec.message(f).unwrap() else { panic!("events") };
            back.extend(envs);
        }
        assert_eq!(back, events);
    }

    #[test]
    fn done_replies_are_five_bytes_and_others_round_trip() {
        let mut enc = ServerEncoder::new();
        let mut dec = ServerDecoder::new();
        let done = ServerMsg::Reply { id: 77, result: Ok(Reply::Done) };
        let bytes = enc.message(&done);
        assert_eq!(bytes.len(), 5);
        assert_eq!(dec.message(&bytes).unwrap(), done);
        let failed = ServerMsg::Reply { id: 78, result: Err("no".into()) };
        assert_eq!(dec.message(&enc.message(&failed)).unwrap(), failed);
        let call = ClientMsg::Call { id: 3, command: Command::Search { query: "x".into(), limit: 5 } };
        assert_eq!(enc.client(&dec.client(&call)).unwrap(), call);
        assert!(dec.message(&[]).is_err() && dec.message(&[9]).is_err());
    }
}
