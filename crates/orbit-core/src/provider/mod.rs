//! Agent providers: the seam between the engine and agent runtimes.
//!
//! Each provider adapter owns one child process per thread session (Claude
//! Code) or per host (Codex `app-server`), translates the runtime's wire
//! protocol into normalized [`AgentEvent`]s, and accepts [`SessionInput`]s.
//! Adapters never touch storage or the UI; the engine turns their events into
//! timeline items.
//!
//! Channels are crossfire: the engine sends inputs on a bounded MPSC sender
//! per session, and every session reports on one shared MPSC sender tagged
//! with its thread id.

use std::path::PathBuf;

use orbit_proto::*;

pub mod acp;
pub mod claude;
pub mod codex;
pub mod jsonrpc;
pub mod oneshot;
pub mod opencode;
pub mod pi;
pub mod presets;
pub mod process;

/// Everything an adapter needs to start or resume a session.
#[derive(Clone, Debug)]
pub struct SessionSpec {
    pub thread: ThreadId,
    /// Working directory: the thread's worktree, else the project root.
    pub cwd: PathBuf,
    pub config: ThreadConfig,
    /// The provider's session id to resume, if the thread has one.
    pub resume: Option<String>,
    /// Resume at this provider message id (rollback / edit-and-resend).
    pub resume_at: Option<String>,
    /// Explicit binary path from settings; `None` resolves from PATH and the
    /// known install locations. A bare name is looked up on PATH.
    pub binary: Option<PathBuf>,
    /// The instance's launch arguments (an ACP agent's command line after
    /// the binary; extra flags for the other runtimes).
    pub args: Vec<String>,
    /// Extra environment variables (worktree ports, user overrides).
    pub env: Vec<(String, String)>,
    /// Extra directories the agent may read (attachments directory).
    pub extra_dirs: Vec<PathBuf>,
}

/// An attachment ready to send: its record and the blob's path on disk.
#[derive(Clone, Debug)]
pub struct PromptAttachment {
    pub attachment: Attachment,
    pub path: PathBuf,
}

/// Engine → adapter.
#[derive(Clone, Debug)]
pub enum SessionInput {
    /// Start a turn, or steer the running one when `steer` is set. `uuid`
    /// identifies the prompt so its acknowledgement can be matched.
    Prompt { uuid: String, text: String, attachments: Vec<PromptAttachment>, steer: bool },
    /// Stop the current turn without killing background work when the
    /// runtime allows it.
    Interrupt,
    /// The user's answer to [`AgentEvent::Request`].
    Resolve { provider_ref: String, decision: Decision },
    /// Apply a new model, options, mode or access level, live if the runtime
    /// supports it and by restarting with resume otherwise.
    Configure(ThreadConfig),
    /// End the session and its process tree.
    Shutdown,
}

/// The kind of a streamed content block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Reasoning,
    /// A tool call's input JSON as it streams; `block` is the call id.
    ToolInput,
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed { message: String },
    /// Quota exhausted; `resets_ms` is the Unix time it resets, 0 if unknown.
    RateLimited { resets_ms: u64 },
}

