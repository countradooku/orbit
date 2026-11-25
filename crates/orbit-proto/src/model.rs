//! The domain model: projects, threads, timeline items and agent requests.
//!
//! These are record types shared by the engine, the disk log and every
//! client. They are deliberately plain data with public fields; behavior lives
//! in the engine and presentation in the client.

use bitcode::{Decode, Encode};

use crate::ids::*;

/// A repository or folder the user works in. Always an existing local
/// checkout: Orbit never clones or asks for a hosting account.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
    /// Position in the sidebar, user-controlled.
    pub order: i64,
    pub scripts: ProjectScripts,
    /// The default branch new worktrees fork from; detected when added.
    pub base_branch: Option<String>,
}

/// Commands that prepare and clean up a worktree, and the project's run
/// targets (dev servers, tests).
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ProjectScripts {
    pub setup: Option<String>,
    pub teardown: Option<String>,
    pub run: Vec<RunScript>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RunScript {
    pub name: String,
    pub command: String,
}

/// Which agent runtime a thread talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Encode, Decode)]
pub enum ProviderKind {
    #[default]
    Claude,
    Codex,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 2] = [ProviderKind::Claude, ProviderKind::Codex];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Claude => "Claude Code",
            ProviderKind::Codex => "Codex",
        }
    }
}

/// One adjustable option of a model, as the provider reports it at runtime
/// (reasoning effort, fast mode, thinking on/off…). Orbit never hardcodes the
/// list: Claude Code describes its models in its `initialize` response and
/// Codex in `model/list`, and the composer renders whatever comes back.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct OptionDescriptor {
    /// Stable id the provider adapter understands, e.g. `effort`.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub kind: OptionKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum OptionKind {
    /// Pick one of `choices`.
    Select { choices: Vec<OptionChoice> },
    /// On or off.
    Toggle { default: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct OptionChoice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
}

/// A chosen value for one [`OptionDescriptor`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct OptionValue {
    pub id: String,
    pub value: OptionSetting,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum OptionSetting {
    Choice(String),
    Toggle(bool),
}

impl OptionDescriptor {
    /// The provider's default for this option.
    pub fn default_setting(&self) -> OptionSetting {
        match &self.kind {
            OptionKind::Select { choices } => OptionSetting::Choice(
                choices
                    .iter()
                    .find(|c| c.is_default)
                    .or(choices.first())
                    .map(|c| c.id.clone())
                    .unwrap_or_default(),
            ),
            OptionKind::Toggle { default } => OptionSetting::Toggle(*default),
        }
    }
}

/// Turns a provider id such as `xhigh` or `extra_high` into interface copy
/// (`Extra high`) when the provider supplies only ids. Sentence case, per the
/// interface-language rules.
pub fn humanize_id(id: &str) -> String {
    let spaced = match id {
        "xhigh" => "extra high".to_owned(),
        "xlow" => "extra low".to_owned(),
        _ => id.replace(['_', '-'], " "),
    };
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether the agent acts or first proposes a plan for approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Encode, Decode)]
pub enum Mode {
    #[default]
    Build,
    Plan,
}

/// What the agent may do without asking. New threads never start at `Full`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Encode, Decode)]
pub enum Access {
    Readonly,
    #[default]
    Ask,
    AutoEdit,
    Full,
}

impl Access {
    pub const ALL: [Access; 4] = [Access::Readonly, Access::Ask, Access::AutoEdit, Access::Full];

    pub fn label(self) -> &'static str {
        match self {
            Access::Readonly => "Readonly",
            Access::Ask => "Ask first",
            Access::AutoEdit => "Auto-edit",
            Access::Full => "Full access",
        }
    }
}

/// The model and behavior a thread runs with. The last used config becomes
/// the default for the next thread.
#[derive(Clone, Debug, PartialEq, Eq, Default, Encode, Decode)]
pub struct ThreadConfig {
    pub provider: ProviderKind,
    /// Provider model slug; empty means the provider's default.
    pub model: String,
    /// Values for the model's reported options. An option that is absent
    /// uses the provider's default; values for options the model does not
    /// report are ignored, so switching models never sends invalid flags.
    pub options: Vec<OptionValue>,
    pub mode: Mode,
    pub access: Access,
}

