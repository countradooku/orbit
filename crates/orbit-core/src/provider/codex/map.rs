//! Pure translations between Codex's protocol and Orbit's domain types.
//! Kept free of I/O so every rule is unit-tested.

use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};

use super::super::{AgentEvent, TurnOutcome};
use super::wire::*;

/// Option ids Orbit uses for Codex model options. The choices behind them
/// always come from `model/list`.
pub const EFFORT: &str = "effort";
pub const SERVICE_TIER: &str = "service_tier";
/// The implicit tier a model runs at when no `serviceTier` is sent.
const STANDARD_TIER: &str = "default";

// ---------------------------------------------------------------------------
// Catalog

/// Maps `model/list` entries to Orbit models. Names, descriptions, effort
/// levels and their descriptions are the server's; only ids without copy
/// are humanized.
pub fn models(models: &[Model]) -> Vec<ModelInfo> {
    models.iter().filter(|m| !m.hidden).map(model).collect()
}

fn model(m: &Model) -> ModelInfo {
    let slug = if m.model.is_empty() { m.id.clone() } else { m.model.clone() };
    let mut options = Vec::new();
    if !m.supported_reasoning_efforts.is_empty() {
        let default = m.default_reasoning_effort.as_deref();
        options.push(OptionDescriptor {
            id: EFFORT.into(),
            label: "Reasoning effort".into(),
            description: None,
            kind: OptionKind::Select {
                choices: m
                    .supported_reasoning_efforts
                    .iter()
                    .map(|e| OptionChoice {
                        id: e.reasoning_effort.clone(),
                        label: humanize_id(&e.reasoning_effort),
                        description: non_empty(&e.description),
                        is_default: Some(e.reasoning_effort.as_str()) == default,
                    })
                    .collect(),
            },
        });
    }
    let tiers: Vec<ServiceTier> = if !m.service_tiers.is_empty() {
        m.service_tiers.clone()
    } else {
        m.additional_speed_tiers
            .iter()
            .map(|id| ServiceTier { id: id.clone(), name: String::new(), description: String::new() })
            .collect()
    };
    let tiers: Vec<&ServiceTier> = tiers.iter().filter(|t| t.id != STANDARD_TIER).collect::<Vec<_>>();
    if !tiers.is_empty() {
        let default = m
            .default_service_tier
            .as_deref()
            .filter(|d| tiers.iter().any(|t| t.id == *d))
            .unwrap_or(STANDARD_TIER);
        let mut choices = vec![OptionChoice {
            id: STANDARD_TIER.into(),
            label: "Standard".into(),
            description: None,
            is_default: default == STANDARD_TIER,
        }];
        choices.extend(tiers.iter().map(|t| OptionChoice {
            id: t.id.clone(),
            label: if t.name.is_empty() { humanize_id(&t.id) } else { t.name.clone() },
            description: non_empty(&t.description),
            is_default: default == t.id,
        }));
        options.push(OptionDescriptor {
            id: SERVICE_TIER.into(),
            label: "Speed".into(),
            description: None,
            kind: OptionKind::Select { choices },
        });
    }
    ModelInfo {
        name: if m.display_name.is_empty() { humanize_id(&slug) } else { m.display_name.clone() },
        slug,
        description: non_empty(&m.description),
        context_window: 0,
        is_default: m.is_default,
        options,
    }
}

/// Interaction modes from `collaborationMode/list`, deduplicated by kind.
pub fn modes(masks: &[CollaborationModeMask]) -> Vec<ModeInfo> {
    let mut out: Vec<ModeInfo> = Vec::new();
    for mask in masks {
        let mode = match mask.mode.as_deref() {
            Some("plan") => Mode::Plan,
            Some("default") => Mode::Build,
            _ => continue,
        };
        if out.iter().any(|m| m.mode == mode) {
            continue;
        }
        let label = if mask.name.is_empty() { humanize_id(mask.mode.as_deref().unwrap_or_default()) } else { mask.name.clone() };
        out.push(ModeInfo { mode, label, description: None });
    }
    out
}

pub fn skills(response: SkillsListResponse) -> Vec<SlashCommand> {
    let mut out: Vec<SlashCommand> = Vec::new();
    for skill in response.data.into_iter().flat_map(|e| e.skills) {
        if !skill.enabled || out.iter().any(|s| s.name == skill.name) {
            continue;
        }
        out.push(SlashCommand {
            description: skill.short_description.filter(|d| !d.is_empty()).unwrap_or(skill.description),
            name: skill.name,
            argument_hint: String::new(),
        });
    }
    out
}

