//! Messages on the wire: commands in, events out.

use bitcode::{Decode, Encode};

use crate::bots::{BotCommand, BotReply, BotsState, Via, ViaAction};
use crate::forge::{ForgeCommand, ForgeReply};
use crate::guard::GuardSettings;
use crate::ids::*;
use crate::model::*;
use crate::settings::Settings;
use crate::status::*;

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
    /// Boxed: the snapshot travels inside small enums.
    pub settings: Box<Settings>,
    /// Live status evidence of threads that have any (see [`ThreadLive`]).
    pub live: Vec<(ThreadId, ThreadLive)>,
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
    /// A composer's unsent message changed; an empty draft means none.
    DraftSaved { target: DraftTarget, draft: Draft },
    /// A thread's live status evidence changed: its session went quiet or
    /// unverified, or an agent in one of its terminals changed state.
    /// Ephemeral; the latest value is in the snapshot.
    ThreadLive { thread: ThreadId, live: ThreadLive },
    /// An agent started, changed state or ended (`None`) in a terminal,
    /// whether or not the terminal belongs to a thread. Ephemeral.
    TerminalAgent { term: TermId, agent: Option<TerminalAgent> },
    /// The chat bots' settings, pairings or connections changed. Ephemeral;
    /// read the current state with [`BotCommand::State`].
    Bots(BotsState),
    /// The guard list changed. Ephemeral; read it with `Command::Guard(None)`.
    GuardChanged(GuardSettings),
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
                | Event::ThreadLive { .. }
                | Event::TerminalAgent { .. }
                | Event::Bots(_)
                | Event::GuardChanged(_)
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
    /// Everything from before the given turn to the working tree now: what
    /// restoring to before that turn would undo.
    SinceTurn(u32),
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
    /// End the agent's process now, for when Stop did not take. The turn
    /// ends as stopped; the next message resumes the session.
    ForceStop { thread: ThreadId },
    /// Run the thread's last failed or rate-limited turn again. The
    /// provider's history is rewound to before the turn and its message is
    /// sent again; the failed attempt stays in the timeline.
    Retry { thread: ThreadId },
    Resolve { thread: ThreadId, request: RequestId, decision: Decision },

    Diff { thread: ThreadId, scope: DiffScope },
    /// Reverse-apply a patch (one hunk or file of a diff) to the workspace.
    RevertPatch { thread: ThreadId, patch: String },
    /// Restore the workspace to how it was before `turn` started (undo the
    /// turn and everything after it); the current state is snapshotted
    /// first so the restore itself can be undone.
    RestoreCheckpoint { thread: ThreadId, turn: u32 },
    /// Undo the thread's last restore (itself undoable: it swaps back).
    UndoRestore { thread: ThreadId },
    Commit { thread: ThreadId, message: String, push: bool },
    /// Push the thread's branch, setting its upstream on the first push.
    /// Replies with [`Reply::Text`] naming where it went (`origin/topic`).
    Push { thread: ThreadId },
    CreatePullRequest { thread: ThreadId, title: String, body: String },
    Branches { project: ProjectId },
    /// Merge the thread's branch into `into` locally, without a pull
    /// request. Uncommitted work is committed first with `message`. With
    /// `dry_run` nothing changes and the reply previews the merge.
    MergeLocal { thread: ThreadId, into: String, message: String, dry_run: bool },

    Search { query: String, limit: u32 },
    /// Store bytes in the blob store; returns [`Reply::Blob`].
    PutBlob { name: String, mime: String, data: Vec<u8> },
    /// Read a stored blob by its hash (full tool output, an attachment).
    /// Returns [`Reply::Bytes`].
    ReadBlob { hash: String },

    OpenTerminal { cwd: String, cols: u16, rows: u16 },
    TerminalInput { term: TermId, data: Vec<u8> },
    ResizeTerminal { term: TermId, cols: u16, rows: u16 },
    CloseTerminal { term: TermId },
    /// Say which thread a terminal belongs to (`None`: no thread), so the
    /// agents running in it count toward that thread's status. Terminals
    /// opened in a thread's own folder are bound to it automatically.
    BindTerminal { term: TermId, thread: Option<ThreadId> },

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
    /// Replace the settings. The engine sanitizes them, persists them,
    /// applies what affects it (provider binaries and environment, git and
    /// worktree locations) and broadcasts [`Event::SettingsChanged`].
    /// Boxed: settings are by far the largest command, and every other
    /// command would otherwise be as large.
    UpdateSettings { settings: Box<Settings> },

    /// One-shot text generation (titles, commit messages, pull request
    /// text) on the thread's instance in a cheap mode, or on any available
    /// one when `thread` is `None`. Replies [`Reply::Text`].
    GenerateText { thread: Option<ThreadId>, task: TextTask, input: String },
    /// End the thread's agent session; the next message starts a fresh one
    /// in the same thread (what `/clear` does).
    NewSession { thread: ThreadId },
    /// Pull requests, checks, merges, issues and tracker settings; see
    /// [`ForgeCommand`]. Replies [`Reply::Forge`] (or `Thread`, `Text` or
    /// `Done`, per command).
    Forge(ForgeCommand),

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

    // ── Chat bots and the guard ──────────────────────────────────────────
    /// Bot settings, pairing and status; see [`BotCommand`]. Replies
    /// [`Reply::Bots`] or `Done`. Fails when the host runs no bots.
    Bots(BotCommand),
    /// Reads the guard list (`None`) or replaces it. Replies
    /// [`Reply::Guard`] with the list now in force.
    Guard(Option<GuardSettings>),
    /// An action on behalf of someone outside the app (a chat bot). The
    /// engine notes the origin in the thread's timeline and refuses
    /// approvals the guard reserves for the app. Replies as the matching
    /// command would.
    Via { via: Via, action: ViaAction },
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

