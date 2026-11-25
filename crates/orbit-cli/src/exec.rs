//! What each command does. Commands get their arguments as a JSON object
//! (from the command line or an MCP tool call), return a JSON result and,
//! for people, a text rendering of it.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::{self, Conn, Env};
use crate::error::CliError;
use crate::json::{self, State};
use crate::render;
use crate::select::{self, Answering};
use crate::spec::{Spec, exit};

/// Who is calling: a person or script on the command line, or an agent
/// through MCP (which gets bounded output and lineage rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Cli,
    Tool,
}

pub struct Ctx {
    pub env: Env,
    pub global: Map<String, Value>,
    pub origin: Origin,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
}

/// A command's result.
pub struct Outcome {
    pub value: Value,
    pub exit: i32,
    /// Text for people; `None` when the command already printed (streams).
    pub human: Option<String>,
}

impl Outcome {
    pub(crate) fn ok(value: Value, human: String) -> Self {
        Self { value, exit: exit::OK, human: Some(human) }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A state must hold this long before `wait` believes it: a turn ending and
/// the next queued one starting are separate events.
const SETTLE: Duration = Duration::from_millis(200);

impl Ctx {
    pub fn new(env: Env, global: Map<String, Value>, origin: Origin) -> Self {
        Self { env, global, origin, conn: tokio::sync::Mutex::new(None) }
    }

    pub fn json(&self) -> bool {
        self.global.get("json").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The connection, made on first use and remade when the engine went
    /// away (it restarts on a new port with a new token).
    pub async fn conn(&self) -> Result<Arc<Conn>, CliError> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref()
            && conn.connected()
        {
            return Ok(conn.clone());
        }
        let target = conn::resolve(&self.global, &self.env)?;
        let conn = Arc::new(Conn::connect(target, CONNECT_TIMEOUT).await?);
        *slot = Some(conn.clone());
        Ok(conn)
    }
}

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn int(args: &Map<String, Value>, name: &str) -> Option<u64> {
    args.get(name).and_then(Value::as_u64)
}

/// `90s`, `30m`, `2h`, `1d`, `1h30m`, or plain seconds. `0` is `None`
/// (no limit).
pub fn duration(text: &str) -> Result<Option<Duration>, CliError> {
    let text = text.trim();
    let bad = || CliError::usage(format!("“{text}” is not a duration; use e.g. 90s, 10m, 2h or 1d."));
    let mut total = 0u64;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: u64 = number.parse().map_err(|_| bad())?;
        number.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(bad()),
        };
    }
    if !number.is_empty() {
        total += number.parse::<u64>().map_err(|_| bad())?;
    }
    if text.is_empty() {
        return Err(bad());
    }
    Ok((total > 0).then(|| Duration::from_secs(total)))
}

/// Free text, with `-` read from standard input.
fn text(args: &Map<String, Value>, name: &str, origin: Origin) -> Result<String, CliError> {
    let value = s(args, name).unwrap_or_default();
    if value == "-" && origin == Origin::Cli {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .map_err(|e| CliError::usage(format!("Couldn’t read standard input: {e}")))?;
        return Ok(input);
    }
    Ok(value.to_owned())
}

pub async fn execute(ctx: &Ctx, spec: &Spec, args: &Map<String, Value>, out: &mut dyn Write) -> Result<Outcome, CliError> {
    match spec.path {
        ["agent-context"] => Ok(Outcome::ok(json!({"guide": crate::help::agent_context()}), crate::help::agent_context())),
        ["help"] => Ok(crate::help::help(s(args, "command"), ctx.json())),
        ["completions"] => {
            let shell = s(args, "shell").unwrap_or("bash");
            Ok(Outcome { value: Value::Null, exit: exit::OK, human: Some(crate::help::completions(shell)?) })
        }
        ["mcp", "install"] => crate::mcp::install(s(args, "client").unwrap_or("all"), flag(args, "apply"), ctx.json()),
        ["open"] => open(ctx, args).await,
        _ => {
            let conn = ctx.conn().await?;
            run(ctx, &conn, spec, args, out).await
        }
    }
}

