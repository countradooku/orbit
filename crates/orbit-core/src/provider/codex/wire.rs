//! The slice of the `codex app-server` protocol (v2) Orbit reads.
//!
//! Every struct names only the fields the adapter uses; sonic-rs skips the
//! rest without materializing them, so a `turn/completed` carrying the whole
//! turn's items costs a scan, not an allocation per item. Shapes follow
//! `codex-rs/app-server-protocol/src/protocol/v2` (camelCase on the wire).

use std::borrow::Cow;

use serde::Deserialize;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, LazyValue, Value};

/// One JSON-RPC line. There is no `jsonrpc` field: `method` + `id` is a
/// server request, `method` alone a notification, `id` alone a response.
#[derive(Deserialize)]
pub struct RpcLine<'a> {
    #[serde(borrow, default)]
    pub id: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    pub method: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub params: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    pub result: Option<LazyValue<'a>>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RpcError {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// JSON-RPC "method not found": the app-server predates the method.
pub const METHOD_NOT_FOUND: i64 = -32601;

// ---------------------------------------------------------------------------
// Responses

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(default)]
    pub user_agent: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    #[serde(default)]
    pub data: Vec<Model>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
    #[serde(default)]
    pub default_service_tier: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServiceTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountResponse {
    #[serde(default)]
    pub account: Option<Account>,
    #[serde(default)]
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
}

#[derive(Deserialize)]
pub struct CollaborationModeListResponse {
    #[serde(default)]
    pub data: Vec<CollaborationModeMask>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CollaborationModeMask {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Deserialize)]
pub struct SkillsListResponse {
    #[serde(default)]
    pub data: Vec<SkillsListEntry>,
}

#[derive(Deserialize)]
pub struct SkillsListEntry {
    #[serde(default)]
    pub skills: Vec<Skill>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub short_description: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ConfigReadResponse {
    #[serde(default)]
    pub config: Config,
}

#[derive(Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub model_reasoning_summary: Option<String>,
}

/// `thread/start`, `thread/resume` and `thread/fork` all answer with the
/// thread and the model it runs.
#[derive(Deserialize)]
pub struct ThreadOpenResponse {
    pub thread: ThreadRef,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Deserialize)]
pub struct ThreadRef {
    pub id: String,
}

#[derive(Deserialize)]
pub struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsResponse {
    pub rate_limits: RateLimitSnapshot,
    /// Every limit by its id (`codex`, a model-specific one…), when the
    /// server has more than one.
    #[serde(default)]
    pub rate_limits_by_limit_id: Option<std::collections::BTreeMap<String, RateLimitSnapshot>>,
}

// ---------------------------------------------------------------------------
// Notifications

#[derive(Deserialize)]
pub struct TurnNotification {
    pub turn: Turn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub additional_details: Option<String>,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: TurnError,
    #[serde(default)]
    pub will_retry: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification<'a> {
    #[serde(borrow)]
    pub item: LazyValue<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaNotification {
    pub item_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub delta: String,
    #[serde(default)]
    pub summary_index: Option<u32>,
}

#[derive(Deserialize)]
pub struct PlanUpdatedNotification {
    #[serde(default)]
    pub plan: Vec<PlanStepWire>,
}

#[derive(Deserialize)]
pub struct PlanStepWire {
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageNotification {
    pub token_usage: ThreadTokenUsage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    pub total: TokenBreakdown,
    pub last: TokenBreakdown,
    #[serde(default)]
    pub model_context_window: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsNotification {
    pub rate_limits: RateLimitSnapshot,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
    #[serde(default)]
    pub rate_limit_reached_type: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    #[serde(default)]
    pub used_percent: f64,
    #[serde(default)]
    pub window_duration_mins: Option<i64>,
    /// Unix seconds.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadNameNotification {
    #[serde(default)]
    pub thread_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolved<'a> {
    #[serde(borrow)]
    pub request_id: LazyValue<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRerouted {
    #[serde(default)]
    pub from_model: String,
    #[serde(default)]
    pub to_model: String,
    #[serde(default)]
    pub reason: Option<Value>,
}

/// `warning`, `configWarning`, `deprecationNotice`, `guardianWarning`.
#[derive(Deserialize)]
pub struct WarningNotification {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
}

#[derive(Deserialize)]
pub struct McpServerStatus {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Thread items

#[derive(Deserialize)]
pub struct ItemHead {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
}

#[derive(Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

#[derive(Deserialize)]
pub struct ReasoningItem {
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandItem {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub command_actions: Vec<CommandAction>,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommandAction {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Deserialize)]
pub struct FileChangeItem {
    #[serde(default)]
    pub changes: Vec<FileUpdateChange>,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PatchChangeKind {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub move_path: Option<String>,
}

#[derive(Deserialize)]
pub struct McpToolCallItem {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<Value>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub result: Option<McpResult>,
    #[serde(default)]
    pub error: Option<MessageOnly>,
}

#[derive(Deserialize)]
pub struct McpResult {
    #[serde(default)]
    pub content: Vec<Value>,
}

#[derive(Deserialize)]
pub struct MessageOnly {
    #[serde(default)]
    pub message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallItem {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<Value>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub content_items: Option<Vec<Value>>,
}

#[derive(Deserialize)]
pub struct WebSearchItem {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub action: Option<Value>,
}

#[derive(Deserialize)]
pub struct PathItem {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize)]
pub struct CollabAgentItem {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageGenerationItem {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub saved_path: Option<String>,
    #[serde(default)]
    pub revised_prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// Server requests

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApprovalParams {
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub network_approval_context: Option<NetworkContext>,
}

#[derive(Deserialize)]
pub struct NetworkContext {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub protocol: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileApprovalParams {
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub grant_root: Option<String>,
}

#[derive(Deserialize)]
pub struct UserInputParams {
    #[serde(default)]
    pub questions: Vec<UserInputQuestion>,
}

#[derive(Deserialize)]
pub struct UserInputQuestion {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Deserialize)]
pub struct UserInputOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationParams {
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Deserialize)]
pub struct PermissionsParams<'a> {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(borrow)]
    pub permissions: LazyValue<'a>,
}

// ---------------------------------------------------------------------------
// Error classification

/// `codexErrorInfo`, the structured cause Codex attaches to turn errors.
/// Unit variants arrive as camelCase strings, variants with data as a
/// single-key object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorInfo {
    Code(String),
    /// A transport variant (`httpConnectionFailed`, `responseStream*`,
    /// `responseTooManyFailedAttempts`) with its upstream HTTP status.
    Http { kind: String, status: Option<u16> },
    Other,
}

impl ErrorInfo {
    pub fn parse(value: &Value) -> Self {
        if let Some(code) = value.as_str() {
            return ErrorInfo::Code(code.to_owned());
        }
        if let Some(obj) = value.as_object()
            && let Some((key, inner)) = obj.iter().next()
        {
            let status = inner.get("httpStatusCode").and_then(|s| s.as_u64()).map(|s| s as u16);
            return ErrorInfo::Http { kind: key.to_owned(), status };
        }
        ErrorInfo::Other
    }

    /// The quota ran out: the turn can be retried once the window resets.
    /// Spend caps (`apiKeySpendLimitExceeded`…) do not reset on their own
    /// and are reported as failures instead.
    pub fn is_usage_limit(&self) -> bool {
        match self {
            ErrorInfo::Code(code) => matches!(code.as_str(), "usageLimitExceeded" | "rateLimitExceeded"),
            ErrorInfo::Http { kind, status } => kind == "responseTooManyFailedAttempts" && *status == Some(429),
            ErrorInfo::Other => false,
        }
    }
}
