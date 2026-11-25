//! Claude Code adapter: one long-lived `claude -p` process per thread,
//! speaking stream-json with the control protocol on stdin and stdout.
//!
//! * The process lives as long as the thread's session. Every new process
//!   pays about 41K cache-creation tokens on its first turn, so Orbit never
//!   respawns per turn (T3 Code #7338). Model, permission mode, effort and
//!   fast mode switch live through control requests; only a change of the
//!   tool set (readonly), of thinking, or clearing the effort restarts it,
//!   with `--resume`, and only between turns.
//! * Stop sends the `interrupt` control request and keeps the process, so
//!   background tasks and subagents survive (T3 Code killed it on every
//!   Stop). The process is killed only when interrupt fails or times out,
//!   and on shutdown.
//! * Inherited host-session variables are removed, so Orbit works when it
//!   itself runs inside a Claude Code session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use orbit_proto::*;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::process::{AgentProcess, JsonlReader, JsonlWriter, resolve_binary};
use super::{AgentEvent, EventSender, PromptAttachment, SessionHandle, SessionInput, SessionSpec, TurnOutcome};

pub mod catalog;
pub mod parser;
pub mod tools;

use parser::{INIT_ID, INTERRUPT_PREFIX, Output, Parser};

/// How long Stop waits for the CLI to end the turn before killing it.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a new process may take to answer `initialize`.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Images larger than this are sent by path only (the API's limit is 5 MB
/// of base64).
const MAX_INLINE_IMAGE: u64 = 3_750_000;

/// Variables a parent Claude Code session sets for its own children. Passed
/// through, they make the CLI believe it is part of that session.
pub const HOST_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_DESKTOP_APP_VERSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
    "CLAUDE_CODE_ACCOUNT_UUID",
    "CLAUDE_CODE_ORGANIZATION_UUID",
    "CLAUDE_CODE_USER_EMAIL",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES",
    "CLAUDE_CODE_EMIT_STARTUP_TIMING",
    "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
    "CLAUDE_CODE_TERMINAL_MCP_TOOLS",
    "CLAUDE_CODE_REPORT_FINDINGS",
    "CLAUDE_CODE_EAGER_FLUSH",
    "CLAUDE_CODE_HOST_SHORT_TEMP_DIRS",
    "CLAUDE_CODE_DISABLE_TERMINAL_TITLE",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_DISABLE_CRON",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_AGENT_SDK_MCP_NO_PREFIX",
    "CLAUDE_PREVIEW_CLASSIFIER_FLOOR",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "ELECTRON_RUN_AS_NODE",
];

/// Where Claude Code installs itself besides PATH.
fn install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".claude").join("local"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(local) = dirs::data_local_dir() {
        let winget = local.join("Microsoft").join("WinGet");
        dirs.push(winget.join("Links"));
        dirs.push(winget.join("Packages").join("Anthropic.ClaudeCode_Microsoft.Winget.Source_8wekyb3d8bbwe"));
    }
    dirs
}

/// The Claude Code binary: the explicit path, else PATH and the known
/// install locations.
pub fn resolve(binary: Option<&Path>) -> Option<PathBuf> {
    resolve_binary("claude", binary, &install_dirs())
}

/// The environment every Claude process gets on top of the inherited one.
fn base_env() -> Vec<(String, String)> {
    vec![
        ("CLAUDE_CODE_ENTRYPOINT".to_owned(), "orbit".to_owned()),
        ("CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL".to_owned(), "true".to_owned()),
    ]
}

const STREAM_ARGS: &[&str] = &[
    "-p",
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool",
    "stdio",
];

/// The CLI permission mode for a config.
pub fn permission_mode(config: &ThreadConfig) -> &'static str {
    if config.mode == Mode::Plan {
        return "plan";
    }
    parser::access_mode(config.access)
}

/// Settings that need a new process to change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SpawnKey {
    readonly: bool,
    thinking_off: bool,
}

/// What the running process was told, so only real changes are sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Applied {
    model: String,
    mode: String,
    effort: Option<String>,
    fast: bool,
}

fn spawn_key(config: &ThreadConfig, model: Option<&ModelInfo>) -> SpawnKey {
    SpawnKey {
        readonly: config.access == Access::Readonly,
        thinking_off: catalog::toggle(config, model, catalog::OPT_THINKING) == Some(false),
    }
}

const NOT_FOUND: &str =
    "Claude Code was not found. Install it from https://claude.com/claude-code, or set its path in Settings.";

