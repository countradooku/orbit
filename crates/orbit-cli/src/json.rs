//! The JSON shapes of Orbit's records, shared by `--json` and MCP. Field
//! names are stable; new fields may be added.

use orbit_net::state::ClientState;
use orbit_proto::*;
use serde_json::{Value, json};

/// A thread's state as one word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Working,
    NeedsYou,
    Idle,
    RateLimited,
    Failed,
}

impl State {
    pub fn of(thread: &Thread) -> State {
        match &thread.status {
            ThreadStatus::Starting => State::Starting,
            ThreadStatus::Running => State::Working,
            ThreadStatus::Waiting => State::NeedsYou,
            ThreadStatus::RateLimited { .. } => State::RateLimited,
            ThreadStatus::Failed { .. } => State::Failed,
            ThreadStatus::Idle if !thread.pending.is_empty() => State::NeedsYou,
            // A queued message is about to start.
            ThreadStatus::Idle if thread.queued > 0 => State::Working,
            ThreadStatus::Idle => State::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Working => "working",
            State::NeedsYou => "needs-you",
            State::Idle => "idle",
            State::RateLimited => "rate-limited",
            State::Failed => "failed",
        }
    }

    /// A one-character mark for human lists.
    pub fn glyph(self) -> &'static str {
        match self {
            State::Starting | State::Working => "●",
            State::NeedsYou => "?",
            State::Idle => "○",
            State::RateLimited => "‖",
            State::Failed => "✕",
        }
    }
}

pub fn title(thread: &Thread) -> &str {
    if thread.title.is_empty() { "New thread" } else { &thread.title }
}

pub fn provider_id(kind: ProviderKind) -> &'static str {
    kind.id()
}

pub fn access_id(access: Access) -> &'static str {
    match access {
        Access::Readonly => "readonly",
        Access::Ask => "ask",
        Access::AutoEdit => "auto-edit",
        Access::Full => "full",
    }
}

pub fn mode_id(mode: Mode) -> &'static str {
    match mode {
        Mode::Build => "build",
        Mode::Plan => "plan",
    }
}

pub fn effort(config: &ThreadConfig) -> Option<&str> {
    config.options.iter().find(|o| o.id == "effort").and_then(|o| match &o.value {
        OptionSetting::Choice(c) => Some(c.as_str()),
        OptionSetting::Toggle(_) => None,
    })
}

pub fn thread(state: &ClientState, t: &Thread) -> Value {
    let project = t.project.and_then(|id| state.projects.iter().find(|p| p.id == id));
    let usage = state.usage(t.id);
    let mut v = json!({
        "id": t.id.to_string(),
        "title": title(t),
        "state": State::of(t).as_str(),
        "project": project.map(|p| json!({"id": p.id.to_string(), "name": p.name})),
        "parent": t.parent.map(|p| p.to_string()),
        "provider": provider_id(t.config.provider),
        "instance": t.config.instance_id(),
        "model": t.config.model,
        "effort": effort(&t.config),
        "mode": mode_id(t.config.mode),
        "access": access_id(t.config.access),
        "branch": t.workspace.branch,
        "worktree": t.workspace.worktree,
        "turns": t.turns,
        "queued": t.queued,
        "unread": t.unread,
        "archived": t.archived,
        "settled": t.settled_ms.is_some(),
        "snoozed_until_ms": t.snoozed_until,
        "pending": t.pending.iter().map(pending).collect::<Vec<_>>(),
        "usage": usage_json(&usage),
        "created_ms": t.created_ms,
        "updated_ms": t.updated_ms,
    });
    // The status engine's evidence while a turn runs: quiet (no output
    // for a while) or unverified (the agent can't be observed).
    if let Some(live) = state.live.get(&t.id) {
        match &live.liveness {
            Liveness::Live => {}
            Liveness::Quiet { since_ms, tool } => {
                v["liveness"] = json!({"state": "quiet", "since_ms": since_ms, "tool": tool});
            }
            Liveness::Unverified { since_ms, reason } => {
                v["liveness"] = json!({"state": "unverified", "since_ms": since_ms, "reason": reason});
            }
        }
    }
    match &t.status {
        ThreadStatus::Failed { message } => v["error"] = json!(message),
        ThreadStatus::RateLimited { resets_ms } if *resets_ms > 0 => v["resets_ms"] = json!(resets_ms),
        _ => {}
    }
    v
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cost_usd": u.cost_micros as f64 / 1_000_000.0,
        "context_tokens": u.context_tokens,
        "context_window": u.context_window,
    })
}

pub fn project(state: &ClientState, p: &Project) -> Value {
    let threads = state.threads.values().filter(|t| t.project == Some(p.id) && !t.archived).count();
    json!({
        "id": p.id.to_string(),
        "name": p.name,
        "path": p.path,
        "base_branch": p.base_branch,
        "threads": threads,
    })
}

pub fn pending(p: &PendingRequest) -> Value {
    let mut v = request_kind(&p.request.kind);
    v["id"] = json!(p.request.id.to_string());
    v["summary"] = json!(request_summary(&p.request.kind));
    v["created_ms"] = json!(p.created_ms);
    v
}

fn request_kind(kind: &RequestKind) -> Value {
    match kind {
        RequestKind::Command { command, cwd, reason } => {
            json!({"kind": "command", "command": command, "cwd": cwd, "reason": reason})
        }
        RequestKind::FileChange { path, diff } => json!({"kind": "file_change", "path": path, "diff": diff}),
        RequestKind::Tool { name, input } => json!({"kind": "tool", "tool": name, "input": input}),
        RequestKind::Question { question, options, multi_select } => {
            json!({"kind": "question", "question": question, "options": options, "multi_select": multi_select})
        }
        RequestKind::Plan { plan } => json!({"kind": "plan", "plan": plan}),
    }
}