/// `(signed_in, account label, message)` from `account/read`.
pub fn account(response: &AccountResponse) -> (Option<bool>, Option<String>, Option<String>) {
    match &response.account {
        Some(account) => {
            let label = match account.kind.as_str() {
                "chatgpt" => {
                    let plan = account.plan_type.as_deref().filter(|p| *p != "unknown").map(humanize_id);
                    match (&account.email, plan) {
                        (Some(email), Some(plan)) => Some(format!("{email} (ChatGPT {plan})")),
                        (Some(email), None) => Some(email.clone()),
                        (None, Some(plan)) => Some(format!("ChatGPT {plan}")),
                        (None, None) => Some("ChatGPT".into()),
                    }
                }
                "apiKey" => Some("OpenAI API key".into()),
                "amazonBedrock" => Some("Amazon Bedrock".into()),
                other => Some(humanize_id(other)),
            };
            (Some(true), label, None)
        }
        None if response.requires_openai_auth => (
            Some(false),
            None,
            Some("Codex is signed out. Run `codex login` in a terminal, then check again.".into()),
        ),
        // A custom model provider that needs no OpenAI login.
        None => (Some(true), None, None),
    }
}

/// The Codex version from the `initialize` user agent, e.g.
/// `codex_cli_rs/0.47.0 (Windows 10.0.26100; x86_64) …`. Takes the first
/// `/`-separated token that is a dotted version, so an originator that
/// itself contains `/` cannot shift the parse (T3 Code §10.12).
pub fn version_from_user_agent(user_agent: &str) -> Option<String> {
    user_agent.split_whitespace().find_map(|token| {
        let (_, version) = token.rsplit_once('/')?;
        let version = version.trim_start_matches('v');
        let mut parts = version.split(['.', '-', '+']);
        let numeric = parts.by_ref().take(2).all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        (numeric && version.contains('.')).then(|| version.to_owned())
    })
}

// ---------------------------------------------------------------------------
// Policy

/// What Codex is told for each access level: `(approvalPolicy, sandbox mode
/// for thread/start, sandboxPolicy for turn/start)`.
///
/// * Readonly: read-only sandbox and `never`, so a denied write is reported
///   to the model instead of escalating to a prompt the user could approve.
/// * Ask: `untrusted` (every edit and every command that is not known to be
///   read-only asks first) inside a workspace-write sandbox. Under
///   `on-request` Codex applies in-workspace edits without asking, which is
///   Auto-edit, not Ask.
/// * AutoEdit: `on-request` with a workspace-write sandbox: edits in the
///   workspace apply, anything that needs to leave the sandbox asks.
/// * Full: danger-full-access with granular approvals that still let MCP
///   elicitations reach the user (`never` would reject them unseen).
///
/// There is no fallback arm: an unknown level cannot become Full.
pub fn policy(access: Access) -> (Value, &'static str, Value) {
    match access {
        Access::Readonly => (json!("never"), "read-only", json!({"type": "readOnly"})),
        Access::Ask => (json!("untrusted"), "workspace-write", json!({"type": "workspaceWrite"})),
        Access::AutoEdit => (json!("on-request"), "workspace-write", json!({"type": "workspaceWrite"})),
        Access::Full => (
            json!({"granular": {
                "mcp_elicitations": true,
                "request_permissions": false,
                "rules": false,
                "sandbox_approval": false,
                "skill_approval": false
            }}),
            "danger-full-access",
            json!({"type": "dangerFullAccess"}),
        ),
    }
}

/// [`policy`] under a guard: Full access asks Codex to raise every command
/// that is not known to be read-only (`untrusted`), still without a
/// sandbox, so Orbit sees each one and approves all but the guarded ones
/// itself. The other levels already raise what leaves their sandbox.
pub fn guarded_policy(access: Access, guard_active: bool) -> (Value, &'static str, Value) {
    match access {
        Access::Full if guard_active => (json!("untrusted"), "danger-full-access", json!({"type": "dangerFullAccess"})),
        _ => policy(access),
    }
}

/// The value of option `id` for `model`, only if the model reports that
/// option and the choice exists; anything else is dropped so a model switch
/// never sends a value the new model rejects. Before the catalog has loaded
/// (a turn racing `model/list`) the chosen value passes through: it was
/// picked from the catalog the probe reported.
pub fn option_choice<'a>(models: &[ModelInfo], model: &str, values: &'a [OptionValue], id: &str) -> Option<&'a str> {
    let chosen = || match &values.iter().find(|v| v.id == id)?.value {
        OptionSetting::Choice(choice) => Some(choice.as_str()),
        OptionSetting::Toggle(_) => None,
    };
    if models.is_empty() {
        return chosen();
    }
    let descriptor = models.iter().find(|m| m.slug == model)?.options.iter().find(|o| o.id == id)?;
    let OptionKind::Select { choices } = &descriptor.kind else { return None };
    let choice = chosen()?;
    choices.iter().any(|c| c.id == choice).then_some(choice)
}

