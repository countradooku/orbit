//! Claude Code's stream-json protocol → [`AgentEvent`]s.
//!
//! The parser is synchronous and does no process I/O, so recorded sessions
//! replay through it in tests. Each line is scanned once into a targeted
//! struct; nested values Orbit does not need stay unparsed [`LazyValue`]s, and
//! deltas never build a JSON tree.
//!
//! What it fixes relative to T3 Code:
//! * tool input streams (`input_json_delta`) and the tool card appears at
//!   `content_block_start`, not when the block completes;
//! * turn outcomes come from `terminal_reason` / `subtype`, never from
//!   substring matches on error text;
//! * "allow for session" adds rules scoped to the command or file, never a
//!   blanket `{toolName}` rule, and never writes to settings files;
//! * `AskUserQuestion` supports several questions and multi-select, sends
//!   multi-select answers as arrays (no comma ambiguity) and keys requests
//!   by index, so duplicate question texts cannot collide on Orbit's side;
//! * approvals that arrive outside a turn (background agents) are shown,
//!   never auto-denied.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use orbit_proto::*;
use serde::Deserialize;
use sonic_rs::LazyValue;

use super::catalog::{self, Catalog, InitResponse};
use super::tools;
use crate::provider::{AgentEvent, BlockKind, TurnOutcome};

/// Request id of the `initialize` control request.
pub const INIT_ID: &str = "orbit_init";
/// Prefix of interrupt control request ids.
pub const INTERRUPT_PREFIX: &str = "orbit_interrupt_";

/// What one line produced.
#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<AgentEvent>,
    /// JSON lines to write to the CLI's stdin, in order.
    pub replies: Vec<String>,
    /// The `initialize` response arrived.
    pub initialized: bool,
    /// The CLI refused an interrupt; the session falls back to killing it.
    pub interrupt_failed: bool,
    /// The user approved leaving plan mode.
    pub plan_approved: bool,
}

impl Output {
    pub fn clear(&mut self) {
        self.events.clear();
        self.replies.clear();
        self.initialized = false;
        self.interrupt_failed = false;
        self.plan_approved = false;
    }
}

// ---------------------------------------------------------------- frames

#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    subtype: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    uuid: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    session_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    parent_tool_use_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    user_message_uuids: Option<Vec<Cow<'a, str>>>,
    #[serde(borrow, default)]
    event: Option<StreamEvent<'a>>,
    #[serde(borrow, default)]
    message: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    error: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    tool_use_result: Option<LazyValue<'a>>,
    // control protocol
    #[serde(borrow, default)]
    request_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    request: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    response: Option<LazyValue<'a>>,
    // system
    #[serde(borrow, default)]
    status: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow, default, rename = "permissionMode")]
    permission_mode: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    commands: Option<Vec<catalog::Command<'a>>>,
    #[serde(borrow, default)]
    title: Option<Cow<'a, str>>,
    #[serde(default)]
    attempt: Option<u32>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(borrow, default)]
    compact_metadata: Option<CompactMeta<'a>>,
    // command_lifecycle
    #[serde(borrow, default)]
    command_uuid: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    state: Option<Cow<'a, str>>,
    // rate_limit_event
    #[serde(borrow, default)]
    rate_limit_info: Option<RateLimitInfo<'a>>,
    // result
    #[serde(default)]
    is_error: bool,
    #[serde(borrow, default)]
    terminal_reason: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    result: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    errors: Vec<Cow<'a, str>>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    api_error_status: Option<u64>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default, rename = "modelUsage")]
    model_usage: Option<HashMap<String, ModelUsage>>,
    #[serde(default)]
    model_usage_since_last_result: Option<HashMap<String, ModelUsage>>,
}

#[derive(Deserialize)]
struct StreamEvent<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
    #[serde(default)]
    index: u32,
    #[serde(borrow, default)]
    content_block: Option<ContentBlock<'a>>,
    #[serde(borrow, default)]
    delta: Option<Delta<'a>>,
    #[serde(borrow, default)]
    message: Option<StartMessage<'a>>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct ContentBlock<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    thinking: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    input: Option<LazyValue<'a>>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(borrow, rename = "type", default)]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    thinking: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    partial_json: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct StartMessage<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct AssistantMessage<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    content: Vec<ContentBlock<'a>>,
}

#[derive(Deserialize)]
struct UserMessage<'a> {
    #[serde(borrow, default)]
    content: Option<LazyValue<'a>>,
}

#[derive(Deserialize)]
struct ToolResult<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    tool_use_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    content: Option<LazyValue<'a>>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(borrow, rename = "type")]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    iterations: Option<LastIteration>,
}

/// Only the last `iterations[type=message]` entry matters (context math).
#[derive(Clone, Copy, Debug, Default)]
struct LastIteration(Option<[u64; 4]>);

impl<'de> Deserialize<'de> for LastIteration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct It<'a> {
            #[serde(default)]
            input_tokens: u64,
            #[serde(default)]
            output_tokens: u64,
            #[serde(default)]
            cache_creation_input_tokens: u64,
            #[serde(default)]
            cache_read_input_tokens: u64,
            #[serde(borrow, rename = "type", default)]
            ty: Option<Cow<'a, str>>,
        }
        let all: Vec<It> = Vec::deserialize(d)?;
        Ok(LastIteration(all.iter().rev().find(|i| i.ty.as_deref().is_none_or(|t| t == "message")).map(|i| {
            [i.input_tokens, i.output_tokens, i.cache_creation_input_tokens, i.cache_read_input_tokens]
        })))
    }
}

impl RawUsage {
    /// Tokens in the context after this message (report §5).
    fn context(&self) -> u64 {
        match self.iterations.and_then(|i| i.0) {
            Some([i, o, cw, cr]) => i + o + cw + cr,
            None => self.input_tokens + self.output_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens,
        }
    }

