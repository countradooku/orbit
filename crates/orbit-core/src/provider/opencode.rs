//! OpenCode adapter: one `opencode serve` per thread session, driven over
//! its HTTP API with one server-sent-events stream.
//!
//! * The server listens on a free loopback port with a random password
//!   (`OPENCODE_SERVER_PASSWORD`, HTTP basic auth), so no other local
//!   process can drive it. It runs in the thread's worktree and dies with
//!   the session (process tree).
//! * The OpenCode session id is the thread's session; it is reopened after
//!   a restart if OpenCode still has it.
//! * Prompts go out with `prompt_async`; everything else arrives as events:
//!   text and reasoning parts (`message.part.updated` / `.delta`), tool
//!   parts with their state, `todo.updated`, permission requests, and
//!   `session.idle` / `session.error` to end the turn.
//! * Plan mode is OpenCode's own `plan` agent, Build its `build` agent
//!   (T3 Code issue #42213 asked for exactly that).
//! * Access levels become the session's permission rules (`edit`, `bash`,
//!   `webfetch`: allow / ask / deny), so OpenCode enforces them itself.
//! * Stop is `abort`; the turn stays open until OpenCode reports the
//!   session idle or aborted.

mod http;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::time::Instant;

use super::acp::map::{array, str_of, u64_of};
use super::jsonrpc::{common_install_dirs, resolve_command};
use super::process::AgentProcess;
use super::{AgentEvent, BlockKind, EventSender, PromptAttachment, SessionHandle, SessionInput, SessionSpec, TurnOutcome};
pub use http::{Client, SseParser};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const ABORT_TIMEOUT: Duration = Duration::from_secs(10);

pub type Inputs = crossfire::AsyncRx<crossfire::mpsc::Array<SessionInput>>;

pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    let (input, rx) = crossfire::mpsc::bounded_blocking_async(64);
    tokio::spawn(run(spec, events, rx));
    SessionHandle { input }
}

fn resolve(binary: Option<&Path>) -> Option<PathBuf> {
    resolve_command(binary, "opencode", &common_install_dirs())
}

fn not_found(binary: Option<&Path>) -> String {
    match binary {
        Some(path) => format!("OpenCode was not found at {}. Check the path in Settings.", path.display()),
        None => "OpenCode is not installed. Install it with `npm install -g opencode-ai`.".to_owned(),
    }
}

/// A running `opencode serve` and the client for it.
pub struct Server {
    pub process: AgentProcess,
    pub client: Client,
    pub version: Option<String>,
}

/// Starts `opencode serve` on a free loopback port and waits until it says
/// it is listening.
pub async fn serve(binary: &Path, cwd: &Path, env: &[(String, String)]) -> Result<Server, String> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("No free port for OpenCode: {e}"))?;
    let mut password = [0u8; 24];
    for (i, b) in uuid::Uuid::new_v4().as_bytes().iter().chain(uuid::Uuid::new_v4().as_bytes()).take(24).enumerate() {
        password[i] = *b;
    }
    let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(password);
    let mut env = env.to_vec();
    env.push(("OPENCODE_SERVER_PASSWORD".into(), password.clone()));
    let args = ["serve".to_owned(), "--hostname=127.0.0.1".to_owned(), format!("--port={port}")];
    let mut process = AgentProcess::spawn(binary, &args, cwd, &env, super::claude::HOST_ENV)
        .map_err(|e| format!("Couldn't start OpenCode: {e}"))?;
    let stdout = process.stdout.take().ok_or("OpenCode has no output")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let ready = async {
        while let Ok(Some(line)) = lines.next_line().await {
            log::debug!("opencode: {line}");
            if line.to_ascii_lowercase().contains("listening on") {
                return true;
            }
        }
        false
    };
    let ready = tokio::time::timeout(START_TIMEOUT, ready).await;
    // Keep draining stdout, or OpenCode blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    match ready {
        Ok(true) => {}
        Ok(false) => {
            let tail = process.stderr_tail();
            return Err(if tail.is_empty() { "OpenCode exited while starting".into() } else { format!("OpenCode exited while starting: {tail}") });
        }
        Err(_) => return Err(format!("OpenCode didn't start within {} seconds", START_TIMEOUT.as_secs())),
    }
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")));
    let client = Client { addr: format!("127.0.0.1:{port}"), auth: Some(auth) };
    let version = match client.request("GET", "/global/health", None).await {
        Ok(r) if r.ok() => sonic_rs::from_str::<Value>(&r.body).ok().and_then(|v| str_of(&v, "version")),
        _ => None,
    };
    Ok(Server { process, client, version })
}