/// Adapter → engine. One stream per session, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// The process is up and the runtime reported its session.
    Ready { session_id: String, model: Option<String> },
    /// Models, option descriptors, modes, slash commands and account, as the
    /// runtime reports them. May arrive more than once.
    Catalog(ProviderInfo),
    /// A turn started. `prompt` is the uuid of the prompt that started it,
    /// `None` for turns the runtime started by itself (background wake-ups).
    TurnStarted { prompt: Option<String> },
    /// A steer or queued prompt was accepted into the running turn.
    PromptAccepted { uuid: String },
    /// Streamed text for block `block` (stable within a turn).
    Delta { block: String, kind: BlockKind, text: String },
    /// Final content of a block; replaces whatever was streamed.
    BlockDone { block: String, kind: BlockKind, text: String },
    /// A tool call began. Sent again with the same `call_id` once its input
    /// is complete (title, input and files filled in); the later one replaces
    /// the earlier.
    ToolStarted(ToolCall),
    /// Streamed tool output (long-running commands).
    ToolOutput { call_id: String, text: String },
    ToolFinished { call_id: String, output: String, is_error: bool, exit_code: Option<i32>, files: Vec<FileChange> },
    /// The agent's todo list or plan steps.
    Plan(Vec<PlanStep>),
    /// The agent needs the user. Answer with [`SessionInput::Resolve`].
    Request { provider_ref: String, kind: RequestKind },
    /// The runtime withdrew a request (it was cancelled or timed out).
    RequestWithdrawn { provider_ref: String },
    /// Running usage of the current turn (context fill updates as it streams).
    Usage(Usage),
    /// Account quota state; `utilization` is 0.0–1.0 of the current window.
    Quota { window: String, utilization: Option<f32>, resets_ms: u64, rejected: bool },
    Notice { level: NoticeLevel, text: String },
    /// The provider message id at the end of this turn: the rollback cursor.
    Checkpoint { provider_message: String },
    TurnFinished { outcome: TurnOutcome, usage: Usage, duration_ms: u64 },
    TitleChanged(String),
    /// The session's process exited. `error` explains an unexpected exit.
    Exited { error: Option<String> },
}

/// The engine's handle on one running session.
pub struct SessionHandle {
    pub input: crossfire::MTx<crossfire::mpsc::Array<SessionInput>>,
}

impl SessionHandle {
    /// Sends without blocking the engine; a full or closed channel means the
    /// session is gone and the engine will receive [`AgentEvent::Exited`].
    pub fn send(&self, input: SessionInput) -> bool {
        use crossfire::BlockingTxTrait;
        self.input.try_send(input).is_ok()
    }
}

/// Events from every session, tagged with their thread.
pub type EventSender = crossfire::MTx<crossfire::mpsc::List<(ThreadId, AgentEvent)>>;

/// Starts a session for `spec` on the runtime `kind`. Must be called inside
/// the engine's Tokio runtime.
pub fn start(kind: ProviderKind, spec: SessionSpec, events: EventSender) -> SessionHandle {
    match kind {
        ProviderKind::Claude => claude::start(spec, events),
        ProviderKind::Codex => codex::start(spec, events),
        ProviderKind::Acp => acp::start(spec, events),
        ProviderKind::OpenCode => opencode::start(spec, events),
        ProviderKind::Pi => pi::start(spec, events),
    }
}

/// How to start one provider instance: what [`probe`] and a session need,
/// resolved from the settings (see `engine::sessions`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    pub kind: ProviderKind,
    /// Instance id and display name, copied into the probe's result.
    pub instance: String,
    pub name: String,
    pub binary: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Launch {
    /// The built-in instance of `kind` with an explicit binary and
    /// environment (tests, tools).
    pub fn builtin(kind: ProviderKind, binary: Option<PathBuf>, env: Vec<(String, String)>) -> Self {
        Self { kind, instance: kind.id().to_owned(), name: kind.label().to_owned(), binary, args: Vec::new(), env }
    }

    /// The launch of `instance` under its runtime's settings.
    pub fn of(instance: &ProviderInstance, runtime: &ProviderSettings) -> Self {
        let (binary, env) = instance.launch(runtime);
        Self {
            kind: instance.kind,
            instance: instance.id.clone(),
            name: instance.display_name().to_owned(),
            binary: binary.map(PathBuf::from),
            args: instance.args.clone(),
            env,
        }
    }
}

/// Detects whether an instance's runtime is installed and signed in, and
/// reads its models, options, modes and slash commands. Cheap enough to run
/// at startup and on demand; never blocks longer than its own timeout, and
/// returns at once when the binary is missing.
pub async fn probe(launch: Launch) -> ProviderInfo {
    let Launch { kind, instance, name, binary, args, env } = launch;
    let mut info = match kind {
        ProviderKind::Claude => claude::probe(binary, env).await,
        ProviderKind::Codex => codex::probe(binary, env).await,
        ProviderKind::Acp => acp::probe(binary, &args, &env).await,
        ProviderKind::OpenCode => opencode::probe(binary, &env).await,
        ProviderKind::Pi => pi::probe(binary, &args, &env).await,
    };
    info.kind = kind;
    info.instance = instance;
    info.name = name;
    info
}