/// Starts a session task. Must be called inside the engine's Tokio runtime.
pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    let (input, rx) = crossfire::mpsc::bounded_blocking_async(64);
    tokio::spawn(async move {
        let thread = spec.thread;
        match Session::new(spec, events.clone()) {
            Some(session) => session.run(rx).await,
            None => {
                use crossfire::BlockingTxTrait;
                let _ = events.send((thread, AgentEvent::Exited { error: Some(NOT_FOUND.into()) }));
            }
        }
    });
    SessionHandle { input }
}

struct Proc {
    process: AgentProcess,
    writer: Option<JsonlWriter>,
    lines: mpsc::Receiver<Vec<u8>>,
    key: SpawnKey,
    /// Answered `initialize` yet.
    ready: bool,
    started: Instant,
}

struct Session {
    spec: SessionSpec,
    binary: PathBuf,
    events: EventSender,
    parser: Parser,
    proc: Option<Proc>,
    applied: Applied,
    /// The session exists on disk, so a new process must `--resume` it.
    has_history: bool,
    /// `resume_at` applies only to the first process.
    resume_at: Option<String>,
    pending_restart: bool,
    interrupt_deadline: Option<Instant>,
    next_request: u64,
    out: Output,
}

enum Flow {
    Continue,
    Stop,
}

impl Session {
    fn new(spec: SessionSpec, events: EventSender) -> Option<Self> {
        let binary = resolve(spec.binary.as_deref())?;
        let session_id = spec.resume.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut parser = Parser::new(spec.cwd.clone(), session_id, spec.config.access, Some(binary.display().to_string()));
        parser.requested_model = spec.config.model.clone();
        parser.catalog = catalog::last();
        parser.known_windows = parser.catalog.as_ref().map(|c| c.windows.clone()).unwrap_or_default();
        Some(Self {
            has_history: spec.resume.is_some(),
            resume_at: spec.resume_at.clone(),
            spec,
            binary,
            events,
            parser,
            proc: None,
            applied: Applied::default(),
            pending_restart: false,
            interrupt_deadline: None,
            next_request: 0,
            out: Output::default(),
        })
    }

    fn emit(&self, event: AgentEvent) {
        use crossfire::BlockingTxTrait;
        let _ = self.events.send((self.spec.thread, event));
    }

    fn flush(&mut self) {
        use crossfire::BlockingTxTrait;
        for event in self.out.events.drain(..) {
            let _ = self.events.send((self.spec.thread, event));
        }
    }

    fn model_info(&self) -> Option<ModelInfo> {
        self.parser.catalog.as_ref().and_then(|c| c.model(&self.spec.config.model)).cloned()
    }

    async fn run(mut self, rx: crossfire::AsyncRx<crossfire::mpsc::Array<SessionInput>>) {
        if let Err(e) = self.spawn().await {
            self.emit(AgentEvent::Exited { error: Some(format!("Could not start Claude Code: {e}")) });
            return;
        }
        loop {
            let deadline = self.interrupt_deadline;
            let init_deadline = self.proc.as_ref().filter(|p| !p.ready).map(|p| p.started + INIT_TIMEOUT);
            let flow = tokio::select! {
                line = next_line(&mut self.proc) => match line {
                    Some(line) => self.on_line(&line).await,
                    None => self.on_exit().await,
                },
                input = rx.recv() => match input {
                    Ok(input) => self.on_input(input).await,
                    Err(_) => {
                        self.shutdown().await;
                        Flow::Stop
                    }
                },
                _ = sleep_until(deadline) => self.on_interrupt_timeout().await,
                _ = sleep_until(init_deadline) => {
                    let tail = self.proc.as_ref().map(|p| p.process.stderr_tail()).unwrap_or_default();
                    self.proc = None;
                    self.emit(AgentEvent::Exited { error: Some(with_tail("Claude Code did not start in time", &tail)) });
                    Flow::Stop
                }
            };
            if let Flow::Stop = flow {
                break;
            }
        }
    }

    // ------------------------------------------------------------ process

