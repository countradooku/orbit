//! One ACP thread session: an agent process and its protocol state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use base64::Engine as _;
use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};
use tokio::time::Instant;

use super::super::jsonrpc::Pipe;
use super::super::{AgentEvent, BlockKind, EventSender, PromptAttachment, SessionInput, SessionSpec, TurnOutcome};
use super::map::{self, SessionConfig, Source, Tool, str_of};

pub type Inputs = crossfire::AsyncRx<crossfire::mpsc::Array<SessionInput>>;

/// Startup (spawn, `initialize`, session setup) may take this long; agents
/// started through `npx` download themselves on first use.
const INIT_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a cancelled turn may go unacknowledged before the agent is
/// killed.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);
/// Images larger than this are sent as links.
const MAX_INLINE_IMAGE: u64 = 3_750_000;
/// Text files up to this size are embedded when the agent accepts it.
const MAX_EMBEDDED_TEXT: u64 = 256 * 1024;
/// JSON-RPC error the spec uses for "authentication required".
const AUTH_REQUIRED: i64 = -32000;
const METHOD_NOT_FOUND: i64 = -32601;

pub async fn run(spec: SessionSpec, events: EventSender, inputs: Inputs) {
    let emit = |events: &EventSender, thread, event| {
        use crossfire::BlockingTxTrait;
        let _ = events.send((thread, event));
    };
    let Some(binary) = super::resolve(spec.binary.as_deref()) else {
        emit(&events, spec.thread, AgentEvent::Exited { error: Some(super::not_found(spec.binary.as_deref())) });
        return;
    };
    let name = binary.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "The agent".into());
    let (pipe, mut lines) = match Pipe::spawn(&name, &binary, &spec.args, &spec.cwd, &spec.env, true) {
        Ok(pipe) => pipe,
        Err(err) => {
            emit(&events, spec.thread, AgentEvent::Exited { error: Some(format!("Couldn't start {name}: {err}")) });
            return;
        }
    };
    let mut session = Session::new(spec, events, pipe, name);
    session.initialize();
    let init_deadline = Instant::now() + INIT_TIMEOUT;
    let error = loop {
        let deadline = if session.session_id.is_none() { Some(init_deadline) } else { session.cancel_deadline() };
        tokio::select! {
            input = inputs.recv() => match input {
                Ok(input) => {
                    if !session.on_input(input) {
                        break None;
                    }
                }
                Err(_) => break None,
            },
            line = lines.recv() => match line {
                Some(line) => {
                    if let Err(fatal) = session.on_line(&line) {
                        session.fail_turn(&fatal);
                        break Some(fatal);
                    }
                }
                None => {
                    let reason = session.pipe.exit_reason().await;
                    session.fail_turn(&reason);
                    break Some(reason);
                }
            },
            _ = sleep_until(deadline) => {
                if session.session_id.is_none() {
                    let reason = format!("{} didn't start within {} seconds", session.name, INIT_TIMEOUT.as_secs());
                    session.fail_turn(&reason);
                    break Some(reason);
                }
                // The agent never acknowledged the cancel: end the turn here
                // and stop the process; the next message reopens the session.
                session.finish_turn(TurnOutcome::Interrupted, None);
                session.pipe.process.kill();
                break None;
            }
        }
    };
    session.emit(AgentEvent::Exited { error });
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[derive(Clone, Debug)]
struct Prompt {
    /// `None` for prompts the adapter sends itself (denial feedback).
    uuid: Option<String>,
    text: String,
    attachments: Vec<PromptAttachment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    New,
    Load,
    Resume,
}