    fn add_to(&self, u: &mut Usage) {
        u.input_tokens += self.input_tokens;
        u.output_tokens += self.output_tokens;
        u.cache_write_tokens += self.cache_creation_input_tokens;
        u.cache_read_tokens += self.cache_read_input_tokens;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsage {
    #[serde(default, rename = "costUSD")]
    cost_usd: f64,
    #[serde(default)]
    context_window: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo<'a> {
    #[serde(borrow, default)]
    status: Option<Cow<'a, str>>,
    #[serde(default)]
    resets_at: Option<f64>,
    #[serde(borrow, default)]
    rate_limit_type: Option<Cow<'a, str>>,
    #[serde(default)]
    utilization: Option<f32>,
    #[serde(default)]
    unified_windows: Option<HashMap<String, Window<'a>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window<'a> {
    #[serde(default)]
    utilization: Option<f32>,
    #[serde(default)]
    resets_at: Option<f64>,
    #[serde(borrow, default)]
    status: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct CompactMeta<'a> {
    #[serde(borrow, default)]
    trigger: Option<Cow<'a, str>>,
    #[serde(default)]
    pre_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ControlRequest<'a> {
    #[serde(borrow, default)]
    subtype: Cow<'a, str>,
    #[serde(borrow, default)]
    tool_name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    input: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    permission_suggestions: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    tool_use_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    description: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    decision_reason: Option<Cow<'a, str>>,
}

/// The input of a `hook_callback` control request.
#[derive(Deserialize)]
struct HookInput<'a> {
    #[serde(borrow, default)]
    hook_event_name: Cow<'a, str>,
    #[serde(borrow, default)]
    tool_name: Cow<'a, str>,
    #[serde(borrow, default)]
    tool_input: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    tool_use_id: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ControlResponse<'a> {
    #[serde(borrow, default)]
    subtype: Cow<'a, str>,
    #[serde(borrow, default)]
    request_id: Cow<'a, str>,
    #[serde(borrow, default)]
    error: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    response: Option<LazyValue<'a>>,
}

#[derive(Deserialize)]
struct Applied<'a> {
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    mode: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Questions<'a> {
    #[serde(borrow, default)]
    questions: Vec<QuestionIn<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuestionIn<'a> {
    #[serde(borrow, default)]
    question: Cow<'a, str>,
    #[serde(borrow, default)]
    options: Vec<OptionIn<'a>>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Deserialize)]
struct OptionIn<'a> {
    #[serde(borrow, default)]
    label: Cow<'a, str>,
    #[serde(borrow, default)]
    description: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Suggestion<'a> {
    #[serde(borrow, rename = "type", default)]
    ty: Cow<'a, str>,
    #[serde(borrow, default)]
    rules: Vec<Rule<'a>>,
    #[serde(borrow, default)]
    behavior: Option<Cow<'a, str>>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Rule<'a> {
    #[serde(borrow)]
    tool_name: Cow<'a, str>,
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    rule_content: Option<Cow<'a, str>>,
}

// ---------------------------------------------------------------- state

struct Turn {
    prompt: Option<String>,
    started: Instant,
    /// Completed API messages of this turn.
    usage: Usage,
    text_seen: bool,
    rate_limited: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    Tool,
}

struct Block {
    id: String,
    kind: Kind,
    name: String,
    buf: String,
    done: bool,
}

struct Message {
    id: String,
    blocks: HashMap<u32, Block>,
    /// Next block index an `assistant` snapshot content entry maps to.
    snapshot_next: u32,
    usage: RawUsage,
}

struct Pending {
    /// A `PreToolUse` hook call the guard holds: answered with a hook
    /// decision instead of a permission result.
    hook: bool,
    tool: String,
    input: String,
    tool_use_id: Option<String>,
    suggestions: Option<String>,
    questions: Vec<Question>,
}

struct Question {
    text: String,
    options: Vec<QuestionOption>,
    multi: bool,
    answer: Option<Answer>,
}

enum Answer {
    One(String),
    Many(Vec<String>),
}

pub struct Parser {
    cwd: PathBuf,
    binary: Option<String>,
    pub access: Access,
    pub session_id: String,
    /// The model slug the thread asked for (`""` for the default).
    pub requested_model: String,
    /// The API model the CLI runs.
    pub model: Option<String>,
    /// The CLI's current permission mode, as it last reported it.
    pub permission_mode: Option<String>,
    pub catalog: Option<Catalog>,
    /// Context windows other sessions learned, merged into the catalog.
    pub known_windows: HashMap<String, u64>,
    turn: Option<Turn>,
    message: Option<Message>,
    /// Tool names by call id, to finish calls and read their results.
    tools: HashMap<String, String>,
    requests: HashMap<String, Pending>,
    started_prompts: HashSet<String>,
    last_assistant: Option<String>,
    total_cost: f64,
    /// Reset time of a rejected quota window.
    rejected_reset_ms: Option<u64>,
    pub interrupting: bool,
    /// Tool calls matching it are held for a person through the
    /// `PreToolUse` hook (see [`guard_hooks`]), in every permission mode.
    pub guard: GuardSettings,
    /// Plugin hooks that veto tool calls through the same hook.
    pub tool_policy: crate::provider::ToolPolicy,
}

impl Parser {
    pub fn new(cwd: PathBuf, session_id: String, access: Access, binary: Option<String>) -> Self {
        Self {
            cwd,
            binary,
            access,
            session_id,
            requested_model: String::new(),
            model: None,
            permission_mode: None,
            catalog: None,
            known_windows: HashMap::new(),
            turn: None,
            message: None,
            tools: HashMap::new(),
            requests: HashMap::new(),
            started_prompts: HashSet::new(),
            last_assistant: None,
            total_cost: 0.0,
            rejected_reset_ms: None,
            interrupting: false,
            guard: GuardSettings { enabled: false, rules: Vec::new() },
            tool_policy: Default::default(),
        }
    }

    pub fn turn_active(&self) -> bool {
        self.turn.is_some()
    }

    /// Requests waiting on the user.
    pub fn pending_requests(&self) -> usize {
        self.requests.len()
    }

    /// Feeds one stdout line.
    pub fn feed(&mut self, line: &[u8], out: &mut Output) {
        let frame: Frame = match sonic_rs::from_slice(line) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("claude: unparsable frame ({e}): {}", String::from_utf8_lossy(&line[..line.len().min(200)]));
                return;
            }
        };
        match frame.ty.as_ref() {
            "stream_event" => self.on_stream(&frame, out),
            "assistant" => self.on_assistant(&frame, out),
            "user" => self.on_user(&frame, out),
            "system" => self.on_system(&frame, out),
            "command_lifecycle" => self.on_lifecycle(&frame, out),
            "rate_limit_event" => self.on_rate_limit(&frame, out),
            "result" => self.on_result(&frame, out),
            "control_request" => self.on_control_request(&frame, out),
            "control_cancel_request" => self.on_cancel(&frame, out),
            "control_response" => self.on_control_response(&frame, out),
            _ => {}
        }
    }

    fn first_prompt(f: &Frame) -> Option<String> {
        f.user_message_uuids.as_ref().and_then(|u| u.first()).map(|u| u.to_string())
    }

    /// Opens a turn if none is running. `prompt` is `None` for turns the CLI
    /// started by itself.
    fn ensure_turn(&mut self, prompt: Option<String>, out: &mut Output) {
        if self.turn.is_some() {
            return;
        }
        if let Some(p) = &prompt {
            self.started_prompts.insert(p.clone());
        }
        self.turn =
            Some(Turn { prompt: prompt.clone(), started: Instant::now(), usage: Usage::default(), text_seen: false, rate_limited: false });
        self.last_assistant = None;
        out.events.push(AgentEvent::TurnStarted { prompt });
    }

    fn window(&self) -> u64 {
        let (Some(c), Some(m)) = (&self.catalog, &self.model) else { return 0 };
        c.windows.get(m).copied().unwrap_or(0)
    }

    fn running_usage(&self) -> Usage {
        let mut u = self.turn.as_ref().map(|t| t.usage).unwrap_or_default();
        if let Some(m) = &self.message {
            m.usage.add_to(&mut u);
            u.context_tokens = m.usage.context();
        }
        u.context_window = self.window();
        u
    }

    // ------------------------------------------------------------ stream