/// The service tier to send, `None` for the standard tier.
pub fn service_tier<'a>(models: &[ModelInfo], model: &str, values: &'a [OptionValue]) -> Option<&'a str> {
    option_choice(models, model, values, SERVICE_TIER).filter(|t| *t != STANDARD_TIER)
}

// ---------------------------------------------------------------------------
// Turn outcome, usage and quota

pub fn outcome(status: Option<&str>, error: Option<&TurnError>, limits: Option<&RateLimitSnapshot>) -> TurnOutcome {
    match status {
        Some("interrupted") => TurnOutcome::Interrupted,
        Some("failed") => failure(error, limits),
        _ => TurnOutcome::Completed,
    }
}

pub fn failure(error: Option<&TurnError>, limits: Option<&RateLimitSnapshot>) -> TurnOutcome {
    let Some(error) = error else {
        return TurnOutcome::Failed { message: "Codex ended the turn with an error but gave no details".into() };
    };
    let info = error.codex_error_info.as_ref().map(ErrorInfo::parse);
    if info.as_ref().is_some_and(ErrorInfo::is_usage_limit) {
        return TurnOutcome::RateLimited { resets_ms: limits.map(reset_ms).unwrap_or(0) };
    }
    TurnOutcome::Failed { message: error_message(error) }
}

pub fn error_message(error: &TurnError) -> String {
    let mut message = if error.message.trim().is_empty() {
        "Codex reported an error without details".to_owned()
    } else {
        error.message.trim().to_owned()
    };
    if let Some(details) = error.additional_details.as_deref().map(str::trim).filter(|d| !d.is_empty())
        && !message.contains(details)
    {
        message.push('\n');
        message.push_str(details);
    }
    message
}

/// When the exhausted window resets, in Unix ms; 0 when unknown.
pub fn reset_ms(snapshot: &RateLimitSnapshot) -> u64 {
    let windows = [snapshot.primary, snapshot.secondary];
    let exhausted = windows.iter().flatten().filter(|w| w.used_percent >= 100.0).filter_map(|w| w.resets_at).max();
    let any = || windows.iter().flatten().filter_map(|w| w.resets_at).max();
    let secs = exhausted.or_else(|| snapshot.rate_limit_reached_type.as_ref().and_then(|_| any()));
    secs.filter(|s| *s > 0).map(|s| s as u64 * 1000).unwrap_or(0)
}

pub fn quota_events(snapshot: &RateLimitSnapshot) -> Vec<AgentEvent> {
    let reached = snapshot.rate_limit_reached_type.is_some();
    let windows = [("primary", snapshot.primary), ("secondary", snapshot.secondary)];
    let any_full = windows.iter().any(|(_, w)| w.is_some_and(|w| w.used_percent >= 100.0));
    windows
        .iter()
        .filter_map(|(slot, window)| {
            let w = (*window)?;
            let full = w.used_percent >= 100.0;
            Some(AgentEvent::Quota {
                window: window_label(slot, w.window_duration_mins, snapshot.limit_name.as_deref()),
                utilization: Some((w.used_percent / 100.0).clamp(0.0, 1.0) as f32),
                resets_ms: w.resets_at.filter(|s| *s > 0).map(|s| s as u64 * 1000).unwrap_or(0),
                // A reached limit with no full window blames the primary one.
                rejected: full || (reached && !any_full && *slot == "primary"),
            })
        })
        .collect()
}

fn window_label(slot: &str, minutes: Option<i64>, limit: Option<&str>) -> String {
    let span = match minutes {
        Some(m) if m > 0 && m % (60 * 24 * 7) == 0 => format!("{}w", m / (60 * 24 * 7)),
        Some(m) if m > 0 && m % (60 * 24) == 0 => format!("{}d", m / (60 * 24)),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => slot.to_owned(),
    };
    match limit {
        Some(name) if !name.is_empty() => format!("{name} {span}"),
        _ => span,
    }
}

