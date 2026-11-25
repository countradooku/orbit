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

/// Which agent runtime (driver) a thread talks to. A thread names a
/// configured [`crate::settings::ProviderInstance`] of one of these; most users only have the
/// built-in instance of each, whose id is [`ProviderKind::id`].
///
/// New runtimes are appended. Records on disk hold exactly one kind per
/// frame, and logs written before this enum grew are read through
/// [`crate::legacy`], so old data stays readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Encode, Decode)]
pub enum ProviderKind {
    #[default]
    Claude,
    Codex,
    /// Any Agent Client Protocol agent (Gemini CLI, Grok, Kiro, Cursor…),
    /// launched by its instance's command and arguments.
    Acp,
    /// `opencode serve`, over HTTP and server-sent events.
    OpenCode,
    /// `pi --mode rpc`, over JSON lines.
    Pi,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 5] =
        [ProviderKind::Claude, ProviderKind::Codex, ProviderKind::Acp, ProviderKind::OpenCode, ProviderKind::Pi];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Claude => "Claude Code",
            ProviderKind::Codex => "Codex",
            ProviderKind::Acp => "ACP agent",
            ProviderKind::OpenCode => "OpenCode",
            ProviderKind::Pi => "Pi",
        }
    }

    /// The id of this runtime's built-in instance.
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::Claude => "claude",
            ProviderKind::Codex => "codex",
            ProviderKind::Acp => "acp",
            ProviderKind::OpenCode => "opencode",
            ProviderKind::Pi => "pi",
        }
    }

    /// The environment variable that points the runtime at another
    /// configuration (and so another signed-in account), if it has one.
    pub fn config_dir_env(self) -> Option<&'static str> {
        match self {
            ProviderKind::Claude => Some("CLAUDE_CONFIG_DIR"),
            ProviderKind::Codex => Some("CODEX_HOME"),
            ProviderKind::OpenCode => Some("OPENCODE_CONFIG_DIR"),
            ProviderKind::Pi => Some("PI_CODING_AGENT_DIR"),
            ProviderKind::Acp => None,
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
    /// The [`crate::settings::ProviderInstance`] the thread runs on; empty means the
    /// built-in instance of `provider`.
    pub instance: String,
}

impl ThreadConfig {
    /// The id of the instance this config runs on.
    pub fn instance_id(&self) -> &str {
        if self.instance.is_empty() { self.provider.id() } else { &self.instance }
    }

    /// Whether two configs run on the same instance.
    pub fn same_instance(&self, other: &ThreadConfig) -> bool {
        self.provider == other.provider && self.instance_id() == other.instance_id()
    }
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
    /// Done for now: kept, searchable, out of the active list. Any new
    /// message un-settles the thread.
    pub settled_ms: Option<u64>,
    /// Hidden until this time (Unix ms). The thread keeps running; it wakes
    /// early when it needs the user or fails.
    pub snoozed_until: Option<u64>,
    /// Woke from a snooze and the user has not looked yet.
    pub woke: bool,
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
    /// The thread moved to another agent, which received this summary of
    /// the conversation so far.
    Handoff(Handoff),
}

/// The context handed to a new agent when a thread switches provider or
/// account mid-conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Handoff {
    /// Display names of the previous and the new instance.
    pub from: String,
    pub to: String,
    /// Turns the summary covers in full, and turns condensed or left out to
    /// stay within the budget.
    pub turns: u32,
    pub condensed: u32,
    /// Estimated size of `context` in tokens.
    pub tokens: u32,
    /// Exactly what the new agent receives before the next message.
    pub context: String,
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

/// Bytes of tool output kept on an item; the full output is a blob.
pub const INLINE_OUTPUT: usize = 64 * 1024;

/// Appends streamed tool output, keeping only the last [`INLINE_OUTPUT`]
/// bytes: the end of a long build log is what explains it. Engine and
/// clients apply the same rule to the same deltas, so they agree on the text.
pub fn append_output_tail(output: &mut String, delta: &str) {
    output.push_str(delta);
    if output.len() > INLINE_OUTPUT {
        output.replace_range(..tail_start(output, INLINE_OUTPUT), "");
    }
}

/// The last `max` bytes of `text`, starting at a line break when one is
/// near, else at a character boundary.
pub fn output_tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    &text[tail_start(text, max)..]
}

fn tail_start(text: &str, max: usize) -> usize {
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    // Start on a whole line when one begins within the next 512 bytes.
    match text[cut..].bytes().take(512).position(|b| b == b'\n') {
        Some(nl) => cut + nl + 1,
        None => cut,
    }
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
    /// Output, cut to its last [`INLINE_OUTPUT`] bytes; when it was cut,
    /// the full output is the blob `output_blob`.
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
    /// Git tree id of the workspace after the turn; the restore target.
    pub checkpoint: Option<String>,
    /// Git tree id the turn started from (the previous turn's checkpoint, or
    /// the baseline before the first turn). The turn's diff is
    /// `base_checkpoint..checkpoint`.
    pub base_checkpoint: Option<String>,
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
    /// The [`crate::settings::ProviderInstance`] this describes, and its display name.
    pub instance: String,
    pub name: String,
}