/// Models from `GET /config/providers` (or `/provider`), with the configured
/// default from `GET /config`.
pub async fn models(client: &Client) -> Vec<ModelInfo> {
    let default = match client.request("GET", "/config", None).await {
        Ok(r) if r.ok() => sonic_rs::from_str::<Value>(&r.body).ok().and_then(|v| str_of(&v, "model")),
        _ => None,
    };
    let Ok(response) = client.request("GET", "/config/providers", None).await else { return Vec::new() };
    let Ok(value) = sonic_rs::from_str::<Value>(&response.body) else { return Vec::new() };
    let mut out = parse_models(&value);
    let default = default.or_else(|| {
        let defaults = value.get("default")?;
        let first = array(&value, "providers").next().or_else(|| array(&value, "all").next())?;
        let provider = str_of(first, "id")?;
        Some(format!("{provider}/{}", str_of(defaults, &provider)?))
    });
    for model in &mut out {
        model.is_default = Some(&model.slug) == default.as_ref();
    }
    out
}

fn parse_models(value: &Value) -> Vec<ModelInfo> {
    let providers = array(value, "providers").chain(array(value, "all"));
    let mut out = Vec::new();
    for provider in providers {
        let Some(pid) = str_of(provider, "id") else { continue };
        let pname = str_of(provider, "name").unwrap_or_else(|| pid.clone());
        let Some(models) = provider.get("models").and_then(|m| m.as_object()) else { continue };
        for (key, model) in models.iter() {
            let id = str_of(model, "id").unwrap_or_else(|| key.to_owned());
            out.push(ModelInfo {
                slug: format!("{pid}/{id}"),
                name: str_of(model, "name").unwrap_or_else(|| id.clone()),
                description: Some(pname.clone()),
                context_window: model.get("limit").map(|l| u64_of(l, "context")).unwrap_or(0),
                is_default: false,
                options: Vec::new(),
            });
        }
    }
    out
}

fn modes() -> Vec<ModeInfo> {
    vec![
        ModeInfo { mode: Mode::Build, label: "Build".into(), description: Some("OpenCode's build agent".into()) },
        ModeInfo { mode: Mode::Plan, label: "Plan".into(), description: Some("OpenCode's plan agent: reads and proposes, no edits".into()) },
    ]
}

/// Detects OpenCode: starts a server in a scratch folder, reads its models
/// and stops it.
pub async fn probe(binary: Option<PathBuf>, env: &[(String, String)]) -> ProviderInfo {
    let base = ProviderInfo { kind: ProviderKind::OpenCode, binary: binary.as_ref().map(|b| b.display().to_string()), ..Default::default() };
    let Some(path) = resolve(binary.as_deref()) else {
        return ProviderInfo { message: Some(not_found(binary.as_deref())), ..base };
    };
    let base = ProviderInfo { installed: true, binary: Some(path.display().to_string()), ..base };
    let run = async {
        let server = serve(&path, &std::env::temp_dir(), env).await?;
        let models = models(&server.client).await;
        Ok::<_, String>((server.version.clone(), models))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Ok(Ok((version, models))) => {
            let message = models.is_empty().then(|| "OpenCode has no models yet. Run `opencode auth login` in a terminal.".to_owned());
            ProviderInfo { version, signed_in: Some(!models.is_empty()), models, modes: modes(), message, ..base }
        }
        Ok(Err(message)) => ProviderInfo { message: Some(message), ..base },
        Err(_) => ProviderInfo { message: Some(format!("OpenCode didn't answer within {} seconds.", PROBE_TIMEOUT.as_secs())), ..base },
    }
}

/// The session's permission rules for an access level.
pub fn permission_rules(access: Access) -> Value {
    let (edit, bash, web) = match access {
        Access::Readonly => ("deny", "ask", "ask"),
        Access::Ask => ("ask", "ask", "ask"),
        Access::AutoEdit => ("allow", "ask", "ask"),
        Access::Full => ("allow", "allow", "allow"),
    };
    json!([
        {"permission": "edit", "pattern": "*", "action": edit},
        {"permission": "bash", "pattern": "*", "action": bash},
        {"permission": "webfetch", "pattern": "*", "action": web}
    ])
}

