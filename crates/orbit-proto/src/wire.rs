//! Messages on the wire: commands in, events out.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::*;

/// Client → engine.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ClientMsg {
    /// First message on a connection. `epoch` and `since` identify the last
    /// event the client holds (0 for none). Within the same epoch the engine
    /// replays everything after `since`; otherwise, or when the gap is too
    /// old, it sends a fresh snapshot.
    Hello { version: u16, token: String, epoch: u64, since: u64 },
    Call { id: u32, command: Command },
}

/// Engine → client.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ServerMsg {
    Welcome { version: u16, snapshot: Snapshot },
    /// Events in order. Ephemeral events have `seq == 0`.
    Events(Vec<Envelope>),
    Reply { id: u32, result: Result<Reply, String> },
    /// The connection is being closed; `reason` is shown to the user.
    Goodbye { reason: String },
}

/// Everything a client needs to draw the shell. Timelines are not included;
/// they are fetched with [`Command::OpenThread`].
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Snapshot {
    /// The engine's process lifetime; `seq` is only meaningful within it.
    pub epoch: u64,
    pub seq: u64,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
    pub providers: Vec<ProviderInfo>,
    pub defaults: ThreadConfig,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Envelope {
    pub seq: u64,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Event {
    ProjectUpserted(Project),
    ProjectRemoved(ProjectId),
    ThreadUpserted(Thread),
    ThreadRemoved(ThreadId),
    /// A timeline item was added or replaced.
    ItemUpserted { thread: ThreadId, item: Item },
    /// Streamed text appended to an item. Ephemeral.
    ItemDelta { thread: ThreadId, item: ItemId, delta: Delta },
    ItemRemoved { thread: ThreadId, item: ItemId },
    Providers(Vec<ProviderInfo>),
    DefaultsChanged(ThreadConfig),
    /// The thread's working tree changed on disk. Ephemeral.
    WorkspaceChanged { thread: ThreadId, files: Vec<FileChange> },
    /// Ephemeral.
    TerminalOutput { term: TermId, data: Vec<u8> },
    TerminalExited { term: TermId, code: Option<i32> },
}

impl Event {
    /// Durable events are logged and replayable; ephemeral ones are not.
    pub fn is_durable(&self) -> bool {
        !matches!(
            self,
            Event::ItemDelta { .. } | Event::WorkspaceChanged { .. } | Event::TerminalOutput { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    ToolOutput(String),
}

/// What part of a thread's changes a diff covers.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum DiffScope {
    /// The given turn only.
    Turn(u32),
    /// Everything since the thread started.
    Thread,
    /// The working tree against a branch or commit.
    Base(String),
    /// Uncommitted changes in the working tree.
    Uncommitted,
}

/// A field-wise update to a thread; `None` leaves the field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ThreadPatch {
    pub title: Option<String>,
    pub config: Option<ThreadConfig>,
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
    pub unread: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Command {
    AddProject { path: String },
    UpdateProject { project: Project },
    RemoveProject { id: ProjectId },

    CreateThread {
        project: Option<ProjectId>,
        config: ThreadConfig,
        /// Give the thread its own worktree on a new branch.
        worktree: bool,
    },
    UpdateThread { id: ThreadId, patch: ThreadPatch },
    /// Deletes the thread, its timeline, blobs only it references,
    /// checkpoints and worktree.
    DeleteThread { id: ThreadId },
    /// New thread with the timeline up to and including `at`.
    ForkThread { id: ThreadId, at: ItemId },
    /// Returns [`Reply::Timeline`] and starts streaming the thread's deltas.
    OpenThread { id: ThreadId },
    CloseThread { id: ThreadId },

    /// Send a message. While a turn runs, `steer` injects it into the turn;
    /// otherwise it is queued for after the turn.
    Send { thread: ThreadId, text: String, attachments: Vec<Attachment>, steer: bool },
    /// Replace a queued message, or remove it with empty text.
    EditQueued { thread: ThreadId, item: ItemId, text: String },
    /// Remove the timeline after `item` and send `text` in its place.
    EditAndResend { thread: ThreadId, item: ItemId, text: String },
    Interrupt { thread: ThreadId },
    Resolve { thread: ThreadId, request: RequestId, decision: Decision },

    Diff { thread: ThreadId, scope: DiffScope },
    /// Reverse-apply a patch (one hunk or file of a diff) to the workspace.
    RevertPatch { thread: ThreadId, patch: String },
    /// Restore the workspace to the checkpoint after `turn`; the current
    /// state is checkpointed first so the revert itself can be undone.
    RestoreCheckpoint { thread: ThreadId, turn: u32 },
    Commit { thread: ThreadId, message: String, push: bool },
    CreatePullRequest { thread: ThreadId, title: String, body: String },
    Branches { project: ProjectId },

    Search { query: String, limit: u32 },
    /// Store bytes in the blob store; returns [`Reply::Blob`].
    PutBlob { name: String, mime: String, data: Vec<u8> },

    OpenTerminal { cwd: String, cols: u16, rows: u16 },
    TerminalInput { term: TermId, data: Vec<u8> },
    ResizeTerminal { term: TermId, cols: u16, rows: u16 },
    CloseTerminal { term: TermId },

    RefreshProviders,
    SetDefaults { config: ThreadConfig },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Reply {
    Done,
    Project(ProjectId),
    Thread(ThreadId),
    Timeline(Vec<Item>),
    Diff(String),
    Branches { current: Option<String>, all: Vec<String> },
    SearchHits(Vec<SearchHit>),
    Blob(Attachment),
    Terminal(TermId),
    PullRequest { url: String },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct SearchHit {
    pub thread: ThreadId,
    pub item: ItemId,
    /// Text around the match.
    pub excerpt: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn command_round_trips() {
        let msg = ClientMsg::Call {
            id: 7,
            command: Command::Send {
                thread: ThreadId::new(),
                text: "Run the tests".into(),
                attachments: vec![],
                steer: true,
            },
        };
        let back: ClientMsg = decode(&encode(&msg)).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn deltas_are_ephemeral() {
        let delta = Event::ItemDelta {
            thread: ThreadId::new(),
            item: ItemId::new(),
            delta: Delta::Text("hi".into()),
        };
        assert!(!delta.is_durable());
        assert!(Event::ThreadRemoved(ThreadId::new()).is_durable());
    }

    #[test]
    fn delta_frame_is_compact() {
        let env = Envelope {
            seq: 0,
            event: Event::ItemDelta {
                thread: ThreadId(u64::MAX),
                item: ItemId(u64::MAX),
                delta: Delta::Text("token".into()),
            },
        };
        // 2 ids + tags + 5 bytes of text: far below a JSON encoding (~90 B).
        assert!(encode(&env).len() < 32, "{} bytes", encode(&env).len());
    }
}