/// What [`Command::GenerateText`] writes.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum TextTask {
    /// A short thread title from the conversation in `input`.
    ThreadTitle,
    /// A commit message for the changes (diff and context) in `input`.
    CommitMessage,
    /// A pull request title on the first line, then its body.
    PullRequest,
    /// Free-form: `instructions` applied to `input`.
    Custom { instructions: String },
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
    Bytes(Vec<u8>),
    Terminal(TermId),
    PullRequest { url: String },
    Merge(MergeInfo),
    Files(Vec<FileMatch>),
    Text(String),
    Forge(ForgeReply),
    /// Raw output (with escape sequences); `dropped` bytes were older than
    /// what is kept.
    TerminalTail { info: TerminalInfo, data: Vec<u8>, dropped: u64 },
    Terminals(Vec<TerminalInfo>),
    Bots(BotReply),
    Guard(GuardSettings),
}

/// A local merge of a thread's branch, previewed or done.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct MergeInfo {
    /// The thread's branch.
    pub source: String,
    /// The branch it merges into.
    pub target: String,
    /// Commits on `source` that `target` does not have yet.
    pub commits: u32,
    /// Files with uncommitted changes, committed before merging.
    pub uncommitted: u32,
    /// What the merge changes on `target`.
    pub files: Vec<FileChange>,
    /// Paths both branches changed in ways git cannot combine.
    pub conflicts: Vec<String>,
    /// `target` only moves forward; no merge commit is needed.
    pub fast_forward: bool,
    /// The new tip of `target` once merged.
    pub merged: Option<String>,
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
    use bitcode::Encode;

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
    fn live_status_is_ephemeral_and_round_trips() {
        let live = Event::ThreadLive {
            thread: ThreadId(3),
            live: ThreadLive {
                liveness: Liveness::Quiet { since_ms: 9, tool: Some("cargo test".into()) },
                last_signal_ms: 9,
                terminals: vec![TerminalAgent {
                    term: TermId(4),
                    agent: "claude".into(),
                    state: AgentState::NeedsYou(NeedKind::Permission),
                    evidence: Evidence::Hook,
                    since_ms: 1,
                    last_signal_ms: 2,
                    detail: "Bash".into(),
                }],
            },
        };
        assert!(!live.is_durable());
        assert!(!Event::TerminalAgent { term: TermId(4), agent: None }.is_durable());
        let env = Envelope { seq: 0, event: live };
        let back: Envelope = decode(&encode(&env)).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn durable_events_logged_before_live_status_still_decode() {
        // `Event` as meta schema 3 wrote it (17 variants). The two live
        // status variants keep it in the same packing class, so logged
        // records read unchanged and no schema bump is needed.
        #[derive(Encode)]
        #[allow(dead_code)]
        enum V3Event {
            ProjectUpserted(Project),
            ProjectRemoved(ProjectId),
            ThreadUpserted(Thread),
            ThreadRemoved(ThreadId),
            ItemUpserted { thread: ThreadId, item: Item },
            ItemDelta { thread: ThreadId, item: ItemId, delta: Delta },
            ItemRemoved { thread: ThreadId, item: ItemId },
            Providers(Vec<ProviderInfo>),
            DefaultsChanged(ThreadConfig),
            SettingsChanged(Settings),
            WorkspaceChanged { thread: ThreadId, files: Vec<FileChange> },
            UsageChanged { thread: ThreadId, usage: Usage },
            Conflicts(Vec<Conflict>),
            TerminalOutput { term: TermId, data: Vec<u8> },
            TimelineMark { thread: ThreadId, mark: u64 },
            TerminalExited { term: TermId, code: Option<i32> },
            DraftSaved { target: DraftTarget, draft: Draft },
        }
        let thread = Thread { id: ThreadId(5), title: "Before live status".into(), ..Default::default() };
        let old = vec![
            V3Event::ThreadUpserted(thread.clone()),
            V3Event::DraftSaved { target: DraftTarget::Home, draft: Draft::default() },
            V3Event::ThreadRemoved(ThreadId(5)),
        ];
        let back: Vec<Event> = bitcode::decode(&bitcode::encode(&old)).unwrap();
        assert_eq!(
            back,
            vec![
                Event::ThreadUpserted(thread),
                Event::DraftSaved { target: DraftTarget::Home, draft: Draft::default() },
                Event::ThreadRemoved(ThreadId(5)),
            ]
        );
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