/// [`permission_rules`] followed by an `ask` rule per guard pattern, for
/// commands and for edits (OpenCode lets the last matching rule win), so
/// even Full access asks before a guarded action.
pub fn guarded_rules(access: Access, guard: &GuardSettings) -> Value {
    let mut rules = permission_rules(access);
    if guard.active()
        && let Some(list) = rules.as_array_mut()
    {
        for rule in &guard.rules {
            let pattern = format!("*{}*", rule.pattern.trim());
            for permission in ["bash", "edit"] {
                list.push(json!({"permission": permission, "pattern": pattern.clone(), "action": "ask"}));
            }
        }
    }
    rules
}

/// An ascending OpenCode message id (`msg_` + time + random), so the
/// prompt's own parts can be told apart from the reply's.
fn message_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = now_ms().wrapping_mul(0x1000).wrapping_add(n & 0xfff);
    let random: String = uuid::Uuid::new_v4().simple().to_string().chars().take(14).collect();
    format!("msg_{:012x}{random}", stamp & 0xffff_ffff_ffff)
}

struct Turn {
    started: Instant,
    usage: Usage,
    abort_deadline: Option<Instant>,
    failure: Option<String>,
    prompt_message: String,
}

#[derive(Default)]
struct Part {
    kind: String,
    text: String,
    block: String,
    done: bool,
}

struct Session {
    spec: SessionSpec,
    events: EventSender,
    client: Client,
    session_id: String,
    queue: VecDeque<(String, String, Vec<PromptAttachment>)>,
    turn: Option<Turn>,
    /// Message id → role.
    roles: HashMap<String, String>,
    parts: HashMap<String, Part>,
    tools_done: HashSet<String>,
    /// Assistant messages whose usage was counted.
    counted: HashSet<String>,
    /// provider_ref → (request id, is question)
    asks: HashMap<String, (String, bool)>,
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
    let mut server = match serve(&binary, &spec.cwd, &spec.env).await {
        Ok(server) => server,
        Err(message) => {
            emit(&events, spec.thread, AgentEvent::Exited { error: Some(message) });
            return;
        }
    };
    let client = server.client.clone();
    let mut stream = match client.events("/event").await {
        Ok(stream) => stream,
        Err(err) => {
            emit(&events, spec.thread, AgentEvent::Exited { error: Some(format!("OpenCode's event stream failed: {err}")) });
            return;
        }
    };
    // Reopen the thread's session if OpenCode still has it.
    let mut session_id = None;
    if let Some(id) = &spec.resume {
        match client.request("GET", &format!("/session/{id}"), None).await {
            Ok(r) if r.ok() => session_id = Some(id.clone()),
            _ => emit(
                &events,
                spec.thread,
                AgentEvent::Notice { level: NoticeLevel::Info, text: "OpenCode no longer has the earlier session, so this one starts fresh.".into() },
            ),
        }
    }
    let session_id = match session_id {
        Some(id) => {
            let body = json!({"permission": guarded_rules(spec.config.access, &spec.guard)});
            let _ = client.request("PATCH", &format!("/session/{id}"), Some(&sonic_rs::to_string(&body).unwrap_or_default())).await;
            id
        }
        None => {
            let body = json!({"permission": guarded_rules(spec.config.access, &spec.guard)});
            match client.request("POST", "/session", Some(&sonic_rs::to_string(&body).unwrap_or_default())).await {
                Ok(r) if r.ok() => match sonic_rs::from_str::<Value>(&r.body).ok().and_then(|v| str_of(&v, "id")) {
                    Some(id) => id,
                    None => {
                        emit(&events, spec.thread, AgentEvent::Exited { error: Some("OpenCode returned no session id".into()) });
                        return;
                    }
                },
                Ok(r) => {
                    emit(&events, spec.thread, AgentEvent::Exited { error: Some(format!("OpenCode couldn't start a session ({}): {}", r.status, r.body)) });
                    return;
                }
                Err(err) => {
                    emit(&events, spec.thread, AgentEvent::Exited { error: Some(format!("OpenCode couldn't start a session: {err}")) });
                    return;
                }
            }
        }
    };
    let mut session = Session {
        spec,
        events,
        client: client.clone(),
        session_id: session_id.clone(),
        queue: VecDeque::new(),
        turn: None,
        roles: HashMap::new(),
        parts: HashMap::new(),
        tools_done: HashSet::new(),
        counted: HashSet::new(),
        asks: HashMap::new(),
    };
    session.emit(AgentEvent::Ready { session_id, model: (!session.spec.config.model.is_empty()).then(|| session.spec.config.model.clone()) });
    let catalog = models(&client).await;
    session.emit(AgentEvent::Catalog(ProviderInfo {
        kind: ProviderKind::OpenCode,
        installed: true,
        version: server.version.clone(),
        signed_in: Some(!catalog.is_empty()),
        models: catalog,
        modes: modes(),
        ..Default::default()
    }));
    let mut exit = server.process.exit_watch();
    let error = loop {
        let deadline = session.turn.as_ref().and_then(|t| t.abort_deadline);
        tokio::select! {
            input = inputs.recv() => match input {
                Ok(SessionInput::Shutdown) | Err(_) => break None,
                Ok(input) => session.on_input(input).await,
            },
            event = stream.recv() => match event {
                Some(data) => session.on_event(&data).await,
                None => {
                    let reason = ended(&mut server).await;
                    session.finish(TurnOutcome::Failed { message: reason.clone() }).await;
                    break Some(reason);
                }
            },
            // The server died but something it started keeps the event
            // stream's socket open: do not wait for the stream.
            _ = exit.drained() => {
                let reason = ended(&mut server).await;
                session.finish(TurnOutcome::Failed { message: reason.clone() }).await;
                break Some(reason);
            }
            _ = async { match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                session.finish(TurnOutcome::Interrupted).await;
                server.process.kill();
                break None;
            }
        }
    };
    session.emit(AgentEvent::Exited { error });
}