    fn on_stream(&mut self, f: &Frame, out: &mut Output) {
        if f.parent_tool_use_id.is_some() {
            return; // subagent internals; summarized from its snapshots
        }
        let Some(ev) = &f.event else { return };
        self.ensure_turn(Self::first_prompt(f), out);
        match ev.ty.as_ref() {
            "message_start" => {
                self.finish_message();
                let msg = ev.message.as_ref();
                if let Some(model) = msg.and_then(|m| m.model.as_deref()).filter(|m| *m != "<synthetic>") {
                    self.model = Some(model.to_owned());
                }
                self.message = Some(Message {
                    id: msg.and_then(|m| m.id.as_deref()).unwrap_or_default().to_owned(),
                    blocks: HashMap::new(),
                    snapshot_next: 0,
                    usage: msg.and_then(|m| m.usage).unwrap_or_default(),
                });
                out.events.push(AgentEvent::Usage(self.running_usage()));
            }
            "content_block_start" => {
                let Some(cb) = &ev.content_block else { return };
                let msg = self.message_mut(None);
                let id = format!("{}:{}", msg.id, ev.index);
                let (kind, name) = match cb.ty.as_ref() {
                    "text" => (Kind::Text, String::new()),
                    "thinking" | "redacted_thinking" => (Kind::Reasoning, String::new()),
                    "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                        (Kind::Tool, cb.name.as_deref().unwrap_or_default().to_owned())
                    }
                    _ => return,
                };
                let block_id = if kind == Kind::Tool { cb.id.as_deref().unwrap_or(&id).to_owned() } else { id };
                let mut buf = String::new();
                if let Some(t) = cb.text.as_deref().or(cb.thinking.as_deref()) {
                    buf.push_str(t);
                }
                msg.blocks.insert(ev.index, Block { id: block_id.clone(), kind, name: name.clone(), buf, done: false });
                if kind == Kind::Tool {
                    self.tools.insert(block_id.clone(), name.clone());
                    out.events.push(AgentEvent::ToolStarted(tools::tool_call(&block_id, &name, "", &self.cwd)));
                }
            }
            "content_block_delta" => {
                let Some(d) = &ev.delta else { return };
                let Some(block) = self.message.as_mut().and_then(|m| m.blocks.get_mut(&ev.index)) else { return };
                if block.done {
                    return;
                }
                let (text, kind) = match d.ty.as_ref() {
                    "text_delta" => (d.text.as_deref(), BlockKind::Text),
                    "thinking_delta" => (d.thinking.as_deref(), BlockKind::Reasoning),
                    "input_json_delta" => (d.partial_json.as_deref(), BlockKind::ToolInput),
                    _ => return,
                };
                let Some(text) = text.filter(|t| !t.is_empty()) else { return };
                block.buf.push_str(text);
                if kind == BlockKind::Text
                    && let Some(t) = self.turn.as_mut()
                {
                    t.text_seen = true;
                }
                out.events.push(AgentEvent::Delta { block: block.id.clone(), kind, text: text.to_owned() });
            }
            "content_block_stop" => {
                let Some(block) = self.message.as_mut().and_then(|m| m.blocks.get_mut(&ev.index)) else { return };
                if block.done {
                    return;
                }
                block.done = true;
                let (kind, id, name, buf) = (block.kind, block.id.clone(), block.name.clone(), std::mem::take(&mut block.buf));
                self.emit_block(kind, &id, &name, &buf, out);
            }
            "message_delta" => {
                if let (Some(u), Some(m)) = (ev.usage, self.message.as_mut()) {
                    m.usage = u;
                }
                out.events.push(AgentEvent::Usage(self.running_usage()));
            }
            "message_stop" => self.finish_message(),
            _ => {}
        }
    }

    /// Folds the current API message's usage into the turn.
    fn finish_message(&mut self) {
        if let (Some(m), Some(t)) = (self.message.take(), self.turn.as_mut()) {
            m.usage.add_to(&mut t.usage);
            t.usage.context_tokens = m.usage.context();
        }
    }

    fn message_mut(&mut self, id: Option<&str>) -> &mut Message {
        let stale = match (&self.message, id) {
            (None, _) => true,
            (Some(m), Some(id)) => !m.id.is_empty() && m.id != id,
            _ => false,
        };
        if stale {
            self.finish_message();
            self.message = Some(Message {
                id: id.unwrap_or_default().to_owned(),
                blocks: HashMap::new(),
                snapshot_next: 0,
                usage: RawUsage::default(),
            });
        }
        self.message.as_mut().unwrap()
    }

    /// Emits the final form of a completed block.
    fn emit_block(&mut self, kind: Kind, id: &str, name: &str, content: &str, out: &mut Output) {
        match kind {
            Kind::Text | Kind::Reasoning => {
                if content.is_empty() {
                    return;
                }
                let kind = if kind == Kind::Text { BlockKind::Text } else { BlockKind::Reasoning };
                if kind == BlockKind::Text
                    && let Some(t) = self.turn.as_mut()
                {
                    t.text_seen = true;
                }
                out.events.push(AgentEvent::BlockDone { block: id.to_owned(), kind, text: content.to_owned() });
            }
            Kind::Tool => {
                let input = if content.trim().is_empty() { "{}" } else { content };
                out.events.push(AgentEvent::ToolStarted(tools::tool_call(id, name, input, &self.cwd)));
                if let Some(steps) = tools::plan_steps(name, input) {
                    out.events.push(AgentEvent::Plan(steps));
                }
            }
        }
    }

    // ------------------------------------------------------------ snapshots

    fn on_assistant(&mut self, f: &Frame, out: &mut Output) {
        let Some(raw) = &f.message else { return };
        let msg: AssistantMessage = match sonic_rs::from_str(raw.as_raw_str()) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("claude: bad assistant message: {e}");
                return;
            }
        };
        if let Some(parent) = &f.parent_tool_use_id {
            // A subagent's step: show it as progress of the parent task.
            for b in msg.content.iter().filter(|b| b.ty == "tool_use") {
                let name = b.name.as_deref().unwrap_or_default();
                let input = b.input.as_ref().map(|i| i.as_raw_str()).unwrap_or("{}");
                let call = tools::tool_call(b.id.as_deref().unwrap_or_default(), name, input, &self.cwd);
                out.events.push(AgentEvent::ToolOutput { call_id: parent.to_string(), text: format!("{}: {}\n", name, call.title) });
            }
            return;
        }
        self.ensure_turn(Self::first_prompt(f), out);
        let synthetic = msg.model.as_deref() == Some("<synthetic>");
        if !synthetic {
            if let Some(m) = &msg.model {
                self.model = Some(m.to_string());
            }
            if let Some(uuid) = &f.uuid {
                self.last_assistant = Some(uuid.to_string());
            }
        }
        if f.error.as_deref() == Some("rate_limit")
            && let Some(t) = self.turn.as_mut()
        {
            t.rate_limited = true;
        }
        for b in &msg.content {
            let message = self.message_mut(msg.id.as_deref());
            let index = message.snapshot_next;
            message.snapshot_next += 1;
            let (kind, content, name, call_id): (Kind, Cow<str>, &str, Option<&str>) = match b.ty.as_ref() {
                "text" => (Kind::Text, b.text.clone().unwrap_or_default(), "", None),
                "thinking" => (Kind::Reasoning, b.thinking.clone().unwrap_or_default(), "", None),
                "tool_use" | "server_tool_use" | "mcp_tool_use" => (
                    Kind::Tool,
                    Cow::Borrowed(b.input.as_ref().map(|i| i.as_raw_str()).unwrap_or("{}")),
                    b.name.as_deref().unwrap_or_default(),
                    b.id.as_deref(),
                ),
                _ => continue,
            };
            let fallback_id = format!("{}:{}", message.id, index);
            let block = message.blocks.entry(index).or_insert_with(|| Block {
                id: call_id.map(str::to_owned).unwrap_or(fallback_id),
                kind,
                name: name.to_owned(),
                buf: String::new(),
                done: false,
            });
            if block.done || block.kind != kind {
                continue;
            }
            block.done = true;
            block.buf.clear();
            let id = block.id.clone();
            if kind == Kind::Tool && !self.tools.contains_key(&id) {
                // Never streamed: the card starts now.
                self.tools.insert(id.clone(), name.to_owned());
                out.events.push(AgentEvent::ToolStarted(tools::tool_call(&id, name, "", &self.cwd)));
            }
            self.emit_block(kind, &id, name, &content, out);
        }
    }

    fn on_user(&mut self, f: &Frame, out: &mut Output) {
        if f.parent_tool_use_id.is_some() {
            return;
        }
        let Some(raw) = &f.message else { return };
        let Ok(msg) = sonic_rs::from_str::<UserMessage>(raw.as_raw_str()) else { return };
        let Some(content) = msg.content else { return };
        let raw = content.as_raw_str();
        if !raw.starts_with('[') {
            return; // a plain string: a prompt echo
        }
        let Ok(results) = sonic_rs::from_str::<Vec<ToolResult>>(raw) else { return };
        let results: Vec<_> = results.iter().filter(|r| r.ty == "tool_result").collect();
        for r in &results {
            let Some(call_id) = r.tool_use_id.as_deref() else { continue };
            let name = self.tools.remove(call_id).unwrap_or_default();
            let output = r.content.as_ref().map(content_text).unwrap_or_default();
            let files = match (&f.tool_use_result, results.len()) {
                (Some(res), 1) => tools::result_changes(&name, res.as_raw_str()).unwrap_or_default(),
                _ => Vec::new(),
            };
            let exit_code = if tools::kind_of(&name) == ToolKind::Command && name != "BashOutput" {
                if r.is_error { exit_code_of(&output) } else { Some(0) }
            } else {
                None
            };
            out.events.push(AgentEvent::ToolFinished {
                call_id: call_id.to_owned(),
                output,
                is_error: r.is_error,
                exit_code,
                files,
            });
        }
    }

    // ------------------------------------------------------------ system

    fn on_system(&mut self, f: &Frame, out: &mut Output) {
        match f.subtype.as_deref().unwrap_or_default() {
            "init" => {
                if let Some(id) = &f.session_id
                    && *id != self.session_id
                {
                    self.session_id = id.to_string();
                    out.events.push(AgentEvent::Ready { session_id: self.session_id.clone(), model: f.model.as_deref().map(str::to_owned) });
                }
                if let Some(m) = &f.model {
                    self.model = Some(m.to_string());
                }
                if let Some(mode) = &f.permission_mode {
                    self.permission_mode = Some(mode.to_string());
                }
                self.ensure_turn(Self::first_prompt(f), out);
            }
            "status" => {
                match f.status.as_deref() {
                    Some("requesting") => self.ensure_turn(Self::first_prompt(f), out),
                    Some("compacting") => out
                        .events
                        .push(AgentEvent::Notice { level: NoticeLevel::Info, text: "Compacting the conversation".into() }),
                    _ => {}
                }
                if let Some(mode) = &f.permission_mode {
                    self.permission_mode = Some(mode.to_string());
                }
            }
            "compact_boundary" => {
                let meta = f.compact_metadata.as_ref();
                let text = match meta.and_then(|m| m.pre_tokens) {
                    Some(pre) => format!("Conversation compacted ({} tokens before)", pre),
                    None => "Conversation compacted".to_owned(),
                };
                let text = match meta.and_then(|m| m.trigger.as_deref()) {
                    Some("auto") => format!("{text}, automatically"),
                    _ => text,
                };
                out.events.push(AgentEvent::Notice { level: NoticeLevel::Info, text });
            }
            "api_retry" => {
                let text = match (f.attempt, f.max_retries) {
                    (Some(a), Some(m)) => format!("The API request failed; retrying ({a} of {m})"),
                    _ => "The API request failed; retrying".to_owned(),
                };
                out.events.push(AgentEvent::Notice { level: NoticeLevel::Warning, text });
            }
            "commands_changed" => {
                let (Some(commands), Some(catalog)) = (&f.commands, self.catalog.as_mut()) else { return };
                let next: Vec<SlashCommand> = commands.iter().map(catalog::slash_command).collect();
                if next != catalog.info.slash_commands {
                    catalog.info.slash_commands = next;
                    catalog::remember(catalog);
                    out.events.push(AgentEvent::Catalog(catalog.info.clone()));
                }
            }
            "session_title_changed" => {
                if let Some(t) = f.title.as_deref().filter(|t| !t.is_empty()) {
                    out.events.push(AgentEvent::TitleChanged(t.to_owned()));
                }
            }
            _ => {}
        }
    }

    fn on_lifecycle(&mut self, f: &Frame, out: &mut Output) {
        let Some(uuid) = f.command_uuid.as_deref() else { return };
        match f.state.as_deref() {
            Some("started") => match &self.turn {
                None => self.ensure_turn(Some(uuid.to_owned()), out),
                Some(t) if t.prompt.as_deref() != Some(uuid) && self.started_prompts.insert(uuid.to_owned()) => {
                    out.events.push(AgentEvent::PromptAccepted { uuid: uuid.to_owned() });
                }
                Some(_) => {}
            },
            Some("discarded" | "refused") if !self.started_prompts.contains(uuid) => {
                out.events.push(AgentEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: "Claude Code did not accept a queued message; send it again".into(),
                });
            }
            Some("completed" | "cancelled" | "discarded" | "refused") => {
                self.started_prompts.remove(uuid);
            }
            _ => {}
        }
    }

    fn on_rate_limit(&mut self, f: &Frame, out: &mut Output) {
        let Some(info) = &f.rate_limit_info else { return };
        let ms = |s: Option<f64>| s.map(|s| (s * 1000.0) as u64).unwrap_or(0);
        let mut rejected_any = false;
        match &info.unified_windows {
            Some(windows) if !windows.is_empty() => {
                let mut names: Vec<_> = windows.keys().collect();
                names.sort();
                for name in names {
                    let w = &windows[name];
                    let rejected = w.status.as_deref() == Some("rejected");
                    rejected_any |= rejected;
                    if rejected {
                        self.rejected_reset_ms = Some(ms(w.resets_at));
                    }
                    out.events.push(AgentEvent::Quota { window: name.clone(), utilization: w.utilization, resets_ms: ms(w.resets_at), rejected });
                }
            }
            _ => {
                let rejected = info.status.as_deref() == Some("rejected");
                rejected_any = rejected;
                if rejected {
                    self.rejected_reset_ms = Some(ms(info.resets_at));
                }
                out.events.push(AgentEvent::Quota {
                    window: info.rate_limit_type.as_deref().unwrap_or("default").to_owned(),
                    utilization: info.utilization,
                    resets_ms: ms(info.resets_at),
                    rejected,
                });
            }
        }
        if info.status.as_deref() == Some("rejected") || rejected_any {
            if let Some(t) = self.turn.as_mut() {
                t.rate_limited = true;
            }
        } else {
            self.rejected_reset_ms = None;
        }
    }

    // ------------------------------------------------------------ result

    fn on_result(&mut self, f: &Frame, out: &mut Output) {
        self.ensure_turn(Self::first_prompt(f), out);
        self.finish_message();
        let turn = self.turn.take().expect("turn opened above");
        let mut catalog_changed = false;
        if let Some(models) = &f.model_usage {
            for (model, mu) in models {
                if let Some(c) = self.catalog.as_mut() {
                    catalog_changed |= c.learn_window(model, mu.context_window);
                } else {
                    catalog::remember_window(model, mu.context_window);
                }
            }
        }
        if catalog_changed && let Some(c) = &self.catalog {
            out.events.push(AgentEvent::Catalog(c.info.clone()));
        }
        let success = f.subtype.as_deref() == Some("success") && !f.is_error;
        if success
            && !turn.text_seen
            && let Some(text) = f.result.as_deref().filter(|t| !t.trim().is_empty())
        {
            let block = format!("{}:result", f.uuid.as_deref().unwrap_or("turn"));
            out.events.push(AgentEvent::BlockDone { block, kind: BlockKind::Text, text: text.to_owned() });
        }

        let mut usage = Usage::default();
        match f.usage {
            Some(u) => {
                u.add_to(&mut usage);
                usage.context_tokens = u.context();
            }
            None => usage = turn.usage,
        }
        if usage.context_tokens == 0 {
            usage.context_tokens = turn.usage.context_tokens;
        }
        let window = f
            .model_usage
            .as_ref()
            .and_then(|m| self.model.as_ref().and_then(|model| m.get(model)).or_else(|| m.values().max_by_key(|u| u.context_window)))
            .map(|u| u.context_window)
            .unwrap_or(0);
        usage.context_window = if window > 0 { window } else { self.window() };
        let cost = match (&f.model_usage_since_last_result, f.total_cost_usd) {
            (Some(m), _) if !m.is_empty() => m.values().map(|u| u.cost_usd).sum::<f64>(),
            (_, Some(total)) => (total - self.total_cost).max(0.0),
            _ => 0.0,
        };
        if let Some(total) = f.total_cost_usd {
            self.total_cost = total;
        }
        usage.cost_micros = (cost * 1_000_000.0).round() as u64;

        let outcome = self.outcome(f, &turn);
        if let Some(uuid) = self.last_assistant.take() {
            out.events.push(AgentEvent::Checkpoint { provider_message: uuid });
        }
        let duration_ms = f.duration_ms.unwrap_or_else(|| turn.started.elapsed().as_millis() as u64);
        out.events.push(AgentEvent::TurnFinished { outcome, usage, duration_ms });
        self.interrupting = false;
        self.message = None;
        self.tools.clear();
    }

    fn outcome(&self, f: &Frame, turn: &Turn) -> TurnOutcome {
        let subtype = f.subtype.as_deref().unwrap_or_default();
        let reason = f.terminal_reason.as_deref();
        if matches!(reason, Some("aborted_tools" | "aborted_streaming")) || (self.interrupting && subtype != "success") {
            return TurnOutcome::Interrupted;
        }
        if subtype == "success" && !f.is_error && reason.is_none_or(|r| r == "completed") {
            return TurnOutcome::Completed;
        }
        if f.api_error_status == Some(429) || turn.rate_limited || self.rejected_reset_ms.is_some() && f.is_error {
            return TurnOutcome::RateLimited { resets_ms: self.rejected_reset_ms.unwrap_or(0) };
        }
        let errors: Vec<&str> = f
            .errors
            .iter()
            .flat_map(|e| e.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("[ede_diagnostic]"))
            .collect();
        let message = if subtype == "success"
            && let Some(r) = f.result.as_deref().filter(|r| !r.trim().is_empty())
        {
            r.trim().to_owned()
        } else if !errors.is_empty() {
            errors.join("\n")
        } else {
            match reason.filter(|r| *r != "completed") {
                Some(r) => humanize_id(r),
                None => humanize_id(subtype.trim_start_matches("error_")),
            }
        };
        TurnOutcome::Failed { message }
    }

    /// Ends the running turn without a result (the process died or was
    /// killed). No-op when no turn runs.
    pub fn force_finish(&mut self, outcome: TurnOutcome, out: &mut Output) {
        self.finish_message();
        let Some(turn) = self.turn.take() else { return };
        for (id, p) in self.requests.drain() {
            for r in request_refs(&id, &p) {
                out.events.push(AgentEvent::RequestWithdrawn { provider_ref: r });
            }
        }
        if let Some(uuid) = self.last_assistant.take() {
            out.events.push(AgentEvent::Checkpoint { provider_message: uuid });
        }
        let mut usage = turn.usage;
        usage.context_window = self.window();
        out.events.push(AgentEvent::TurnFinished { outcome, usage, duration_ms: turn.started.elapsed().as_millis() as u64 });
        self.interrupting = false;
        self.message = None;
        self.tools.clear();
    }

    // ------------------------------------------------------------ control

    fn on_control_response(&mut self, f: &Frame, out: &mut Output) {
        let Some(raw) = &f.response else { return };
        let Ok(r) = sonic_rs::from_str::<ControlResponse>(raw.as_raw_str()) else { return };
        let ok = r.subtype == "success";
        if r.request_id == INIT_ID {
            let parsed = r.response.as_ref().and_then(|v| sonic_rs::from_str::<InitResponse>(v.as_raw_str()).ok());
            match parsed {
                Some(init) if ok => {
                    let catalog = Catalog::from_init(&init, self.binary.clone()).with_windows(&self.known_windows);
                    self.permission_mode = init.current_permission_mode.as_deref().map(str::to_owned);
                    self.model = Some(catalog.api_model(&self.requested_model).to_owned());
                    catalog::remember(&catalog);
                    out.events.push(AgentEvent::Catalog(catalog.info.clone()));
                    self.catalog = Some(catalog);
                }
                _ => out.events.push(AgentEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("Claude Code did not report its models: {}", r.error.as_deref().unwrap_or("no details")),
                }),
            }
            out.initialized = true;
            out.events.push(AgentEvent::Ready { session_id: self.session_id.clone(), model: self.model.clone() });
            return;
        }
        if ok {
            // `set_model` answers {model}, `set_permission_mode` {mode}.
            if let Some(v) = &r.response
                && let Ok(set) = sonic_rs::from_str::<Applied>(v.as_raw_str())
            {
                if let Some(m) = set.model {
                    self.model = Some(m.into_owned());
                }
                if let Some(m) = set.mode {
                    self.permission_mode = Some(m.into_owned());
                }
            }
            return;
        }
        if r.request_id.starts_with(INTERRUPT_PREFIX) {
            out.interrupt_failed = true;
            return;
        }
        out.events.push(AgentEvent::Notice {
            level: NoticeLevel::Warning,
            text: format!("Claude Code refused a setting change: {}", r.error.as_deref().unwrap_or("no details")),
        });
    }

    fn on_cancel(&mut self, f: &Frame, out: &mut Output) {
        let Some(id) = f.request_id.as_deref() else { return };
        if let Some(p) = self.requests.remove(id) {
            for r in request_refs(id, &p) {
                out.events.push(AgentEvent::RequestWithdrawn { provider_ref: r });
            }
        }
    }

    fn on_control_request(&mut self, f: &Frame, out: &mut Output) {
        let Some(id) = f.request_id.as_deref() else { return };
        let Some(raw) = &f.request else { return };
        let Ok(req) = sonic_rs::from_str::<ControlRequest>(raw.as_raw_str()) else {
            out.replies.push(error_reply(id, "Orbit could not read this request"));
            return;
        };
        if req.subtype == "hook_callback" {
            self.on_guard_hook(id, &req, out);
            return;
        }
        if req.subtype != "can_use_tool" {
            out.replies.push(error_reply(id, &format!("Orbit does not support {}", req.subtype)));
            return;
        }
        let tool = req.tool_name.as_deref().unwrap_or_default().to_owned();
        let input = req.input.as_ref().map(|i| i.as_raw_str().to_owned()).unwrap_or_else(|| "{}".to_owned());
        let tool_use_id = req.tool_use_id.as_deref().map(str::to_owned);
        if self.access == Access::Readonly && !tools::READONLY_TOOLS.contains(&tool.as_str()) {
            let msg = format!(
                "This thread is read-only in Orbit, so {tool} is not available. Continue with read-only tools, or tell the user what you would change."
            );
            out.replies.push(deny_reply(id, &msg, tool_use_id.as_deref(), false));
            return;
        }
        let mut pending = Pending {
            hook: false,
            tool: tool.clone(),
            input,
            tool_use_id,
            suggestions: req.permission_suggestions.as_ref().map(|s| s.as_raw_str().to_owned()),
            questions: Vec::new(),
        };
        if tool == "AskUserQuestion"
            && let Ok(q) = sonic_rs::from_str::<Questions>(&pending.input)
            && !q.questions.is_empty()
        {
            pending.questions = q
                .questions
                .iter()
                .map(|q| Question {
                    text: q.question.to_string(),
                    options: q.options.iter().map(|o| QuestionOption::new(o.label.trim(), o.description.trim())).collect(),
                    multi: q.multi_select,
                    answer: None,
                })
                .collect();
            for (i, q) in pending.questions.iter().enumerate() {
                out.events.push(AgentEvent::Request {
                    provider_ref: format!("{id}#{i}"),
                    kind: RequestKind::Question { question: q.text.clone(), options: q.options.clone(), multi_select: q.multi },
                });
            }
        } else {
            let kind = if tool == "ExitPlanMode" {
                RequestKind::Plan { plan: tools::plan_text(&pending.input) }
            } else {
                let reason = req.description.as_deref().or(req.decision_reason.as_deref());
                tools::request_kind(&tool, &pending.input, reason, &self.cwd)
            };
            out.events.push(AgentEvent::Request { provider_ref: id.to_owned(), kind });
        }
        self.requests.insert(id.to_owned(), pending);
    }

    /// A `PreToolUse` hook call (registered by [`guard_hooks`]): a tool
    /// call the guard matches is raised as a request and the hook answers
    /// once the person decides; anything else continues untouched.
    fn on_guard_hook(&mut self, id: &str, req: &ControlRequest, out: &mut Output) {
        let hook = req.input.as_ref().and_then(|i| sonic_rs::from_str::<HookInput>(i.as_raw_str()).ok());
        let Some(hook) = hook.filter(|h| h.hook_event_name == "PreToolUse") else {
            out.replies.push(success_reply(id, "{}"));
            return;
        };
        let tool = hook.tool_name.to_string();
        let input = hook.tool_input.as_ref().map(|i| i.as_raw_str().to_owned()).unwrap_or_else(|| "{}".to_owned());
        let kind = tools::request_kind(&tool, &input, None, &self.cwd);
        // A plugin's veto applies in every mode, before anyone is asked.
        if let Some(reason) = self.tool_policy.check(&kind) {
            out.replies.push(hook_reply(id, "deny", &reason));
            out.events.push(AgentEvent::Notice { level: NoticeLevel::Warning, text: reason });
            return;
        }
        // Read-only threads are refused by the permission check; let it.
        if self.access == Access::Readonly || self.guard.check(&kind).is_none() {
            out.replies.push(success_reply(id, "{}"));
            return;
        }
        let tool_use_id = hook.tool_use_id.as_deref().or(req.tool_use_id.as_deref()).map(str::to_owned);
        out.events.push(AgentEvent::Request { provider_ref: id.to_owned(), kind });
        self.requests.insert(
            id.to_owned(),
            Pending { hook: true, tool, input, tool_use_id, suggestions: None, questions: Vec::new() },
        );
    }

    /// Applies the user's decision. Returns false for an unknown request.
    pub fn resolve(&mut self, provider_ref: &str, decision: &Decision, out: &mut Output) -> bool {
        let (id, index) = match provider_ref.rsplit_once('#') {
            Some((id, i)) if i.parse::<usize>().is_ok() => (id, i.parse::<usize>().ok()),
            _ => (provider_ref, None),
        };
        let Some(pending) = self.requests.get_mut(id) else { return false };
        let tool_use_id = pending.tool_use_id.clone();

        if let Some(i) = index.filter(|_| !pending.questions.is_empty()) {
            let Some(q) = pending.questions.get_mut(i) else { return false };
            match decision {
                Decision::Deny { feedback } => {
                    let p = self.requests.remove(id).unwrap();
                    let msg = if feedback.trim().is_empty() {
                        "The user declined to answer.".to_owned()
                    } else {
                        format!("The user declined to answer and said: {feedback}")
                    };
                    out.replies.push(deny_reply(id, &msg, tool_use_id.as_deref(), false));
                    for (j, q) in p.questions.iter().enumerate() {
                        if j != i && q.answer.is_none() {
                            out.events.push(AgentEvent::RequestWithdrawn { provider_ref: format!("{id}#{j}") });
                        }
                    }
                    return true;
                }
                Decision::Answer { text } => {
                    q.answer = Some(if q.multi {
                        Answer::Many(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
                    } else {
                        Answer::One(text.clone())
                    });
                }
                Decision::Allow | Decision::AllowForSession => {
                    let first = q.options.first().map(|o| o.label.clone()).unwrap_or_default();
                    q.answer = Some(if q.multi { Answer::Many(vec![first]) } else { Answer::One(first) });
                }
            }
            if pending.questions.iter().all(|q| q.answer.is_some()) {
                let p = self.requests.remove(id).unwrap();
                let input = with_answers(&p.input, &p.questions);
                out.replies.push(allow_reply(id, &input, tool_use_id.as_deref(), None));
            }
            return true;
        }

        let p = self.requests.remove(id).unwrap();
        if p.hook {
            let (verdict, reason) = match decision {
                Decision::Allow | Decision::AllowForSession => ("allow", "Approved in Orbit.".to_owned()),
                Decision::Deny { feedback } | Decision::Answer { text: feedback } if !feedback.trim().is_empty() => {
                    ("deny", format!("The user declined this tool call and said: {feedback}"))
                }
                _ => ("deny", "The user declined this tool call.".to_owned()),
            };
            out.replies.push(hook_reply(id, verdict, &reason));
            return true;
        }
        let approve_plan = p.tool == "ExitPlanMode" && matches!(decision, Decision::Allow | Decision::AllowForSession);
        if approve_plan {
            // Leave plan mode for the thread's access level in the same
            // message, as Claude Code's own plan approval does.
            let mode = format!(
                r#"[{{"type":"setMode","mode":"{}","destination":"session"}}]"#,
                access_mode(self.access)
            );
            out.replies.push(allow_reply(id, &p.input, tool_use_id.as_deref(), Some(&mode)));
            out.plan_approved = true;
            return true;
        }
        match decision {
            Decision::Allow => out.replies.push(allow_reply(id, &p.input, tool_use_id.as_deref(), None)),
            Decision::AllowForSession => {
                let perms = session_rules(&p, &self.cwd);
                out.replies.push(allow_reply(id, &p.input, tool_use_id.as_deref(), perms.as_deref()));
            }
            Decision::Deny { feedback } | Decision::Answer { text: feedback } => {
                let msg = match (p.tool.as_str(), feedback.trim().is_empty()) {
                    ("ExitPlanMode", true) => "The user wants to keep planning. Ask what to change.".to_owned(),
                    ("ExitPlanMode", false) => format!("The user wants changes to the plan: {feedback}"),
                    (_, true) => "The user declined this tool call.".to_owned(),
                    (_, false) => format!("The user declined this tool call and said: {feedback}"),
                };
                out.replies.push(deny_reply(id, &msg, tool_use_id.as_deref(), false));
            }
        }
        true
    }
}

