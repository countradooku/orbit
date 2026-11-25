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
    /// Requests matching it are always raised for a person, never approved
    /// by the adapter on the access level's behalf (see `engine::guard`).
    pub guard: GuardSettings,
    /// MCP servers for this session only (the Orbit tools of a team
    /// member), added to the agent's own through its launch flags or
    /// session parameters; the user's agent configuration is never touched.
    pub mcp: Vec<McpServer>,
}

/// A stdio MCP server to give one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// For Claude Code, which takes the configuration as a file: where it
    /// was written.
    pub config_file: Option<PathBuf>,
}

impl McpServer {
    fn json_env(&self, pairs: bool) -> String {
        let items: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| {
                if pairs {
                    format!("{{\"name\":{},\"value\":{}}}", json_str(k), json_str(v))
                } else {
                    format!("{}:{}", json_str(k), json_str(v))
                }
            })
            .collect();
        if pairs { format!("[{}]", items.join(",")) } else { format!("{{{}}}", items.join(",")) }
    }

    fn json_args(&self) -> String {
        let args: Vec<String> = self.args.iter().map(|a| json_str(a)).collect();
        format!("[{}]", args.join(","))
    }

    /// `{"mcpServers": {...}}`, as Claude Code's `--mcp-config` reads it.
    pub fn claude_config(servers: &[McpServer]) -> String {
        let entries: Vec<String> = servers
            .iter()
            .map(|s| {
                format!(
                    "{}:{{\"type\":\"stdio\",\"command\":{},\"args\":{},\"env\":{}}}",
                    json_str(&s.name),
                    json_str(&s.command.display().to_string()),
                    s.json_args(),
                    s.json_env(false)
                )
            })
            .collect();
        format!("{{\"mcpServers\":{{{}}}}}", entries.join(","))
    }

    /// ACP's `mcpServers` session parameter.
    pub fn acp_params(servers: &[McpServer]) -> String {
        let entries: Vec<String> = servers
            .iter()
            .map(|s| {
                format!(
                    "{{\"name\":{},\"command\":{},\"args\":{},\"env\":{}}}",
                    json_str(&s.name),
                    json_str(&s.command.display().to_string()),
                    s.json_args(),
                    s.json_env(true)
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }

    /// Codex `-c` overrides that add the servers to its configuration.
    pub fn codex_overrides(servers: &[McpServer]) -> Vec<String> {
        let mut out = Vec::new();
        for s in servers {
            let key = format!("mcp_servers.{}", s.name);
            out.extend(["-c".to_owned(), format!("{key}.command={}", toml_str(&s.command.display().to_string()))]);
            let args: Vec<String> = s.args.iter().map(|a| toml_str(a)).collect();
            out.extend(["-c".to_owned(), format!("{key}.args=[{}]", args.join(","))]);
            if !s.env.is_empty() {
                let env: Vec<String> = s.env.iter().map(|(k, v)| format!("{k}={}", toml_str(v))).collect();
                out.extend(["-c".to_owned(), format!("{key}.env={{{}}}", env.join(","))]);
            }
        }
        out
    }

    /// OpenCode's inline configuration (`OPENCODE_CONFIG_CONTENT`).
    pub fn opencode_config(servers: &[McpServer]) -> String {
        let entries: Vec<String> = servers
            .iter()
            .map(|s| {
                let mut command = vec![json_str(&s.command.display().to_string())];
                command.extend(s.args.iter().map(|a| json_str(a)));
                format!(
                    "{}:{{\"type\":\"local\",\"command\":[{}],\"environment\":{},\"enabled\":true}}",
                    json_str(&s.name),
                    command.join(","),
                    s.json_env(false)
                )
            })
            .collect();
        format!("{{\"mcp\":{{{}}}}}", entries.join(","))
    }
}

/// A JSON string literal.
fn json_str(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// A TOML basic string (the same escapes as JSON for what paths hold).
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod mcp_tests {
    use super::*;
    use sonic_rs::JsonValueTrait;

    fn server() -> McpServer {
        McpServer {
            name: "orbit".into(),
            command: PathBuf::from(r"C:\Program Files\Orbit\orbit.exe"),
            args: vec!["mcp".into()],
            env: vec![("ORBIT_THREAD".into(), "thr_1".into())],
            config_file: None,
        }
    }

    #[test]
    fn every_runtime_gets_well_formed_config() {
        let s = [server()];
        let claude: sonic_rs::Value = sonic_rs::from_str(&McpServer::claude_config(&s)).unwrap();
        assert_eq!(claude["mcpServers"]["orbit"]["command"].as_str(), Some(r"C:\Program Files\Orbit\orbit.exe"));
        assert_eq!(claude["mcpServers"]["orbit"]["env"]["ORBIT_THREAD"].as_str(), Some("thr_1"));
        let acp: sonic_rs::Value = sonic_rs::from_str(&McpServer::acp_params(&s)).unwrap();
        assert_eq!(acp[0]["env"][0]["value"].as_str(), Some("thr_1"));
        let opencode: sonic_rs::Value = sonic_rs::from_str(&McpServer::opencode_config(&s)).unwrap();
        assert_eq!(opencode["mcp"]["orbit"]["command"][1].as_str(), Some("mcp"));
        let codex = McpServer::codex_overrides(&s);
        assert_eq!(codex[1], r#"mcp_servers.orbit.command="C:\\Program Files\\Orbit\\orbit.exe""#);
        assert_eq!(codex[3], r#"mcp_servers.orbit.args=["mcp"]"#);
        assert_eq!(codex[5], r#"mcp_servers.orbit.env={ORBIT_THREAD="thr_1"}"#);
    }
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
