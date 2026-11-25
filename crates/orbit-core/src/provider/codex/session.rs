//! One thread session: an app-server process, supervised.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use orbit_proto::*;
use serde::Serialize;
use sonic_rs::{JsonValueTrait, LazyValue, Value, json};
use tokio::time::Instant;

use super::super::{AgentEvent, BlockKind, EventSender, PromptAttachment, SessionInput, SessionSpec, TurnOutcome};
use super::map;
use super::rpc::Connection;
use super::wire::*;

pub type Inputs = crossfire::AsyncRx<crossfire::mpsc::Array<SessionInput>>;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// Setup and turn-start requests that take longer mean a wedged server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a stop may go unconfirmed before the turn is closed locally.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Crash recovery budget: this many restarts per window, then give up.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(300);
/// Turn ids remembered after they closed, to drop their late notifications.
const STALE_TURNS: usize = 8;

/// Runs the session until shutdown or an unrecoverable failure, then
/// reports [`AgentEvent::Exited`].
pub async fn run(spec: SessionSpec, events: EventSender, mut inputs: Inputs) {
    let mut session = Session::new(spec, events);
    let mut restarts: VecDeque<Instant> = VecDeque::new();
    let error = loop {
        match session.connect(&mut inputs).await {
            Exit::Shutdown => break None,
            Exit::Fatal(message) => break Some(message),
            Exit::Crashed(message) => {
                session.after_crash(&message);
                if !session.ready_once {
                    break Some(message);
                }
                let now = Instant::now();
                while restarts.front().is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW) {
                    restarts.pop_front();
                }
                if restarts.len() >= MAX_RESTARTS {
                    break Some(format!("{message}. Stopped restarting after {MAX_RESTARTS} crashes in 5 minutes."));
                }
                restarts.push_back(now);
                session.emit(AgentEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("{message}. Restarting Codex and resuming the thread."),
                });
                let backoff = Duration::from_millis(250 << (restarts.len() - 1));
                if !session.wait_offline(&mut inputs, backoff).await {
                    break None;
                }
            }
        }
    };
    session.emit(AgentEvent::Exited { error });
}

enum Exit {
    /// The engine asked to stop, or dropped the session.
    Shutdown,
    /// The process died or stopped answering; worth a restart.
    Crashed(String),
    /// Restarting cannot help (missing binary, rejected handshake).
    Fatal(String),
}

/// A prompt waiting to run. `uuid` is `None` for prompts the adapter sends
/// itself (denial feedback, plan approval).
#[derive(Clone, Debug)]
struct Prompt {
    uuid: Option<String>,
    text: String,
    attachments: Vec<PromptAttachment>,
}

/// What a response answers.
enum Call {
    Initialize,
    ModelList,
    Account,
    Modes,
    Skills,
    ConfigRead,
    RateLimits,
    ThreadOpen(Open),
    TurnStart { seq: u64 },
    Steer(Prompt),
    Interrupt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Open {
    Start,
    Resume,
    Fork,
}

impl Call {
    fn method(&self) -> &'static str {
        match self {
            Call::Initialize => "initialize",
            Call::ThreadOpen(Open::Start) => "thread/start",
            Call::ThreadOpen(Open::Resume) => "thread/resume",
            Call::ThreadOpen(Open::Fork) => "thread/fork",
            Call::TurnStart { .. } => "turn/start",
            _ => "request",
        }
    }
}

struct ActiveTurn {
    seq: u64,
    turn_id: Option<String>,
    started: Instant,
    usage: Usage,
    /// Stop requested before Codex told us the turn id.
    interrupt_pending: bool,
    interrupt_deadline: Option<Instant>,
    /// The last non-retried `error` notification, used if `turn/completed`
    /// carries no error of its own.
    failure: Option<TurnError>,
    /// The proposed plan of a plan-mode turn.
    plan: Option<String>,
    /// Steers sent before the turn id was known.
    steer_waiting: Vec<Prompt>,
}

/// A request shown to the user, keyed by its `provider_ref`.
struct Pending {
    raw_id: String,
    kind: PendingKind,
}

enum PendingKind {
    Command,
    FileChange,
    Permissions { requested: String },
    Elicitation { url: bool },
    Question { group: u64, question_id: String },
    /// Leave plan mode and implement; local, not a Codex request.
    Plan,
}

struct QuestionGroup {
    raw_id: String,
    remaining: usize,
    answers: Vec<(String, Vec<String>)>,
}

struct Session {
    spec: SessionSpec,
    events: EventSender,
    thread_id: Option<String>,
    /// The model Codex reports for the thread (its default when the config
    /// names none).
    model: Option<String>,
    version: Option<String>,
    models: Vec<ModelInfo>,
    modes: Vec<ModeInfo>,
    skills: Vec<SlashCommand>,
    signed_in: Option<bool>,
    account: Option<String>,
    account_message: Option<String>,
    reasoning_summary: Option<String>,
    rate_limits: Option<RateLimitSnapshot>,
    /// `turn/steer` exists on this server until it answers "method not found".
    steer_supported: bool,
    /// Plan approved: run in build mode until the engine reconfigures.
    mode_override: Option<Mode>,
    ready: bool,
    ready_once: bool,
    queue: VecDeque<Prompt>,
    turn: Option<ActiveTurn>,
    turn_seq: u64,
    stale_turns: VecDeque<String>,
    usage_total: Option<TokenBreakdown>,
    requests: HashMap<String, Pending>,
    groups: HashMap<u64, QuestionGroup>,
    next_ref: u64,
    /// Item id → file changes / command, for approvals that refer to items.
    item_files: HashMap<String, Vec<FileUpdateChange>>,
    item_commands: HashMap<String, String>,
    open_tools: HashSet<String>,
    /// Last reasoning summary index streamed per item.
    reasoning_parts: HashMap<String, u32>,
    // Per connection.
    calls: HashMap<u64, (Call, Option<Instant>)>,
    catalog_waiting: usize,
    model_buffer: Vec<Model>,
}

