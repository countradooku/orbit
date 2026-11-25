//! Pi adapter: one `pi --mode rpc` process per thread session, JSON lines
//! on stdio (commands `{"type": …, "id": …}`, responses
//! `{"type": "response", "id", "command", "success", "data" | "error"}`,
//! and events).
//!
//! * The session id is Pi's session file (from `get_state`); a thread
//!   resumes by starting Pi with `--session <file>`.
//! * Models come from `get_available_models` (`provider/id` slugs) and the
//!   thinking level is the model's one option, set live with `set_model` /
//!   `set_thinking_level`, never with a restart.
//! * Steering is native: a message sent during a turn goes out with
//!   `streamingBehavior: "steer"`.
//! * Stop is `abort`; the turn stays open until Pi reports it ended
//!   (`agent_end` / `agent_settled`), and Pi is killed only if it never does.
//! * Pi has no approval protocol of its own; its extensions can ask through
//!   `extension_ui_request`, which becomes an ordinary request card. A thread
//!   that is not on full access is told once that Pi runs tools without
//!   asking.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use orbit_proto::*;
use sonic_rs::{JsonValueMutTrait, JsonValueTrait, Value, json};
use tokio::time::Instant;

use super::acp::map::{array, str_of, u64_of};
use super::jsonrpc::{Pipe, common_install_dirs, resolve_command};
use super::{AgentEvent, BlockKind, EventSender, PromptAttachment, SessionHandle, SessionInput, SessionSpec, TurnOutcome};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const ABORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Pi's thinking levels, lowest first.
const THINKING: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];
pub const OPT_THINKING: &str = "thinking";

pub type Inputs = crossfire::AsyncRx<crossfire::mpsc::Array<SessionInput>>;

pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    let (input, rx) = crossfire::mpsc::bounded_blocking_async(64);
    tokio::spawn(run(spec, events, rx));
    SessionHandle { input }
}

fn resolve(binary: Option<&Path>) -> Option<PathBuf> {
    resolve_command(binary, "pi", &common_install_dirs())
}

fn not_found(binary: Option<&Path>) -> String {
    match binary {
        Some(path) => format!("Pi was not found at {}. Check the path in Settings.", path.display()),
        None => "Pi is not installed. Install it with `npm install -g @mariozechner/pi-coding-agent`.".to_owned(),
    }
}

fn command(kind: &str, id: u64, mut fields: Value) -> String {
    if let Some(object) = fields.as_object_mut() {
        object.insert("type", Value::from(kind));
        object.insert("id", Value::from(format!("o-{id}").as_str()));
    }
    sonic_rs::to_string(&fields).unwrap_or_default()
}

/// Splits a `provider/id` slug. Model ids can contain `/` themselves, so
/// the provider is the part before the first one.
fn split_slug(slug: &str) -> Option<(&str, &str)> {
    slug.split_once('/')
}

fn models(data: &Value) -> Vec<ModelInfo> {
    let option = OptionDescriptor {
        id: OPT_THINKING.into(),
        label: "Thinking".into(),
        description: None,
        kind: OptionKind::Select {
            choices: THINKING
                .iter()
                .map(|l| OptionChoice { id: (*l).into(), label: humanize_id(l), description: None, is_default: *l == "medium" })
                .collect(),
        },
    };
    array(data, "models")
        .filter_map(|m| {
            let provider = str_of(m, "provider")?;
            let id = str_of(m, "id")?;
            let reasoning = m.get("reasoning").and_then(|r| r.as_bool()).unwrap_or(true);
            Some(ModelInfo {
                slug: format!("{provider}/{id}"),
                name: str_of(m, "name").unwrap_or_else(|| id.clone()),
                description: Some(provider),
                context_window: u64_of(m, "contextWindow"),
                is_default: false,
                options: if reasoning { vec![option.clone()] } else { Vec::new() },
            })
        })
        .collect()
}

