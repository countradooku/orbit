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

pub mod automation;
pub mod bots;
pub mod codec;
pub mod diff;
pub mod forge;
pub mod guard;
pub mod ids;
pub mod legacy;
pub mod model;
pub mod settings;
pub mod status;
pub mod wire;

pub use automation::*;
pub use codec::{Codec, FRAME_HEADER, FrameError, FrameReader, decode, encode, write_frame, write_raw_frame};
pub use diff::{
    DiffLine, FileDiff, FileDiffKind, Hunk, LineKind, LineRanges, Span, file_patch, hunk_patch, hunks_patch, word_diff,
};
pub use bots::*;
pub use forge::*;
pub use guard::*;
pub use ids::*;
pub use model::*;
pub use settings::*;
pub use status::*;
pub use wire::*;

/// Bumped on any incompatible wire change. Clients and servers refuse to talk
/// across versions instead of misreading frames.
/// Version 3 added live status evidence (`Snapshot::live`,
/// `Event::ThreadLive`, `Event::TerminalAgent`, `Command::BindTerminal`);
/// version 4, question options with descriptions and `Command::Push`;
/// version 5, automations (`Snapshot::automations`, `Event::Automation*`,
/// `Command::Automation`);
/// version 6, chat bots and the guard (`Command::Bots`, `Command::Guard`,
/// `Command::Via` and their replies and events).
pub const PROTOCOL_VERSION: u16 = 6;

/// WebSocket subprotocol name negotiated during the upgrade.
pub const WS_SUBPROTOCOL: &str = "orbit.v1";