/// Why the server stopped: its exit status and the tail of stderr.
async fn ended(server: &mut Server) -> String {
    let status = tokio::time::timeout(Duration::from_millis(500), server.process.wait()).await;
    let tail = server.process.stderr_tail();
    let mut reason = match status {
        Ok(Ok(status)) => format!("OpenCode exited ({status})"),
        _ => "OpenCode's event stream ended".to_owned(),
    };
    if !tail.is_empty() {
        reason.push_str(": ");
        reason.push_str(&tail);
    }
    reason
}

impl Session {
    fn emit(&self, event: AgentEvent) {
        use crossfire::BlockingTxTrait;
        let _ = self.events.send((self.spec.thread, event));
    }

    async fn post(&self, path: &str, body: &Value) -> Result<http::Response, String> {
        let body = sonic_rs::to_string(body).unwrap_or_default();
        self.client.request("POST", path, Some(&body)).await.map_err(|e| e.to_string())
    }

    async fn on_input(&mut self, input: SessionInput) {
        match input {
            SessionInput::Prompt { uuid, text, attachments, .. } => {
                // OpenCode cannot steer: a steer runs as the next prompt.
                self.queue.push_back((uuid, text, attachments));
                self.start_next().await;
            }
            SessionInput::Interrupt => {
                if let Some(turn) = self.turn.as_mut()
                    && turn.abort_deadline.is_none()
                {
                    turn.abort_deadline = Some(Instant::now() + ABORT_TIMEOUT);
                    let path = format!("/session/{}/abort", self.session_id);
                    let _ = self.post(&path, &json!({})).await;
                }
            }
            SessionInput::Resolve { provider_ref, decision } => {
                let Some((id, question)) = self.asks.remove(&provider_ref) else { return };
                if question {
                    let body = match decision {
                        Decision::Answer { text } => json!({"answers": [text.lines().map(str::to_owned).collect::<Vec<_>>()]}),
                        _ => json!({"answers": []}),
                    };
                    let _ = self.post(&format!("/question/{id}/reply"), &body).await;
                    return;
                }
                let reply = match decision {
                    Decision::Allow | Decision::Answer { .. } => "once",
                    Decision::AllowForSession => "always",
                    Decision::Deny { .. } => "reject",
                };
                let replied = self.post(&format!("/permission/{id}/reply"), &json!({"reply": reply})).await;
                if !replied.is_ok_and(|r| r.ok()) {
                    // Older servers answer per session.
                    let path = format!("/session/{}/permissions/{id}", self.session_id);
                    let _ = self.post(&path, &json!({"response": reply})).await;
                }
            }
            SessionInput::Configure(config) => {
                if config.access != self.spec.config.access {
                    let body = sonic_rs::to_string(&json!({"permission": guarded_rules(config.access, &self.spec.guard)})).unwrap_or_default();
                    let _ = self.client.request("PATCH", &format!("/session/{}", self.session_id), Some(&body)).await;
                }
                // Model and agent go with each prompt.
                self.spec.config = config;
            }
            SessionInput::Shutdown => {}
        }
    }