impl ProviderInfo {
    /// Whether this is the instance `config` runs on.
    pub fn serves(&self, config: &ThreadConfig) -> bool {
        self.kind == config.provider && self.instance_id() == config.instance_id()
    }

    pub fn instance_id(&self) -> &str {
        if self.instance.is_empty() { self.kind.id() } else { &self.instance }
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { self.kind.label() } else { &self.name }
    }
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

/// Which composer a draft belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum DraftTarget {
    /// The composer that starts a new thread.
    Home,
    Thread(ThreadId),
}

/// An unsent message, kept on the host so it survives restarts and follows
/// the user to every client of that host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Draft {
    pub text: String,
    /// Stored blobs, including context references (terminal selections,
    /// review comments, thread digests).
    pub attachments: Vec<Attachment>,
    /// When the host last saved it (Unix ms).
    pub updated_ms: u64,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }

    /// Same content, whenever it was saved.
    pub fn same_content(&self, other: &Draft) -> bool {
        self.text == other.text && self.attachments == other.attachments
    }
}

/// A file or folder in a project, as `@` mentions find it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FileMatch {
    /// Relative to the workspace root, `/`-separated; folders end in `/`.
    pub path: String,
    pub is_dir: bool,
}

/// The longest message the composer and engine accept, in characters.
pub const MAX_MESSAGE_CHARS: usize = 120_000;
/// Attachments on one message.
pub const MAX_ATTACHMENTS: usize = 100;
/// Largest single image.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Largest single non-image file.
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// All images of one message together.
pub const MAX_TOTAL_IMAGE_BYTES: u64 = 80 * 1024 * 1024;

/// Whether `mime` is an image the agents can see.
pub fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// Checks a message against the limits; the error is interface copy.
pub fn validate_message(text: &str, attachments: &[Attachment]) -> Result<(), String> {
    let chars = text.chars().count();
    if chars > MAX_MESSAGE_CHARS {
        return Err(format!(
            "The message is {} characters over the {} limit. Attach long text as a file instead.",
            group_digits((chars - MAX_MESSAGE_CHARS) as u64),
            group_digits(MAX_MESSAGE_CHARS as u64)
        ));
    }
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(format!("A message can carry up to {MAX_ATTACHMENTS} attachments."));
    }
    let mut images = 0u64;
    for a in attachments {
        validate_attachment(&a.name, &a.mime, a.size)?;
        if is_image_mime(&a.mime) {
            images += a.size;
        }
    }
    if images > MAX_TOTAL_IMAGE_BYTES {
        return Err(format!("Images on one message can add up to {} MB.", MAX_TOTAL_IMAGE_BYTES / (1024 * 1024)));
    }
    Ok(())
}

/// Checks one attachment's size; the error is interface copy.
pub fn validate_attachment(name: &str, mime: &str, size: u64) -> Result<(), String> {
    let (limit, what) = if is_image_mime(mime) { (MAX_IMAGE_BYTES, "Images") } else { (MAX_FILE_BYTES, "Files") };
    if size > limit {
        return Err(format!(
            "“{name}” is {} MB. {what} can be up to {} MB.",
            size.div_ceil(1024 * 1024),
            limit / (1024 * 1024)
        ));
    }
    Ok(())
}

/// `120000` → `120,000`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_limits_explain_themselves() {
        assert!(validate_message("hi", &[]).is_ok());
        let long = "a".repeat(MAX_MESSAGE_CHARS + 1234);
        assert_eq!(
            validate_message(&long, &[]).unwrap_err(),
            "The message is 1,234 characters over the 120,000 limit. Attach long text as a file instead."
        );
        let photo = Attachment { name: "photo.png".into(), mime: "image/png".into(), hash: "ab".into(), size: 11 << 20 };
        assert!(validate_message("", &[photo]).unwrap_err().contains("Images can be up to 10 MB"));
        let many: Vec<Attachment> = (0..=MAX_ATTACHMENTS)
            .map(|i| Attachment { name: format!("{i}"), mime: "text/plain".into(), hash: format!("{i}"), size: 1 })
            .collect();
        assert!(validate_message("", &many).is_err());
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn streamed_output_keeps_its_tail_on_line_boundaries() {
        let mut out = String::new();
        for i in 0..20_000 {
            append_output_tail(&mut out, &format!("line {i}\n"));
        }
        assert!(out.len() <= INLINE_OUTPUT);
        assert!(out.ends_with("line 19999\n"));
        assert!(out.starts_with("line "), "starts on a whole line: {:?}", &out[..12]);
        // Multi-byte text never splits a character.
        let wide = "é".repeat(INLINE_OUTPUT);
        assert!(output_tail(&wide, 1001).chars().all(|c| c == 'é'));
        assert_eq!(output_tail("short", 10), "short");
    }

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
