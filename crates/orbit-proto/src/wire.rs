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
    /// Reply to `Hello`. `snapshot` is `None` when the client's `since` was
    /// replayable; the missed events follow in `Events`.
    Welcome { version: u16, snapshot: Option<Snapshot> },
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
    /// Unsent messages, per composer.
    pub drafts: Vec<(DraftTarget, Draft)>,
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
    /// Live token and context accounting of a running turn. Ephemeral; the
    /// durable total lands on the thread when the turn ends.
    UsageChanged { thread: ThreadId, usage: Usage },
    /// Files changed in more than one live worktree. Ephemeral.
    Conflicts(Vec<Conflict>),
    /// Ephemeral.
    TerminalOutput { term: TermId, data: Vec<u8> },
    /// Marks the instant a timeline was read for an `OpenThread` reply:
    /// events before it are in the reply, events after it are not.
    /// Ephemeral.
    TimelineMark { thread: ThreadId, mark: u64 },
    TerminalExited { term: TermId, code: Option<i32> },
    /// A composer's unsent message changed; an empty draft means none.
    DraftSaved { target: DraftTarget, draft: Draft },
}

impl Event {
    /// Durable events are logged and replayable; ephemeral ones are not.
    pub fn is_durable(&self) -> bool {
        !matches!(
            self,
            Event::ItemDelta { .. }
                | Event::WorkspaceChanged { .. }
                | Event::UsageChanged { .. }
                | Event::Conflicts(_)
                | Event::Providers(_)
                | Event::TerminalOutput { .. }
                | Event::TimelineMark { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    ToolOutput(String),
    /// A tool call's input JSON as the model writes it.
    ToolInput(String),
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
    pub settled: Option<bool>,
    /// `Some(Some(t))` snoozes until `t` (Unix ms); `Some(None)` wakes now.
    pub snooze_until: Option<Option<u64>>,
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
    /// Restore the workspace to how it was before `turn` started (undo the
    /// turn and everything after it); the current state is snapshotted
    /// first so the restore itself can be undone.
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

    /// Keep a composer's unsent message; an empty draft removes it.
    SaveDraft { target: DraftTarget, draft: Draft },
    /// Fuzzy-find files and folders in a thread's workspace (or a project's
    /// folder when `thread` is `None`); returns [`Reply::Files`].
    SearchFiles { thread: Option<ThreadId>, project: Option<ProjectId>, query: String, limit: u32 },
    /// A readable digest of a thread (its messages and replies) stored as a
    /// blob, so another thread's agent can read it; returns [`Reply::Blob`].
    ExportThread { thread: ThreadId },
    /// Move a queued message to position `to` of the queue (0 = next).
    MoveQueued { thread: ThreadId, item: ItemId, to: u32 },
    /// Send a queued message now: into the running turn, or as the next
    /// turn when nothing runs.
    SteerQueued { thread: ThreadId, item: ItemId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Reply {
    Done,
    Project(ProjectId),
    Thread(ThreadId),
    /// A thread's items and the `TimelineMark` emitted when they were read.
    Timeline { items: Vec<Item>, mark: u64 },
    Diff(String),
    Branches { current: Option<String>, all: Vec<String> },
    SearchHits(Vec<SearchHit>),
    Blob(Attachment),
    Terminal(TermId),
    PullRequest { url: String },
    Files(Vec<FileMatch>),
}

/// A path that more than one thread's worktree changed.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Conflict {
    pub path: String,
    pub threads: Vec<ThreadId>,
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