async fn run(ctx: &Ctx, conn: &Conn, spec: &Spec, args: &Map<String, Value>, out: &mut dyn Write) -> Result<Outcome, CliError> {
    let thread_arg = |name: &str| -> Result<ThreadId, CliError> {
        let selector = s(args, name).ok_or_else(|| CliError::usage(format!("Name a {name}.")))?;
        select::thread(&conn.state(), &ctx.env, selector)
    };
    match spec.path {
        ["status"] => match s(args, "thread") {
            Some(_) => thread_status(conn, thread_arg("thread")?).await,
            None => Ok(overview(conn)),
        },
        ["projects"] => {
            let state = conn.state();
            let list: Vec<Value> = state.projects.iter().map(|p| json::project(&state, p)).collect();
            let human = render::projects(&list);
            Ok(Outcome::ok(json!({"projects": list}), human))
        }
        ["projects", "add"] => {
            let path = ctx.env.cwd.join(s(args, "path").unwrap_or("."));
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            let path = path.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
            let Reply::Project(id) = conn.call(Command::AddProject { path: path.clone() }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            Ok(Outcome::ok(json!({"project": {"id": id.to_string(), "path": path}}), format!("{id}  {path}")))
        }
        ["threads"] => threads(ctx, conn, args),
        ["new"] => new_thread(ctx, conn, args).await,
        ["send"] => {
            let id = thread_arg("thread")?;
            check_lineage(ctx, conn, id, Lineage::Talk)?;
            let message = text(args, "message", ctx.origin)?;
            let steer = flag(args, "steer");
            conn.call(Command::Send { thread: id, text: message, attachments: Vec::new(), steer }).await?;
            let sent = json!({"thread": id.to_string(), "steer": steer});
            if flag(args, "wait") {
                return wait_and_report(ctx, conn, vec![id], Until::Finished, false, args, Some(sent)).await;
            }
            let how = if steer { "Steered" } else { "Sent to" };
            let title = conn.state().threads.get(&id).map(|t| json::title(t).to_owned()).unwrap_or_default();
            Ok(Outcome::ok(sent, format!("{how} {id}  {title}")))
        }
        ["wait"] => {
            let selectors: Vec<&str> =
                args.get("threads").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let ids = {
                let state = conn.state();
                selectors.iter().map(|sel| select::thread(&state, &ctx.env, sel)).collect::<Result<Vec<_>, _>>()?
            };
            let until = Until::parse(s(args, "until").unwrap_or("finished"));
            wait_and_report(ctx, conn, ids, until, flag(args, "any"), args, None).await
        }
        ["log"] if flag(args, "follow") => tail(ctx, conn, thread_arg("thread")?, args, out).await,
        ["tail"] => tail(ctx, conn, thread_arg("thread")?, args, out).await,
        ["log"] => {
            let id = thread_arg("thread")?;
            let after = s(args, "after").map(|a| a.parse::<ItemId>().map_err(|_| CliError::usage(format!("“{a}” is not an item id."))));
            let after = after.transpose()?;
            let limit = int(args, "limit").unwrap_or(40) as u32;
            let Reply::Timeline { items, .. } = conn.call(Command::ReadTimeline { id, after, limit }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let full = flag(args, "full");
            let thread = conn.state().threads.get(&id).cloned().ok_or_else(|| CliError::not_found("That thread no longer exists."))?;
            let human = render::timeline(&thread, &items, full);
            let value = json!({
                "thread": json::thread(&conn.state(), &thread),
                "items": items.iter().map(|i| json::item(i, full)).collect::<Vec<_>>(),
                "last_item": items.last().map(|i| i.id.to_string()),
            });
            Ok(Outcome::ok(value, human))
        }
        ["approve"] | ["deny"] | ["answer"] => resolve(ctx, conn, spec, args, thread_arg("thread")?).await,
        ["stop"] => {
            let id = thread_arg("thread")?;
            check_lineage(ctx, conn, id, Lineage::Control)?;
            let force = flag(args, "force");
            conn.call(if force { Command::ForceStop { thread: id } } else { Command::Interrupt { thread: id } }).await?;
            let human = if force { format!("Ended {id}’s agent") } else { format!("Stopped {id}") };
            Ok(Outcome::ok(json!({"thread": id.to_string(), "stopped": true, "forced": force}), human))
        }
        ["retry"] => {
            let id = thread_arg("thread")?;
            check_lineage(ctx, conn, id, Lineage::Control)?;
            conn.call(Command::Retry { thread: id }).await?;
            Ok(Outcome::ok(json!({"thread": id.to_string(), "retrying": true}), format!("Retrying {id}’s last turn")))
        }
        ["new-session"] => {
            let id = thread_arg("thread")?;
            conn.call(Command::NewSession { thread: id }).await?;
            Ok(Outcome::ok(json!({"thread": id.to_string(), "new_session": true}), format!("{id} starts a fresh session with its next message")))
        }
        ["export"] => {
            let id = thread_arg("thread")?;
            let Reply::Blob(blob) = conn.call(Command::ExportThread { thread: id }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let Reply::Bytes(bytes) = conn.call(Command::ReadBlob { hash: blob.hash.clone() }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let text = String::from_utf8_lossy(&bytes).into_owned();
            Ok(Outcome::ok(json!({"thread": id.to_string(), "blob": blob.hash, "digest": text}), text))
        }
        ["merge"] => merge(ctx, conn, thread_arg("thread")?, args).await,
        ["files"] => {
            let query = text(args, "query", ctx.origin)?;
            let (thread, project) = {
                let state = conn.state();
                let thread = s(args, "thread").map(|t| select::thread(&state, &ctx.env, t)).transpose()?;
                let project = match (thread, s(args, "project")) {
                    (Some(_), _) => None,
                    (None, Some(p)) => Some(select::project(&state, &ctx.env, p)?),
                    (None, None) => Some(select::containing(&state, &ctx.env.cwd).ok_or_else(|| {
                        CliError::not_found("The current folder is not in a project.").hint("Pass --thread or --project.")
                    })?),
                };
                (thread, project)
            };
            let limit = int(args, "limit").unwrap_or(50).min(1000) as u32;
            let Reply::Files(files) = conn.call(Command::SearchFiles { thread, project, query, limit }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let human = if files.is_empty() { "No matches.".to_owned() } else { files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join("\n") };
            let list: Vec<Value> = files.iter().map(|f| json!({"path": f.path, "is_dir": f.is_dir})).collect();
            Ok(Outcome::ok(json!({"files": list}), human))
        }
        ["queue"] => {
            let id = thread_arg("thread")?;
            let raw = s(args, "item").unwrap_or_default();
            let item: ItemId = raw.parse().map_err(|_| CliError::usage(format!("“{raw}” is not an item id.")))?;
            if flag(args, "now") {
                conn.call(Command::SteerQueued { thread: id, item }).await?;
                Ok(Outcome::ok(json!({"thread": id.to_string(), "item": item.to_string(), "sent": true}), format!("Sent {item} now")))
            } else {
                let to = int(args, "to").ok_or_else(|| CliError::usage("Say where with --to, or send it with --now."))?;
                conn.call(Command::MoveQueued { thread: id, item, to: to as u32 }).await?;
                Ok(Outcome::ok(json!({"thread": id.to_string(), "item": item.to_string(), "position": to}), format!("Moved {item} to place {to}")))
            }
        }
        ["draft"] => {
            let target = if flag(args, "home") { DraftTarget::Home } else { DraftTarget::Thread(thread_arg("thread")?) };
            let text = text(args, "text", ctx.origin)?;
            let draft = Draft { text: text.clone(), attachments: Vec::new(), updated_ms: now_ms() };
            conn.call(Command::SaveDraft { target, draft }).await?;
            let human = if text.trim().is_empty() { "Cleared the draft.".to_owned() } else { "Saved the draft.".to_owned() };
            Ok(Outcome::ok(json!({"saved": !text.trim().is_empty()}), human))
        }
        ["generate"] => {
            let input = text(args, "input", ctx.origin)?;
            let task = match s(args, "task").unwrap_or("custom") {
                "title" => TextTask::ThreadTitle,
                "commit" => TextTask::CommitMessage,
                "pr" => TextTask::PullRequest,
                _ => TextTask::Custom {
                    instructions: s(args, "instructions")
                        .ok_or_else(|| CliError::usage("`custom` needs --instructions."))?
                        .to_owned(),
                },
            };
            let thread = match s(args, "thread") {
                Some(sel) => Some(select::thread(&conn.state(), &ctx.env, sel)?),
                None => None,
            };
            let Reply::Text(text) = conn.call(Command::GenerateText { thread, task, input }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            Ok(Outcome::ok(json!({"text": text}), text))
        }
        ["settle"] => {
            let id = thread_arg("thread")?;
            check_lineage(ctx, conn, id, Lineage::Control)?;
            let settled = !flag(args, "undo");
            patch(conn, id, ThreadPatch { settled: Some(settled), ..Default::default() }).await?;
            let word = if settled { "Settled" } else { "Unsettled" };
            Ok(Outcome::ok(json!({"thread": id.to_string(), "settled": settled}), format!("{word} {id}")))
        }
        ["archive"] => {
            let id = thread_arg("thread")?;
            check_lineage(ctx, conn, id, Lineage::Control)?;
            let archived = !flag(args, "undo");
            patch(conn, id, ThreadPatch { archived: Some(archived), ..Default::default() }).await?;
            let word = if archived { "Archived" } else { "Unarchived" };
            Ok(Outcome::ok(json!({"thread": id.to_string(), "archived": archived}), format!("{word} {id}")))
        }
        ["snooze"] => {
            let id = thread_arg("thread")?;
            let until = if flag(args, "wake") {
                None
            } else {
                let text = s(args, "for").ok_or_else(|| CliError::usage("Say for how long (e.g. 2h), or --wake."))?;
                let d = duration(text)?.ok_or_else(|| CliError::usage("Snooze for longer than 0."))?;
                Some(now_ms() + d.as_millis() as u64)
            };
            patch(conn, id, ThreadPatch { snooze_until: Some(until), ..Default::default() }).await?;
            let human = match until {
                Some(t) => format!("Snoozed {id} for {}", render::ago_or_in(t)),
                None => format!("Woke {id}"),
            };
            Ok(Outcome::ok(json!({"thread": id.to_string(), "snoozed_until_ms": until}), human))
        }
        ["diff"] => diff(conn, thread_arg("thread")?, args).await,
        ["undo-turn"] if flag(args, "redo") => {
            let id = thread_arg("thread")?;
            conn.call(Command::UndoRestore { thread: id }).await?;
            Ok(Outcome::ok(json!({"thread": id.to_string(), "undid_restore": true}), format!("Put {id}’s files back as they were before the last restore.")))
        }
        ["undo-turn"] => {
            let id = thread_arg("thread")?;
            let turns = conn.state().threads.get(&id).map(|t| t.turns).unwrap_or(0);
            let turn = int(args, "turn").map(|t| t as u32).unwrap_or(turns);
            if turn == 0 || turn > turns {
                return Err(CliError::usage(if turns == 0 { "This thread has no turns yet.".into() } else { format!("Pick a turn from 1 to {turns}.") }));
            }
            conn.call(Command::RestoreCheckpoint { thread: id, turn }).await?;
            Ok(Outcome::ok(
                json!({"thread": id.to_string(), "restored_before_turn": turn}),
                format!("Restored {id} to before turn {turn}. Put the files back with `orbit undo-turn {id} --redo`."),
            ))
        }
        ["terminal", "run"] => terminal_run(ctx, conn, args, out).await,
        ["pr", _] | ["issue", _] | ["ship"] => crate::forge::run(ctx, conn, spec.path, args).await,
        ["team", _] | ["race"] => crate::teams::run(ctx, conn, spec.path, args).await,
        ["terminal", "read"] => {
            let raw = s(args, "terminal").unwrap_or_default();
            let term: TermId = raw.parse().map_err(|_| CliError::usage(format!("“{raw}” is not a terminal id.")))?;
            let max_bytes = int(args, "bytes").unwrap_or(0).min(u32::MAX as u64) as u32;
            let reply = conn.call(Command::ReadTerminal { term, max_bytes }).await.map_err(not_found_if_engine)?;
            let Reply::TerminalTail { info, data, dropped } = reply else { return Err(CliError::engine("Unexpected reply.")) };
            let output = if flag(args, "raw") { String::from_utf8_lossy(&data).into_owned() } else { render::strip_ansi(&data) };
            let value = json!({"terminal": json::terminal(&info), "output": output, "dropped_bytes": dropped});
            Ok(Outcome::ok(value, output))
        }
        ["terminal", "list"] => {
            let Reply::Terminals(list) = conn.call(Command::ListTerminals).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let human = render::terminals(&list);
            Ok(Outcome::ok(json!({"terminals": list.iter().map(json::terminal).collect::<Vec<_>>()}), human))
        }
        ["search"] => {
            let query = text(args, "query", ctx.origin)?;
            let limit = int(args, "limit").unwrap_or(20).min(500) as u32;
            let Reply::SearchHits(hits) = conn.call(Command::Search { query, limit }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let state = conn.state();
            let list: Vec<Value> = hits
                .iter()
                .map(|h| {
                    let title = state.threads.get(&h.thread).map(json::title).unwrap_or("");
                    json!({"thread": h.thread.to_string(), "title": title, "item": h.item.to_string(), "excerpt": h.excerpt})
                })
                .collect();
            let human = render::search(&list);
            Ok(Outcome::ok(json!({"hits": list}), human))
        }
        _ => Err(CliError::usage(format!("`{}` can’t run here.", spec.name()))),
    }
}

fn not_found_if_engine(err: CliError) -> CliError {
    if err.exit == exit::ERROR { CliError { exit: exit::NOT_FOUND, ..err } } else { err }
}

async fn patch(conn: &Conn, id: ThreadId, patch: ThreadPatch) -> Result<(), CliError> {
    conn.call(Command::UpdateThread { id, patch }).await.map(drop)
}

// ── Status and lists ────────────────────────────────────────────────────

fn overview(conn: &Conn) -> Outcome {
    let state = conn.state();
    let active: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
    let count = |want: State| active.iter().filter(|t| State::of(t) == want).count();
    let mut needs: Vec<&Thread> = active.iter().copied().filter(|t| State::of(t) == State::NeedsYou).collect();
    needs.sort_by_key(|t| t.updated_ms);
    let target = &conn.target;
    let value = json!({
        "engine": {
            "url": target.url,
            "kind": target.endpoint.as_ref().map(|e| e.kind.as_str()).unwrap_or("remote"),
            "version": target.endpoint.as_ref().map(|e| e.version.as_str()),
            "pid": target.endpoint.as_ref().map(|e| e.pid),
        },
        "counts": {
            "working": count(State::Working) + count(State::Starting),
            "needs_you": count(State::NeedsYou),
            "idle": count(State::Idle),
            "failed": count(State::Failed),
            "rate_limited": count(State::RateLimited),
            "unread": active.iter().filter(|t| t.unread).count(),
            "threads": active.len(),
            "projects": state.projects.len(),
        },
        "needs_you": needs.iter().map(|t| json!({
            "id": t.id.to_string(),
            "title": json::title(t),
            "pending": t.pending.iter().map(json::pending).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "providers": state.providers.iter().map(|p| json!({
            "provider": json::provider_id(p.kind),
            "installed": p.installed,
            "version": p.version,
            "signed_in": p.signed_in,
            "account": p.account,
            "message": p.message,
        })).collect::<Vec<_>>(),
    });
    let human = render::overview(&value);
    Outcome::ok(value, human)
}

async fn thread_status(conn: &Conn, id: ThreadId) -> Result<Outcome, CliError> {
    let items = read_items(conn, id, 30).await?;
    let state = conn.state();
    let thread = state.threads.get(&id).ok_or_else(|| CliError::not_found("That thread no longer exists."))?;
    let mut value = json::thread(&state, thread);
    value["last_message"] = json!(json::last_message(&items));
    let children: Vec<Value> = state
        .threads
        .values()
        .filter(|t| t.parent == Some(id) && !t.archived)
        .map(|t| json!({"id": t.id.to_string(), "title": json::title(t), "state": State::of(t).as_str()}))
        .collect();
    value["children"] = json!(children);
    let human = render::thread_detail(&value);
    Ok(Outcome::ok(value, human))
}

async fn read_items(conn: &Conn, id: ThreadId, limit: u32) -> Result<Vec<Item>, CliError> {
    match conn.call(Command::ReadTimeline { id, after: None, limit }).await? {
        Reply::Timeline { items, .. } => Ok(items),
        _ => Err(CliError::engine("Unexpected reply.")),
    }
}

fn threads(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let state = conn.state();
    let project = s(args, "project").map(|p| select::project(&state, &ctx.env, p)).transpose()?;
    let parent = s(args, "parent").map(|p| select::thread(&state, &ctx.env, p)).transpose()?;
    let status = s(args, "status");
    let all = flag(args, "all");
    let needs_you = flag(args, "needs-you");
    let mut list: Vec<&Thread> = state
        .threads
        .values()
        .filter(|t| all || (!t.archived && t.settled_ms.is_none()))
        .filter(|t| project.is_none_or(|p| t.project == Some(p)))
        .filter(|t| parent.is_none_or(|p| t.parent == Some(p)))
        .filter(|t| !needs_you || State::of(t) == State::NeedsYou)
        .filter(|t| match status {
            None => true,
            Some("unread") => t.unread,
            Some("working") => matches!(State::of(t), State::Working | State::Starting),
            Some(want) => State::of(t).as_str() == want,
        })
        .collect();
    list.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.updated_ms.cmp(&a.updated_ms)));
    let total = list.len();
    let limit = int(args, "limit").unwrap_or(0) as usize;
    if limit > 0 {
        list.truncate(limit);
    }
    let values: Vec<Value> = list.iter().map(|t| json::thread(&state, t)).collect();
    let human = render::threads(&values, total);
    Ok(Outcome::ok(json!({"threads": values, "total": total}), human))
}

// ── Starting threads ────────────────────────────────────────────────────

fn access_rank(a: Access) -> u8 {
    match a {
        Access::Readonly => 0,
        Access::Ask => 1,
        Access::AutoEdit => 2,
        Access::Full => 3,
    }
}

fn parse_access(s: &str) -> Access {
    match s {
        "readonly" => Access::Readonly,
        "auto-edit" => Access::AutoEdit,
        "full" => Access::Full,
        _ => Access::Ask,
    }
}

/// The thread acting through this process, if it exists.
fn caller(ctx: &Ctx, conn: &Conn) -> Option<Thread> {
    let own = ctx.env.caller()?;
    let id: ThreadId = own.parse().ok()?;
    conn.state().threads.get(&id).cloned()
}

async fn new_thread(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let prompt = text(args, "prompt", ctx.origin)?;
    if prompt.trim().is_empty() {
        return Err(CliError::usage("Write a prompt."));
    }
    let caller = caller(ctx, conn);
    let (start, capped) = {
        let state = conn.state();
        let parent = match s(args, "parent") {
            Some(sel) if ctx.origin == Origin::Cli => Some(state.threads[&select::thread(&state, &ctx.env, sel)?].clone()),
            _ => caller.clone(),
        };
        let project = match s(args, "project") {
            Some(sel) => Some(select::project(&state, &ctx.env, sel)?),
            None => parent.as_ref().and_then(|p| p.project).or_else(|| select::containing(&state, &ctx.env.cwd)),
        };
        let mut config = parent.as_ref().map(|p| p.config.clone()).unwrap_or_else(|| state.defaults.clone());
        if let Some(provider) = s(args, "provider") {
            let info = state.providers.iter().find(|p| p.instance_id().eq_ignore_ascii_case(provider));
            let (kind, instance) = match (info, provider.to_lowercase().as_str()) {
                (Some(info), _) => (info.kind, info.instance.clone()),
                (None, "claude") => (ProviderKind::Claude, String::new()),
                (None, "codex") => (ProviderKind::Codex, String::new()),
                (None, _) => {
                    let known: Vec<&str> = state.providers.iter().map(|p| p.instance_id()).collect();
                    return Err(CliError::usage(format!("No agent “{provider}”; this Orbit has {}.", known.join(", "))));
                }
            };
            let switched = ThreadConfig { provider: kind, instance: instance.clone(), ..Default::default() };
            if !config.same_instance(&switched) {
                // Models and options belong to the old agent.
                config = ThreadConfig { mode: config.mode, access: config.access, ..switched };
            }
        }
        if let Some(model) = s(args, "model") {
            config.model = model.to_owned();
        }
        if let Some(effort) = s(args, "effort") {
            check_effort(&state, &config, effort)?;
            config.options.retain(|o| o.id != "effort");
            config.options.push(OptionValue { id: "effort".into(), value: OptionSetting::Choice(effort.to_owned()) });
        }
        if let Some(mode) = s(args, "mode") {
            config.mode = if mode == "plan" { Mode::Plan } else { Mode::Build };
        }
        if let Some(access) = s(args, "access") {
            config.access = parse_access(access);
        }
        // An agent never hands a helper more power than it has itself.
        let mut capped = false;
        if ctx.origin == Origin::Tool
            && let Some(caller) = &caller
            && access_rank(config.access) > access_rank(caller.config.access)
        {
            config.access = caller.config.access;
            capped = true;
        }
        let start = ThreadStart {
            project,
            config,
            worktree: flag(args, "worktree"),
            parent: parent.map(|p| p.id),
            title: s(args, "title").unwrap_or_default().to_owned(),
            prompt: Some(prompt),
        };
        (start, capped)
    };
    let Reply::Thread(id) = conn.call(Command::StartThread(start)).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    let mut started = json!({"thread": id.to_string()});
    if capped {
        started["access_capped"] = json!(true);
    }
    if flag(args, "wait") {
        let mut outcome = wait_and_report(ctx, conn, vec![id], Until::Finished, false, args, Some(started)).await?;
        let state = conn.state();
        if let Some(thread) = state.threads.get(&id) {
            outcome.value["details"] = json::thread(&state, thread);
        }
        return Ok(outcome);
    }
    // The thread record may not have reached us yet; give it a moment.
    let thread = wait_for_thread(conn, id).await;
    if let Some(thread) = &thread {
        started["details"] = json::thread(&conn.state(), thread);
    }
    let human = match &thread {
        Some(t) => format!(
            "{id}  {}\nFollow it with `orbit tail {id}`; wait for it with `orbit wait {id}`.",
            json::title(t)
        ),
        None => id.to_string(),
    };
    Ok(Outcome::ok(started, human))
}

/// A just-created thread's record, once the event carrying it arrives.
async fn wait_for_thread(conn: &Conn, id: ThreadId) -> Option<Thread> {
    let mut changes = conn.changes();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(t) = conn.state().threads.get(&id) {
            return Some(t.clone());
        }
        if tokio::time::timeout_at(deadline, changes.changed()).await.is_err() {
            return None;
        }
    }
}

fn check_effort(state: &orbit_net::state::ClientState, config: &ThreadConfig, effort: &str) -> Result<(), CliError> {
    let Some(provider) = state.provider_for(config) else { return Ok(()) };
    let model = provider
        .models
        .iter()
        .find(|m| m.slug == config.model)
        .or_else(|| provider.models.iter().find(|m| m.is_default && config.model.is_empty()));
    let Some(model) = model else { return Ok(()) };
    let Some(descriptor) = model.options.iter().find(|o| o.id == "effort") else {
        return Err(CliError::usage(format!("{} doesn’t take a reasoning effort.", model.name)));
    };
    if let OptionKind::Select { choices } = &descriptor.kind
        && !choices.iter().any(|c| c.id == effort)
    {
        let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
        return Err(CliError::usage(format!("{} takes effort {}; not “{effort}”.", model.name, ids.join(", "))));
    }
    Ok(())
}

// ── Lineage: what an agent may do to other threads ──────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lineage {
    /// Send messages: to its own descendants and ancestors.
    Talk,
    /// Stop, settle, archive, deny, answer: its descendants only.
    Control,
    /// Approve: its descendants, and only when it has full access itself.
    Approve,
}

fn is_ancestor(state: &orbit_net::state::ClientState, ancestor: ThreadId, mut of: ThreadId) -> bool {
    for _ in 0..64 {
        match state.threads.get(&of).and_then(|t| t.parent) {
            Some(parent) if parent == ancestor => return true,
            Some(parent) => of = parent,
            None => return false,
        }
    }
    false
}

/// Agents (MCP calls from inside an Orbit thread) act only on their own
/// helpers, so an agent can't steer, stop or approve the user's other work.
/// The command line is the user's and is not limited.
fn check_lineage(ctx: &Ctx, conn: &Conn, target: ThreadId, lineage: Lineage) -> Result<(), CliError> {
    if ctx.origin != Origin::Tool {
        return Ok(());
    }
    let Some(caller) = caller(ctx, conn) else { return Ok(()) };
    let state = conn.state();
    let descendant = is_ancestor(&state, caller.id, target);
    let allowed = match lineage {
        Lineage::Talk => descendant || target == caller.id || is_ancestor(&state, target, caller.id),
        Lineage::Control => descendant,
        Lineage::Approve => descendant && caller.config.access == Access::Full,
    };
    if allowed {
        return Ok(());
    }
    let message = match lineage {
        Lineage::Approve if descendant => "Only a thread with full access may approve its helpers’ requests; the user can.",
        Lineage::Talk => "Agents may message only the threads they started and their own parents.",
        _ => "Agents may act only on the threads they started.",
    };
    Err(CliError::new(exit::ERROR, message))
}

// ── Requests ────────────────────────────────────────────────────────────

async fn resolve(ctx: &Ctx, conn: &Conn, spec: &Spec, args: &Map<String, Value>, id: ThreadId) -> Result<Outcome, CliError> {
    let (answering, lineage) = match spec.path {
        ["approve"] => (Answering::Approval, Lineage::Approve),
        ["deny"] => (Answering::Any, Lineage::Control),
        _ => (Answering::Question, Lineage::Control),
    };
    check_lineage(ctx, conn, id, lineage)?;
    let thread = conn.state().threads.get(&id).cloned().ok_or_else(|| CliError::not_found("That thread no longer exists."))?;
    let pending = select::request(&thread, s(args, "request"), answering)?;
    let decision = match spec.path {
        ["approve"] if flag(args, "session") => Decision::AllowForSession,
        ["approve"] => Decision::Allow,
        ["deny"] => Decision::Deny { feedback: s(args, "reason").unwrap_or_default().to_owned() },
        _ => Decision::Answer { text: text(args, "text", ctx.origin)? },
    };
    conn.call(Command::Resolve { thread: id, request: pending.request.id, decision: decision.clone() }).await?;
    let summary = json::request_summary(&pending.request.kind);
    let verb = match &decision {
        Decision::Allow => "Approved",
        Decision::AllowForSession => "Approved for the session",
        Decision::Deny { .. } => "Denied",
        Decision::Answer { .. } => "Answered",
    };
    let value = json!({
        "thread": id.to_string(),
        "request": pending.request.id.to_string(),
        "summary": summary,
        "decision": json::decision(&decision),
    });
    Ok(Outcome::ok(value, format!("{verb}: {summary}")))
}

// ── Waiting ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    Finished,
    Idle,
    NeedsYou,
}

impl Until {
    pub fn parse(s: &str) -> Until {
        match s {
            "idle" => Until::Idle,
            "needs-you" => Until::NeedsYou,
            _ => Until::Finished,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Until::Finished => "finished",
            Until::Idle => "idle",
            Until::NeedsYou => "needs-you",
        }
    }

    /// The exit code once a thread in `state` ends the wait, or `None` while
    /// it should keep waiting.
    pub fn verdict(self, state: Option<State>) -> Option<i32> {
        let Some(state) = state else { return Some(exit::NOT_FOUND) };
        match (self, state) {
            (_, State::Working | State::Starting) => None,
            (Until::Finished, State::Idle) | (Until::Idle, State::Idle) | (Until::NeedsYou, State::NeedsYou) => Some(exit::OK),
            (Until::Finished, State::NeedsYou) => Some(exit::NEEDS_YOU),
            (Until::Idle, State::NeedsYou) => None,
            // It resumes by itself when the quota resets.
            (Until::Idle, State::RateLimited) => None,
            (_, State::Failed | State::RateLimited) => Some(exit::FAILED),
            (Until::NeedsYou, State::Idle) => Some(exit::IDLE),
        }
    }
}

pub struct WaitResult {
    /// Per thread: its state (`None` once deleted) and verdict.
    pub threads: Vec<(ThreadId, Option<State>, Option<i32>)>,
    pub timed_out: bool,
    pub exit: i32,
}

/// Waits until `until` holds for all (or, with `any`, one) of `ids`. A
/// state must hold for [`SETTLE`] before it counts, so the gap between a
/// turn ending and a queued message starting is never taken for the end.
pub async fn wait_for(conn: &Conn, ids: &[ThreadId], until: Until, any: bool, timeout: Option<Duration>) -> WaitResult {
    let mut changes = conn.changes();
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let mut holding_since: Option<tokio::time::Instant> = None;
    loop {
        let threads: Vec<(ThreadId, Option<State>, Option<i32>)> = {
            let state = conn.state();
            ids.iter()
                .map(|id| {
                    let st = state.threads.get(id).map(State::of);
                    (*id, st, until.verdict(st))
                })
                .collect()
        };
        let done = if any { threads.iter().any(|t| t.2.is_some()) } else { threads.iter().all(|t| t.2.is_some()) };
        let now = tokio::time::Instant::now();
        if done {
            let since = *holding_since.get_or_insert(now);
            if now >= since + SETTLE {
                let exit = if any {
                    // Prefer a thread that reached what was asked for.
                    threads.iter().filter_map(|t| t.2).min_by_key(|code| (*code != exit::OK, *code)).unwrap_or(exit::OK)
                } else {
                    threads.iter().filter_map(|t| t.2).find(|code| *code != exit::OK).unwrap_or(exit::OK)
                };
                return WaitResult { threads, timed_out: false, exit };
            }
        } else {
            holding_since = None;
            if deadline.is_some_and(|d| now >= d) {
                return WaitResult { threads, timed_out: true, exit: exit::TIMEOUT };
            }
        }
        let wake = match (holding_since, deadline) {
            (Some(since), _) => Some(since + SETTLE),
            (None, deadline) => deadline,
        };
        match wake {
            Some(at) => {
                let _ = tokio::time::timeout_at(at, changes.changed()).await;
            }
            None => {
                if changes.changed().await.is_err() {
                    // The connection is gone.
                    return WaitResult { threads, timed_out: false, exit: exit::UNAVAILABLE };
                }
            }
        }
        if !conn.connected() {
            let threads = ids.iter().map(|id| (*id, None, None)).collect();
            return WaitResult { threads, timed_out: false, exit: exit::UNAVAILABLE };
        }
    }
}

async fn wait_and_report(
    ctx: &Ctx,
    conn: &Conn,
    ids: Vec<ThreadId>,
    until: Until,
    any: bool,
    args: &Map<String, Value>,
    started: Option<Value>,
) -> Result<Outcome, CliError> {
    let timeout = duration(s(args, "timeout").unwrap_or("10m"))?;
    let _ = ctx;
    let result = wait_for(conn, &ids, until, any, timeout).await;
    if result.exit == exit::UNAVAILABLE {
        return Err(CliError::unavailable("Lost the connection to the engine while waiting."));
    }
    let mut threads = Vec::new();
    let mut human = String::new();
    for (id, st, verdict) in &result.threads {
        let mut entry = json!({"id": id.to_string(), "state": st.map(State::as_str).unwrap_or("deleted")});
        if let Some(code) = verdict {
            entry["result"] = json!(crate::spec::exit::ALL.iter().find(|e| e.0 == *code).map(|e| e.1).unwrap_or("error"));
        }
        let thread = conn.state().threads.get(id).cloned();
        if let Some(thread) = &thread {
            entry["title"] = json!(json::title(thread));
            if !thread.pending.is_empty() {
                entry["pending"] = json!(thread.pending.iter().map(json::pending).collect::<Vec<_>>());
            }
            if let ThreadStatus::Failed { message } = &thread.status {
                entry["error"] = json!(message);
            }
            if verdict.is_some() && *st != Some(State::NeedsYou) {
                let items = read_items(conn, *id, 30).await.unwrap_or_default();
                entry["last_message"] = json!(json::last_message(&items));
            }
        }
        human.push_str(&render::wait_entry(&entry));
        threads.push(entry);
    }
    if result.timed_out {
        human.push_str("Timed out.\n");
    }
    let mut value = started.unwrap_or_else(|| json!({}));
    value["until"] = json!(until.as_str());
    value["timed_out"] = json!(result.timed_out);
    value["threads"] = json!(threads);
    Ok(Outcome { value, exit: result.exit, human: Some(human.trim_end().to_owned()) })
}

// ── Streaming a timeline ────────────────────────────────────────────────

async fn tail(ctx: &Ctx, conn: &Conn, id: ThreadId, args: &Map<String, Value>, out: &mut dyn Write) -> Result<Outcome, CliError> {
    let json_out = ctx.json();
    let full = flag(args, "full");
    let until = s(args, "until").map(Until::parse);
    let limit = int(args, "limit").unwrap_or(10) as u32;
    // Subscribe first: the read's mark arrives on the stream.
    let mut events = conn.events();
    let Reply::Timeline { items, mark } = conn.call(Command::ReadTimeline { id, after: None, limit }).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    let thread = conn.state().threads.get(&id).cloned().ok_or_else(|| CliError::not_found("That thread no longer exists."))?;
    let mut printer = render::Printer::new(full, json_out);
    let write = |out: &mut dyn Write, text: String| -> Result<(), CliError> {
        out.write_all(text.as_bytes()).and_then(|_| out.flush()).map_err(|e| CliError::engine(format!("Couldn’t write: {e}")))
    };
    write(out, printer.header(&thread))?;
    for item in &items {
        write(out, printer.item(item))?;
    }
    let mut past_mark = false;
    let mut changes = conn.changes();
    let mut holding_since: Option<tokio::time::Instant> = None;
    loop {
        // Stop condition, as in `wait`.
        if let Some(until) = until {
            let st = conn.state().threads.get(&id).map(State::of);
            match until.verdict(st) {
                Some(code) => {
                    let now = tokio::time::Instant::now();
                    let since = *holding_since.get_or_insert(now);
                    if now >= since + SETTLE {
                        return Ok(Outcome { value: json!({"thread": id.to_string(), "state": st.map(State::as_str)}), exit: code, human: None });
                    }
                }
                None => holding_since = None,
            }
        }
        let batch = tokio::select! {
            batch = events.recv() => batch,
            _ = changes.changed() => continue,
            _ = tokio::time::sleep(SETTLE), if holding_since.is_some() => continue,
        };
        let batch = match batch {
            Ok(batch) => batch,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                write(out, printer.notice(&format!("(skipped {n} updates)")))?;
                continue;
            }
            Err(_) => return Err(CliError::unavailable("Lost the connection to the engine.")),
        };
        for env in batch.iter() {
            match &env.event {
                Event::TimelineMark { thread, mark: m } if *thread == id && *m == mark => past_mark = true,
                _ if !past_mark => {}
                Event::ItemUpserted { thread, item } if *thread == id => write(out, printer.item(item))?,
                Event::ItemDelta { thread, item, delta } if *thread == id => write(out, printer.delta(*item, delta))?,
                Event::ThreadUpserted(t) if t.id == id => write(out, printer.thread(t))?,
                Event::ThreadRemoved(t) if *t == id => {
                    write(out, printer.notice("The thread was deleted."))?;
                    return Ok(Outcome { value: Value::Null, exit: exit::NOT_FOUND, human: None });
                }
                _ => {}
            }
        }
    }
}

// ── Diffs ───────────────────────────────────────────────────────────────

async fn diff(conn: &Conn, id: ThreadId, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let turns = conn.state().threads.get(&id).map(|t| t.turns).unwrap_or(0);
    let (scope, label) = if let Some(turn) = int(args, "turn") {
        (DiffScope::Turn(turn as u32), format!("turn {turn}"))
    } else if let Some(turn) = int(args, "since-turn") {
        (DiffScope::SinceTurn(turn as u32), format!("since turn {turn}"))
    } else if flag(args, "last") {
        if turns == 0 {
            return Err(CliError::usage("This thread has no turns yet."));
        }
        (DiffScope::Turn(turns), format!("turn {turns}"))
    } else if let Some(base) = s(args, "base") {
        (DiffScope::Base(base.to_owned()), format!("against {base}"))
    } else if flag(args, "uncommitted") {
        (DiffScope::Uncommitted, "uncommitted".into())
    } else {
        (DiffScope::Thread, "thread".into())
    };
    let Reply::Diff(mut patch) = conn.call(Command::Diff { thread: id, scope }).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    let files: Vec<FileChange> = orbit_proto::diff::parse(&patch).iter().map(|f| f.to_change()).collect();
    let max = int(args, "max-bytes").unwrap_or(0) as usize;
    let mut truncated = false;
    if max > 0 && patch.len() > max {
        let mut cut = max;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        // End on a whole line.
        if let Some(nl) = patch[..cut].rfind('\n') {
            cut = nl + 1;
        }
        patch.truncate(cut);
        truncated = true;
    }
    let stat = flag(args, "stat");
    let mut value = json!({
        "thread": id.to_string(),
        "scope": label,
        "files": files.iter().map(json::file).collect::<Vec<_>>(),
        "additions": files.iter().map(|f| f.additions as u64).sum::<u64>(),
        "deletions": files.iter().map(|f| f.deletions as u64).sum::<u64>(),
    });
    if !stat {
        value["patch"] = json!(patch);
        value["truncated"] = json!(truncated);
    }
    let human = if stat || patch.is_empty() { render::diff_stat(&files) } else { patch };
    Ok(Outcome::ok(value, human))
}

// ── Merging ─────────────────────────────────────────────────────────────

async fn merge(ctx: &Ctx, conn: &Conn, id: ThreadId, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let dry_run = flag(args, "dry-run");
    if !dry_run {
        check_lineage(ctx, conn, id, Lineage::Control)?;
    }
    let (into, message) = {
        let state = conn.state();
        let thread = state.threads.get(&id).ok_or_else(|| CliError::not_found("That thread no longer exists."))?;
        let base = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).and_then(|p| p.base_branch.clone());
        let into = s(args, "into").map(str::to_owned).or(base).unwrap_or_else(|| "main".into());
        let message = s(args, "message").map(str::to_owned).unwrap_or_else(|| json::title(thread).to_owned());
        (into, message)
    };
    let Reply::Merge(info) = conn.call(Command::MergeLocal { thread: id, into, message, dry_run }).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    let value = json!({
        "thread": id.to_string(),
        "dry_run": dry_run,
        "source": info.source,
        "target": info.target,
        "commits": info.commits,
        "uncommitted": info.uncommitted,
        "files": info.files.iter().map(json::file).collect::<Vec<_>>(),
        "conflicts": info.conflicts,
        "fast_forward": info.fast_forward,
        "merged": info.merged,
    });
    let mut human = format!(
        "{} {} into {}: {} commits, {} files{}{}",
        if dry_run { "Would merge" } else { "Merged" },
        info.source,
        info.target,
        info.commits,
        info.files.len(),
        if info.uncommitted > 0 { format!(", {} uncommitted files committed first", info.uncommitted) } else { String::new() },
        if info.fast_forward { " (fast-forward)" } else { "" },
    );
    if !info.conflicts.is_empty() {
        human.push_str(&format!("\nConflicts: {}", info.conflicts.join(", ")));
    }
    let exit = if dry_run && !info.conflicts.is_empty() { exit::ERROR } else { exit::OK };
    Ok(Outcome { value, exit, human: Some(human) })
}

// ── Terminals ───────────────────────────────────────────────────────────

async fn terminal_run(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>, out: &mut dyn Write) -> Result<Outcome, CliError> {
    let command = s(args, "command").ok_or_else(|| CliError::usage("Give a command to run."))?.to_owned();
    let cwd = if let Some(sel) = s(args, "thread") {
        let state = conn.state();
        let id = select::thread(&state, &ctx.env, sel)?;
        let thread = &state.threads[&id];
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
        thread
            .workspace
            .worktree
            .clone()
            .or_else(|| project.map(|p| p.path.clone()))
            .ok_or_else(|| CliError::usage("That thread has no project folder; use --cwd."))?
    } else {
        let dir = ctx.env.cwd.join(s(args, "cwd").unwrap_or("."));
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        dir.to_string_lossy().trim_start_matches(r"\\?\").to_owned()
    };
    let timeout = duration(s(args, "timeout").unwrap_or("0"))?;
    let raw = flag(args, "raw");
    let json_out = ctx.json();
    let mut events = conn.events();
    let (cols, rows) = (200, 50);
    let Reply::Terminal(term) = conn.call(Command::RunInTerminal { cwd: cwd.clone(), command: command.clone(), cols, rows }).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    let info = json!({"terminal": term.to_string(), "command": command, "cwd": cwd});
    if flag(args, "detach") {
        return Ok(Outcome::ok(info, format!("{term}\nRead it with `orbit terminal read {term}`.")));
    }
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut captured: Vec<u8> = Vec::new();
    let mut ansi = render::AnsiStripper::default();
    let code = loop {
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let next = match remaining {
            Some(r) => match tokio::time::timeout(r, events.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    let _ = conn.call(Command::CloseTerminal { term }).await;
                    let mut value = info;
                    value["timed_out"] = json!(true);
                    value["output"] = json!(String::from_utf8_lossy(&captured));
                    return Ok(Outcome { value, exit: exit::TIMEOUT, human: if json_out { None } else { Some("Timed out; stopped it.".into()) } });
                }
            },
            None => events.recv().await,
        };
        let batch = match next {
            Ok(batch) => batch,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return Err(CliError::unavailable("Lost the connection to the engine.")),
        };
        let mut finished = None;
        for env in batch.iter() {
            match &env.event {
                Event::TerminalOutput { term: t, data } if *t == term => {
                    let text = if raw { String::from_utf8_lossy(data).into_owned() } else { ansi.feed(data) };
                    if json_out {
                        captured.extend_from_slice(text.as_bytes());
                    } else {
                        let _ = out.write_all(text.as_bytes());
                        let _ = out.flush();
                    }
                }
                Event::TerminalExited { term: t, code } if *t == term => finished = Some(*code),
                _ => {}
            }
        }
        if let Some(code) = finished {
            break code;
        }
    };
    let mut value = info;
    value["exit_code"] = json!(code);
    if json_out {
        value["output"] = json!(String::from_utf8_lossy(&captured));
    }
    // Pass the command's own status through, like `ssh` and `docker run`.
    let exit = match code {
        Some(c) => c.clamp(0, 255),
        None => exit::ERROR,
    };
    Ok(Outcome { value, exit, human: None })
}

// ── The desktop app ─────────────────────────────────────────────────────

async fn open(ctx: &Ctx, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    // Resolve the selector through the engine when one runs; a full id
    // works without it.
    let engine = conn::resolve(&ctx.global, &ctx.env).ok();
    if engine.as_ref().is_some_and(|t| t.endpoint.as_ref().is_some_and(|e| e.kind == "orbitd")) {
        return Err(CliError::unavailable("This data folder is served by orbitd, not the desktop app.")
            .hint("Open the desktop app connected to that host (ORBIT_HOST) to see it."));
    }
    let thread = match s(args, "thread") {
        None => None,
        Some(sel) if engine.is_some() => Some(select::thread(&ctx.conn().await?.state(), &ctx.env, sel)?),
        Some(sel) => Some(sel.parse::<ThreadId>().map_err(|_| {
            CliError::unavailable("Orbit isn’t running, so only a full thread id can be opened.")
        })?),
    };
    launch_desktop(ctx, thread).await?;
    let human = match thread {
        Some(id) => format!("Showing {id} in Orbit."),
        None => "Brought Orbit forward.".into(),
    };
    Ok(Outcome::ok(json!({"thread": thread.map(|t| t.to_string()), "opened": true}), human))
}

/// Runs `orbit-desktop [--thread <id>]` from next to this executable. When
/// the app already runs for this data folder, that launch hands its
/// arguments to it (the desktop's single-instance hand-off: it comes
/// forward and shows the thread) and exits; otherwise it is the app.
async fn launch_desktop(ctx: &Ctx, thread: Option<ThreadId>) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(|e| CliError::unavailable(e.to_string()))?;
    let name = if cfg!(windows) { "orbit-desktop.exe" } else { "orbit-desktop" };
    let app = exe.with_file_name(name);
    if !app.is_file() {
        return Err(CliError::unavailable(format!("Couldn’t find the desktop app ({}).", app.display()))
            .hint("Start Orbit yourself; `orbit` looks for orbit-desktop next to itself."));
    }
    let mut cmd = std::process::Command::new(&app);
    if let Some(id) = thread {
        cmd.arg("--thread").arg(id.to_string());
    }
    cmd.env("ORBIT_DATA", conn::data_dir(&ctx.global, &ctx.env));
    cmd.current_dir(&ctx.env.cwd);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| CliError::unavailable(format!("Couldn’t start {}: {e}", app.display())))?;
    // A hand-off finishes in milliseconds; a process still running after
    // a few seconds is the app itself, now open.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let mut message = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut message);
                }
                let message = message.trim();
                return Err(CliError::unavailable(if message.is_empty() { "Orbit didn’t open.".to_owned() } else { message.to_owned() }));
            }
            Ok(None) if Instant::now() >= deadline => return Ok(()),
            // It is the app, and its engine is up.
            Ok(None) if conn::resolve(&ctx.global, &ctx.env)
                .is_ok_and(|t| t.endpoint.is_some_and(|e| e.pid == child.id())) =>
            {
                return Ok(());
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(e) => return Err(CliError::unavailable(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration("90s").unwrap(), Some(Duration::from_secs(90)));
        assert_eq!(duration("10m").unwrap(), Some(Duration::from_secs(600)));
        assert_eq!(duration("1h30m").unwrap(), Some(Duration::from_secs(5400)));
        assert_eq!(duration("45").unwrap(), Some(Duration::from_secs(45)));
        assert_eq!(duration("0").unwrap(), None);
        assert!(duration("soon").is_err());
        assert!(duration("").is_err());
    }

    #[test]
    fn wait_verdicts() {
        use Until::*;
        assert_eq!(Finished.verdict(Some(State::Working)), None);
        assert_eq!(Finished.verdict(Some(State::Idle)), Some(exit::OK));
        assert_eq!(Finished.verdict(Some(State::NeedsYou)), Some(exit::NEEDS_YOU));
        assert_eq!(Finished.verdict(Some(State::Failed)), Some(exit::FAILED));
        assert_eq!(Idle.verdict(Some(State::NeedsYou)), None);
        assert_eq!(Idle.verdict(Some(State::RateLimited)), None);
        assert_eq!(NeedsYou.verdict(Some(State::NeedsYou)), Some(exit::OK));
        assert_eq!(NeedsYou.verdict(Some(State::Idle)), Some(exit::IDLE));
        assert_eq!(Finished.verdict(None), Some(exit::NOT_FOUND));
    }
}