    fn args(&self, key: &SpawnKey, model: Option<&ModelInfo>) -> Vec<String> {
        let config = &self.spec.config;
        let mut args: Vec<String> = STREAM_ARGS.iter().map(|s| (*s).to_owned()).collect();
        // Lets Full access switch on live; the mode itself is chosen below.
        args.push("--allow-dangerously-skip-permissions".into());
        args.extend(["--permission-mode".into(), permission_mode(config).into()]);
        if !config.model.is_empty() {
            args.extend(["--model".into(), config.model.clone()]);
        }
        if let Some(effort) = model.and_then(|m| catalog::effort(config, Some(m))) {
            args.extend(["--effort".into(), effort]);
        }
        let fast = model.and_then(|m| catalog::toggle(config, Some(m), catalog::OPT_FAST)).unwrap_or(false);
        let mut settings = Vec::new();
        if key.thinking_off {
            settings.push(r#""alwaysThinkingEnabled":false"#);
        }
        if fast {
            settings.push(r#""fastMode":true"#);
        }
        if !settings.is_empty() {
            args.extend(["--settings".into(), format!("{{{}}}", settings.join(","))]);
        }
        if key.readonly {
            args.extend(["--tools".into(), tools::READONLY_TOOLS.join(",")]);
        }
        for dir in &self.spec.extra_dirs {
            args.extend(["--add-dir".into(), dir.display().to_string()]);
        }
        if self.has_history {
            args.extend(["--resume".into(), self.parser.session_id.clone()]);
            if let Some(at) = &self.resume_at {
                args.extend(["--resume-session-at".into(), at.clone()]);
            }
        } else {
            args.extend(["--session-id".into(), self.parser.session_id.clone()]);
        }
        args
    }

    async fn spawn(&mut self) -> std::io::Result<()> {
        let model = self.model_info();
        let key = spawn_key(&self.spec.config, model.as_ref());
        let args = self.args(&key, model.as_ref());
        let mut env = base_env();
        if self.has_history {
            // Connect MCP servers before the first resumed request.
            env.push(("MCP_CONNECTION_NONBLOCKING".into(), "0".into()));
        }
        env.extend(self.spec.env.iter().cloned());
        log::debug!("claude: spawning {} {}", self.binary.display(), args.join(" "));
        let mut process = AgentProcess::spawn(&self.binary, &args, &self.spec.cwd, &env, HOST_ENV)?;
        let stdin = process.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = process.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        // A task owns the reader: `read_until` is not cancel-safe, so it
        // cannot sit in the session's `select!`.
        let (tx, lines) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut reader = JsonlReader::new(stdout);
            while let Ok(Some(line)) = reader.next_line().await {
                if tx.send(line.to_vec()).await.is_err() {
                    break;
                }
            }
        });
        let mut writer = JsonlWriter::new(stdin);
        writer.send_raw(&init_request()).await?;
        self.resume_at = None;
        self.applied = Applied {
            model: self.spec.config.model.clone(),
            mode: permission_mode(&self.spec.config).to_owned(),
            effort: model.as_ref().and_then(|m| catalog::effort(&self.spec.config, Some(m))),
            fast: model.as_ref().and_then(|m| catalog::toggle(&self.spec.config, Some(m), catalog::OPT_FAST)).unwrap_or(false),
        };
        self.proc = Some(Proc { process, writer: Some(writer), lines, key, ready: false, started: Instant::now() });
        Ok(())
    }

    /// Replaces the process with a fresh one resuming the same session.
    async fn restart(&mut self) -> Flow {
        self.pending_restart = false;
        self.proc = None; // dropping kills the tree
        match self.spawn().await {
            Ok(()) => Flow::Continue,
            Err(e) => {
                self.emit(AgentEvent::Exited { error: Some(format!("Could not restart Claude Code: {e}")) });
                Flow::Stop
            }
        }
    }

    async fn write(&mut self, line: &str) {
        let Some(writer) = self.proc.as_mut().and_then(|p| p.writer.as_mut()) else { return };
        if let Err(e) = writer.send_raw(line).await {
            // The reader sees the exit and reports it.
            log::warn!("claude: write failed: {e}");
        }
    }

    async fn write_replies(&mut self) {
        let replies = std::mem::take(&mut self.out.replies);
        for r in &replies {
            self.write(r).await;
        }
    }