/// Folds `thread/tokenUsage/updated` into the running turn's usage.
/// `prev_total` is the last thread-wide total seen by this process; the
/// turn's share is the growth of that total, or the last request's usage
/// when the counter restarted (new process, resumed thread).
pub fn add_usage(turn: &mut Usage, prev_total: Option<TokenBreakdown>, update: &ThreadTokenUsage) {
    let delta = match prev_total {
        Some(prev) if update.total.total_tokens >= prev.total_tokens => TokenBreakdown {
            input_tokens: update.total.input_tokens.saturating_sub(prev.input_tokens),
            cached_input_tokens: update.total.cached_input_tokens.saturating_sub(prev.cached_input_tokens),
            cache_write_input_tokens: update.total.cache_write_input_tokens.saturating_sub(prev.cache_write_input_tokens),
            output_tokens: update.total.output_tokens.saturating_sub(prev.output_tokens),
            total_tokens: update.total.total_tokens - prev.total_tokens,
        },
        _ => update.last,
    };
    // OpenAI counts cached tokens inside `inputTokens`; Orbit keeps them
    // apart so cost estimates do not count them twice.
    turn.input_tokens += delta.input_tokens.saturating_sub(delta.cached_input_tokens);
    turn.cache_read_tokens += delta.cached_input_tokens;
    turn.cache_write_tokens += delta.cache_write_input_tokens;
    turn.output_tokens += delta.output_tokens;
    turn.context_tokens = update.last.total_tokens;
    if let Some(window) = update.model_context_window.filter(|w| *w > 0) {
        turn.context_window = window;
    }
}