/// One line describing a request.
pub fn request_summary(kind: &RequestKind) -> String {
    match kind {
        RequestKind::Command { command, .. } => format!("Run `{}`", one_line(command, 100)),
        RequestKind::FileChange { path, .. } => format!("Change {path}"),
        RequestKind::Tool { name, .. } => format!("Use {name}"),
        RequestKind::Question { question, .. } => one_line(question, 120),
        RequestKind::Plan { .. } => "Carry out the plan".into(),
    }
}

pub fn decision(d: &Decision) -> Value {
    match d {
        Decision::Allow => json!({"decision": "allow"}),
        Decision::AllowForSession => json!({"decision": "allow_for_session"}),
        Decision::Deny { feedback } => json!({"decision": "deny", "feedback": feedback}),
        Decision::Answer { text } => json!({"decision": "answer", "text": text}),
    }
}

fn change_kind(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

pub fn file(f: &FileChange) -> Value {
    json!({"path": f.path, "change": change_kind(f.kind), "additions": f.additions, "deletions": f.deletions})
}

fn item_status(s: ItemStatus) -> &'static str {
    match s {
        ItemStatus::Streaming => "streaming",
        ItemStatus::Done => "done",
        ItemStatus::Failed => "failed",
        ItemStatus::Cancelled => "cancelled",
        ItemStatus::Queued => "queued",
    }
}

fn tool_kind(k: ToolKind) -> &'static str {
    match k {
        ToolKind::Command => "command",
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Search => "search",
        ToolKind::Web => "web",
        ToolKind::Task => "task",
        ToolKind::Mcp => "mcp",
        ToolKind::Todo => "todo",
        ToolKind::Other => "other",
    }
}

/// Bytes of tool output kept in compact items.
const OUTPUT_PREVIEW: usize = 2000;

/// A timeline item. `full` adds reasoning text and tool input and output.
pub fn item(i: &Item, full: bool) -> Value {
    let mut v = json!({"id": i.id.to_string(), "turn": i.turn, "status": item_status(i.status)});
    match &i.body {
        ItemBody::User(m) => {
            v["kind"] = json!("user");
            v["text"] = json!(m.text);
            if m.steer {
                v["steer"] = json!(true);
            }
            if !m.attachments.is_empty() {
                v["attachments"] = json!(m.attachments.iter().map(|a| a.name.as_str()).collect::<Vec<_>>());
            }
        }
        ItemBody::Assistant { text } => {
            v["kind"] = json!("assistant");
            v["text"] = json!(text);
        }
        ItemBody::Reasoning { text } => {
            v["kind"] = json!("reasoning");
            if full {
                v["text"] = json!(text);
            }
        }
        ItemBody::Tool(call) => {
            v["kind"] = json!("tool");
            v["tool"] = json!(call.name);
            v["tool_kind"] = json!(tool_kind(call.kind));
            v["title"] = json!(call.title);
            v["exit_code"] = json!(call.exit_code);
            if !call.files.is_empty() {
                v["files"] = json!(call.files.iter().map(file).collect::<Vec<_>>());
            }
            if full {
                v["input"] = json!(call.input);
                v["output"] = json!(call.output);
            } else if i.status == ItemStatus::Failed || call.exit_code.is_some_and(|c| c != 0) {
                v["output"] = json!(tail_chars(&call.output, OUTPUT_PREVIEW));
            }
        }
        ItemBody::Plan { steps } => {
            v["kind"] = json!("plan");
            v["steps"] = json!(steps
                .iter()
                .map(|s| json!({"text": s.text, "state": match s.state {
                    StepState::Pending => "pending",
                    StepState::Active => "active",
                    StepState::Done => "done",
                }}))
                .collect::<Vec<_>>());
        }
        ItemBody::Request(r) => {
            v["kind"] = json!("request");
            v["request"] = request_kind(&r.kind);
            v["request"]["id"] = json!(r.id.to_string());
            v["summary"] = json!(request_summary(&r.kind));
            v["resolution"] = r.resolution.as_ref().map(decision).unwrap_or(Value::Null);
        }
        ItemBody::Notice { level, text } => {
            v["kind"] = json!("notice");
            v["level"] = json!(match level {
                NoticeLevel::Info => "info",
                NoticeLevel::Warning => "warning",
                NoticeLevel::Error => "error",
            });
            v["text"] = json!(text);
        }
        ItemBody::TurnEnd(s) => {
            v["kind"] = json!("turn_end");
            v["duration_ms"] = json!(s.duration_ms);
            v["interrupted"] = json!(s.interrupted);
            v["files"] = json!(s.files.iter().map(file).collect::<Vec<_>>());
            v["usage"] = usage_json(&s.usage);
        }
        ItemBody::Handoff(h) => {
            v["kind"] = json!("handoff");
            v["from"] = json!(h.from);
            v["to"] = json!(h.to);
            if full {
                v["context"] = json!(h.context);
            }
        }
    }
    v
}

/// The last assistant message in `items`, if any.
pub fn last_message(items: &[Item]) -> Option<&str> {
    items.iter().rev().find_map(|i| match &i.body {
        ItemBody::Assistant { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

pub fn terminal(t: &TerminalInfo) -> Value {
    json!({
        "id": t.id.to_string(),
        "command": t.command,
        "cwd": t.cwd,
        "running": t.running,
        "exit_code": t.exit_code,
    })
}

pub fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - max).collect();
    format!("…{tail}")
}