/// Where the thread's agent works.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Workspace {
    /// The checked-out branch, when known.
    pub branch: Option<String>,
    /// Absolute path of the thread's own worktree, if it has one.
    pub worktree: Option<String>,
    /// First port of the range reserved for this worktree's services.
    pub port_base: Option<u16>,
}

/// What a thread is doing. `Waiting` (on the user) is never shown like
/// `Running`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum ThreadStatus {
    #[default]
    Idle,
    Starting,
    Running,
    /// Blocked on a request only the user can answer.
    Waiting,
    /// The provider's quota is exhausted until `resets_ms` (0 when unknown).
    RateLimited { resets_ms: u64 },
    Failed { message: String },
}

impl ThreadStatus {
    pub fn is_busy(&self) -> bool {
        matches!(self, ThreadStatus::Starting | ThreadStatus::Running | ThreadStatus::Waiting)
    }
}

/// Token, cost and context accounting, cumulative for a thread or per turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Cost in millionths of a US dollar.
    pub cost_micros: u64,
    /// Tokens in the context window after the last turn.
    pub context_tokens: u64,
    /// Size of the model's context window, 0 when unknown.
    pub context_window: u64,
}

impl Usage {
    pub fn add_turn(&mut self, turn: &Usage) {
        self.input_tokens += turn.input_tokens;
        self.output_tokens += turn.output_tokens;
        self.cache_read_tokens += turn.cache_read_tokens;
        self.cache_write_tokens += turn.cache_write_tokens;
        self.cost_micros += turn.cost_micros;
        if turn.context_tokens > 0 {
            self.context_tokens = turn.context_tokens;
        }
        if turn.context_window > 0 {
            self.context_window = turn.context_window;
        }
    }

    /// Context fill in `0.0..=1.0`, or `None` when the window is unknown.
    pub fn context_fill(&self) -> Option<f32> {
        (self.context_window > 0)
            .then(|| (self.context_tokens as f32 / self.context_window as f32).clamp(0.0, 1.0))
    }
}

/// A conversation with one agent. Small: the timeline lives in its own log
/// and is loaded only when the thread is opened.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Thread {
    pub id: ThreadId,
    /// `None` for a scratch thread outside any project.
    pub project: Option<ProjectId>,
    pub title: String,
    pub config: ThreadConfig,
    pub workspace: Workspace,
    pub status: ThreadStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// The provider's own session id, used to resume.
    pub session: Option<String>,
    /// The thread this one was forked from, or the parent of a subagent.
    pub parent: Option<ThreadId>,
    pub archived: bool,
    pub pinned: bool,
    /// The latest result has not been seen.
    pub unread: bool,
    pub usage: Usage,
    pub turns: u32,
    /// Messages queued to send after the current turn.
    pub queued: u32,
    /// Unanswered agent requests, kept on the thread record so the inbox
    /// can show every request across threads without loading timelines.
    pub pending: Vec<PendingRequest>,
}

/// An unanswered request as the inbox shows it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct PendingRequest {
    pub item: ItemId,
    pub request: AgentRequest,
    pub created_ms: u64,
}