/// The CLI permission mode for an access level outside plan mode.
pub fn access_mode(access: Access) -> &'static str {
    match access {
        Access::Readonly | Access::Ask => "default",
        Access::AutoEdit => "acceptEdits",
        Access::Full => "bypassPermissions",
    }
}

fn request_refs(id: &str, p: &Pending) -> Vec<String> {
    if p.questions.is_empty() {
        vec![id.to_owned()]
    } else {
        (0..p.questions.len()).filter(|i| p.questions[*i].answer.is_none()).map(|i| format!("{id}#{i}")).collect()
    }
}

/// Text of a tool result's `content` (a string or content parts).
fn content_text(v: &LazyValue) -> String {
    let raw = v.as_raw_str();
    if raw.starts_with('"') {
        return sonic_rs::from_str::<String>(raw).unwrap_or_default();
    }
    let Ok(parts) = sonic_rs::from_str::<Vec<Part>>(raw) else { return String::new() };
    let mut text = String::new();
    for p in parts {
        if !text.is_empty() {
            text.push('\n');
        }
        match (p.ty.as_ref(), p.text) {
            ("text", Some(t)) => text.push_str(&t),
            ("image", _) => text.push_str("[image]"),
            _ => {}
        }
    }
    text
}

/// `Exit code N` at the start of a failed command's output.
fn exit_code_of(output: &str) -> Option<i32> {
    let rest = output.trim_start().strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

fn json_str(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

pub fn error_reply(id: &str, message: &str) -> String {
    format!(
        r#"{{"type":"control_response","response":{{"subtype":"error","request_id":{},"error":{}}}}}"#,
        json_str(id),
        json_str(message)
    )
}

fn allow_reply(id: &str, input: &str, tool_use_id: Option<&str>, permissions: Option<&str>) -> String {
    let mut body = format!(r#"{{"behavior":"allow","updatedInput":{input}"#);
    if let Some(t) = tool_use_id {
        body.push_str(&format!(r#","toolUseID":{}"#, json_str(t)));
    }
    if let Some(p) = permissions {
        body.push_str(&format!(r#","updatedPermissions":{p}"#));
    }
    body.push('}');
    success_reply(id, &body)
}

fn deny_reply(id: &str, message: &str, tool_use_id: Option<&str>, interrupt: bool) -> String {
    let mut body = format!(r#"{{"behavior":"deny","message":{}"#, json_str(message));
    if let Some(t) = tool_use_id {
        body.push_str(&format!(r#","toolUseID":{}"#, json_str(t)));
    }
    if interrupt {
        body.push_str(r#","interrupt":true"#);
    }
    body.push('}');
    success_reply(id, &body)
}

/// The callback id Orbit registers its guard hook under.
pub const GUARD_HOOK: &str = "orbit_guard";

/// The `hooks` value of the `initialize` request: a `PreToolUse` hook on
/// the tools that can do harm, when a guard is active. A hook's decision
/// applies in every permission mode, `bypassPermissions` included, so a
/// guarded call reaches a person even under Full access. With plugin
/// tool hooks (`plugins`) every tool call goes through it.
pub fn guard_hooks(guard: &GuardSettings, access: Access, plugins: bool) -> &'static str {
    if plugins {
        // Plugin hooks see every tool call: no matcher.
        concat!(r#"{"PreToolUse":[{"hookCallbackIds":[""#, "orbit_guard", r#""]}]}"#)
    } else if guard.active() && access != Access::Readonly {
        concat!(
            r#"{"PreToolUse":[{"matcher":"Bash|PowerShell|Write|Edit|MultiEdit|NotebookEdit|Read","hookCallbackIds":[""#,
            "orbit_guard",
            r#""]}]}"#
        )
    } else {
        "null"
    }
}

fn hook_reply(id: &str, decision: &str, reason: &str) -> String {
    let body = format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"{decision}","permissionDecisionReason":{}}}}}"#,
        json_str(reason)
    );
    success_reply(id, &body)
}

fn success_reply(id: &str, body: &str) -> String {
    format!(r#"{{"type":"control_response","response":{{"subtype":"success","request_id":{},"response":{body}}}}}"#, json_str(id))
}

/// The tool input with an `answers` object appended: one entry per question
/// text, an array for multi-select questions.
fn with_answers(input: &str, questions: &[Question]) -> String {
    let mut answers = String::from("{");
    for (i, q) in questions.iter().enumerate() {
        if i > 0 {
            answers.push(',');
        }
        answers.push_str(&json_str(&q.text));
        answers.push(':');
        match &q.answer {
            Some(Answer::One(a)) => answers.push_str(&json_str(a)),
            Some(Answer::Many(list)) => {
                answers.push('[');
                answers.push_str(&list.iter().map(|a| json_str(a)).collect::<Vec<_>>().join(","));
                answers.push(']');
            }
            None => answers.push_str("\"\""),
        }
    }
    answers.push('}');
    let body = input.trim_end();
    let Some(open) = body.strip_suffix('}') else { return format!(r#"{{"answers":{answers}}}"#) };
    let has_fields = open.trim_end() != "{" && !open.trim_end().is_empty();
    format!("{}{}\"answers\":{answers}}}", open, if has_fields { "," } else { "" })
}

/// Session-scoped allow rules for "allow for session": the CLI's own
/// suggestions when they are scoped to this command, file or domain, else a
/// rule Orbit scopes itself. Never a bare `{toolName}` rule for a built-in
/// tool, never `setMode` (that would silently change the access level) and
/// never a settings-file destination.
fn session_rules(p: &Pending, cwd: &std::path::Path) -> Option<String> {
    let scoped_ok = |r: &Rule| r.rule_content.as_deref().is_some_and(|c| !c.is_empty()) || (r.tool_name.starts_with("mcp__") && r.tool_name == p.tool);
    let mut rules: Vec<Rule> = Vec::new();
    let suggestions: Vec<Suggestion> =
        p.suggestions.as_deref().and_then(|s| sonic_rs::from_str(s).ok()).unwrap_or_default();
    for s in &suggestions {
        if s.ty == "addRules" && s.behavior.as_deref().is_none_or(|b| b == "allow") && !s.rules.is_empty() && s.rules.iter().all(scoped_ok) {
            rules.extend(s.rules.iter().map(|r| Rule { tool_name: r.tool_name.clone(), rule_content: r.rule_content.clone() }));
        }
    }
    if rules.is_empty() {
        match tools::kind_of(&p.tool) {
            ToolKind::Command if p.tool == "Bash" || p.tool == "PowerShell" => {
                let cmd = tools::command_of(&p.input)?;
                rules.push(Rule { tool_name: Cow::Borrowed(p.tool.as_str()), rule_content: Some(Cow::Owned(cmd)) });
            }
            ToolKind::Edit => {
                let file = tools::file_of(&p.input)?;
                rules.push(Rule { tool_name: Cow::Borrowed("Edit"), rule_content: Some(Cow::Owned(path_rule(&file, cwd))) });
            }
            _ => return None,
        }
    }
    let rules = sonic_rs::to_string(&rules).ok()?;
    Some(format!(r#"[{{"type":"addRules","rules":{rules},"behavior":"allow","destination":"session"}}]"#))
}

/// A permission-rule path for one file: relative to the session directory
/// when inside it, else absolute (`//` prefix), with forward slashes.
fn path_rule(file: &str, cwd: &std::path::Path) -> String {
    let path = std::path::Path::new(file);
    match path.strip_prefix(cwd) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => format!("//{}", file.replace('\\', "/").trim_start_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_appended_to_the_input() {
        let qs = vec![
            Question { text: "Pick".into(), options: vec![], multi: true, answer: Some(Answer::Many(vec!["Red, warm".into(), "Blue".into()])) },
            Question { text: "Size".into(), options: vec![], multi: false, answer: Some(Answer::One("L".into())) },
        ];
        let out = with_answers(r#"{"questions":[]}"#, &qs);
        assert_eq!(out, r#"{"questions":[],"answers":{"Pick":["Red, warm","Blue"],"Size":"L"}}"#);
        assert_eq!(with_answers("{}", &qs[1..]), r#"{"answers":{"Size":"L"}}"#);
    }

    #[test]
    fn exit_codes_are_read_strictly() {
        assert_eq!(exit_code_of("Exit code 2\nerror: x"), Some(2));
        assert_eq!(exit_code_of("command failed, exit code 2"), None);
    }

    #[test]
    fn the_guard_hook_holds_matching_calls_in_every_mode() {
        let mut parser = Parser::new("/repo".into(), "s".into(), Access::Full, None);
        parser.guard = GuardSettings::default();
        assert!(guard_hooks(&parser.guard, Access::Full, false).contains(GUARD_HOOK));
        assert_eq!(guard_hooks(&parser.guard, Access::Readonly, false), "null");
        let hook = |id: &str, command: &str| {
            format!(
                r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"hook_callback","callback_id":"orbit_guard","tool_use_id":"tu_{id}","input":{{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{command}"}}}}}}}}"#
            )
        };
        // Unguarded: the hook lets the call continue untouched.
        let mut out = Output::default();
        parser.feed(hook("h1", "cargo test").as_bytes(), &mut out);
        assert!(out.events.is_empty());
        assert!(out.replies[0].contains(r#""request_id":"h1","response":{}"#), "{}", out.replies[0]);
        // Guarded: raised as a request and answered with a hook decision.
        let mut out = Output::default();
        parser.feed(hook("h2", "git push --force").as_bytes(), &mut out);
        assert!(out.replies.is_empty());
        assert!(matches!(&out.events[..], [AgentEvent::Request { provider_ref, kind: RequestKind::Command { command, .. } }]
            if provider_ref == "h2" && command == "git push --force"));
        let mut out = Output::default();
        assert!(parser.resolve("h2", &Decision::AllowForSession, &mut out));
        assert!(out.replies[0].contains(r#""permissionDecision":"allow""#), "{}", out.replies[0]);
        let mut out = Output::default();
        parser.feed(hook("h3", "rm -rf /").as_bytes(), &mut out);
        let mut out = Output::default();
        parser.resolve("h3", &Decision::Deny { feedback: "no".into() }, &mut out);
        assert!(out.replies[0].contains(r#""permissionDecision":"deny""#) && out.replies[0].contains("said: no"));
    }

    #[test]
    fn plugin_policies_veto_every_tool_through_the_hook() {
        let mut parser = Parser::new("/repo".into(), "s".into(), Access::Full, None);
        parser.tool_policy = crate::provider::ToolPolicy::new(
            |kind| match kind {
                RequestKind::Command { command, .. } if command.contains("rm -rf /") => Some("Blocked by the Guard plugin: no".into()),
                _ => None,
            },
            true,
        );
        let hooks = guard_hooks(&parser.guard, Access::Readonly, true);
        assert!(hooks.contains(GUARD_HOOK) && !hooks.contains("matcher"), "every tool, every mode: {hooks}");
        let hook = |id: &str, command: &str| {
            format!(
                r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"hook_callback","callback_id":"orbit_guard","input":{{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{command}"}}}}}}}}"#
            )
        };
        let mut out = Output::default();
        parser.feed(hook("p1", "rm -rf /").as_bytes(), &mut out);
        assert!(out.replies[0].contains(r#""permissionDecision":"deny""#) && out.replies[0].contains("Guard plugin"), "{}", out.replies[0]);
        assert!(matches!(&out.events[..], [AgentEvent::Notice { level: NoticeLevel::Warning, .. }]));
        let mut out = Output::default();
        parser.feed(hook("p2", "ls").as_bytes(), &mut out);
        assert!(out.replies[0].contains(r#""request_id":"p2","response":{}"#), "{}", out.replies[0]);
    }

    #[test]
    fn path_rules() {
        let cwd = std::path::Path::new("/repo");
        assert_eq!(path_rule("/repo/src/a.rs", cwd), "src/a.rs");
        assert_eq!(path_rule("/etc/hosts", cwd), "//etc/hosts");
    }
}
