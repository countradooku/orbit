//! The host link's message codec: hot messages bypass the big enums.
//!
//! `bitcode` keeps one column per field of every variant of a type, and each
//! encode or decode visits all of them, whatever variant the value holds. A
//! one-byte `Reply::Done` therefore costs about as much as the largest reply,
//! and a streamed token pays for every kind of event. On the link the two
//! hot messages get frames of their own, told apart by the first byte:
//!
//! * [`DELTAS`]: a run of streamed tokens as [`WireDelta`]s;
//! * [`DONE`]: `Ok(Reply::Done)` for call `id`, four bytes;
//! * [`MSG`]: any other [`ServerMsg`].
//!
//! Each side keeps one [`ServerEncoder`] or [`ServerDecoder`] per connection,
//! so bitcode's buffers are reused too. Clients see plain [`ServerMsg`]s.

use bitcode::{Decode, Encode};

use crate::codec::{Codec, FrameError};
use crate::ids::{ItemId, ThreadId};
use crate::wire::{ClientMsg, Delta, Envelope, Event, Reply, ServerMsg};

/// First byte: a bitcode [`ServerMsg`] follows.
pub const MSG: u8 = 0;
/// First byte: a bitcode `Vec<WireDelta>` follows.
pub const DELTAS: u8 = 1;
/// First byte: a call id (`u32` LE) whose reply is `Ok(Reply::Done)`.
pub const DONE: u8 = 2;

/// A streamed token without the [`Event`] around it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WireDelta {
    pub thread: ThreadId,
    pub item: ItemId,
    pub delta: Delta,
}

/// The host's side: frames server messages for the link.
#[derive(Default)]
pub struct ServerEncoder {
    msg: Codec,
    deltas: Codec,
    run: Vec<WireDelta>,
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
                    self.run.push(WireDelta { thread, item, delta });
                }
                env => {
                    if !self.run.is_empty() {
                        frames.push(frame(DELTAS, self.deltas.encode(&self.run)));
                        self.run.clear();
                    }
                    other.push(env);
                }
            }
        }
        if !self.run.is_empty() {
            frames.push(frame(DELTAS, self.deltas.encode(&self.run)));
            self.run.clear();
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
                let run: Vec<WireDelta> = self.deltas.decode(body)?;
                Ok(ServerMsg::Events(
                    run.into_iter()
                        .map(|d| Envelope { seq: 0, event: Event::ItemDelta { thread: d.thread, item: d.item, delta: d.delta } })
                        .collect(),
                ))
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