    async fn start_next(&mut self) {
        if self.turn.is_some() {
            return;
        }
        let Some((uuid, text, attachments)) = self.queue.pop_front() else { return };
        let message = message_id();
        let mut parts = vec![json!({"type": "text", "text": text})];
        for a in &attachments {
            let url = format!("file:///{}", a.path.display().to_string().replace('\\', "/").trim_start_matches('/'));
            parts.push(json!({"type": "file", "mime": a.attachment.mime, "filename": a.attachment.name, "url": url}));
        }
        let config = &self.spec.config;
        let mut body = json!({
            "messageID": message,
            "agent": if config.mode == Mode::Plan { "plan" } else { "build" },
            "parts": parts
        });
        if let Some((provider, model)) = config.model.split_once('/') {
            body.as_object_mut().unwrap().insert("model", json!({"providerID": provider, "modelID": model}));
        }
        self.roles.insert(message.clone(), "user".into());
        self.turn = Some(Turn {
            started: Instant::now(),
            usage: Usage::default(),
            abort_deadline: None,
            failure: None,
            prompt_message: message,
        });
        self.emit(AgentEvent::TurnStarted { prompt: Some(uuid) });
        let path = format!("/session/{}/prompt_async", self.session_id);
        match self.post(&path, &body).await {
            Ok(r) if r.ok() => {}
            Ok(r) => self.finish(TurnOutcome::Failed { message: format!("OpenCode refused the message ({}): {}", r.status, r.body) }).await,
            Err(err) => self.finish(TurnOutcome::Failed { message: format!("OpenCode didn't take the message: {err}") }).await,
        }
    }