    async fn control(&mut self, kind: &str, request: &str) {
        self.next_request += 1;
        let line =
            format!(r#"{{"type":"control_request","request_id":"orbit_{kind}_{}","request":{request}}}"#, self.next_request);
        self.write(&line).await;
    }

    // ------------------------------------------------------------ events

    async fn on_line(&mut self, line: &[u8]) -> Flow {
        self.out.clear();
        self.parser.feed(line, &mut self.out);
        self.write_replies().await;
        let (initialized, interrupt_failed, plan_approved) =
            (self.out.initialized, self.out.interrupt_failed, self.out.plan_approved);
        self.flush();
        if initialized {
            if let Some(p) = self.proc.as_mut() {
                p.ready = true;
            }
            // Options the catalog could not validate at spawn time.
            self.apply_live().await;
        }
        if plan_approved {
            self.on_plan_approved().await;
        }
        if interrupt_failed && self.parser.turn_active() {
            return self.on_interrupt_timeout().await;
        }
        if !self.parser.turn_active() {
            self.interrupt_deadline = None;
            if self.pending_restart {
                return self.restart().await;
            }
        }
        Flow::Continue
    }

    async fn on_exit(&mut self) -> Flow {
        let Some(mut proc) = self.proc.take() else { return Flow::Continue };
        let status = tokio::time::timeout(Duration::from_secs(2), proc.process.wait()).await;
        let tail = proc.process.stderr_tail();
        let message = match status.ok().and_then(Result::ok).and_then(|s| s.code()) {
            Some(0) if !self.parser.turn_active() => None,
            Some(c) => Some(with_tail(&format!("Claude Code exited with code {c}"), &tail)),
            None => Some(with_tail("Claude Code stopped unexpectedly", &tail)),
        };
        self.out.clear();
        let failure = message.clone().unwrap_or_else(|| "Claude Code exited".into());
        self.parser.force_finish(TurnOutcome::Failed { message: failure }, &mut self.out);
        self.flush();
        self.emit(AgentEvent::Exited { error: message });
        Flow::Stop
    }

    async fn on_interrupt_timeout(&mut self) -> Flow {
        self.interrupt_deadline = None;
        if !self.parser.turn_active() {
            return Flow::Continue;
        }
        self.out.clear();
        self.parser.force_finish(TurnOutcome::Interrupted, &mut self.out);
        self.flush();
        self.emit(AgentEvent::Notice {
            level: NoticeLevel::Warning,
            text: "Claude Code did not stop in time, so Orbit restarted it".into(),
        });
        self.restart().await
    }

    // ------------------------------------------------------------ inputs

    async fn on_input(&mut self, input: SessionInput) -> Flow {
        match input {
            SessionInput::Prompt { uuid, text, attachments, steer } => {
                if self.proc.is_none()
                    && let Flow::Stop = self.restart().await
                {
                    return Flow::Stop;
                }
                let line = user_message(&uuid, &text, &attachments, steer).await;
                self.write(&line).await;
                self.has_history = true;
            }
            SessionInput::Interrupt => {
                if self.parser.turn_active() && self.proc.is_some() {
                    self.parser.interrupting = true;
                    self.next_request += 1;
                    let line = format!(
                        r#"{{"type":"control_request","request_id":"{INTERRUPT_PREFIX}{}","request":{{"subtype":"interrupt"}}}}"#,
                        self.next_request
                    );
                    self.write(&line).await;
                    self.interrupt_deadline = Some(Instant::now() + INTERRUPT_TIMEOUT);
                }
            }
            SessionInput::Resolve { provider_ref, decision } => {
                self.out.clear();
                if !self.parser.resolve(&provider_ref, &decision, &mut self.out) {
                    log::debug!("claude: resolve for unknown request {provider_ref}");
                }
                self.write_replies().await;
                let plan_approved = self.out.plan_approved;
                self.flush();
                if plan_approved {
                    self.on_plan_approved().await;
                }
            }
            SessionInput::Configure(config) => return self.configure(config).await,
            SessionInput::Shutdown => {
                self.shutdown().await;
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    /// The user approved the plan: leave plan mode for the thread's access
    /// level. The engine records the mode change on its side.
    async fn on_plan_approved(&mut self) {
        self.spec.config.mode = Mode::Build;
        self.apply_live().await;
    }

    async fn configure(&mut self, config: ThreadConfig) -> Flow {
        self.spec.config = config;
        self.parser.access = self.spec.config.access;
        self.parser.requested_model = self.spec.config.model.clone();
        let Some(proc) = &self.proc else { return Flow::Continue };
        let model = self.model_info();
        let effort_cleared =
            self.applied.effort.is_some() && catalog::effort(&self.spec.config, model.as_ref()).is_none();
        if proc.key != spawn_key(&self.spec.config, model.as_ref()) || effort_cleared {
            if self.parser.turn_active() {
                self.pending_restart = true;
                return Flow::Continue;
            }
            return self.restart().await;
        }
        self.apply_live().await;
        Flow::Continue
    }

    /// Sends what changed through control requests.
    async fn apply_live(&mut self) {
        if self.proc.is_none() {
            return;
        }
        let config = self.spec.config.clone();
        let model = self.model_info();
        if config.model != self.applied.model {
            let slug = if config.model.is_empty() { "default" } else { config.model.as_str() };
            self.control("model", &format!(r#"{{"subtype":"set_model","model":{}}}"#, json_str(slug))).await;
            self.applied.model = config.model.clone();
        }
        let mode = permission_mode(&config);
        if mode != self.applied.mode {
            self.control("mode", &format!(r#"{{"subtype":"set_permission_mode","mode":"{mode}"}}"#)).await;
            self.applied.mode = mode.to_owned();
        }
        let mut flags = Vec::new();
        let effort = model.as_ref().and_then(|m| catalog::effort(&config, Some(m)));
        if let Some(level) = &effort
            && effort != self.applied.effort
        {
            flags.push(format!(r#""effortLevel":{}"#, json_str(level)));
            self.applied.effort = effort;
        }
        let fast = model.as_ref().and_then(|m| catalog::toggle(&config, Some(m), catalog::OPT_FAST)).unwrap_or(false);
        if fast != self.applied.fast {
            flags.push(format!(r#""fastMode":{fast}"#));
            self.applied.fast = fast;
        }
        if !flags.is_empty() {
            let request = format!(r#"{{"subtype":"apply_flag_settings","settings":{{{}}}}}"#, flags.join(","));
            self.control("flags", &request).await;
        }
    }

    async fn shutdown(&mut self) {
        if let Some(mut proc) = self.proc.take() {
            // Closing stdin ends the CLI cleanly; dropping kills what is left.
            proc.writer = None;
            let _ = tokio::time::timeout(Duration::from_secs(3), proc.process.wait()).await;
        }
        self.out.clear();
        self.parser.force_finish(TurnOutcome::Interrupted, &mut self.out);
        self.flush();
        self.emit(AgentEvent::Exited { error: None });
    }
}

fn init_request() -> String {
    format!(r#"{{"type":"control_request","request_id":"{INIT_ID}","request":{{"subtype":"initialize","hooks":null}}}}"#)
}

async fn next_line(proc: &mut Option<Proc>) -> Option<Vec<u8>> {
    match proc {
        Some(p) => p.lines.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// `message` with the last lines of stderr, which carry the reason.
fn with_tail(message: &str, tail: &str) -> String {
    let tail = tail.trim();
    if tail.is_empty() {
        return message.to_owned();
    }
    let lines: Vec<&str> = tail.lines().collect();
    let start = lines.len().saturating_sub(12);
    format!("{message}:\n{}", lines[start..].join("\n"))
}

fn json_str(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn is_inline_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

/// A stream-json user message: images as base64 blocks, then a text block
/// naming every attachment's path, then the prompt itself as the last block
/// (Claude Code expands a `/command` only at the start of the last block).
pub async fn user_message(uuid: &str, text: &str, attachments: &[PromptAttachment], steer: bool) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut notes = Vec::new();
    for a in attachments {
        let name = &a.attachment.name;
        let path = a.path.display();
        let mime = a.attachment.mime.as_str();
        if is_inline_image(mime) {
            let size = tokio::fs::metadata(&a.path).await.map(|m| m.len()).unwrap_or(u64::MAX);
            let data = if size <= MAX_INLINE_IMAGE { tokio::fs::read(&a.path).await.ok() } else { None };
            match data {
                Some(bytes) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    blocks.push(format!(
                        r#"{{"type":"image","source":{{"type":"base64","media_type":"{mime}","data":"{b64}"}}}}"#
                    ));
                    notes.push(format!("[Attached image \"{name}\" is saved at: {path}]"));
                }
                None => notes.push(format!(
                    "[Attached image \"{name}\" is saved at: {path} (too large to show inline; read it from disk)]"
                )),
            }
        } else {
            notes.push(format!("[Attached file \"{name}\" ({mime}) is saved at: {path}]"));
        }
    }
    if !notes.is_empty() {
        blocks.push(format!(r#"{{"type":"text","text":{}}}"#, json_str(&notes.join("\n"))));
    }
    blocks.push(format!(r#"{{"type":"text","text":{}}}"#, json_str(text)));
    let priority = if steer { r#","priority":"next""# } else { "" };
    format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{}]}},"parent_tool_use_id":null,"uuid":{}{priority}}}"#,
        blocks.join(","),
        json_str(uuid)
    )
}

/// Detects Claude Code and reads its catalog with an `initialize` request
/// and no prompt, so it costs nothing.
pub async fn probe(binary: Option<PathBuf>, user_env: Vec<(String, String)>) -> ProviderInfo {
    let Some(path) = resolve(binary.as_deref()) else {
        return ProviderInfo {
            kind: ProviderKind::Claude,
            binary: binary.as_ref().map(|b| b.display().to_string()),
            message: Some(NOT_FOUND.to_owned()),
            ..Default::default()
        };
    };
    let shown = path.display().to_string();
    let failed = |message: String| ProviderInfo {
        kind: ProviderKind::Claude,
        installed: true,
        binary: Some(shown.clone()),
        message: Some(message),
        ..Default::default()
    };
    let mut args: Vec<&str> = STREAM_ARGS.to_vec();
    args.extend([
        "--permission-mode",
        "default",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ]);
    let mut env = base_env();
    env.extend(user_env);
    env.push(("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into()));
    env.push(("CLAUDE_CODE_AUTO_CONNECT_IDE".into(), "0".into()));
    let cwd = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let mut process = match AgentProcess::spawn(&path, &args, &cwd, &env, HOST_ENV) {
        Ok(p) => p,
        Err(e) => return failed(format!("Could not start Claude Code: {e}")),
    };
    let (Some(stdin), Some(stdout)) = (process.stdin.take(), process.stdout.take()) else {
        return failed("Could not talk to Claude Code".into());
    };
    let mut writer = JsonlWriter::new(stdin);
    let mut reader = JsonlReader::new(stdout);
    let exchange = async {
        writer.send_raw(&init_request()).await.ok()?;
        let mut parser = Parser::new(cwd.clone(), String::new(), Access::Ask, Some(shown.clone()));
        let mut out = Output::default();
        while let Ok(Some(line)) = reader.next_line().await {
            out.clear();
            parser.feed(line, &mut out);
            if out.initialized {
                return parser.catalog.take();
            }
        }
        None
    };
    let result = tokio::time::timeout(PROBE_TIMEOUT, exchange).await;
    let tail = process.stderr_tail();
    drop(process);
    match result {
        Ok(Some(catalog)) => catalog.info,
        Ok(None) => failed(with_tail("Claude Code exited before reporting its models", &tail)),
        Err(_) => failed(with_tail("Claude Code did not answer within 20 seconds", &tail)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_maps_to_permission_modes() {
        let mut c = ThreadConfig::default();
        assert_eq!(permission_mode(&c), "default");
        c.access = Access::AutoEdit;
        assert_eq!(permission_mode(&c), "acceptEdits");
        c.access = Access::Full;
        assert_eq!(permission_mode(&c), "bypassPermissions");
        c.mode = Mode::Plan;
        assert_eq!(permission_mode(&c), "plan");
        c.mode = Mode::Build;
        c.access = Access::Readonly;
        assert_eq!(permission_mode(&c), "default");
        assert!(spawn_key(&c, None).readonly);
    }

    #[tokio::test]
    async fn prompts_carry_images_paths_and_priority() {
        use sonic_rs::{JsonContainerTrait, JsonValueTrait};
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("abc");
        std::fs::write(&img, [137u8, 80, 78, 71]).unwrap();
        let att = PromptAttachment {
            attachment: Attachment { name: "shot.png".into(), mime: "image/png".into(), hash: "abc".into(), size: 4 },
            path: img.clone(),
        };
        let line = user_message("u-1", "/review now", &[att], true).await;
        let v: sonic_rs::Value = sonic_rs::from_str(&line).unwrap();
        assert_eq!(v["priority"].as_str(), Some("next"));
        assert_eq!(v["uuid"].as_str(), Some("u-1"));
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"].as_str(), Some("image"));
        assert_eq!(content[0]["source"]["data"].as_str(), Some("iVBORw=="));
        assert!(content[1]["text"].as_str().unwrap().contains(&img.display().to_string()));
        assert_eq!(content[2]["text"].as_str(), Some("/review now"));
        let plain = user_message("u-2", "hi", &[], false).await;
        assert!(!plain.contains("priority"));
    }
}
