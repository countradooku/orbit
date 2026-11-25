//! # Orbit protocol (OWP/1)
//!
//! One protocol for every hop: the in-process bus between the engine and the
//! GPUI client, the on-disk event log, and the WebSocket link to remote clients.
//!
//! * **Commands** flow client → engine, each tagged with a request id.
//! * **Events** flow engine → clients. Every durable event has a global,
//!   monotonically increasing sequence number, so a client that reconnects
//!   asks for `since: seq` and receives exactly what it missed.
//! * **Deltas** (streamed text, terminal bytes) are ephemeral: they are
//!   broadcast immediately and coalesced before they reach the log.
//!
//! Everything is encoded with `bitcode` (compact, allocation-light) and framed
//! with a length + CRC32 header so torn writes on disk are detected and
//! truncated on recovery.

pub mod codec;
pub mod ids;
pub mod model;
pub mod wire;

pub use codec::{FRAME_HEADER, FrameError, FrameReader, decode, encode, write_frame};
pub use ids::*;
pub use model::*;
pub use wire::*;

/// Bumped on any incompatible wire change. Clients and servers refuse to talk
/// across versions instead of misreading frames.
pub const PROTOCOL_VERSION: u16 = 1;

/// WebSocket subprotocol name negotiated during the upgrade.
pub const WS_SUBPROTOCOL: &str = "orbit.v1";