    async fn finish(&mut self, outcome: TurnOutcome) {
        let Some(turn) = self.turn.take() else { return };
        let mut parts: Vec<(String, Part)> = self.parts.drain().collect();
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, part) in parts {
            if !part.done && !part.block.is_empty() {
                let kind = if part.kind == "reasoning" { BlockKind::Reasoning } else { BlockKind::Text };
                self.emit(AgentEvent::BlockDone { block: part.block, kind, text: part.text });
            }
        }
        for (provider_ref, _) in std::mem::take(&mut self.asks) {
            self.emit(AgentEvent::RequestWithdrawn { provider_ref });
        }
        let outcome = match (outcome, turn.failure) {
            (TurnOutcome::Completed, Some(message)) => TurnOutcome::Failed { message },
            (outcome, _) => outcome,
        };
        let outcome = if turn.abort_deadline.is_some() && matches!(outcome, TurnOutcome::Completed | TurnOutcome::Failed { .. }) {
            TurnOutcome::Interrupted
        } else {
            outcome
        };
        self.emit(AgentEvent::TurnFinished { outcome, usage: turn.usage, duration_ms: turn.started.elapsed().as_millis() as u64 });
        self.tools_done.clear();
        Box::pin(self.start_next()).await;
    }

    fn ours(&self, props: &Value) -> bool {
        let session = props
            .get("sessionID")
            .or_else(|| props.get("info").and_then(|i| i.get("sessionID")))
            .or_else(|| props.get("part").and_then(|p| p.get("sessionID")))
            .and_then(|s| s.as_str());
        session.is_none_or(|s| s == self.session_id)
    }

    async fn on_event(&mut self, data: &str) {
        let Ok(event) = sonic_rs::from_str::<Value>(data) else { return };
        let kind = str_of(&event, "type").unwrap_or_default();
        let Some(props) = event.get("properties") else { return };
        if !self.ours(props) {
            return;
        }
        match kind.as_str() {
            "message.updated" => {
                let Some(info) = props.get("info") else { return };
                let (Some(id), Some(role)) = (str_of(info, "id"), str_of(info, "role")) else { return };
                self.roles.insert(id.clone(), role.clone());
                let completed = info.get("time").and_then(|t| t.get("completed")).is_some_and(|c| !c.is_null());
                if role == "assistant" {
                    if let Some(error) = info.get("error").filter(|e| !e.is_null()) {
                        self.on_error(error);
                    }
                    if completed && self.counted.insert(id) {
                        self.count_usage(info);
                    }
                }
            }
            "message.part.updated" => {
                let Some(part) = props.get("part") else { return };
                let delta = str_of(props, "delta");
                self.on_part(part, delta);
            }
            "message.part.delta" => {
                let (Some(part_id), Some(delta)) = (str_of(props, "partID"), str_of(props, "delta")) else { return };
                let message = str_of(props, "messageID").unwrap_or_default();
                if self.roles.get(&message).is_some_and(|r| r == "user") {
                    return;
                }
                if let Some(part) = self.parts.get_mut(&part_id) {
                    part.text.push_str(&delta);
                    let (block, kind) = (part.block.clone(), if part.kind == "reasoning" { BlockKind::Reasoning } else { BlockKind::Text });
                    self.emit(AgentEvent::Delta { block, kind, text: delta });
                }
            }
            "todo.updated" => {
                let steps = array(props, "todos")
                    .filter_map(|t| {
                        let state = match str_of(t, "status").as_deref() {
                            Some("completed") => StepState::Done,
                            Some("in_progress") => StepState::Active,
                            Some("cancelled") => return None,
                            _ => StepState::Pending,
                        };
                        Some(PlanStep { text: str_of(t, "content")?, state })
                    })
                    .collect();
                self.emit(AgentEvent::Plan(steps));
            }
            "permission.asked" | "permission.updated" => self.on_permission(props),
            "question.asked" => self.on_question(props),
            "permission.replied" | "question.replied" | "question.rejected" => {
                let Some(id) = str_of(props, "requestID").or_else(|| str_of(props, "permissionID")).or_else(|| str_of(props, "id")) else { return };
                let provider_ref = format!("oc-{id}");
                if self.asks.remove(&provider_ref).is_some() {
                    self.emit(AgentEvent::RequestWithdrawn { provider_ref });
                }
            }
            "session.idle" => self.finish(TurnOutcome::Completed).await,
            "session.status" => {
                if props.get("status").and_then(|s| str_of(s, "type")).as_deref() == Some("idle") {
                    self.finish(TurnOutcome::Completed).await;
                }
            }
            "session.error" => {
                if let Some(error) = props.get("error") {
                    self.on_error(error);
                }
            }
            "session.updated" => {
                if let Some(title) = props.get("info").and_then(|i| str_of(i, "title"))
                    && !title.is_empty()
                    && !title.starts_with("New session")
                {
                    self.emit(AgentEvent::TitleChanged(title));
                }
            }
            "session.compacted" => self.emit(AgentEvent::Notice {
                level: NoticeLevel::Info,
                text: "OpenCode compacted the conversation to fit its context window.".into(),
            }),
            _ => {}
        }
    }

    fn on_error(&mut self, error: &Value) {
        let name = str_of(error, "name").unwrap_or_default();
        let message = error
            .get("data")
            .and_then(|d| str_of(d, "message"))
            .or_else(|| str_of(error, "message"))
            .unwrap_or_else(|| name.clone());
        match name.as_str() {
            "MessageAbortedError" => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.abort_deadline.get_or_insert(Instant::now() + ABORT_TIMEOUT);
                }
            }
            // OpenCode compacts and retries by itself.
            "ContextOverflowError" => {}
            _ => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.failure = Some(message);
                }
            }
        }
    }

    fn count_usage(&mut self, info: &Value) {
        let Some(turn) = self.turn.as_mut() else { return };
        if let Some(tokens) = info.get("tokens") {
            let cache = tokens.get("cache");
            let read = cache.map(|c| u64_of(c, "read")).unwrap_or(0);
            let write = cache.map(|c| u64_of(c, "write")).unwrap_or(0);
            let (input, output) = (u64_of(tokens, "input"), u64_of(tokens, "output") + u64_of(tokens, "reasoning"));
            turn.usage.input_tokens += input;
            turn.usage.output_tokens += output;
            turn.usage.cache_read_tokens += read;
            turn.usage.cache_write_tokens += write;
            turn.usage.context_tokens = input + read + write + output;
        }
        if let Some(cost) = info.get("cost").and_then(|c| c.as_f64()) {
            turn.usage.cost_micros += (cost * 1_000_000.0).round().max(0.0) as u64;
        }
        let usage = turn.usage;
        self.emit(AgentEvent::Usage(usage));
    }

    fn on_part(&mut self, part: &Value, delta: Option<String>) {
        let (Some(id), Some(kind)) = (str_of(part, "id"), str_of(part, "type")) else { return };
        let message = str_of(part, "messageID").unwrap_or_default();
        let ours = self.turn.as_ref().is_some_and(|t| t.prompt_message == message);
        if ours || self.roles.get(&message).is_some_and(|r| r == "user") {
            return;
        }
        match kind.as_str() {
            "text" | "reasoning" => {
                if part.get("synthetic").and_then(|s| s.as_bool()).unwrap_or(false) {
                    return;
                }
                let text = str_of(part, "text").unwrap_or_default();
                let block_kind = if kind == "reasoning" { BlockKind::Reasoning } else { BlockKind::Text };
                let entry = self.parts.entry(id.clone()).or_insert_with(|| Part {
                    kind: kind.clone(),
                    block: format!("oc-{id}"),
                    ..Default::default()
                });
                let finished = part.get("time").and_then(|t| t.get("end")).is_some_and(|e| !e.is_null());
                let addition = match delta {
                    Some(d) => Some(d),
                    None => text.strip_prefix(entry.text.as_str()).map(str::to_owned),
                };
                match addition {
                    Some(add) if !add.is_empty() => {
                        entry.text.push_str(&add);
                        let block = entry.block.clone();
                        self.emit(AgentEvent::Delta { block, kind: block_kind, text: add });
                    }
                    Some(_) => {}
                    None => {
                        // Rewritten rather than extended: replace it.
                        entry.text = text.clone();
                        let block = entry.block.clone();
                        self.emit(AgentEvent::BlockDone { block, kind: block_kind, text });
                    }
                }
                if finished && let Some(entry) = self.parts.get_mut(&id) {
                    entry.done = true;
                    let (block, text) = (entry.block.clone(), entry.text.clone());
                    self.emit(AgentEvent::BlockDone { block, kind: block_kind, text });
                }
            }
            "tool" => {
                let call_id = str_of(part, "callID").unwrap_or(id);
                if self.tools_done.contains(&call_id) {
                    return;
                }
                let tool = str_of(part, "tool").unwrap_or_default();
                let state = part.get("state").cloned().unwrap_or_default();
                let status = str_of(&state, "status").unwrap_or_default();
                let call = tool_call(&call_id, &tool, &state);
                self.emit(AgentEvent::ToolStarted(call.clone()));
                if status == "completed" || status == "error" {
                    self.tools_done.insert(call_id.clone());
                    let output = str_of(&state, "output").or_else(|| str_of(&state, "error")).unwrap_or_default();
                    self.emit(AgentEvent::ToolFinished {
                        call_id,
                        output,
                        is_error: status == "error",
                        exit_code: state.get("metadata").and_then(|m| m.get("exit")).and_then(|e| e.as_i64()).map(|e| e as i32),
                        files: call.files,
                    });
                }
            }
            _ => {}
        }
    }

    fn on_permission(&mut self, props: &Value) {
        let Some(id) = str_of(props, "id") else { return };
        let permission = str_of(props, "permission").or_else(|| str_of(props, "type")).unwrap_or_default();
        let patterns: Vec<String> = match props.get("patterns").and_then(|p| p.as_array()) {
            Some(list) => list.iter().filter_map(|p| p.as_str().map(str::to_owned)).collect(),
            None => props.get("pattern").and_then(|p| p.as_str()).map(|p| vec![p.to_owned()]).unwrap_or_default(),
        };
        let metadata = props.get("metadata").cloned().unwrap_or_default();
        let title = str_of(props, "title").unwrap_or_else(|| humanize_id(&permission));
        let kind = match permission.as_str() {
            "bash" => RequestKind::Command {
                command: str_of(&metadata, "command").unwrap_or_else(|| patterns.join(" ")),
                cwd: self.spec.cwd.display().to_string(),
                reason: title,
            },
            "edit" => RequestKind::FileChange {
                path: str_of(&metadata, "filepath")
                    .or_else(|| str_of(&metadata, "filePath"))
                    .or_else(|| patterns.first().cloned())
                    .unwrap_or_default(),
                diff: str_of(&metadata, "diff").unwrap_or_default(),
            },
            _ => RequestKind::Tool { name: title, input: patterns.join("\n") },
        };
        let provider_ref = format!("oc-{id}");
        self.asks.insert(provider_ref.clone(), (id, false));
        self.emit(AgentEvent::Request { provider_ref, kind });
    }

    fn on_question(&mut self, props: &Value) {
        let Some(id) = str_of(props, "id") else { return };
        let Some(first) = array(props, "questions").next() else { return };
        let question = [str_of(first, "header"), str_of(first, "question")].into_iter().flatten().collect::<Vec<_>>().join("\n\n");
        let options = array(first, "options")
            .filter_map(|o| Some(QuestionOption::new(str_of(o, "label")?, str_of(o, "description").unwrap_or_default())))
            .collect();
        let multi_select = first.get("multiple").and_then(|m| m.as_bool()).unwrap_or(false);
        let provider_ref = format!("oc-{id}");
        self.asks.insert(provider_ref.clone(), (id, true));
        self.emit(AgentEvent::Request { provider_ref, kind: RequestKind::Question { question, options, multi_select } });
    }
}

