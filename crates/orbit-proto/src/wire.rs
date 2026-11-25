//! Messages on the wire: commands in, events out.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::*;
use crate::settings::Settings;

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
    /// Boxed: the snapshot travels inside small enums.
    pub settings: Box<Settings>,
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
    /// The user's settings were replaced (whole record, already sanitized).
    SettingsChanged(Settings),
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
    /// Someone (the `orbit open` command) asked the desktop client to come
    /// to the front, showing `thread` when given. Ephemeral.
    FocusRequested { thread: Option<ThreadId> },
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
                | Event::FocusRequested { .. }
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
    /// Replace the settings. The engine sanitizes them, persists them,
    /// applies what affects it (provider binaries and environment, git and
    /// worktree locations) and broadcasts [`Event::SettingsChanged`].
    UpdateSettings { settings: Settings },

    // ── Automation (the `orbit` CLI and MCP server) ──────────────────────
    /// Create a thread and, when `prompt` is set, send it as the first
    /// message in the same step. Returns [`Reply::Thread`].
    StartThread(ThreadStart),
    /// A thread's items after `after` (all when `None`), at most `limit`
    /// of the newest (0 = no limit), without opening the thread or marking
    /// it read. Returns [`Reply::Timeline`].
    ReadTimeline { id: ThreadId, after: Option<ItemId>, limit: u32 },
    /// Run `command` through the user's shell in a new terminal in `cwd`.
    /// Its output arrives as [`Event::TerminalOutput`] and its exit as
    /// [`Event::TerminalExited`]. Returns [`Reply::Terminal`].
    RunInTerminal { cwd: String, command: String, cols: u16, rows: u16 },
    /// The last `max_bytes` of a terminal's output (0 = all that is kept).
    /// Returns [`Reply::TerminalTail`].
    ReadTerminal { term: TermId, max_bytes: u32 },
    /// Returns [`Reply::Terminals`].
    ListTerminals,
    /// Ask the desktop client to come to the front (and show `thread`).
    /// Broadcasts [`Event::FocusRequested`].
    Focus { thread: Option<ThreadId> },
}

/// Everything needed to start a thread in one step.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ThreadStart {
    pub project: Option<ProjectId>,
    pub config: ThreadConfig,
    /// Give the thread its own worktree on a new branch.
    pub worktree: bool,
    /// The thread that started this one (an agent delegating work).
    pub parent: Option<ThreadId>,
    /// Empty: titled from the first message.
    pub title: String,
    /// Sent as the first message; queued until a new worktree is ready.
    pub prompt: Option<String>,
}

/// A terminal the engine runs.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TerminalInfo {
    pub id: TermId,
    /// The command given to [`Command::RunInTerminal`]; empty for an
    /// interactive shell.
    pub command: String,
    pub cwd: String,
    pub running: bool,
    pub exit_code: Option<i32>,
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
    /// Raw output (with escape sequences); `dropped` bytes were older than
    /// what is kept.
    TerminalTail { info: TerminalInfo, data: Vec<u8>, dropped: u64 },
    Terminals(Vec<TerminalInfo>),
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
        assert!(Event::SettingsChanged(Settings::default()).is_durable());
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