enum Call {
    Initialize,
    Authenticate(Open),
    Open(Open),
    Prompt,
    /// `session/set_config_option`; the label names it in error notices.
    SetOption(String),
    Setter(&'static str),
}

struct Turn {
    started: Instant,
    usage: Usage,
    cancel_deadline: Option<Instant>,
}

struct Permission {
    raw_id: String,
    options: Vec<(String, String)>,
    /// Identifies "the same request" for session-wide grants.
    grant_key: String,
}

#[derive(Default)]
struct Caps {
    load: bool,
    resume: bool,
    image: bool,
    embedded: bool,
    /// (id, type) of each advertised sign-in method.
    auth: Vec<(String, String)>,
}

struct Session {
    spec: SessionSpec,
    events: EventSender,
    pipe: Pipe,
    name: String,
    calls: HashMap<u64, Call>,
    caps: Caps,
    version: Option<String>,
    session_id: Option<String>,
    /// `session/load` replays history as updates; Orbit has it already.
    replaying: bool,
    config: SessionConfig,
    /// The mode the session started in, restored when plan mode ends.
    build_mode: Option<String>,
    commands: Vec<SlashCommand>,
    queue: VecDeque<Prompt>,
    turn: Option<Turn>,
    /// Open text and reasoning blocks: (block id, text so far).
    text: Option<(String, String)>,
    thought: Option<(String, String)>,
    message_id: Option<String>,
    blocks: u64,
    tools: HashMap<String, Tool>,
    tools_done: HashSet<String>,
    permissions: HashMap<String, Permission>,
    /// "Allow for session" grants the agent had no option for.
    grants: HashSet<String>,
}

impl Session {
    fn new(spec: SessionSpec, events: EventSender, pipe: Pipe, name: String) -> Self {
        Self {
            spec,
            events,
            pipe,
            name,
            calls: HashMap::new(),
            caps: Caps::default(),
            version: None,
            session_id: None,
            replaying: false,
            config: SessionConfig::default(),
            build_mode: None,
            commands: Vec::new(),
            queue: VecDeque::new(),
            turn: None,
            text: None,
            thought: None,
            message_id: None,
            blocks: 0,
            tools: HashMap::new(),
            tools_done: HashSet::new(),
            permissions: HashMap::new(),
            grants: HashSet::new(),
        }
    }

    fn emit(&self, event: AgentEvent) {
        use crossfire::BlockingTxTrait;
        let _ = self.events.send((self.spec.thread, event));
    }

    fn request(&mut self, method: &str, params: Value, call: Call) {
        let id = self.pipe.request(method, &params);
        self.calls.insert(id, call);
    }

    fn cancel_deadline(&self) -> Option<Instant> {
        self.turn.as_ref().and_then(|t| t.cancel_deadline)
    }

    fn cwd(&self) -> String {
        self.spec.cwd.display().to_string()
    }

    // ── Setup ────────────────────────────────────────────────────────────

    fn initialize(&mut self) {
        self.request("initialize", super::initialize_params(), Call::Initialize);
    }

    fn open(&mut self, open: Open) {
        let cwd = self.cwd();
        let (method, params) = match (open, self.spec.resume.as_deref()) {
            (Open::Load, Some(id)) => ("session/load", json!({"sessionId": id, "cwd": cwd, "mcpServers": []})),
            (Open::Resume, Some(id)) => ("session/resume", json!({"sessionId": id, "cwd": cwd, "mcpServers": []})),
            _ => ("session/new", json!({"cwd": cwd, "mcpServers": []})),
        };
        self.replaying = open == Open::Load;
        self.request(method, params, Call::Open(open));
    }

    fn first_open(&mut self) -> Open {
        match self.spec.resume {
            Some(_) if self.caps.resume => Open::Resume,
            Some(_) if self.caps.load => Open::Load,
            Some(_) => {
                self.emit(AgentEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("{} can't reopen earlier sessions, so this one starts fresh.", self.name),
                });
                Open::New
            }
            None => Open::New,
        }
    }