fn tool_call(call_id: &str, tool: &str, state: &Value) -> ToolCall {
    let input = state.get("input").cloned().unwrap_or_default();
    let path = str_of(&input, "filePath").or_else(|| str_of(&input, "path"));
    let title = str_of(state, "title").filter(|t| !t.is_empty());
    let (kind, fallback) = match tool {
        "bash" => (ToolKind::Command, str_of(&input, "command")),
        "read" => (ToolKind::Read, path.clone()),
        "edit" | "write" | "patch" | "multiedit" => (ToolKind::Edit, path.clone()),
        "grep" | "glob" | "list" | "codesearch" => (ToolKind::Search, str_of(&input, "pattern").or_else(|| path.clone())),
        "webfetch" | "websearch" => (ToolKind::Web, str_of(&input, "url").or_else(|| str_of(&input, "query"))),
        "task" => (ToolKind::Task, str_of(&input, "description")),
        "todowrite" | "todoread" => (ToolKind::Todo, None),
        _ if tool.contains('_') => (ToolKind::Mcp, None),
        _ => (ToolKind::Other, None),
    };
    let files = match (tool, &path) {
        ("edit" | "patch" | "multiedit", Some(p)) => vec![FileChange { path: p.clone(), kind: ChangeKind::Modified, additions: 0, deletions: 0 }],
        ("write", Some(p)) => vec![FileChange {
            path: p.clone(),
            kind: ChangeKind::Added,
            additions: str_of(&input, "content").map(|c| c.lines().count() as u32).unwrap_or(0),
            deletions: 0,
        }],
        _ => Vec::new(),
    };
    ToolCall {
        call_id: call_id.to_owned(),
        kind,
        name: tool.to_owned(),
        title: fallback.or(title).unwrap_or_else(|| humanize_id(tool)),
        input: if input.is_null() { String::new() } else { sonic_rs::to_string(&input).unwrap_or_default() },
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
    fn models_come_from_every_provider() {
        let value = json!({"providers": [{"id": "anthropic", "name": "Anthropic", "models": {
            "claude-x": {"id": "claude-x", "name": "Claude X", "limit": {"context": 200000}}
        }}], "default": {"anthropic": "claude-x"}});
        let models = parse_models(&value);
        assert_eq!(models[0].slug, "anthropic/claude-x");
        assert_eq!(models[0].context_window, 200_000);
    }

    #[test]
    fn access_levels_become_permission_rules() {
        let rules = permission_rules(Access::AutoEdit);
        let edit = rules.as_array().unwrap().iter().find(|r| str_of(r, "permission").as_deref() == Some("edit")).unwrap();
        assert_eq!(str_of(edit, "action").as_deref(), Some("allow"));
        let rules = permission_rules(Access::Readonly);
        assert_eq!(str_of(&rules.as_array().unwrap()[0], "action").as_deref(), Some("deny"));
        let guard = GuardSettings { enabled: true, rules: vec![GuardRule::new("rm -rf", "Deletes")] };
        let rules = guarded_rules(Access::Full, &guard);
        let last = rules.as_array().unwrap().last().unwrap();
        assert_eq!((str_of(last, "pattern").as_deref(), str_of(last, "action").as_deref()), (Some("*rm -rf*"), Some("ask")));
        assert_eq!(guarded_rules(Access::Full, &GuardSettings { enabled: false, ..guard }), permission_rules(Access::Full));
    }

    #[test]
    fn message_ids_ascend() {
        let (a, b) = (message_id(), message_id());
        assert!(a.starts_with("msg_") && a[..16] <= b[..16], "{a} {b}");
    }

    #[test]
    fn tools_are_named_by_their_input() {
        let call = tool_call("c", "bash", &json!({"status": "running", "input": {"command": "ls"}}));
        assert_eq!((call.kind, call.title.as_str()), (ToolKind::Command, "ls"));
        let call = tool_call("c", "edit", &json!({"input": {"filePath": "/w/a.rs"}}));
        assert_eq!(call.files[0].path, "/w/a.rs");
    }
}