pub fn plan(update: &PlanUpdatedNotification) -> Vec<PlanStep> {
    update
        .plan
        .iter()
        .map(|s| PlanStep {
            text: s.step.clone(),
            state: match s.status.as_str() {
                "completed" => StepState::Done,
                "inProgress" => StepState::Active,
                _ => StepState::Pending,
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Items

pub fn file_changes(changes: &[FileUpdateChange]) -> Vec<FileChange> {
    changes
        .iter()
        .map(|c| {
            let (additions, deletions) = diff_stats(c);
            let (kind, path) = match (c.kind.kind.as_str(), c.kind.move_path.as_deref()) {
                ("add", _) => (ChangeKind::Added, c.path.clone()),
                ("delete", _) => (ChangeKind::Deleted, c.path.clone()),
                (_, Some(to)) if !to.is_empty() && to != c.path => (ChangeKind::Renamed, to.to_owned()),
                _ => (ChangeKind::Modified, c.path.clone()),
            };
            FileChange { path, kind, additions, deletions }
        })
        .collect()
}

/// Added and removed lines. Updates carry a unified diff; adds and deletes
/// may carry the raw file content instead.
fn diff_stats(change: &FileUpdateChange) -> (u32, u32) {
    let diff = &change.diff;
    let unified = diff.lines().any(|l| l.starts_with("@@"));
    if !unified {
        let lines = diff.lines().count() as u32;
        return match change.kind.kind.as_str() {
            "add" => (lines, 0),
            "delete" => (0, lines),
            _ => (0, 0),
        };
    }
    let (mut add, mut del) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            add += 1;
        } else if line.starts_with('-') {
            del += 1;
        }
    }
    (add, del)
}

/// A reviewable diff of a file-change item for an approval prompt.
pub fn approval_diff(changes: &[FileUpdateChange]) -> (String, String) {
    let path = match changes {
        [] => String::new(),
        [one] => one.path.clone(),
        [first, rest @ ..] => format!("{} and {} more", first.path, rest.len()),
    };
    let mut diff = String::new();
    for c in changes {
        let unified = c.diff.lines().any(|l| l.starts_with("@@"));
        if !c.diff.starts_with("---") {
            let (from, to) = match c.kind.kind.as_str() {
                "add" => ("/dev/null".to_owned(), format!("b/{}", c.path)),
                "delete" => (format!("a/{}", c.path), "/dev/null".to_owned()),
                _ => (format!("a/{}", c.path), format!("b/{}", c.kind.move_path.as_deref().unwrap_or(&c.path))),
            };
            diff.push_str(&format!("--- {from}\n+++ {to}\n"));
        }
        if unified {
            diff.push_str(&c.diff);
        } else {
            let sign = if c.kind.kind == "delete" { '-' } else { '+' };
            for line in c.diff.lines() {
                diff.push(sign);
                diff.push_str(line);
                diff.push('\n');
            }
        }
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
    }
    (path, diff)
}

fn tool(call_id: &str, kind: ToolKind, name: &str, title: String, input: String) -> ToolCall {
    ToolCall {
        call_id: call_id.to_owned(),
        kind,
        name: name.to_owned(),
        title,
        input,
        output: String::new(),
        output_blob: None,
        exit_code: None,
        files: Vec::new(),
    }
}

/// The tool call an item represents, or `None` for items that are not tools
/// (messages, reasoning, plans, compaction).
pub fn tool_call(head: &ItemHead, raw: &str) -> Option<ToolCall> {
    let id = head.id.as_str();
    match head.kind.as_str() {
        "commandExecution" => {
            let item: CommandItem = sonic_rs::from_str(raw).ok()?;
            Some(command_call(id, &item))
        }
        "fileChange" => {
            let item: FileChangeItem = sonic_rs::from_str(raw).ok()?;
            let files = file_changes(&item.changes);
            let title = match files.as_slice() {
                [] => "Edit files".to_owned(),
                [one] => one.path.clone(),
                [first, rest @ ..] => format!("{} and {} more", first.path, rest.len()),
            };
            let paths: Vec<&str> = item.changes.iter().map(|c| c.path.as_str()).collect();
            let mut call = tool(id, ToolKind::Edit, "apply_patch", title, sonic_rs::to_string(&json!({"paths": paths})).unwrap_or_default());
            call.files = files;
            Some(call)
        }
        "mcpToolCall" => {
            let item: McpToolCallItem = sonic_rs::from_str(raw).ok()?;
            Some(tool(
                id,
                ToolKind::Mcp,
                &format!("mcp__{}__{}", item.server, item.tool),
                format!("{}: {}", item.server, item.tool),
                compact(item.arguments.as_ref()),
            ))
        }
        "dynamicToolCall" => {
            let item: DynamicToolCallItem = sonic_rs::from_str(raw).ok()?;
            Some(tool(id, ToolKind::Other, &item.tool, humanize_id(&item.tool), compact(item.arguments.as_ref())))
        }
        "webSearch" => {
            let item: WebSearchItem = sonic_rs::from_str(raw).ok()?;
            Some(tool(id, ToolKind::Web, "web_search", web_title(&item), sonic_rs::to_string(&json!({"query": item.query})).unwrap_or_default()))
        }
        "imageView" => {
            let item: PathItem = sonic_rs::from_str(raw).ok()?;
            Some(tool(id, ToolKind::Read, "view_image", item.path.clone(), sonic_rs::to_string(&json!({"path": item.path})).unwrap_or_default()))
        }
        "collabAgentToolCall" => {
            let item: CollabAgentItem = sonic_rs::from_str(raw).ok()?;
            let title = item.prompt.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| humanize_id(&item.tool));
            Some(tool(id, ToolKind::Task, &item.tool, one_line(&title), sonic_rs::to_string(&json!({"prompt": item.prompt})).unwrap_or_default()))
        }
        "imageGeneration" => {
            let item: ImageGenerationItem = sonic_rs::from_str(raw).ok()?;
            let title = item.revised_prompt.clone().unwrap_or_else(|| "Generate image".into());
            Some(tool(id, ToolKind::Other, "image_generation", one_line(&title), "{}".into()))
        }
        _ => None,
    }
}

fn command_call(id: &str, item: &CommandItem) -> ToolCall {
    // Codex parses the command into actions; a command that only reads or
    // searches is shown as such instead of as a generic shell call.
    let actions = &item.command_actions;
    let (kind, title) = if !actions.is_empty() && actions.iter().all(|a| a.kind == "read") {
        let paths: Vec<&str> = actions.iter().filter_map(|a| a.path.as_deref()).collect();
        (ToolKind::Read, if paths.is_empty() { item.command.clone() } else { paths.join(", ") })
    } else if !actions.is_empty() && actions.iter().all(|a| a.kind == "search" || a.kind == "listFiles") {
        let query = actions.iter().find_map(|a| a.query.as_deref().or(a.path.as_deref()));
        (ToolKind::Search, query.map(str::to_owned).unwrap_or_else(|| item.command.clone()))
    } else {
        (ToolKind::Command, item.command.clone())
    };
    tool(id, kind, "shell", one_line(&title), sonic_rs::to_string(&json!({"command": item.command, "cwd": item.cwd})).unwrap_or_default())
}

/// `(output, is_error, exit_code)` of a completed tool item.
pub fn tool_result(kind: &str, raw: &str) -> (String, bool, Option<i32>) {
    match kind {
        "commandExecution" => match sonic_rs::from_str::<CommandItem>(raw) {
            Ok(item) => {
                let failed = matches!(item.status.as_str(), "failed" | "declined") || item.exit_code.is_some_and(|c| c != 0);
                let mut output = item.aggregated_output.unwrap_or_default();
                if item.status == "declined" && output.is_empty() {
                    output = "Declined".into();
                }
                (output, failed, item.exit_code)
            }
            Err(_) => (String::new(), false, None),
        },
        "fileChange" => match sonic_rs::from_str::<FileChangeItem>(raw) {
            Ok(item) => {
                let failed = item.status != "completed";
                (if item.status == "declined" { "Declined".into() } else { String::new() }, failed, None)
            }
            Err(_) => (String::new(), false, None),
        },
        "mcpToolCall" => match sonic_rs::from_str::<McpToolCallItem>(raw) {
            Ok(item) => match item.error {
                Some(err) => (err.message, true, None),
                None => (content_text(item.result.map(|r| r.content).unwrap_or_default()), item.status == "failed", None),
            },
            Err(_) => (String::new(), false, None),
        },
        "dynamicToolCall" => match sonic_rs::from_str::<DynamicToolCallItem>(raw) {
            Ok(item) => (
                content_text(item.content_items.unwrap_or_default()),
                item.status == "failed" || item.success == Some(false),
                None,
            ),
            Err(_) => (String::new(), false, None),
        },
        "webSearch" => match sonic_rs::from_str::<WebSearchItem>(raw) {
            Ok(item) => (web_title(&item), false, None),
            Err(_) => (String::new(), false, None),
        },
        "imageGeneration" => match sonic_rs::from_str::<ImageGenerationItem>(raw) {
            Ok(item) => (item.saved_path.unwrap_or_default(), item.status == "failed", None),
            Err(_) => (String::new(), false, None),
        },
        "collabAgentToolCall" => match sonic_rs::from_str::<CollabAgentItem>(raw) {
            Ok(item) => (String::new(), item.status == "failed", None),
            Err(_) => (String::new(), false, None),
        },
        _ => (String::new(), false, None),
    }
}

/// The updated title of a tool once its item completed (web searches learn
/// their query late).
pub fn completed_title(kind: &str, raw: &str) -> Option<String> {
    (kind == "webSearch").then(|| sonic_rs::from_str::<WebSearchItem>(raw).ok().map(|i| web_title(&i))).flatten()
}

fn web_title(item: &WebSearchItem) -> String {
    if !item.query.is_empty() {
        return item.query.clone();
    }
    let Some(action) = &item.action else { return "Web search".into() };
    action
        .get("url")
        .or_else(|| action.get("query"))
        .or_else(|| action.get("pattern"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "Web search".into())
}

/// Text of MCP / dynamic tool content items (`{type:"text", text}`), other
/// content as compact JSON.
fn content_text(items: Vec<Value>) -> String {
    let mut out = String::new();
    for item in &items {
        if !out.is_empty() {
            out.push('\n');
        }
        match item.get("text").and_then(|t| t.as_str()) {
            Some(text) => out.push_str(text),
            None => out.push_str(&sonic_rs::to_string(item).unwrap_or_default()),
        }
    }
    out
}

fn compact(value: Option<&Value>) -> String {
    value.map(|v| sonic_rs::to_string(v).unwrap_or_default()).unwrap_or_else(|| "{}".into())
}

fn one_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() > 200 { line.chars().take(200).chain(['…']).collect() } else { line.to_owned() }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_model(json: &str) -> Model {
        sonic_rs::from_str(json).unwrap()
    }

    #[test]
    fn model_list_maps_efforts_and_tiers_from_the_server() {
        let m = wire_model(
            r#"{"id":"m1","model":"gpt-x","displayName":"GPT X","description":"Fast and smart","hidden":false,"isDefault":true,
                "defaultReasoningEffort":"medium",
                "supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Quick"},{"reasoningEffort":"medium","description":""},{"reasoningEffort":"xhigh","description":"Deepest"}],
                "serviceTiers":[{"id":"fast","name":"Fast","description":"Lower latency"}],"defaultServiceTier":null,"extra":{"ignored":[1,2,3]}}"#,
        );
        let hidden = wire_model(r#"{"id":"h","model":"hidden","displayName":"H","description":"","hidden":true,"isDefault":false,"supportedReasoningEfforts":[]}"#);
        let list = models(&[m, hidden]);
        assert_eq!(list.len(), 1);
        let info = &list[0];
        assert_eq!((info.slug.as_str(), info.name.as_str(), info.is_default), ("gpt-x", "GPT X", true));
        assert_eq!(info.description.as_deref(), Some("Fast and smart"));
        let OptionKind::Select { choices } = &info.options[0].kind else { panic!() };
        assert_eq!(info.options[0].id, EFFORT);
        assert_eq!(choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "xhigh"]);
        assert_eq!(choices[2].label, "Extra high");
        assert_eq!(choices[0].description.as_deref(), Some("Quick"));
        assert_eq!(choices[1].description, None);
        assert!(choices[1].is_default && !choices[0].is_default);
        assert_eq!(info.options[0].default_setting(), OptionSetting::Choice("medium".into()));
        let OptionKind::Select { choices } = &info.options[1].kind else { panic!() };
        assert_eq!(info.options[1].id, SERVICE_TIER);
        assert_eq!(choices.iter().map(|c| (c.id.as_str(), c.is_default)).collect::<Vec<_>>(), [("default", true), ("fast", false)]);
        assert_eq!(choices[1].description.as_deref(), Some("Lower latency"));
    }

    #[test]
    fn options_for_other_models_are_dropped() {
        let list = models(&[wire_model(
            r#"{"id":"a","model":"a","displayName":"A","description":"","hidden":false,"isDefault":true,"defaultReasoningEffort":"low","supportedReasoningEfforts":[{"reasoningEffort":"low","description":""}]}"#,
        )]);
        let values = vec![OptionValue { id: EFFORT.into(), value: OptionSetting::Choice("high".into()) }];
        assert_eq!(option_choice(&list, "a", &values, EFFORT), None);
        let values = vec![OptionValue { id: EFFORT.into(), value: OptionSetting::Choice("low".into()) }];
        assert_eq!(option_choice(&list, "a", &values, EFFORT), Some("low"));
        assert_eq!(option_choice(&list, "b", &values, EFFORT), None);
        assert_eq!(option_choice(&[], "b", &values, EFFORT), Some("low"));
    }

    #[test]
    fn access_levels_never_fall_through_to_full_access() {
        let (approval, mode, sandbox) = policy(Access::Readonly);
        assert_eq!((approval.as_str(), mode, sandbox.get("type").and_then(|t| t.as_str())), (Some("never"), "read-only", Some("readOnly")));
        let (approval, mode, sandbox) = policy(Access::Ask);
        assert_eq!((approval.as_str(), mode, sandbox.get("type").and_then(|t| t.as_str())), (Some("untrusted"), "workspace-write", Some("workspaceWrite")));
        let (approval, mode, _) = policy(Access::AutoEdit);
        assert_eq!((approval.as_str(), mode), (Some("on-request"), "workspace-write"));
        let (approval, mode, sandbox) = policy(Access::Full);
        assert!(approval.get("granular").is_some());
        assert_eq!((mode, sandbox.get("type").and_then(|t| t.as_str())), ("danger-full-access", Some("dangerFullAccess")));
        assert_eq!(policy(Access::default()).1, "workspace-write");
        let (approval, mode, _) = guarded_policy(Access::Full, true);
        assert_eq!((approval, mode), (json!("untrusted"), "danger-full-access"), "guarded Full still has no sandbox");
        assert_eq!(guarded_policy(Access::Full, false), policy(Access::Full));
        assert_eq!(guarded_policy(Access::AutoEdit, true), policy(Access::AutoEdit));
    }

    #[test]
    fn version_parse_ignores_slashes_in_the_originator() {
        assert_eq!(version_from_user_agent("codex_cli_rs/0.47.0 (Windows 10.0.26100; x86_64) WindowsTerminal").as_deref(), Some("0.47.0"));
        assert_eq!(version_from_user_agent("my/app codex_cli_rs/0.160.1-alpha.2 (Mac OS 15)").as_deref(), Some("0.160.1-alpha.2"));
        assert_eq!(version_from_user_agent("no version here"), None);
    }

    #[test]
    fn usage_limit_comes_from_structured_error_info() {
        let limits = RateLimitSnapshot {
            primary: Some(RateLimitWindow { used_percent: 100.0, window_duration_mins: Some(300), resets_at: Some(1_900_000_000) }),
            secondary: Some(RateLimitWindow { used_percent: 40.0, window_duration_mins: Some(10080), resets_at: Some(1_900_500_000) }),
            ..Default::default()
        };
        let err = |info: &str| TurnError { message: "You've hit your usage limit".into(), additional_details: None, codex_error_info: Some(sonic_rs::from_str(info).unwrap()) };
        assert_eq!(failure(Some(&err(r#""usageLimitExceeded""#)), Some(&limits)), TurnOutcome::RateLimited { resets_ms: 1_900_000_000_000 });
        assert_eq!(
            failure(Some(&err(r#"{"responseTooManyFailedAttempts":{"httpStatusCode":429}}"#)), None),
            TurnOutcome::RateLimited { resets_ms: 0 }
        );
        // The message text alone never decides: no structured info, no rate limit.
        let plain = TurnError { message: "usage limit".into(), additional_details: Some("details".into()), codex_error_info: None };
        assert_eq!(failure(Some(&plain), Some(&limits)), TurnOutcome::Failed { message: "usage limit\ndetails".into() });
        assert!(matches!(failure(Some(&err(r#""apiKeySpendLimitExceeded""#)), None), TurnOutcome::Failed { .. }));
        assert!(matches!(failure(Some(&err(r#"{"responseStreamDisconnected":{"httpStatusCode":null}}"#)), None), TurnOutcome::Failed { .. }));
    }

    #[test]
    fn quota_windows_are_labelled_by_duration() {
        let snapshot = RateLimitSnapshot {
            primary: Some(RateLimitWindow { used_percent: 25.0, window_duration_mins: Some(300), resets_at: Some(100) }),
            secondary: Some(RateLimitWindow { used_percent: 100.0, window_duration_mins: Some(10080), resets_at: None }),
            ..Default::default()
        };
        let events = quota_events(&snapshot);
        assert_eq!(
            events,
            vec![
                AgentEvent::Quota { window: "5h".into(), utilization: Some(0.25), resets_ms: 100_000, rejected: false },
                AgentEvent::Quota { window: "1w".into(), utilization: Some(1.0), resets_ms: 0, rejected: true },
            ]
        );
    }

    #[test]
    fn usage_is_the_growth_of_the_thread_total() {
        let b = |input, cached, output| TokenBreakdown { input_tokens: input, cached_input_tokens: cached, cache_write_input_tokens: 0, output_tokens: output, total_tokens: input + output };
        let mut usage = Usage::default();
        let first = ThreadTokenUsage { total: b(1000, 200, 100), last: b(1000, 200, 100), model_context_window: Some(200_000) };
        add_usage(&mut usage, None, &first);
        let second = ThreadTokenUsage { total: b(2500, 900, 150), last: b(1500, 700, 50), model_context_window: Some(200_000) };
        add_usage(&mut usage, Some(first.total), &second);
        assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (1600, 900, 150));
        assert_eq!((usage.context_tokens, usage.context_window), (1550, 200_000));
        // A counter reset (new process) falls back to the last request.
        let mut usage = Usage::default();
        add_usage(&mut usage, Some(second.total), &first);
        assert_eq!(usage.input_tokens, 800);
    }

    #[test]
    fn file_changes_count_lines_and_detect_renames() {
        let changes: Vec<FileUpdateChange> = sonic_rs::from_str(
            r#"[{"path":"src/a.rs","kind":{"type":"update","move_path":null},"diff":"--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n ctx\n"},
                {"path":"new.txt","kind":{"type":"add"},"diff":"one\ntwo\n"},
                {"path":"gone.txt","kind":{"type":"delete"},"diff":"x\n"},
                {"path":"from.rs","kind":{"type":"update","move_path":"to.rs"},"diff":"@@ -1 +1 @@\n-a\n+b\n"}]"#,
        )
        .unwrap();
        let files = file_changes(&changes);
        assert_eq!(files[0], FileChange { path: "src/a.rs".into(), kind: ChangeKind::Modified, additions: 2, deletions: 1 });
        assert_eq!(files[1], FileChange { path: "new.txt".into(), kind: ChangeKind::Added, additions: 2, deletions: 0 });
        assert_eq!(files[2], FileChange { path: "gone.txt".into(), kind: ChangeKind::Deleted, additions: 0, deletions: 1 });
        assert_eq!(files[3], FileChange { path: "to.rs".into(), kind: ChangeKind::Renamed, additions: 1, deletions: 1 });
        let (path, diff) = approval_diff(&changes[1..2]);
        assert_eq!(path, "new.txt");
        assert_eq!(diff, "--- /dev/null\n+++ b/new.txt\n+one\n+two\n");
    }

    #[test]
    fn read_only_commands_are_shown_as_reads() {
        let head = ItemHead { kind: "commandExecution".into(), id: "c1".into() };
        let call = tool_call(&head, r#"{"type":"commandExecution","id":"c1","command":"cat src/main.rs","cwd":"/w","commandActions":[{"type":"read","command":"cat","name":"main.rs","path":"src/main.rs"}],"status":"inProgress"}"#).unwrap();
        assert_eq!((call.kind, call.title.as_str()), (ToolKind::Read, "src/main.rs"));
        let call = tool_call(&head, r#"{"type":"commandExecution","id":"c1","command":"cargo test","cwd":"/w","commandActions":[{"type":"unknown","command":"cargo test"}],"status":"inProgress"}"#).unwrap();
        assert_eq!((call.kind, call.title.as_str()), (ToolKind::Command, "cargo test"));
        let (out, failed, code) = tool_result("commandExecution", r#"{"command":"x","cwd":"/","commandActions":[],"status":"failed","exitCode":2,"aggregatedOutput":"boom"}"#);
        assert_eq!((out.as_str(), failed, code), ("boom", true, Some(2)));
    }
}