/// Detects Pi and lists its models, without creating a session.
pub async fn probe(binary: Option<PathBuf>, args: &[String], env: &[(String, String)]) -> ProviderInfo {
    let base = ProviderInfo { kind: ProviderKind::Pi, binary: binary.as_ref().map(|b| b.display().to_string()), ..Default::default() };
    let Some(path) = resolve(binary.as_deref()) else {
        return ProviderInfo { message: Some(not_found(binary.as_deref())), ..base };
    };
    let base = ProviderInfo { installed: true, binary: Some(path.display().to_string()), ..base };
    let mut argv = vec!["--mode".to_owned(), "rpc".to_owned(), "--no-session".to_owned()];
    argv.extend(args.iter().cloned());
    let (mut pipe, mut lines) = match Pipe::spawn("Pi", &path, &argv, &std::env::temp_dir(), env, false) {
        Ok(p) => p,
        Err(err) => return ProviderInfo { message: Some(format!("Couldn't start Pi: {err}")), ..base },
    };
    let models_id = pipe.next_id();
    pipe.send_raw(command("get_available_models", models_id, json!({})));
    let state_id = pipe.next_id();
    pipe.send_raw(command("get_state", state_id, json!({})));
    let exchange = async {
        let (mut models_data, mut state) = (None, None);
        while let Some(line) = lines.recv().await {
            let Ok(message) = sonic_rs::from_slice::<Value>(&line) else { continue };
            if str_of(&message, "type").as_deref() != Some("response") {
                continue;
            }
            let data = message.get("data").cloned().unwrap_or_default();
            match str_of(&message, "id") {
                Some(id) if id == format!("o-{models_id}") => models_data = Some(data),
                Some(id) if id == format!("o-{state_id}") => state = Some(data),
                _ => {}
            }
            if models_data.is_some() && state.is_some() {
                break;
            }
        }
        (models_data, state)
    };
    match tokio::time::timeout(PROBE_TIMEOUT, exchange).await {
        Ok((Some(data), state)) => {
            let mut list = models(&data);
            let current = state.as_ref().and_then(|s| s.get("model")).and_then(|m| {
                Some(format!("{}/{}", str_of(m, "provider")?, str_of(m, "id")?))
            });
            for model in &mut list {
                model.is_default = Some(&model.slug) == current.as_ref();
            }
            let message = list.is_empty().then(|| "Pi has no models yet. Sign in with `pi` in a terminal, then refresh.".to_owned());
            ProviderInfo { signed_in: Some(!list.is_empty()), models: list, message, ..base }
        }
        Ok((None, _)) => {
            let reason = pipe.exit_reason().await;
            ProviderInfo { message: Some(reason), ..base }
        }
        Err(_) => ProviderInfo { message: Some(format!("Pi didn't answer within {} seconds.", PROBE_TIMEOUT.as_secs())), ..base },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Call {
    State,
    Models,
    Prompt,
    Steer,
    Setter,
    Stats,
    Abort,
}

struct Turn {
    started: Instant,
    usage: Usage,
    abort_deadline: Option<Instant>,
    failure: Option<String>,
}

#[derive(Default)]
struct ToolRun {
    name: String,
    args: Value,
    output: String,
}

struct Session {
    spec: SessionSpec,
    events: EventSender,
    pipe: Pipe,
    calls: HashMap<String, (Call, Option<String>)>,
    session_file: Option<String>,
    ready: bool,
    models: Vec<ModelInfo>,
    model: Option<String>,
    thinking: Option<String>,
    queue: VecDeque<(String, String, Vec<PromptAttachment>)>,
    turn: Option<Turn>,
    /// Assistant messages seen this session, to keep block ids unique.
    message_seq: u64,
    /// Open blocks by content index: (block id, kind, text).
    blocks: HashMap<u64, (String, BlockKind, String)>,
    tools: HashMap<String, ToolRun>,
    /// Pending extension questions: provider_ref → (pi id, method).
    asks: HashMap<String, (String, String)>,
    warned: bool,
}

pub async fn run(spec: SessionSpec, events: EventSender, inputs: Inputs) {
    let emit = |events: &EventSender, thread, event| {
        use crossfire::BlockingTxTrait;
        let _ = events.send((thread, event));
    };
    let Some(binary) = resolve(spec.binary.as_deref()) else {
        emit(&events, spec.thread, AgentEvent::Exited { error: Some(not_found(spec.binary.as_deref())) });
        return;
    };
    let mut argv = vec!["--mode".to_owned(), "rpc".to_owned()];
    let mut resumed = false;
    if let Some(file) = spec.resume.as_ref().filter(|f| Path::new(f).is_file()) {
        argv.extend(["--session".to_owned(), file.clone()]);
        resumed = true;
    }
    argv.extend(spec.args.iter().cloned());
    let (pipe, mut lines) = match Pipe::spawn("Pi", &binary, &argv, &spec.cwd, &spec.env, false) {
        Ok(p) => p,
        Err(err) => {
            emit(&events, spec.thread, AgentEvent::Exited { error: Some(format!("Couldn't start Pi: {err}")) });
            return;
        }
    };
    let mut session = Session {
        spec,
        events,
        pipe,
        calls: HashMap::new(),
        session_file: None,
        ready: false,
        models: Vec::new(),
        model: None,
        thinking: None,
        queue: VecDeque::new(),
        turn: None,
        message_seq: 0,
        blocks: HashMap::new(),
        tools: HashMap::new(),
        asks: HashMap::new(),
        warned: false,
    };
    if session.spec.resume.is_some() && !resumed {
        session.emit(AgentEvent::Notice {
            level: NoticeLevel::Info,
            text: "Pi's earlier session file is gone, so this session starts fresh.".into(),
        });
    }
    session.call("get_available_models", json!({}), Call::Models, None);
    session.call("get_state", json!({}), Call::State, None);
    let init_deadline = Instant::now() + INIT_TIMEOUT;
    let error = loop {
        let deadline = if !session.ready { Some(init_deadline) } else { session.turn.as_ref().and_then(|t| t.abort_deadline) };
        tokio::select! {
            input = inputs.recv() => match input {
                Ok(SessionInput::Shutdown) | Err(_) => break None,
                Ok(input) => session.on_input(input),
            },
            line = lines.recv() => match line {
                Some(line) => session.on_line(&line),
                None => {
                    let reason = session.pipe.exit_reason().await;
                    session.finish(TurnOutcome::Failed { message: reason.clone() });
                    break Some(reason);
                }
            },
            _ = async { match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                if !session.ready {
                    break Some(format!("Pi didn't start within {} seconds", INIT_TIMEOUT.as_secs()));
                }
                session.finish(TurnOutcome::Interrupted);
                session.pipe.process.kill();
                break None;
            }
        }
    };
    session.emit(AgentEvent::Exited { error });
}

impl Session {
    fn emit(&self, event: AgentEvent) {
        use crossfire::BlockingTxTrait;
        let _ = self.events.send((self.spec.thread, event));
    }

    fn call(&mut self, kind: &str, fields: Value, call: Call, uuid: Option<String>) {
        let id = self.pipe.next_id();
        self.calls.insert(format!("o-{id}"), (call, uuid));
        self.pipe.send_raw(command(kind, id, fields));
    }

    fn on_input(&mut self, input: SessionInput) {
        match input {
            SessionInput::Prompt { uuid, text, attachments, steer } => {
                if self.ready && self.turn.is_some() && steer {
                    let mut fields = prompt_fields(&text, &attachments);
                    fields.as_object_mut().unwrap().insert("streamingBehavior", Value::from("steer"));
                    self.call("prompt", fields, Call::Steer, Some(uuid));
                } else {
                    self.queue.push_back((uuid, text, attachments));
                    self.start_next();
                }
            }
            SessionInput::Interrupt => {
                if let Some(turn) = self.turn.as_mut()
                    && turn.abort_deadline.is_none()
                {
                    turn.abort_deadline = Some(Instant::now() + ABORT_TIMEOUT);
                    self.call("abort", json!({}), Call::Abort, None);
                    self.withdraw_asks();
                }
            }
            SessionInput::Resolve { provider_ref, decision } => {
                let Some((id, method)) = self.asks.remove(&provider_ref) else { return };
                let response = match (method.as_str(), decision) {
                    ("confirm", Decision::Allow | Decision::AllowForSession) => json!({"confirmed": true}),
                    ("confirm", _) => json!({"confirmed": false}),
                    (_, Decision::Answer { text }) => json!({"value": text}),
                    (_, Decision::Allow | Decision::AllowForSession) => json!({"confirmed": true}),
                    (_, Decision::Deny { .. }) => json!({"cancelled": true}),
                };
                self.ui_response(&id, response);
            }
            SessionInput::Configure(config) => {
                self.spec.config = config;
                self.apply_config();
            }
            SessionInput::Shutdown => {}
        }
    }

    fn withdraw_asks(&mut self) {
        for (provider_ref, (id, _)) in std::mem::take(&mut self.asks) {
            self.ui_response(&id, json!({"cancelled": true}));
            self.emit(AgentEvent::RequestWithdrawn { provider_ref });
        }
    }

    /// Answers an extension's question; `id` is the question's own id.
    fn ui_response(&self, id: &str, mut fields: Value) {
        if let Some(object) = fields.as_object_mut() {
            object.insert("type", Value::from("extension_ui_response"));
            object.insert("id", Value::from(id));
        }
        self.pipe.send_raw(sonic_rs::to_string(&fields).unwrap_or_default());
    }

    fn apply_config(&mut self) {
        if !self.ready {
            return;
        }
        let config = self.spec.config.clone();
        if !config.model.is_empty()
            && self.model.as_deref() != Some(config.model.as_str())
            && let Some((provider, id)) = split_slug(&config.model)
        {
            self.call("set_model", json!({"provider": provider, "modelId": id}), Call::Setter, None);
            self.model = Some(config.model.clone());
            // Pi resets the thinking level with the model.
            self.thinking = None;
        }
        let wanted = config.options.iter().find(|o| o.id == OPT_THINKING).and_then(|o| match &o.value {
            OptionSetting::Choice(level) => Some(level.clone()),
            OptionSetting::Toggle(_) => None,
        });
        if let Some(level) = wanted
            && self.thinking.as_ref() != Some(&level)
        {
            self.call("set_thinking_level", json!({"level": level}), Call::Setter, None);
            self.thinking = Some(level);
        }
    }

    fn start_next(&mut self) {
        if !self.ready || self.turn.is_some() {
            return;
        }
        let Some((uuid, text, attachments)) = self.queue.pop_front() else { return };
        if !self.warned && self.spec.config.access != Access::Full {
            self.warned = true;
            self.emit(AgentEvent::Notice {
                level: NoticeLevel::Info,
                text: "Pi runs its tools without asking first; its extensions can still ask you questions.".into(),
            });
        }
        self.call("prompt", prompt_fields(&text, &attachments), Call::Prompt, Some(uuid.clone()));
        self.turn = Some(Turn { started: Instant::now(), usage: Usage::default(), abort_deadline: None, failure: None });
        self.emit(AgentEvent::TurnStarted { prompt: Some(uuid) });
    }

    fn finish(&mut self, outcome: TurnOutcome) {
        let Some(turn) = self.turn.take() else { return };
        for (_, (block, kind, text)) in std::mem::take(&mut self.blocks) {
            self.emit(AgentEvent::BlockDone { block, kind, text });
        }
        self.withdraw_asks();
        let outcome = match (outcome, turn.failure) {
            (TurnOutcome::Completed, Some(message)) => TurnOutcome::Failed { message },
            (outcome, _) => outcome,
        };
        self.emit(AgentEvent::TurnFinished { outcome, usage: turn.usage, duration_ms: turn.started.elapsed().as_millis() as u64 });
        // The context fill after the turn.
        self.call("get_session_stats", json!({}), Call::Stats, None);
        self.start_next();
    }

    fn on_line(&mut self, line: &[u8]) {
        let Ok(message) = sonic_rs::from_slice::<Value>(line) else { return };
        let kind = str_of(&message, "type").unwrap_or_default();
        match kind.as_str() {
            "response" => self.on_response(&message),
            "message_update" => self.on_message_update(&message),
            "message_end" => {
                let Some(msg) = message.get("message") else { return };
                if str_of(msg, "role").as_deref() != Some("assistant") {
                    return;
                }
                if let Some(usage) = msg.get("usage") {
                    self.add_usage(usage);
                }
                if str_of(msg, "stopReason").as_deref() == Some("error")
                    && let Some(turn) = self.turn.as_mut()
                {
                    turn.failure = Some(str_of(msg, "errorMessage").unwrap_or_else(|| "Pi reported an error.".into()));
                }
                for (_, (block, kind, text)) in std::mem::take(&mut self.blocks) {
                    self.emit(AgentEvent::BlockDone { block, kind, text });
                }
            }
            "message_start" => {
                if message.get("message").and_then(|m| str_of(m, "role")).as_deref() == Some("assistant") {
                    self.message_seq += 1;
                }
            }
            "tool_execution_start" => {
                let Some(id) = str_of(&message, "toolCallId") else { return };
                let name = str_of(&message, "toolName").unwrap_or_default();
                let args = message.get("args").cloned().unwrap_or_default();
                let call = tool_call(&id, &name, &args);
                self.tools.insert(id, ToolRun { name, args, output: String::new() });
                self.emit(AgentEvent::ToolStarted(call));
            }
            "tool_execution_update" => {
                let Some(id) = str_of(&message, "toolCallId") else { return };
                let text = message.get("partialResult").map(result_text).unwrap_or_default();
                let Some(run) = self.tools.get_mut(&id) else { return };
                // Pi resends the whole output so far; forward what is new.
                let delta = match text.strip_prefix(run.output.as_str()) {
                    Some(rest) => rest.to_owned(),
                    None => text.clone(),
                };
                run.output = text;
                if !delta.is_empty() {
                    self.emit(AgentEvent::ToolOutput { call_id: id, text: delta });
                }
            }
            "tool_execution_end" => {
                let Some(id) = str_of(&message, "toolCallId") else { return };
                let run = self.tools.remove(&id).unwrap_or_default();
                let output = message.get("result").map(result_text).unwrap_or(run.output);
                let is_error = message.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
                let files = if is_error { Vec::new() } else { tool_call(&id, &run.name, &run.args).files };
                self.emit(AgentEvent::ToolFinished { call_id: id, output, is_error, exit_code: None, files });
            }
            "agent_end" | "agent_settled" => {
                if self.turn.is_some() {
                    let aborted = message.get("aborted").and_then(|a| a.as_bool()).unwrap_or(false)
                        || self.turn.as_ref().is_some_and(|t| t.abort_deadline.is_some());
                    self.finish(if aborted { TurnOutcome::Interrupted } else { TurnOutcome::Completed });
                }
            }
            "auto_compaction_start" | "compaction_start" => self.emit(AgentEvent::Notice {
                level: NoticeLevel::Info,
                text: "Pi is compacting the conversation to fit its context window.".into(),
            }),
            "auto_retry_start" => {
                let error = str_of(&message, "errorMessage").unwrap_or_default();
                self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Pi is retrying after an error: {error}") });
            }
            "extension_ui_request" => self.on_ui_request(&message),
            "extension_error" | "hook_error" => {
                let error = str_of(&message, "error").or_else(|| str_of(&message, "message")).unwrap_or_default();
                self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("A Pi extension failed: {error}") });
            }
            _ => {}
        }
    }

    fn on_ui_request(&mut self, message: &Value) {
        let Some(id) = str_of(message, "id") else { return };
        let method = str_of(message, "method").unwrap_or_default();
        let title = str_of(message, "title").unwrap_or_default();
        let body = str_of(message, "message").unwrap_or_default();
        let text = [title.as_str(), body.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n\n");
        let kind = match method.as_str() {
            "notify" => {
                self.emit(AgentEvent::Notice { level: NoticeLevel::Info, text });
                return;
            }
            "setWidget" | "setStatus" | "setTitle" => return,
            "confirm" => RequestKind::Tool { name: if title.is_empty() { "Pi asks".into() } else { title.clone() }, input: body },
            "select" => RequestKind::Question {
                question: text,
                options: array(message, "options").filter_map(|o| o.as_str().map(str::to_owned).or_else(|| str_of(o, "label"))).map(QuestionOption::from).collect(),
                multi_select: false,
            },
            _ => RequestKind::Question { question: text, options: Vec::new(), multi_select: false },
        };
        let provider_ref = format!("pi-{id}");
        self.asks.insert(provider_ref.clone(), (id, method));
        self.emit(AgentEvent::Request { provider_ref, kind });
    }

    fn on_message_update(&mut self, message: &Value) {
        if let Some(usage) = message.get("usage").or_else(|| message.get("message").and_then(|m| m.get("usage"))) {
            let live = live_usage(usage);
            if let Some(turn) = self.turn.as_mut() {
                turn.usage.context_tokens = live.context_tokens;
                let usage = turn.usage;
                self.emit(AgentEvent::Usage(usage));
            }
        }
        let Some(event) = message.get("assistantMessageEvent") else { return };
        let index = u64_of(event, "contentIndex");
        let kind = str_of(event, "type").unwrap_or_default();
        let block_kind = if kind.starts_with("thinking") { BlockKind::Reasoning } else { BlockKind::Text };
        match kind.as_str() {
            "text_delta" | "thinking_delta" => {
                let Some(delta) = str_of(event, "delta") else { return };
                let seq = self.message_seq;
                let entry = self
                    .blocks
                    .entry(index)
                    .or_insert_with(|| (format!("pi-{seq}-{index}"), block_kind, String::new()));
                entry.2.push_str(&delta);
                let block = entry.0.clone();
                self.emit(AgentEvent::Delta { block, kind: block_kind, text: delta });
            }
            "text_end" | "thinking_end" => {
                let Some((block, kind, text)) = self.blocks.remove(&index) else { return };
                let text = str_of(event, "content").unwrap_or(text);
                self.emit(AgentEvent::BlockDone { block, kind, text });
            }
            _ => {}
        }
    }

    fn add_usage(&mut self, usage: &Value) {
        let Some(turn) = self.turn.as_mut() else { return };
        turn.usage.input_tokens += u64_of(usage, "input");
        turn.usage.output_tokens += u64_of(usage, "output");
        turn.usage.cache_read_tokens += u64_of(usage, "cacheRead");
        turn.usage.cache_write_tokens += u64_of(usage, "cacheWrite");
        if let Some(total) = usage.get("cost").and_then(|c| c.get("total")).and_then(|t| t.as_f64()) {
            turn.usage.cost_micros += (total * 1_000_000.0).round().max(0.0) as u64;
        }
        turn.usage.context_tokens = live_usage(usage).context_tokens;
    }

    fn on_response(&mut self, message: &Value) {
        let Some((call, uuid)) = str_of(message, "id").and_then(|id| self.calls.remove(&id)) else { return };
        let success = message.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
        let data = message.get("data").cloned().unwrap_or_default();
        let error = str_of(message, "error").unwrap_or_else(|| "unknown error".into());
        match call {
            Call::Models => {
                if success {
                    self.models = models(&data);
                }
            }
            Call::State => {
                self.session_file = str_of(&data, "sessionFile");
                self.model = data.get("model").and_then(|m| Some(format!("{}/{}", str_of(m, "provider")?, str_of(m, "id")?)));
                self.thinking = str_of(&data, "thinkingLevel");
                let first = !self.ready;
                self.ready = true;
                for model in &mut self.models {
                    model.is_default = Some(&model.slug) == self.model.as_ref();
                }
                if first {
                    let session = self.session_file.clone().unwrap_or_default();
                    self.emit(AgentEvent::Ready { session_id: session, model: self.model.clone() });
                    self.emit(AgentEvent::Catalog(ProviderInfo {
                        kind: ProviderKind::Pi,
                        installed: true,
                        signed_in: Some(!self.models.is_empty()),
                        models: self.models.clone(),
                        ..Default::default()
                    }));
                    self.apply_config();
                    self.start_next();
                }
            }
            Call::Prompt if !success => self.finish(TurnOutcome::Failed { message: error }),
            Call::Steer => match (success, uuid) {
                (true, Some(uuid)) => self.emit(AgentEvent::PromptAccepted { uuid }),
                (false, Some(uuid)) => {
                    // Not steerable after all: run it next.
                    let _ = uuid;
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Pi couldn't take that message mid-turn: {error}") });
                }
                _ => {}
            },
            Call::Setter if !success => {
                self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Pi couldn't change the model: {error}") })
            }
            Call::Stats if success => {
                if let Some(context) = data.get("contextUsage") {
                    let usage = Usage {
                        context_tokens: u64_of(context, "tokens"),
                        context_window: u64_of(context, "contextWindow"),
                        ..Default::default()
                    };
                    if usage.context_tokens > 0 {
                        self.emit(AgentEvent::Usage(usage));
                    }
                }
                if self.session_file.is_none() {
                    self.call("get_state", json!({}), Call::State, None);
                }
            }
            _ => {}
        }
    }
}

fn live_usage(usage: &Value) -> Usage {
    let total = u64_of(usage, "totalTokens");
    let context = if total > 0 {
        total
    } else {
        u64_of(usage, "input") + u64_of(usage, "output") + u64_of(usage, "cacheRead") + u64_of(usage, "cacheWrite")
    };
    Usage { context_tokens: context, ..Default::default() }
}

fn prompt_fields(text: &str, attachments: &[PromptAttachment]) -> Value {
    let mut message = text.to_owned();
    let mut images = Vec::new();
    for a in attachments {
        if a.attachment.mime.starts_with("image/")
            && let Ok(bytes) = std::fs::read(&a.path)
        {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            images.push(json!({"type": "image", "data": data, "mimeType": a.attachment.mime}));
        } else {
            message.push_str(&format!("\n\nAttached file: {}", a.path.display()));
        }
    }
    if images.is_empty() { json!({"message": message}) } else { json!({"message": message, "images": images}) }
}

fn result_text(result: &Value) -> String {
    if let Some(text) = result.as_str() {
        return text.to_owned();
    }
    array(result, "content").filter_map(|c| str_of(c, "text")).collect::<Vec<_>>().join("\n")
}

fn tool_call(id: &str, name: &str, args: &Value) -> ToolCall {
    let path = str_of(args, "path").or_else(|| str_of(args, "file_path"));
    let (kind, title) = match name {
        "bash" | "shell" | "powershell" => (ToolKind::Command, str_of(args, "command").unwrap_or_else(|| name.into())),
        "read" => (ToolKind::Read, path.clone().unwrap_or_else(|| "Read".into())),
        "edit" | "write" => (ToolKind::Edit, path.clone().unwrap_or_else(|| humanize_id(name))),
        "grep" | "find" | "ls" | "glob" => (
            ToolKind::Search,
            str_of(args, "pattern").or_else(|| path.clone()).unwrap_or_else(|| humanize_id(name)),
        ),
        "subagent" | "task" => (ToolKind::Task, str_of(args, "task").or_else(|| str_of(args, "description")).unwrap_or_else(|| "Subagent".into())),
        _ => (ToolKind::Other, humanize_id(name)),
    };
    let files = match (name, &path) {
        ("edit", Some(p)) => vec![FileChange { path: p.clone(), kind: ChangeKind::Modified, additions: 0, deletions: 0 }],
        ("write", Some(p)) => vec![FileChange {
            path: p.clone(),
            kind: ChangeKind::Added,
            additions: str_of(args, "content").map(|c| c.lines().count() as u32).unwrap_or(0),
            deletions: 0,
        }],
        _ => Vec::new(),
    };
    ToolCall {
        call_id: id.to_owned(),
        kind,
        name: name.to_owned(),
        title,
        input: sonic_rs::to_string(args).unwrap_or_default(),
        output: String::new(),
        output_blob: None,
        exit_code: None,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_split_at_the_first_slash() {
        assert_eq!(split_slug("openrouter/anthropic/claude"), Some(("openrouter", "anthropic/claude")));
        assert_eq!(split_slug("default"), None);
    }

    #[test]
    fn tools_map_to_orbit_kinds() {
        let call = tool_call("t", "bash", &json!({"command": "cargo test"}));
        assert_eq!((call.kind, call.title.as_str()), (ToolKind::Command, "cargo test"));
        let call = tool_call("t", "write", &json!({"path": "a.rs", "content": "a\nb\n"}));
        assert_eq!(call.files[0].additions, 2);
        assert_eq!(result_text(&json!({"content": [{"type": "text", "text": "ok"}]})), "ok");
    }

    #[test]
    fn models_carry_thinking_levels() {
        let list = models(&json!({"models": [{"provider": "anthropic", "id": "claude-x", "contextWindow": 200000},
                                              {"provider": "openai", "id": "o", "reasoning": false}]}));
        assert_eq!(list[0].slug, "anthropic/claude-x");
        assert_eq!(list[0].options[0].id, OPT_THINKING);
        assert!(list[1].options.is_empty());
    }
}