impl Session {
    fn new(spec: SessionSpec, events: EventSender) -> Self {
        Self {
            spec,
            events,
            thread_id: None,
            model: None,
            version: None,
            models: Vec::new(),
            modes: Vec::new(),
            skills: Vec::new(),
            signed_in: None,
            account: None,
            account_message: None,
            reasoning_summary: None,
            rate_limits: None,
            steer_supported: true,
            mode_override: None,
            ready: false,
            ready_once: false,
            queue: VecDeque::new(),
            turn: None,
            turn_seq: 0,
            stale_turns: VecDeque::new(),
            usage_total: None,
            requests: HashMap::new(),
            groups: HashMap::new(),
            next_ref: 0,
            item_files: HashMap::new(),
            item_commands: HashMap::new(),
            open_tools: HashSet::new(),
            reasoning_parts: HashMap::new(),
            calls: HashMap::new(),
            catalog_waiting: 0,
            model_buffer: Vec::new(),
        }
    }

    fn emit(&self, event: AgentEvent) {
        use crossfire::BlockingTxTrait;
        let _ = self.events.send((self.spec.thread, event));
    }

    // -----------------------------------------------------------------------
    // Connection lifecycle

    async fn connect(&mut self, inputs: &mut Inputs) -> Exit {
        let Some(binary) = super::resolve(self.spec.binary.as_deref()) else {
            return Exit::Fatal(super::not_found_message(self.spec.binary.as_deref()));
        };
        let (mut conn, mut lines) = match Connection::spawn_with(
            &binary,
            &super::super::McpServer::codex_overrides(&self.spec.mcp),
            &self.spec.cwd,
            &self.spec.env,
        ) {
            Ok(conn) => conn,
            Err(err) => return Exit::Fatal(format!("Couldn't start Codex ({}): {err}", binary.display())),
        };
        self.calls.clear();
        self.catalog_waiting = 0;
        let id = conn.request("initialize", &super::initialize_params());
        self.calls.insert(id, (Call::Initialize, Some(Instant::now() + INITIALIZE_TIMEOUT)));

        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => {
                        if let Some(exit) = self.on_line(&mut conn, &line) {
                            return exit;
                        }
                    }
                    None => return Exit::Crashed(conn.exit_reason().await),
                },
                input = inputs.recv() => match input {
                    Ok(SessionInput::Shutdown) | Err(_) => return Exit::Shutdown,
                    Ok(input) => self.on_input(&mut conn, input),
                },
                _ = sleep_until(deadline), if deadline.is_some() => {
                    if let Some(exit) = self.on_deadline(&mut conn) {
                        return exit;
                    }
                }
            }
        }
    }

    /// Between a crash and the restart: keeps taking inputs so prompts queue
    /// up and a shutdown is honored. Returns `false` on shutdown.
    async fn wait_offline(&mut self, inputs: &mut Inputs, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(until) => return true,
                input = inputs.recv() => match input {
                    Ok(SessionInput::Shutdown) | Err(_) => return false,
                    Ok(SessionInput::Prompt { uuid, text, attachments, .. }) => {
                        self.queue.push_back(Prompt { uuid: Some(uuid), text, attachments });
                    }
                    Ok(SessionInput::Configure(config)) => self.configure(config),
                    Ok(SessionInput::Interrupt | SessionInput::Resolve { .. }) => {}
                },
            }
        }
    }

    /// Settles everything that depended on the dead process.
    fn after_crash(&mut self, message: &str) {
        self.ready = false;
        self.usage_total = None;
        self.calls.clear();
        if let Some(mut turn) = self.turn.take() {
            for prompt in turn.steer_waiting.drain(..).rev() {
                self.queue.push_front(prompt);
            }
            self.emit(AgentEvent::TurnFinished {
                outcome: TurnOutcome::Failed { message: message.to_owned() },
                usage: turn.usage,
                duration_ms: turn.started.elapsed().as_millis() as u64,
            });
        }
        self.withdraw_requests(true);
        self.open_tools.clear();
        self.item_files.clear();
        self.item_commands.clear();
        self.reasoning_parts.clear();
    }

    fn next_deadline(&self) -> Option<Instant> {
        let calls = self.calls.values().filter_map(|(_, d)| *d);
        let interrupt = self.turn.as_ref().and_then(|t| t.interrupt_deadline);
        calls.chain(interrupt).min()
    }

    fn on_deadline(&mut self, conn: &mut Connection) -> Option<Exit> {
        let now = Instant::now();
        if let Some((_, (call, _))) = self.calls.iter().find(|(_, (_, d))| d.is_some_and(|d| d <= now)) {
            let message = format!("Codex app-server stopped responding ({} timed out)", call.method());
            return Some(if self.ready_once { Exit::Crashed(message) } else { Exit::Fatal(message) });
        }
        if self.turn.as_ref().and_then(|t| t.interrupt_deadline).is_some_and(|d| d <= now) {
            self.emit(AgentEvent::Notice {
                level: NoticeLevel::Warning,
                text: "Codex didn't confirm the stop in time; the turn was closed as interrupted.".into(),
            });
            self.finish_turn(conn, TurnOutcome::Interrupted, None);
        }
        None
    }

    // -----------------------------------------------------------------------
    // Engine inputs

    fn on_input(&mut self, conn: &mut Connection, input: SessionInput) {
        match input {
            SessionInput::Prompt { uuid, text, attachments, steer } => {
                self.prompt(conn, Prompt { uuid: Some(uuid), text, attachments }, steer)
            }
            SessionInput::Interrupt => self.interrupt(conn),
            SessionInput::Resolve { provider_ref, decision } => self.resolve(conn, &provider_ref, decision),
            SessionInput::Configure(config) => self.configure(config),
            SessionInput::Shutdown => {}
        }
    }

    fn configure(&mut self, config: ThreadConfig) {
        // Codex takes model, effort, mode and policy per turn, so a new
        // config applies from the next turn without a restart.
        self.spec.config = config;
        self.mode_override = None;
    }

    fn prompt(&mut self, conn: &mut Connection, prompt: Prompt, steer: bool) {
        if !self.ready {
            self.queue.push_back(prompt);
            return;
        }
        let Some(turn) = &mut self.turn else {
            self.start_turn(conn, prompt);
            return;
        };
        if !steer || !self.steer_supported {
            self.queue.push_back(prompt);
            return;
        }
        match turn.turn_id.clone() {
            Some(turn_id) => self.send_steer(conn, &turn_id, prompt),
            None => turn.steer_waiting.push(prompt),
        }
    }

    fn send_steer(&mut self, conn: &mut Connection, turn_id: &str, prompt: Prompt) {
        let Some(thread_id) = self.thread_id.as_deref() else { return };
        let params = SteerParams { thread_id, expected_turn_id: turn_id, input: user_input(&prompt) };
        let id = conn.request("turn/steer", &params);
        self.calls.insert(id, (Call::Steer(prompt), None));
    }

    fn start_next(&mut self, conn: &mut Connection) {
        if self.ready
            && self.turn.is_none()
            && let Some(prompt) = self.queue.pop_front()
        {
            self.start_turn(conn, prompt);
        }
    }

    fn effective_model(&self) -> Option<&str> {
        let configured = self.spec.config.model.as_str();
        if !configured.is_empty() {
            return Some(configured);
        }
        self.model.as_deref().or_else(|| self.models.iter().find(|m| m.is_default).map(|m| m.slug.as_str()))
    }

    fn start_turn(&mut self, conn: &mut Connection, prompt: Prompt) {
        if self.thread_id.is_none() {
            self.queue.push_front(prompt);
            return;
        }
        if prompt.uuid.is_some() {
            // The user moved on instead of answering a plan offer.
            self.withdraw_requests(true);
        }
        let Some(thread_id) = self.thread_id.as_deref() else { return };
        let config = &self.spec.config;
        let model = self.effective_model();
        let options = &config.options;
        let effort = model.and_then(|m| map::option_choice(&self.models, m, options, map::EFFORT));
        let service_tier = model.and_then(|m| map::service_tier(&self.models, m, options));
        let mode = self.mode_override.unwrap_or(config.mode);
        // Send the mode only to servers that reported modes, and always both
        // ways so leaving plan mode clears it.
        let collaboration_mode = match model {
            Some(model) if !self.modes.is_empty() => Some(CollaborationMode {
                mode: if mode == Mode::Plan { "plan" } else { "default" },
                settings: CollaborationSettings { model, reasoning_effort: effort },
            }),
            _ => None,
        };
        let (approval_policy, _, sandbox_policy) = map::policy(config.access);
        let cwd = self.spec.cwd.to_string_lossy();
        let params = TurnStartParams {
            thread_id,
            input: user_input(&prompt),
            cwd: &cwd,
            model: (!config.model.is_empty()).then_some(config.model.as_str()),
            effort,
            service_tier,
            summary: self.reasoning_summary.as_deref().unwrap_or("auto"),
            approval_policy,
            approvals_reviewer: "user",
            sandbox_policy,
            collaboration_mode,
        };
        let id = conn.request("turn/start", &params);
        self.turn_seq += 1;
        let seq = self.turn_seq;
        self.calls.insert(id, (Call::TurnStart { seq }, Some(Instant::now() + REQUEST_TIMEOUT)));
        self.turn = Some(ActiveTurn {
            seq,
            turn_id: None,
            started: Instant::now(),
            usage: Usage::default(),
            interrupt_pending: false,
            interrupt_deadline: None,
            failure: None,
            plan: None,
            steer_waiting: Vec::new(),
        });
        self.emit(AgentEvent::TurnStarted { prompt: prompt.uuid });
    }

    fn interrupt(&mut self, conn: &mut Connection) {
        let Some(turn) = &mut self.turn else { return };
        if turn.interrupt_deadline.is_some() {
            return;
        }
        turn.interrupt_deadline = Some(Instant::now() + INTERRUPT_TIMEOUT);
        // Steers that never reached Codex run after the stop instead.
        for prompt in turn.steer_waiting.drain(..).rev() {
            self.queue.push_front(prompt);
        }
        match (turn.turn_id.clone(), self.thread_id.as_deref()) {
            (Some(turn_id), Some(thread_id)) => {
                let id = conn.request("turn/interrupt", &json!({"threadId": thread_id, "turnId": turn_id}));
                self.calls.insert(id, (Call::Interrupt, None));
            }
            _ => turn.interrupt_pending = true,
        }
    }

    fn finish_turn(&mut self, conn: &mut Connection, outcome: TurnOutcome, duration_ms: Option<u64>) {
        let Some(mut turn) = self.turn.take() else { return };
        for prompt in turn.steer_waiting.drain(..).rev() {
            self.queue.push_front(prompt);
        }
        if let Some(turn_id) = turn.turn_id.take() {
            self.emit(AgentEvent::Checkpoint { provider_message: turn_id.clone() });
            if self.stale_turns.len() == STALE_TURNS {
                self.stale_turns.pop_front();
            }
            self.stale_turns.push_back(turn_id);
        }
        // Codex settles its requests with the turn; never leave a card
        // pending for a request nobody can answer (T3 Code §10.5).
        self.withdraw_requests(false);
        self.item_files.clear();
        self.item_commands.clear();
        self.reasoning_parts.clear();
        let completed = outcome == TurnOutcome::Completed;
        self.emit(AgentEvent::TurnFinished {
            outcome,
            usage: turn.usage,
            duration_ms: duration_ms.unwrap_or_else(|| turn.started.elapsed().as_millis() as u64),
        });
        let plan_mode = self.mode_override.unwrap_or(self.spec.config.mode) == Mode::Plan;
        if completed
            && plan_mode
            && let Some(plan) = turn.plan.take()
        {
            let provider_ref = self.new_ref();
            self.requests.insert(provider_ref.clone(), Pending { raw_id: String::new(), kind: PendingKind::Plan });
            self.emit(AgentEvent::Request { provider_ref, kind: RequestKind::Plan { plan } });
        }
        self.start_next(conn);
    }

    fn withdraw_requests(&mut self, include_plan: bool) {
        let refs: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, p)| include_plan || !matches!(p.kind, PendingKind::Plan))
            .map(|(r, _)| r.clone())
            .collect();
        for provider_ref in refs {
            if let Some(Pending { kind: PendingKind::Question { group, .. }, .. }) = self.requests.remove(&provider_ref) {
                self.groups.remove(&group);
            }
            self.emit(AgentEvent::RequestWithdrawn { provider_ref });
        }
    }

    fn new_ref(&mut self) -> String {
        self.next_ref += 1;
        format!("codex-{}", self.next_ref)
    }

    // -----------------------------------------------------------------------
    // Requests to the user

    fn resolve(&mut self, conn: &mut Connection, provider_ref: &str, decision: Decision) {
        let Some(pending) = self.requests.remove(provider_ref) else { return };
        let feedback = match &decision {
            Decision::Deny { feedback } | Decision::Answer { text: feedback } => feedback.trim().to_owned(),
            _ => String::new(),
        };
        match pending.kind {
            PendingKind::Command | PendingKind::FileChange => {
                let decision = match decision {
                    Decision::Allow => "accept",
                    Decision::AllowForSession => "acceptForSession",
                    Decision::Deny { .. } | Decision::Answer { .. } => "decline",
                };
                conn.respond(&pending.raw_id, &format!("{{\"decision\":\"{decision}\"}}"));
                if decision == "decline" && !feedback.is_empty() {
                    // Codex's decline carries no reason; tell the model why.
                    self.prompt(conn, Prompt { uuid: None, text: feedback, attachments: Vec::new() }, true);
                }
            }
            PendingKind::Permissions { requested } => {
                let result = match decision {
                    Decision::Allow => format!("{{\"permissions\":{requested},\"scope\":\"turn\"}}"),
                    Decision::AllowForSession => format!("{{\"permissions\":{requested},\"scope\":\"session\"}}"),
                    _ => "{\"permissions\":{},\"scope\":\"turn\"}".to_owned(),
                };
                conn.respond(&pending.raw_id, &result);
            }
            PendingKind::Elicitation { url } => {
                let result = match decision {
                    Decision::Allow | Decision::AllowForSession if url => "{\"action\":\"accept\"}",
                    Decision::Allow | Decision::AllowForSession => "{\"action\":\"accept\",\"content\":{}}",
                    // A declined URL is remembered as a site denial; cancel
                    // leaves no trace.
                    _ if url => "{\"action\":\"cancel\"}",
                    _ => "{\"action\":\"decline\"}",
                };
                conn.respond(&pending.raw_id, result);
            }
            PendingKind::Question { group, question_id } => {
                let answers = match decision {
                    Decision::Answer { text } => vec![text],
                    _ => Vec::new(),
                };
                let Some(g) = self.groups.get_mut(&group) else { return };
                g.answers.push((question_id, answers));
                g.remaining -= 1;
                if g.remaining == 0
                    && let Some(g) = self.groups.remove(&group)
                {
                    let answers: HashMap<String, Value> =
                        g.answers.into_iter().map(|(id, answers)| (id, json!({"answers": answers}))).collect();
                    let result = sonic_rs::to_string(&json!({"answers": answers})).unwrap_or_else(|_| "{\"answers\":{}}".into());
                    conn.respond(&g.raw_id, &result);
                }
            }
            PendingKind::Plan => match decision {
                Decision::Allow | Decision::AllowForSession => {
                    self.mode_override = Some(Mode::Build);
                    self.prompt(conn, Prompt { uuid: None, text: "Implement the plan.".into(), attachments: Vec::new() }, false);
                }
                _ if !feedback.is_empty() => {
                    self.prompt(conn, Prompt { uuid: None, text: feedback, attachments: Vec::new() }, false)
                }
                _ => {}
            },
        }
    }

    fn ask(&mut self, raw_id: &str, kind: PendingKind, request: RequestKind) {
        let provider_ref = self.new_ref();
        self.requests.insert(provider_ref.clone(), Pending { raw_id: raw_id.to_owned(), kind });
        self.emit(AgentEvent::Request { provider_ref, kind: request });
    }

    fn on_server_request(&mut self, conn: &mut Connection, method: &str, raw_id: &str, params: &str) {
        let full = self.spec.config.access == Access::Full;
        match method {
            "item/commandExecution/requestApproval" => {
                let Ok(p) = sonic_rs::from_str::<CommandApprovalParams>(params) else {
                    // Never answer an approval with an internal error (T3 Code §10.8).
                    conn.respond(raw_id, "{\"decision\":\"decline\"}");
                    return;
                };
                if full {
                    conn.respond(raw_id, "{\"decision\":\"accept\"}");
                    return;
                }
                let command = p.command.or_else(|| self.item_commands.get(&p.item_id).cloned()).unwrap_or_default();
                let reason = match (p.reason, p.network_approval_context) {
                    (Some(reason), _) if !reason.is_empty() => reason,
                    (_, Some(net)) => format!("Network access to {} ({})", net.host, net.protocol),
                    _ => String::new(),
                };
                let cwd = p.cwd.unwrap_or_else(|| self.spec.cwd.display().to_string());
                self.ask(raw_id, PendingKind::Command, RequestKind::Command { command, cwd, reason });
            }
            "item/fileChange/requestApproval" => {
                let Ok(p) = sonic_rs::from_str::<FileApprovalParams>(params) else {
                    conn.respond(raw_id, "{\"decision\":\"decline\"}");
                    return;
                };
                if full {
                    conn.respond(raw_id, "{\"decision\":\"accept\"}");
                    return;
                }
                let changes = self.item_files.get(&p.item_id).map(Vec::as_slice).unwrap_or_default();
                let (mut path, diff) = map::approval_diff(changes);
                if let Some(root) = p.grant_root.filter(|r| !r.is_empty()) {
                    path = format!("Write access to {root}");
                } else if path.is_empty() {
                    path = p.reason.unwrap_or_default();
                }
                self.ask(raw_id, PendingKind::FileChange, RequestKind::FileChange { path, diff });
            }
            "item/tool/requestUserInput" => {
                let questions = sonic_rs::from_str::<UserInputParams>(params).map(|p| p.questions).unwrap_or_default();
                if questions.is_empty() {
                    conn.respond(raw_id, "{\"answers\":{}}");
                    return;
                }
                self.next_ref += 1;
                let group = self.next_ref;
                self.groups.insert(group, QuestionGroup { raw_id: raw_id.to_owned(), remaining: questions.len(), answers: Vec::new() });
                for q in questions {
                    let question = if q.header.is_empty() || q.question.starts_with(&q.header) {
                        q.question
                    } else {
                        format!("{}: {}", q.header, q.question)
                    };
                    let options = q.options.unwrap_or_default().into_iter().map(|o| QuestionOption::new(o.label, o.description)).collect();
                    self.ask(raw_id, PendingKind::Question { group, question_id: q.id }, RequestKind::Question { question, options, multi_select: false });
                }
            }
            "mcpServer/elicitation/request" => {
                let Ok(p) = sonic_rs::from_str::<ElicitationParams>(params) else {
                    conn.respond(raw_id, "{\"action\":\"cancel\"}");
                    return;
                };
                let url = p.mode == "url";
                let input = sonic_rs::to_string(&json!({"message": p.message, "url": p.url})).unwrap_or_default();
                self.ask(raw_id, PendingKind::Elicitation { url }, RequestKind::Tool { name: p.server_name, input });
            }
            "item/permissions/requestApproval" => {
                let Ok(p) = sonic_rs::from_str::<PermissionsParams>(params) else {
                    conn.respond(raw_id, "{\"permissions\":{},\"scope\":\"turn\"}");
                    return;
                };
                let requested = p.permissions.as_raw_str().to_owned();
                let input = match p.reason {
                    Some(reason) => format!("{{\"reason\":{},\"permissions\":{requested}}}", sonic_rs::to_string(&reason).unwrap_or_default()),
                    None => format!("{{\"permissions\":{requested}}}"),
                };
                self.ask(raw_id, PendingKind::Permissions { requested }, RequestKind::Tool { name: "Permissions".into(), input });
            }
            _ => conn.respond_error(raw_id, METHOD_NOT_FOUND, &format!("Orbit does not handle {method}")),
        }
    }

    // -----------------------------------------------------------------------
    // Wire

    fn on_line(&mut self, conn: &mut Connection, line: &[u8]) -> Option<Exit> {
        let envelope: RpcLine = match sonic_rs::from_slice(line) {
            Ok(envelope) => envelope,
            Err(err) => {
                let shown: String = String::from_utf8_lossy(line).chars().take(200).collect();
                log::warn!("codex: unparsable line ({err}): {shown}");
                return None;
            }
        };
        let params = envelope.params.as_ref().map(LazyValue::as_raw_str).unwrap_or("{}");
        match (envelope.method.as_deref(), envelope.id.as_ref()) {
            (Some(method), Some(id)) => {
                self.on_server_request(conn, method, id.as_raw_str(), params);
                None
            }
            (Some(method), None) => {
                self.on_notification(conn, method, params);
                None
            }
            (None, Some(id)) => {
                let (call, _) = self.calls.remove(&id.as_raw_str().parse::<u64>().ok()?)?;
                let result = match envelope.error {
                    Some(error) => Err(error),
                    None => Ok(envelope.result.as_ref().map(LazyValue::as_raw_str).unwrap_or("null")),
                };
                self.on_response(conn, call, result)
            }
            (None, None) => None,
        }
    }

    fn on_response(&mut self, conn: &mut Connection, call: Call, result: Result<&str, RpcError>) -> Option<Exit> {
        match call {
            Call::Initialize => {
                let response = match result {
                    Ok(raw) => sonic_rs::from_str::<InitializeResponse>(raw).ok(),
                    Err(err) => return Some(Exit::Fatal(format!("Codex rejected the handshake: {}", err.message))),
                };
                if let Some(version) = response.and_then(|r| map::version_from_user_agent(&r.user_agent)) {
                    self.version = Some(version);
                }
                conn.notify("initialized");
                self.request_catalog(conn);
                self.open_thread(conn);
            }
            Call::ModelList => {
                match result.map(sonic_rs::from_str::<ModelListResponse>) {
                    Ok(Ok(page)) => {
                        self.model_buffer.extend(page.data);
                        if let Some(cursor) = page.next_cursor.filter(|c| !c.is_empty()) {
                            let id = conn.request("model/list", &json!({"cursor": cursor}));
                            self.calls.insert(id, (Call::ModelList, None));
                            return None;
                        }
                        self.models = map::models(&std::mem::take(&mut self.model_buffer));
                    }
                    Ok(Err(err)) => log::warn!("codex: model/list: {err}"),
                    Err(err) => log::warn!("codex: model/list: {}", err.message),
                }
                self.catalog_part_done();
            }
            Call::Account => {
                if let Ok(Ok(response)) = result.map(sonic_rs::from_str::<AccountResponse>) {
                    (self.signed_in, self.account, self.account_message) = map::account(&response);
                    if response.account.as_ref().is_some_and(|a| a.kind == "chatgpt") {
                        let id = conn.request("account/rateLimits/read", &Value::new_null());
                        self.calls.insert(id, (Call::RateLimits, None));
                    }
                }
                self.catalog_part_done();
            }
            Call::Modes => {
                if let Ok(Ok(response)) = result.map(sonic_rs::from_str::<CollaborationModeListResponse>) {
                    self.modes = map::modes(&response.data);
                }
                self.catalog_part_done();
            }
            Call::Skills => {
                if let Ok(Ok(response)) = result.map(sonic_rs::from_str::<SkillsListResponse>) {
                    self.skills = map::skills(response);
                }
                self.catalog_part_done();
            }
            Call::ConfigRead => {
                if let Ok(Ok(response)) = result.map(sonic_rs::from_str::<ConfigReadResponse>) {
                    self.reasoning_summary = response.config.model_reasoning_summary;
                }
            }
            Call::RateLimits => {
                if let Ok(Ok(response)) = result.map(sonic_rs::from_str::<RateLimitsResponse>) {
                    self.on_rate_limits(response.rate_limits);
                }
            }
            Call::ThreadOpen(kind) => return self.on_thread_open(conn, kind, result),
            Call::TurnStart { seq } => self.on_turn_start(conn, seq, result),
            Call::Steer(prompt) => match result {
                Ok(_) => {
                    if let Some(uuid) = prompt.uuid {
                        self.emit(AgentEvent::PromptAccepted { uuid });
                    }
                }
                Err(err) => {
                    // Not steerable now (turn ended, review or compaction):
                    // the prompt runs as the next turn instead.
                    if err.code == METHOD_NOT_FOUND {
                        self.steer_supported = false;
                    }
                    log::debug!("codex: steer refused ({}), queueing", err.message);
                    self.queue.push_back(prompt);
                    self.start_next(conn);
                }
            },
            Call::Interrupt => {
                if let Err(err) = result {
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("Codex couldn't stop the turn: {}", err.message) });
                }
            }
        }
        None
    }

    fn request_catalog(&mut self, conn: &mut Connection) {
        let cwd = self.spec.cwd.to_string_lossy().into_owned();
        self.model_buffer.clear();
        let calls = [
            (conn.request("model/list", &json!({})), Call::ModelList),
            (conn.request("account/read", &json!({})), Call::Account),
            (conn.request("collaborationMode/list", &json!({})), Call::Modes),
            (conn.request("skills/list", &json!({"cwds": [cwd]})), Call::Skills),
        ];
        self.catalog_waiting = calls.len();
        for (id, call) in calls {
            self.calls.insert(id, (call, None));
        }
        let id = conn.request("config/read", &json!({"cwd": cwd, "includeLayers": false}));
        self.calls.insert(id, (Call::ConfigRead, None));
    }

    fn catalog_part_done(&mut self) {
        self.catalog_waiting = self.catalog_waiting.saturating_sub(1);
        if self.catalog_waiting > 0 {
            return;
        }
        self.emit(AgentEvent::Catalog(ProviderInfo {
            installed: true,
            version: self.version.clone(),
            signed_in: self.signed_in,
            account: self.account.clone(),
            models: self.models.clone(),
            modes: self.modes.clone(),
            slash_commands: self.skills.clone(),
            message: self.account_message.clone(),
            ..super::provider_info(super::resolve(self.spec.binary.as_deref()).as_deref())
        }));
    }

    fn open_thread(&mut self, conn: &mut Connection) {
        let (approval_policy, sandbox, _) = map::policy(self.spec.config.access);
        let cwd = self.spec.cwd.to_string_lossy();
        let model = (!self.spec.config.model.is_empty()).then_some(self.spec.config.model.as_str());
        let mut params = ThreadOpenParams {
            thread_id: None,
            last_turn_id: None,
            exclude_turns: None,
            cwd: &cwd,
            model,
            approval_policy,
            approvals_reviewer: "user",
            sandbox,
        };
        // A thread this session already opened wins over the spec: after a
        // crash it is the one to resume, even if the spec asked for a fork.
        let (method, kind) = match (self.thread_id.as_deref(), self.spec.resume.as_deref(), self.spec.resume_at.as_deref()) {
            (Some(id), _, _) => {
                params.thread_id = Some(id);
                params.exclude_turns = Some(true);
                ("thread/resume", Open::Resume)
            }
            (None, Some(id), Some(at)) => {
                // Rollback / edit-and-resend: a fork that keeps history up to
                // and including turn `at`, leaving the original untouched.
                params.thread_id = Some(id);
                params.last_turn_id = Some(at);
                params.exclude_turns = Some(true);
                ("thread/fork", Open::Fork)
            }
            (None, Some(id), None) => {
                params.thread_id = Some(id);
                params.exclude_turns = Some(true);
                ("thread/resume", Open::Resume)
            }
            (None, None, _) => ("thread/start", Open::Start),
        };
        let id = conn.request(method, &params);
        self.calls.insert(id, (Call::ThreadOpen(kind), Some(Instant::now() + REQUEST_TIMEOUT)));
    }

    fn on_thread_open(&mut self, conn: &mut Connection, kind: Open, result: Result<&str, RpcError>) -> Option<Exit> {
        let response = match result {
            Ok(raw) => sonic_rs::from_str::<ThreadOpenResponse>(raw).map_err(|e| e.to_string()),
            Err(err) => Err(err.message),
        };
        match response {
            Ok(response) => {
                self.thread_id = Some(response.thread.id.clone());
                if response.model.as_deref().is_some_and(|m| !m.is_empty()) {
                    self.model = response.model;
                }
                if kind == Open::Fork {
                    self.spec.resume_at = None;
                }
                self.spec.resume = Some(response.thread.id.clone());
                self.ready = true;
                self.ready_once = true;
                self.emit(AgentEvent::Ready { session_id: response.thread.id, model: self.model.clone() });
                self.start_next(conn);
                None
            }
            Err(message) if kind == Open::Start => Some(Exit::Fatal(format!("Codex couldn't start a thread: {message}"))),
            Err(message) => {
                // The old thread is gone or unreadable: say so and carry on
                // in a new thread rather than leaving the user stuck.
                let what = if kind == Open::Fork { "branch from" } else { "resume" };
                self.emit(AgentEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("Couldn't {what} the Codex thread ({message}). Started a new thread; earlier context is not included."),
                });
                self.thread_id = None;
                self.spec.resume = None;
                self.spec.resume_at = None;
                self.open_thread(conn);
                None
            }
        }
    }

    fn on_turn_start(&mut self, conn: &mut Connection, seq: u64, result: Result<&str, RpcError>) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.seq == seq) else { return };
        match result.map(sonic_rs::from_str::<TurnStartResponse>) {
            Ok(Ok(response)) => {
                // The response is authoritative for the id.
                turn.turn_id = Some(response.turn.id.clone());
                self.turn_id_known(conn, &response.turn.id);
            }
            Ok(Err(err)) => log::warn!("codex: turn/start response: {err}"),
            Err(err) => {
                let info = err.data.as_ref().and_then(|d| d.get("codexErrorInfo")).map(|v| ErrorInfo::parse(&v.clone()));
                let outcome = if info.is_some_and(|i| i.is_usage_limit()) {
                    TurnOutcome::RateLimited { resets_ms: self.rate_limits.as_ref().map(map::reset_ms).unwrap_or(0) }
                } else {
                    TurnOutcome::Failed { message: format!("Codex couldn't start the turn: {}", err.message) }
                };
                self.finish_turn(conn, outcome, Some(0));
            }
        }
    }

    /// Sends what waited for the turn id: a stop, then any steers.
    fn turn_id_known(&mut self, conn: &mut Connection, turn_id: &str) {
        let Some(turn) = &mut self.turn else { return };
        if std::mem::take(&mut turn.interrupt_pending)
            && let Some(thread_id) = self.thread_id.as_deref()
        {
            let id = conn.request("turn/interrupt", &json!({"threadId": thread_id, "turnId": turn_id}));
            self.calls.insert(id, (Call::Interrupt, None));
            return;
        }
        let waiting = std::mem::take(&mut turn.steer_waiting);
        for prompt in waiting {
            self.send_steer(conn, turn_id, prompt);
        }
    }

    fn is_stale(&self, turn_id: Option<&str>) -> bool {
        turn_id.is_some_and(|id| self.stale_turns.iter().any(|s| s == id))
    }

    fn on_notification(&mut self, conn: &mut Connection, method: &str, params: &str) {
        let turn_id = || sonic_rs::get_from_str(params, ["turnId"]).ok().and_then(|v| v.as_str().map(str::to_owned));
        match method {
            "item/agentMessage/delta" | "item/plan/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta"
            | "item/commandExecution/outputDelta" => {
                let Ok(d) = sonic_rs::from_str::<DeltaNotification>(params) else { return };
                if d.delta.is_empty() || self.is_stale(d.turn_id.as_deref()) {
                    return;
                }
                let event = match method {
                    "item/commandExecution/outputDelta" => AgentEvent::ToolOutput { call_id: d.item_id, text: d.delta },
                    "item/reasoning/summaryTextDelta" => {
                        let index = d.summary_index.unwrap_or(0);
                        let last = self.reasoning_parts.insert(d.item_id.clone(), index);
                        let text = if last.is_some_and(|l| l != index) { format!("\n\n{}", d.delta) } else { d.delta };
                        AgentEvent::Delta { block: d.item_id, kind: BlockKind::Reasoning, text }
                    }
                    "item/reasoning/textDelta" => {
                        AgentEvent::Delta { block: format!("{}:raw", d.item_id), kind: BlockKind::Reasoning, text: d.delta }
                    }
                    _ => AgentEvent::Delta { block: d.item_id, kind: BlockKind::Text, text: d.delta },
                };
                self.emit(event);
            }
            "item/started" | "item/completed" => {
                if self.is_stale(turn_id().as_deref()) {
                    return;
                }
                let Ok(n) = sonic_rs::from_str::<ItemNotification>(params) else { return };
                let raw = n.item.as_raw_str();
                let Ok(head) = sonic_rs::from_str::<ItemHead>(raw) else { return };
                if method == "item/started" {
                    self.item_started(&head, raw);
                } else {
                    self.item_completed(&head, raw);
                }
            }
            "turn/started" => {
                let Ok(n) = sonic_rs::from_str::<TurnNotification>(params) else { return };
                let id = n.turn.id;
                let stale = self.is_stale(Some(&id));
                match &mut self.turn {
                    Some(turn) if turn.turn_id.is_none() => {
                        turn.turn_id = Some(id.clone());
                        self.turn_id_known(conn, &id);
                    }
                    Some(_) => {}
                    None if stale => {}
                    None => {
                        // A turn Codex started by itself.
                        self.turn_seq += 1;
                        self.turn = Some(ActiveTurn {
                            seq: self.turn_seq,
                            turn_id: Some(id),
                            started: Instant::now(),
                            usage: Usage::default(),
                            interrupt_pending: false,
                            interrupt_deadline: None,
                            failure: None,
                            plan: None,
                            steer_waiting: Vec::new(),
                        });
                        self.emit(AgentEvent::TurnStarted { prompt: None });
                    }
                }
            }
            "turn/completed" => {
                let Ok(n) = sonic_rs::from_str::<TurnNotification>(params) else { return };
                let Some(turn) = &self.turn else { return };
                if turn.turn_id.as_deref().is_some_and(|id| id != n.turn.id) {
                    log::debug!("codex: completion for turn {} ignored", n.turn.id);
                    return;
                }
                let error = n.turn.error.as_ref().or(turn.failure.as_ref());
                let outcome = map::outcome(n.turn.status.as_deref(), error, self.rate_limits.as_ref());
                if self.turn.as_ref().is_some_and(|t| t.turn_id.is_none())
                    && let Some(turn) = &mut self.turn
                {
                    turn.turn_id = Some(n.turn.id);
                }
                self.finish_turn(conn, outcome, n.turn.duration_ms);
            }
            "turn/plan/updated" => {
                if let Ok(update) = sonic_rs::from_str::<PlanUpdatedNotification>(params) {
                    self.emit(AgentEvent::Plan(map::plan(&update)));
                }
            }
            "thread/tokenUsage/updated" => {
                let Ok(n) = sonic_rs::from_str::<TokenUsageNotification>(params) else { return };
                let prev = self.usage_total.replace(n.token_usage.total);
                if let Some(turn) = &mut self.turn {
                    map::add_usage(&mut turn.usage, prev, &n.token_usage);
                    let usage = turn.usage;
                    self.emit(AgentEvent::Usage(usage));
                }
            }
            "account/rateLimits/updated" => {
                if let Ok(n) = sonic_rs::from_str::<RateLimitsNotification>(params) {
                    self.on_rate_limits(n.rate_limits);
                }
            }
            "error" => {
                let Ok(n) = sonic_rs::from_str::<ErrorNotification>(params) else { return };
                if n.will_retry {
                    self.emit(AgentEvent::Notice {
                        level: NoticeLevel::Warning,
                        text: format!("{} Retrying…", map::error_message(&n.error)),
                    });
                } else if let Some(turn) = &mut self.turn {
                    turn.failure = Some(n.error);
                } else {
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Error, text: map::error_message(&n.error) });
                }
            }
            "thread/name/updated" => {
                if let Ok(n) = sonic_rs::from_str::<ThreadNameNotification>(params)
                    && let Some(name) = n.thread_name.filter(|n| !n.trim().is_empty())
                {
                    self.emit(AgentEvent::TitleChanged(name));
                }
            }
            "serverRequest/resolved" => {
                let Ok(n) = sonic_rs::from_str::<ServerRequestResolved>(params) else { return };
                let raw_id = n.request_id.as_raw_str();
                let refs: Vec<String> =
                    self.requests.iter().filter(|(_, p)| p.raw_id == raw_id).map(|(r, _)| r.clone()).collect();
                for provider_ref in refs {
                    if let Some(Pending { kind: PendingKind::Question { group, .. }, .. }) = self.requests.remove(&provider_ref) {
                        self.groups.remove(&group);
                    }
                    self.emit(AgentEvent::RequestWithdrawn { provider_ref });
                }
            }
            "model/rerouted" => {
                if let Ok(n) = sonic_rs::from_str::<ModelRerouted>(params) {
                    let reason = n.reason.as_ref().and_then(|r| r.as_str()).map(|r| format!(" ({})", humanize_id(r))).unwrap_or_default();
                    self.emit(AgentEvent::Notice {
                        level: NoticeLevel::Info,
                        text: format!("Codex switched from {} to {}{reason}.", n.from_model, n.to_model),
                    });
                }
            }
            "warning" | "configWarning" | "deprecationNotice" | "guardianWarning" => {
                if let Ok(n) = sonic_rs::from_str::<WarningNotification>(params) {
                    let mut text = n.message.or(n.summary).unwrap_or_default();
                    if let Some(details) = n.details.filter(|d| !d.is_empty()) {
                        text = format!("{text}\n{details}");
                    }
                    if !text.trim().is_empty() {
                        self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text });
                    }
                }
            }
            "mcpServer/startupStatus/updated" => {
                if let Ok(n) = sonic_rs::from_str::<McpServerStatus>(params)
                    && n.status == "failed"
                {
                    let error = n.error.map(|e| format!(": {e}")).unwrap_or_default();
                    self.emit(AgentEvent::Notice { level: NoticeLevel::Warning, text: format!("MCP server {} failed to start{error}", n.name) });
                }
            }
            _ => {}
        }
    }

    fn on_rate_limits(&mut self, snapshot: RateLimitSnapshot) {
        for event in map::quota_events(&snapshot) {
            self.emit(event);
        }
        self.rate_limits = Some(snapshot);
    }

    fn item_started(&mut self, head: &ItemHead, raw: &str) {
        match head.kind.as_str() {
            "fileChange" => {
                if let Ok(item) = sonic_rs::from_str::<FileChangeItem>(raw) {
                    self.item_files.insert(head.id.clone(), item.changes);
                }
            }
            "commandExecution" => {
                if let Ok(item) = sonic_rs::from_str::<CommandItem>(raw) {
                    self.item_commands.insert(head.id.clone(), item.command);
                }
            }
            _ => {}
        }
        if let Some(call) = map::tool_call(head, raw)
            && self.open_tools.insert(head.id.clone())
        {
            self.emit(AgentEvent::ToolStarted(call));
        }
    }

    fn item_completed(&mut self, head: &ItemHead, raw: &str) {
        let id = head.id.clone();
        match head.kind.as_str() {
            "agentMessage" => {
                let text = sonic_rs::from_str::<TextItem>(raw).map(|i| i.text).unwrap_or_default();
                self.emit(AgentEvent::BlockDone { block: id, kind: BlockKind::Text, text });
            }
            "plan" => {
                let text = sonic_rs::from_str::<TextItem>(raw).map(|i| i.text).unwrap_or_default();
                if let Some(turn) = &mut self.turn
                    && !text.trim().is_empty()
                {
                    turn.plan = Some(text.clone());
                }
                self.emit(AgentEvent::BlockDone { block: id, kind: BlockKind::Text, text });
            }
            "reasoning" => {
                let Ok(item) = sonic_rs::from_str::<ReasoningItem>(raw) else { return };
                self.reasoning_parts.remove(&id);
                if !item.summary.is_empty() {
                    self.emit(AgentEvent::BlockDone { block: id.clone(), kind: BlockKind::Reasoning, text: item.summary.join("\n\n") });
                }
                if !item.content.is_empty() {
                    self.emit(AgentEvent::BlockDone { block: format!("{id}:raw"), kind: BlockKind::Reasoning, text: item.content.join("\n\n") });
                }
            }
            "contextCompaction" => {
                self.emit(AgentEvent::Notice { level: NoticeLevel::Info, text: "Codex compacted the conversation to free context.".into() })
            }
            _ => {
                let Some(mut call) = map::tool_call(head, raw) else { return };
                if self.open_tools.insert(id.clone()) {
                    if let Some(title) = map::completed_title(&head.kind, raw) {
                        call.title = title;
                    }
                    self.emit(AgentEvent::ToolStarted(call.clone()));
                }
                self.open_tools.remove(&id);
                let (output, is_error, exit_code) = map::tool_result(&head.kind, raw);
                self.item_files.remove(&id);
                self.item_commands.remove(&id);
                self.emit(AgentEvent::ToolFinished { call_id: id, output, is_error, exit_code, files: call.files });
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Request params

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadOpenParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_turns: Option<bool>,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    approval_policy: Value,
    approvals_reviewer: &'a str,
    sandbox: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams<'a> {
    thread_id: &'a str,
    input: Vec<Input<'a>>,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
    summary: &'a str,
    approval_policy: Value,
    approvals_reviewer: &'a str,
    sandbox_policy: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    collaboration_mode: Option<CollaborationMode<'a>>,
}

#[derive(Serialize)]
struct CollaborationMode<'a> {
    mode: &'a str,
    settings: CollaborationSettings<'a>,
}

#[derive(Serialize)]
struct CollaborationSettings<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SteerParams<'a> {
    thread_id: &'a str,
    expected_turn_id: &'a str,
    input: Vec<Input<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Input<'a> {
    Text { text: String },
    LocalImage { path: &'a str },
}

/// The prompt as Codex input: the text, attachment paths for files Codex
/// reads itself, and images by path (no base64 copy in memory).
fn user_input(prompt: &Prompt) -> Vec<Input<'_>> {
    let mut text = prompt.text.clone();
    let mut images = Vec::new();
    let mut files: Vec<&PromptAttachment> = Vec::new();
    for a in &prompt.attachments {
        match a.path.to_str() {
            Some(path) if a.attachment.mime.starts_with("image/") => images.push(Input::LocalImage { path }),
            _ => files.push(a),
        }
    }
    if !files.is_empty() {
        text.push_str("\n\nAttached files:");
        for a in files {
            text.push_str(&format!("\n- {}: {}", a.attachment.name, a.path.display()));
        }
    }
    let mut input = vec![Input::Text { text }];
    input.extend(images);
    input
}