    fn on_initialized(&mut self, result: &Value) {
        let caps = result.get("agentCapabilities");
        let flag = |path: &[&str]| {
            let mut value = caps;
            for key in path {
                value = value.and_then(|v| v.get(*key));
            }
            value.is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()))
        };
        self.caps = Caps {
            load: flag(&["loadSession"]),
            resume: flag(&["sessionCapabilities", "resume"]),
            image: flag(&["promptCapabilities", "image"]),
            embedded: flag(&["promptCapabilities", "embeddedContext"]),
            auth: map::array(result, "authMethods")
                .filter_map(|m| Some((str_of(m, "id")?, str_of(m, "type").unwrap_or_else(|| "agent".into()))))
                .collect(),
        };
        self.version = result.get("agentInfo").and_then(|i| str_of(i, "version"));
        let open = self.first_open();
        self.open(open);
    }

    /// The agent said it needs a sign-in before `open`.
    fn authenticate(&mut self, open: Open) -> Result<(), String> {
        let method = self.caps.auth.iter().find(|(_, kind)| kind == "agent").map(|(id, _)| id.clone());
        match method {
            Some(id) => {
                self.request("authenticate", json!({"methodId": id}), Call::Authenticate(open));
                Ok(())
            }
            None => Err(format!(
                "{} needs you to sign in first. Run it once in a terminal to sign in, then send your message again.",
                self.name
            )),
        }
    }

    fn on_opened(&mut self, open: Open, result: &Value) {
        let id = str_of(result, "sessionId").or_else(|| self.spec.resume.clone()).unwrap_or_default();
        self.session_id = Some(id.clone());
        self.replaying = false;
        self.config = SessionConfig::from_result(result);
        self.build_mode = self.config.modes.as_ref().map(|m| m.current.clone()).filter(|m| !m.is_empty());
        if self.config.plan_mode() == self.build_mode.as_deref() {
            self.build_mode = None;
        }
        let model = self.config.models.as_ref().map(|m| m.current.clone()).filter(|m| !m.is_empty());
        if open != Open::New {
            log::debug!("acp: reopened session {id}");
        }
        self.emit(AgentEvent::Ready { session_id: id, model });
        self.emit_catalog();
        let config = self.spec.config.clone();
        self.apply_config(&config);
        self.start_next();
    }

    fn emit_catalog(&self) {
        self.emit(AgentEvent::Catalog(ProviderInfo {
            kind: ProviderKind::Acp,
            installed: true,
            version: self.version.clone(),
            binary: self.spec.binary.as_ref().map(|b| b.display().to_string()),
            models: self.config.model_infos(),
            modes: self.config.mode_infos(),
            slash_commands: self.commands.clone(),
            ..Default::default()
        }));
    }

    /// Brings the session's model, mode and options in line with `config`.
    fn apply_config(&mut self, config: &ThreadConfig) {
        let Some(session) = self.session_id.clone() else { return };
        if let Some(models) = self.config.models.clone()
            && !config.model.is_empty()
            && models.current != config.model
            && models.choices.iter().any(|c| c.id == config.model)
        {
            match &models.source {
                Source::Option(option) => self.request(
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": option, "value": config.model}),
                    Call::SetOption("the model".into()),
                ),
                Source::Legacy => self.request(
                    "session/set_model",
                    json!({"sessionId": session, "modelId": config.model}),
                    Call::Setter("the model"),
                ),
            }
            if let Some(models) = self.config.models.as_mut() {
                models.current = config.model.clone();
            }
        }
        if let Some(wanted) = self.config.wanted_mode(config, self.build_mode.as_deref())
            && let Some(modes) = self.config.modes.clone()
            && modes.current != wanted
        {
            match &modes.source {
                Source::Option(option) => self.request(
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": option, "value": wanted}),
                    Call::SetOption("the mode".into()),
                ),
                Source::Legacy => self.request(
                    "session/set_mode",
                    json!({"sessionId": session, "modeId": wanted}),
                    Call::Setter("the mode"),
                ),
            }
            if let Some(modes) = self.config.modes.as_mut() {
                modes.current = wanted;
            }
        }
        for value in &config.options {
            let Some(extra) = self.config.extras.iter_mut().find(|e| e.id == value.id) else { continue };
            if extra.current == value.value {
                continue;
            }
            extra.current = value.value.clone();
            let params = match &value.value {
                OptionSetting::Choice(choice) => {
                    json!({"sessionId": session, "configId": value.id, "value": choice})
                }
                OptionSetting::Toggle(on) => {
                    json!({"sessionId": session, "configId": value.id, "type": "boolean", "value": on})
                }
            };
            let label = extra.name.to_lowercase();
            let id = self.pipe.request("session/set_config_option", &params);
            self.calls.insert(id, Call::SetOption(label));
        }
    }

    // ── Inputs from the engine ───────────────────────────────────────────

    /// Returns `false` to end the session.
    fn on_input(&mut self, input: SessionInput) -> bool {
        match input {
            SessionInput::Prompt { uuid, text, attachments, .. } => {
                // ACP cannot steer: a steer runs as the next prompt.
                self.queue.push_back(Prompt { uuid: Some(uuid), text, attachments });
                self.start_next();
            }
            SessionInput::Interrupt => self.cancel(),
            SessionInput::Resolve { provider_ref, decision } => self.resolve(&provider_ref, decision),
            SessionInput::Configure(config) => {
                self.spec.config = config.clone();
                self.apply_config(&config);
            }
            SessionInput::Shutdown => return false,
        }
        true
    }

    fn start_next(&mut self) {
        let Some(session) = self.session_id.clone() else { return };
        if self.turn.is_some() {
            return;
        }
        let Some(prompt) = self.queue.pop_front() else { return };
        let blocks = self.prompt_blocks(&prompt);
        self.request("session/prompt", json!({"sessionId": session, "prompt": blocks}), Call::Prompt);
        self.turn = Some(Turn { started: Instant::now(), usage: Usage::default(), cancel_deadline: None });
        self.tools.clear();
        self.tools_done.clear();
        self.message_id = None;
        self.emit(AgentEvent::TurnStarted { prompt: prompt.uuid });
    }

    fn prompt_blocks(&self, prompt: &Prompt) -> Vec<Value> {
        let mut blocks = Vec::new();
        if !prompt.text.is_empty() {
            blocks.push(json!({"type": "text", "text": prompt.text}));
        }
        for a in &prompt.attachments {
            let mime = a.attachment.mime.as_str();
            if mime.starts_with("image/")
                && self.caps.image
                && a.attachment.size <= MAX_INLINE_IMAGE
                && let Ok(bytes) = std::fs::read(&a.path)
            {
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                blocks.push(json!({"type": "image", "data": data, "mimeType": mime}));
                continue;
            }
            let texty = mime.starts_with("text/") || mime.contains("json") || mime.contains("xml");
            if texty && self.caps.embedded && a.attachment.size <= MAX_EMBEDDED_TEXT
                && let Ok(text) = std::fs::read_to_string(&a.path)
            {
                blocks.push(json!({"type": "resource", "resource": {"uri": file_uri(&a.path), "mimeType": mime, "text": text}}));
                continue;
            }
            // Every ACP agent must accept links.
            blocks.push(json!({
                "type": "resource_link", "uri": file_uri(&a.path), "name": a.attachment.name,
                "mimeType": mime, "size": a.attachment.size
            }));
        }
        if blocks.is_empty() {
            blocks.push(json!({"type": "text", "text": ""}));
        }
        blocks
    }

    /// Stop: ask the agent to cancel and wait for it to say it did.
    fn cancel(&mut self) {
        let Some(turn) = self.turn.as_mut() else { return };
        if turn.cancel_deadline.is_some() {
            return;
        }
        turn.cancel_deadline = Some(Instant::now() + CANCEL_TIMEOUT);
        if let Some(session) = &self.session_id {
            self.pipe.notify("session/cancel", &json!({"sessionId": session}));
        }
        // The spec requires answering open permission requests `cancelled`.
        self.withdraw_permissions();
    }

    fn withdraw_permissions(&mut self) {
        for (provider_ref, permission) in std::mem::take(&mut self.permissions) {
            self.pipe.respond(&permission.raw_id, r#"{"outcome":{"outcome":"cancelled"}}"#);
            self.emit(AgentEvent::RequestWithdrawn { provider_ref });
        }
    }

    fn resolve(&mut self, provider_ref: &str, decision: Decision) {
        let Some(permission) = self.permissions.remove(provider_ref) else { return };
        let option = map::pick_option(&permission.options, &decision);
        if decision == Decision::AllowForSession && !permission.options.iter().any(|(_, k)| k == "allow_always") {
            // No session-scoped option: keep the grant here instead of
            // silently treating it as "once".
            self.grants.insert(permission.grant_key.clone());
        }
        self.answer(&permission.raw_id, option.as_deref());
        if let Decision::Deny { feedback } = decision
            && !feedback.trim().is_empty()
        {
            self.queue.push_front(Prompt { uuid: None, text: feedback, attachments: Vec::new() });
        }
    }

    fn answer(&self, raw_id: &str, option: Option<&str>) {
        let result = match option {
            Some(id) => format!(r#"{{"outcome":{{"outcome":"selected","optionId":{}}}}}"#, super::super::jsonrpc::json_string(id)),
            None => r#"{"outcome":{"outcome":"cancelled"}}"#.to_owned(),
        };
        self.pipe.respond(raw_id, &result);
    }

    // ── Lines from the agent ─────────────────────────────────────────────

    /// Handles one line. `Err` ends the session with that message.
    fn on_line(&mut self, line: &[u8]) -> Result<(), String> {
        let Ok(message) = sonic_rs::from_slice::<Value>(line) else {
            log::debug!("acp: skipping a line that is not JSON");
            return Ok(());
        };
        match (message.get("method").and_then(|m| m.as_str()), message.get("id")) {
            (Some(method), Some(id)) => {
                let raw_id = sonic_rs::to_string(id).unwrap_or_else(|_| "null".into());
                let params = message.get("params").cloned().unwrap_or_default();
                self.on_request(method, &raw_id, &params);
                Ok(())
            }
            (Some(method), None) => {
                if method == "session/update"
                    && let Some(params) = message.get("params")
                {
                    self.on_update(params);
                }
                Ok(())
            }
            (None, Some(id)) => {
                let Some(call) = id.as_u64().and_then(|id| self.calls.remove(&id)) else { return Ok(()) };
                match message.get("error") {
                    Some(error) => self.on_error(call, error),
                    None => {
                        let result = message.get("result").cloned().unwrap_or_default();
                        self.on_result(call, &result);
                        Ok(())
                    }
                }
            }
            (None, None) => Ok(()),
        }
    }

    fn on_result(&mut self, call: Call, result: &Value) {
        match call {
            Call::Initialize => self.on_initialized(result),
            Call::Authenticate(open) => self.open(open),
            Call::Open(open) => self.on_opened(open, result),
            Call::Prompt => {
                let stop = str_of(result, "stopReason").unwrap_or_else(|| "end_turn".into());
                let usage = result.get("usage").map(map::prompt_usage);
                let (outcome, note) = map::outcome(&stop);
                let cancelled = self.turn.as_ref().is_some_and(|t| t.cancel_deadline.is_some());
                let outcome = if cancelled && outcome == TurnOutcome::Completed { TurnOutcome::Interrupted } else { outcome };
                if let Some(note) = note {
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: note.into() });
                }
                self.finish_turn(outcome, usage);
            }
            Call::SetOption(_) => {
                if let Some(options) = result.get("configOptions") {
                    self.config.apply_options(options);
                    self.emit_catalog();
                }
            }
            Call::Setter(_) => {}
        }
    }

    fn on_error(&mut self, call: Call, error: &Value) -> Result<(), String> {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let text = str_of(error, "message").unwrap_or_else(|| "unknown error".into());
        match call {
            Call::Initialize => Err(format!("{} rejected the handshake: {text}", self.name)),
            Call::Authenticate(_) => Err(format!("{} couldn't sign in: {text}", self.name)),
            Call::Open(open) if code == AUTH_REQUIRED => self.authenticate(open),
            Call::Open(Open::New) => Err(format!("{} couldn't start a session: {text}", self.name)),
            Call::Open(_) => {
                self.emit(AgentEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("{} couldn't reopen the earlier session ({text}), so this one starts fresh.", self.name),
                });
                self.open(Open::New);
                Ok(())
            }
            Call::Prompt => {
                let cancelled = self.turn.as_ref().is_some_and(|t| t.cancel_deadline.is_some());
                let outcome = if cancelled { TurnOutcome::Interrupted } else { TurnOutcome::Failed { message: text } };
                self.finish_turn(outcome, None);
                Ok(())
            }
            Call::SetOption(label) => {
                self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Couldn't change {label}: {text}") });
                Ok(())
            }
            Call::Setter(label) => {
                if code != METHOD_NOT_FOUND {
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Couldn't change {label}: {text}") });
                }
                Ok(())
            }
        }
    }

    fn on_request(&mut self, method: &str, raw_id: &str, params: &Value) {
        if method != "session/request_permission" {
            self.pipe.respond_error(raw_id, METHOD_NOT_FOUND, &format!("Orbit doesn't offer {method}"));
            return;
        }
        let Some(update) = params.get("toolCall") else {
            self.answer(raw_id, None);
            return;
        };
        let mut tool = Tool::default();
        tool.merge(update);
        if let Some(known) = self.tools.get(&tool.id) {
            // Some agents (Kiro) omit the kind here; the tool call has it.
            let mut merged = known.clone();
            merged.merge(update);
            tool = merged;
        }
        let options: Vec<(String, String)> = map::array(params, "options")
            .filter_map(|o| Some((str_of(o, "optionId")?, str_of(o, "kind").unwrap_or_default())))
            .collect();
        let grant_key = format!("{}\u{1f}{}", tool.kind, tool.request_kind(&self.cwd()).summary());
        let cancelling = self.turn.as_ref().is_some_and(|t| t.cancel_deadline.is_some());
        if cancelling {
            self.answer(raw_id, None);
            return;
        }
        let decision = if self.grants.contains(&grant_key) {
            Some(Decision::Allow)
        } else {
            auto_decision(self.spec.config.access, &tool.kind)
        };
        if let Some(decision) = decision {
            let option = map::pick_option(&options, &decision);
            self.answer(raw_id, option.as_deref());
            return;
        }
        let provider_ref = format!("acp-{}", raw_id.trim_matches('"'));
        let kind = tool.request_kind(&self.cwd());
        self.permissions.insert(provider_ref.clone(), Permission { raw_id: raw_id.to_owned(), options, grant_key });
        self.emit(AgentEvent::Request { provider_ref, kind });
    }

    fn on_update(&mut self, params: &Value) {
        if self.replaying {
            return;
        }
        if let (Some(ours), Some(theirs)) = (&self.session_id, params.get("sessionId").and_then(|s| s.as_str()))
            && ours != theirs
        {
            return;
        }
        let Some(update) = params.get("update") else { return };
        let kind = str_of(update, "sessionUpdate").unwrap_or_default();
        match kind.as_str() {
            "agent_message_chunk" | "agent_thought_chunk" => {
                let Some(text) = update.get("content").and_then(map::content_text) else { return };
                let message_id = str_of(update, "messageId");
                if message_id.is_some() && message_id != self.message_id {
                    self.close_blocks();
                    self.message_id = message_id;
                }
                let reasoning = kind == "agent_thought_chunk";
                self.stream(reasoning, text);
            }
            "tool_call" | "tool_call_update" => {
                self.close_blocks();
                let Some(id) = str_of(update, "toolCallId") else { return };
                let tool = self.tools.entry(id.clone()).or_default();
                tool.merge(update);
                let tool = tool.clone();
                if tool.finished() {
                    if self.tools_done.insert(id.clone()) {
                        // The final title and files, then the result.
                        self.emit(AgentEvent::ToolStarted(tool.call()));
                        self.emit(AgentEvent::ToolFinished {
                            call_id: id,
                            output: tool.output(),
                            is_error: tool.status == "failed",
                            exit_code: None,
                            files: tool.files(),
                        });
                    }
                } else {
                    self.emit(AgentEvent::ToolStarted(tool.call()));
                }
            }
            "plan" => self.emit(AgentEvent::Plan(map::plan(update))),
            "available_commands_update" => {
                self.commands = map::commands(update);
                self.emit_catalog();
            }
            "current_mode_update" => {
                if let (Some(modes), Some(id)) = (self.config.modes.as_mut(), str_of(update, "currentModeId")) {
                    modes.current = id;
                }
            }
            "config_option_update" => {
                if let Some(options) = update.get("configOptions") {
                    self.config.apply_options(options);
                    self.emit_catalog();
                }
            }
            "usage_update" => {
                let live = map::usage_update(update);
                let usage = match self.turn.as_mut() {
                    Some(turn) => {
                        turn.usage.context_tokens = live.context_tokens;
                        turn.usage.context_window = live.context_window;
                        if live.cost_micros > 0 {
                            turn.usage.cost_micros = live.cost_micros;
                        }
                        turn.usage
                    }
                    None => live,
                };
                self.emit(AgentEvent::Usage(usage));
            }
            "session_info_update" => {
                if let Some(title) = str_of(update, "title").filter(|t| !t.trim().is_empty()) {
                    self.emit(AgentEvent::TitleChanged(title));
                }
            }
            _ => {}
        }
    }

    fn stream(&mut self, reasoning: bool, text: String) {
        // Reasoning and text alternate as separate blocks, in order.
        let other = if reasoning { self.text.take() } else { self.thought.take() };
        if let Some((block, done)) = other {
            let kind = if reasoning { BlockKind::Text } else { BlockKind::Reasoning };
            self.emit(AgentEvent::BlockDone { block, kind, text: done });
        }
        let slot = if reasoning { &mut self.thought } else { &mut self.text };
        if slot.is_none() {
            self.blocks += 1;
            *slot = Some((format!("acp-{}-{}", if reasoning { "r" } else { "t" }, self.blocks), String::new()));
        }
        let (block, acc) = slot.as_mut().unwrap();
        acc.push_str(&text);
        let block = block.clone();
        let kind = if reasoning { BlockKind::Reasoning } else { BlockKind::Text };
        self.emit(AgentEvent::Delta { block, kind, text });
    }

    fn close_blocks(&mut self) {
        if let Some((block, text)) = self.thought.take() {
            self.emit(AgentEvent::BlockDone { block, kind: BlockKind::Reasoning, text });
        }
        if let Some((block, text)) = self.text.take() {
            self.emit(AgentEvent::BlockDone { block, kind: BlockKind::Text, text });
        }
    }

    fn finish_turn(&mut self, outcome: TurnOutcome, usage: Option<Usage>) {
        let Some(turn) = self.turn.take() else { return };
        self.close_blocks();
        self.withdraw_permissions();
        let mut total = turn.usage;
        if let Some(usage) = usage {
            total.input_tokens = usage.input_tokens;
            total.output_tokens = usage.output_tokens;
            total.cache_read_tokens = usage.cache_read_tokens;
            total.cache_write_tokens = usage.cache_write_tokens;
        }
        let duration_ms = turn.started.elapsed().as_millis() as u64;
        self.emit(AgentEvent::TurnFinished { outcome, usage: total, duration_ms });
        self.start_next();
    }

    /// The process is gone: close the running turn with the reason.
    fn fail_turn(&mut self, reason: &str) {
        let cancelled = self.turn.as_ref().is_some_and(|t| t.cancel_deadline.is_some());
        let outcome = if cancelled { TurnOutcome::Interrupted } else { TurnOutcome::Failed { message: reason.to_owned() } };
        self.queue.clear();
        self.finish_turn(outcome, None);
    }
}