/// One entry of a thread's timeline.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Item {
    pub id: ItemId,
    pub turn: u32,
    pub status: ItemStatus,
    pub body: ItemBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ItemStatus {
    Streaming,
    Done,
    Failed,
    Cancelled,
    /// A user message waiting for the current turn to finish.
    Queued,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ItemBody {
    User(UserMessage),
    Assistant { text: String },
    Reasoning { text: String },
    Tool(ToolCall),
    Plan { steps: Vec<PlanStep> },
    Request(AgentRequest),
    Notice { level: NoticeLevel, text: String },
    TurnEnd(TurnSummary),
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct UserMessage {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Sent into a running turn instead of after it.
    pub steer: bool,
}

/// A file or image the user attached, stored once by content hash.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Hex BLAKE3 of the content; the blob store key.
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ToolKind {
    Command,
    Read,
    Edit,
    Search,
    Web,
    /// A subagent task.
    Task,
    Mcp,
    Todo,
    Other,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ToolCall {
    /// The provider's id for the call, used to match its result.
    pub call_id: String,
    pub kind: ToolKind,
    pub name: String,
    /// One-line human summary, e.g. `cargo test -p core` or `src/main.rs`.
    pub title: String,
    /// The raw input as compact JSON.
    pub input: String,
    /// Output, truncated to a bounded size; the full output is a blob.
    pub output: String,
    pub output_blob: Option<String>,
    pub exit_code: Option<i32>,
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PlanStep {
    pub text: String,
    pub state: StepState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum StepState {
    Pending,
    Active,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Something the agent cannot continue without.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AgentRequest {
    pub id: RequestId,
    /// The provider's id for the request, echoed back with the decision.
    pub provider_ref: String,
    pub kind: RequestKind,
    pub resolution: Option<Decision>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum RequestKind {
    /// Run a shell command.
    Command { command: String, cwd: String, reason: String },
    /// Write or edit a file; `diff` previews the change.
    FileChange { path: String, diff: String },
    /// Any other tool, with its raw JSON input.
    Tool { name: String, input: String },
    /// A question with optional suggested answers. Answer with
    /// [`Decision::Answer`]: a suggested option's label, or free text. When
    /// `multi_select` is set, put each chosen label on its own line.
    Question { question: String, options: Vec<String>, multi_select: bool },
    /// Leave plan mode and carry out this plan.
    Plan { plan: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Decision {
    Allow,
    /// Allow this and similar requests for the rest of the session.
    AllowForSession,
    Deny { feedback: String },
    Answer { text: String },
}

/// The closing record of a turn.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct TurnSummary {
    pub usage: Usage,
    /// The provider's message id at the end of the turn: where a rollback or
    /// fork resumes the provider's own history.
    pub provider_cursor: Option<String>,
    pub duration_ms: u64,
    /// Git tree id of the workspace after the turn; the revert target.
    pub checkpoint: Option<String>,
    pub files: Vec<FileChange>,
    pub interrupted: bool,
}

/// Availability of a provider on this host.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ProviderInfo {
    pub kind: ProviderKind,
    pub installed: bool,
    pub version: Option<String>,
    pub signed_in: Option<bool>,
    pub account: Option<String>,
    pub binary: Option<String>,
    pub models: Vec<ModelInfo>,
    pub modes: Vec<ModeInfo>,
    pub slash_commands: Vec<SlashCommand>,
    pub message: Option<String>,
    /// Account quota windows, latest known state.
    pub quota: Vec<QuotaWindow>,
}

/// One rate-limit window of an account (for example the five-hour or
/// seven-day window).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QuotaWindow {
    pub window: String,
    /// Share used, 0.0–1.0, when the provider reports it.
    pub utilization: Option<f32>,
    /// Unix time in milliseconds when the window resets; 0 when unknown.
    pub resets_ms: u64,
    /// Requests are being refused until the reset.
    pub rejected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ModelInfo {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    /// 0 when the provider does not report it.
    pub context_window: u64,
    pub is_default: bool,
    pub options: Vec<OptionDescriptor>,
}

/// An interaction mode the provider supports (for example plan mode), as
/// reported at runtime.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ModeInfo {
    pub mode: Mode,
    pub label: String,
    pub description: Option<String>,
}

/// A slash command or skill the provider exposes, passed through intact.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_become_sentence_case_copy() {
        assert_eq!(humanize_id("xhigh"), "Extra high");
        assert_eq!(humanize_id("max"), "Max");
        assert_eq!(humanize_id("accept_edits"), "Accept edits");
    }

    #[test]
    fn select_default_prefers_flagged_choice() {
        let choice = |id: &str, is_default| OptionChoice {
            id: id.into(),
            label: humanize_id(id),
            description: None,
            is_default,
        };
        let effort = OptionDescriptor {
            id: "effort".into(),
            label: "Reasoning".into(),
            description: None,
            kind: OptionKind::Select { choices: vec![choice("low", false), choice("high", true)] },
        };
        assert_eq!(effort.default_setting(), OptionSetting::Choice("high".into()));
    }
}