/// Requests the thread's access level answers without asking the user.
fn auto_decision(access: Access, kind: &str) -> Option<Decision> {
    let reads = matches!(kind, "read" | "search" | "think");
    let edits = matches!(kind, "edit" | "delete" | "move");
    match access {
        Access::Full => Some(Decision::Allow),
        Access::AutoEdit if reads || edits => Some(Decision::Allow),
        Access::Readonly if edits || kind == "execute" => Some(Decision::Deny { feedback: String::new() }),
        _ if reads => Some(Decision::Allow),
        _ => None,
    }
}

/// A `file://` URI for a local path, on every platform.
fn file_uri(path: &std::path::Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") }
}

trait Summary {
    fn summary(&self) -> String;
}

impl Summary for RequestKind {
    fn summary(&self) -> String {
        match self {
            RequestKind::Command { command, .. } => command.clone(),
            RequestKind::FileChange { path, .. } => path.clone(),
            RequestKind::Tool { name, input } => format!("{name}{input}"),
            RequestKind::Question { question, .. } => question.clone(),
            RequestKind::Plan { plan } => plan.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_answer_what_they_cover() {
        assert_eq!(auto_decision(Access::Ask, "execute"), None);
        assert_eq!(auto_decision(Access::Ask, "read"), Some(Decision::Allow));
        assert_eq!(auto_decision(Access::AutoEdit, "edit"), Some(Decision::Allow));
        assert_eq!(auto_decision(Access::AutoEdit, "execute"), None);
        assert!(matches!(auto_decision(Access::Readonly, "edit"), Some(Decision::Deny { .. })));
        assert_eq!(auto_decision(Access::Full, "execute"), Some(Decision::Allow));
    }

    #[test]
    fn file_uris_are_absolute() {
        assert_eq!(file_uri(std::path::Path::new("/home/u/a.png")), "file:///home/u/a.png");
        assert_eq!(file_uri(std::path::Path::new(r"C:\Users\u\a.png")), "file:///C:/Users/u/a.png");
    }
}
