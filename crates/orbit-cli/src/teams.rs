//! `orbit team …` and `orbit race`: agent teams and races
//! ([`TeamCommand`]). Inside a team member's thread (`ORBIT_THREAD`) the
//! commands act as that member; on the command line `--as` names one.

use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{Ctx, Origin, Outcome};
use crate::select;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn int(args: &Map<String, Value>, name: &str) -> Option<u64> {
    args.get(name).and_then(Value::as_u64)
}

fn list(args: &Map<String, Value>, name: &str) -> Vec<String> {
    s(args, name).map(|v| v.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect()).unwrap_or_default()
}

/// Free text; `-` reads standard input on the command line.
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

/// The team member this runs as: `--as` on the command line, else the
/// thread the process runs in.
fn actor(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<ThreadId, CliError> {
    let state = conn.state();
    if let Some(sel) = s(args, "as")
        && ctx.origin == Origin::Cli
    {
        return select::thread(&state, &ctx.env, sel);
    }
    let own = ctx.env.caller().and_then(|c| c.parse::<ThreadId>().ok()).filter(|id| state.threads.contains_key(id));
    own.ok_or_else(|| CliError::usage("Run this inside a team member's thread, or name one with --as."))
}

async fn team(conn: &Conn, command: TeamCommand) -> Result<Reply, CliError> {
    conn.call(Command::Team(command)).await
}

/// `config` switched to the agent named `provider` (an instance id).
fn with_provider(state: &orbit_net::state::ClientState, mut config: ThreadConfig, provider: &str) -> Result<ThreadConfig, CliError> {
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
    let switched = ThreadConfig { provider: kind, instance, ..Default::default() };
    if !config.same_instance(&switched) {
        config = ThreadConfig { mode: config.mode, access: config.access, ..switched };
    }
    Ok(config)
}

fn access(s: &str) -> Access {
    match s {
        "readonly" => Access::Readonly,
        "auto-edit" => Access::AutoEdit,
        "full" => Access::Full,
        _ => Access::Ask,
    }
}

fn word(debug: impl std::fmt::Debug) -> String {
    let text = format!("{debug:?}");
    let head = text.split([' ', '{', '(']).next().unwrap_or_default();
    let mut out = String::new();
    for (i, c) in head.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn worker_json(w: &Worker) -> Value {
    let mut v = json!({
        "key": w.key,
        "title": w.title,
        "state": word(&w.state),
        "thread": w.thread.map(|t| t.to_string()),
        "parent": w.parent.to_string(),
        "depends_on": w.depends_on,
        "scope": w.scope,
        "summary": w.summary,
    });
    match &w.state {
        WorkerState::Waiting(reason) => v["waiting"] = json!(reason.describe()),
        WorkerState::Failed { message } => v["error"] = json!(message),
        _ => {}
    }
    if let Some(m) = &w.merge {
        v["merge"] = json!({"merged": m.merged, "conflicts": m.conflicts, "files": m.files.len()});
    }
    v
}

fn team_json(t: &Team) -> Value {
    let tally = t.tally();
    json!({
        "team": t.id.to_string(),
        "architect": t.architect.to_string(),
        "tally": {"working": tally.working, "done": tally.done, "waiting": tally.waiting, "failed": tally.failed},
        "workers": t.workers.iter().map(worker_json).collect::<Vec<_>>(),
        "breaker": t.breaker.as_ref().map(|b| b.reason.clone()),
        "limits": {"max_workers": t.limits.max_workers, "max_failures": t.limits.max_failures, "max_cost_micros": t.limits.max_cost_micros},
        "notes": t.notes.len(),
        "overlaps": t.overlaps.iter().map(|o| json!({"path": o.path, "threads": o.threads.iter().map(|t| t.to_string()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    })
}

fn team_text(t: &Team) -> String {
    let tally = t.tally();
    let mut out = format!("{} working · {} done · {} waiting", tally.working, tally.done, tally.waiting);
    if tally.failed > 0 {
        out.push_str(&format!(" · {} failed", tally.failed));
    }
    if let Some(b) = &t.breaker {
        out.push_str(&format!("\nSpawning paused: {}.", b.reason));
    }
    for w in &t.workers {
        let state = match &w.state {
            WorkerState::Waiting(r) => format!("waiting: {}", r.describe()),
            WorkerState::Failed { message } => format!("failed: {message}"),
            other => word(other),
        };
        out.push_str(&format!("\n{:<4} {:<9} {}  ({state})", w.key, w.thread.map(|t| t.to_string()).unwrap_or_default(), w.title));
    }
    out
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["team", "start"] => start(ctx, conn, args).await,
        ["race"] => race(ctx, conn, args).await,
        ["team", "spawn"] => {
            let brief = text(args, "brief", ctx.origin)?;
            let actor = actor(ctx, conn, args)?;
            let config = match (s(args, "provider"), s(args, "model")) {
                (None, None) => None,
                (provider, model) => {
                    let state = conn.state();
                    let mut config = state.threads.get(&actor).map(|t| t.config.clone()).unwrap_or_default();
                    if let Some(p) = provider {
                        config = with_provider(&state, config, p)?;
                    }
                    if let Some(m) = model {
                        config.model = m.to_owned();
                    }
                    Some(config)
                }
            };
            let reply = team(
                conn,
                TeamCommand::SpawnWorker {
                    actor,
                    title: s(args, "title").unwrap_or_default().to_owned(),
                    brief,
                    depends_on: list(args, "depends-on"),
                    scope: list(args, "scope"),
                    config,
                },
            )
            .await?;
            let Reply::Team(TeamReply::Worker { key, thread, state, .. }) = reply else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let human = match &state {
                WorkerState::Waiting(reason) => format!("{key} waits: {}.", reason.describe()),
                _ => format!("{key} started{}.", thread.map(|t| format!(" in {t}")).unwrap_or_default()),
            };
            let mut value = json!({"worker": key, "thread": thread.map(|t| t.to_string()), "state": word(&state)});
            if let WorkerState::Waiting(reason) = &state {
                value["waiting"] = json!(reason.describe());
            }
            Ok(Outcome::ok(value, format!("{human} Its summary reaches you as team mail when it finishes.")))
        }
        ["team", "message"] => {
            let to = s(args, "to").unwrap_or_default().to_owned();
            let body = text(args, "text", ctx.origin)?;
            let kind = MailKind::parse(s(args, "kind").unwrap_or("message")).unwrap_or(MailKind::Message);
            let actor = actor(ctx, conn, args)?;
            let urgent = flag(args, "urgent");
            team(conn, TeamCommand::Message { actor: Some(actor), team: None, to: to.clone(), kind, text: body, urgent }).await?;
            let human = if urgent {
                format!("Sent to {to}; it steers a working recipient now (at most once a minute).")
            } else {
                format!("Stored for {to}; it is delivered between its turns.")
            };
            Ok(Outcome::ok(json!({"to": to, "stored": true, "urgent": urgent}), human))
        }
        ["team", "note"] => {
            let body = text(args, "text", ctx.origin)?;
            let actor = actor(ctx, conn, args)?;
            team(conn, TeamCommand::PostNote { actor: Some(actor), team: None, text: body }).await?;
            Ok(Outcome::ok(json!({"posted": true}), "Posted to the team's notes.".into()))
        }
        ["team", "notes"] => {
            let actor = actor(ctx, conn, args)?;
            let limit = int(args, "limit").unwrap_or(20) as u32;
            let Reply::Team(TeamReply::Notes { notes, more }) =
                team(conn, TeamCommand::ReadNotes { actor: Some(actor), team: None, limit }).await?
            else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let list: Vec<Value> =
                notes.iter().map(|n| json!({"id": n.id, "author": n.author_label, "text": n.text, "at_ms": n.at_ms})).collect();
            let mut human: String = notes.iter().map(|n| format!("[{}] {}\n", n.author_label, n.text)).collect();
            if notes.is_empty() {
                human.push_str("No notes yet.\n");
            }
            if more > 0 {
                human.push_str(&format!("{more} more notes.\n"));
            }
            Ok(Outcome::ok(json!({"notes": list, "more": more}), human.trim_end().to_owned()))
        }
        ["team", "status"] => {
            let actor = actor(ctx, conn, args)?;
            let Reply::Team(TeamReply::Team(t)) = team(conn, TeamCommand::Status { actor: Some(actor), team: None }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            Ok(Outcome::ok(team_json(&t), team_text(&t)))
        }
        ["team", "merge"] => {
            let worker = s(args, "worker").unwrap_or_default().to_owned();
            let dry_run = flag(args, "dry-run");
            let actor = actor(ctx, conn, args)?;
            let Reply::Merge(info) =
                team(conn, TeamCommand::MergeWorker { actor: Some(actor), team: None, worker: worker.clone(), dry_run }).await?
            else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let value = json!({
                "worker": worker,
                "dry_run": dry_run,
                "source": info.source,
                "target": info.target,
                "commits": info.commits,
                "files": info.files.iter().map(|f| json!({"path": f.path, "additions": f.additions, "deletions": f.deletions})).collect::<Vec<_>>(),
                "conflicts": info.conflicts,
                "merged": info.merged,
            });
            let human = if !info.conflicts.is_empty() {
                format!("Merging {worker} would conflict in {}. Nothing changed.", info.conflicts.join(", "))
            } else if info.merged.is_some() {
                format!("Merged {worker} ({}) into {}: {} files.", info.source, info.target, info.files.len())
            } else {
                format!("{worker} would merge cleanly into {}: {} files.", info.target, info.files.len())
            };
            Ok(Outcome::ok(value, human))
        }
        _ => Err(CliError::usage("Not a team command.")),
    }
}

async fn start(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let prompt = text(args, "prompt", ctx.origin)?;
    if prompt.trim().is_empty() {
        return Err(CliError::usage("Write the architect's first message."));
    }
    let start = {
        let state = conn.state();
        let project = match s(args, "project") {
            Some(sel) => Some(select::project(&state, &ctx.env, sel)?),
            None => select::containing(&state, &ctx.env.cwd),
        };
        let project = project.ok_or_else(|| CliError::usage("A team works in a project: pick one with --project."))?;
        let mut config = state.defaults.clone();
        if let Some(p) = s(args, "provider") {
            config = with_provider(&state, config, p)?;
        }
        if let Some(m) = s(args, "model") {
            config.model = m.to_owned();
        }
        if let Some(a) = s(args, "access") {
            config.access = access(a);
        }
        ThreadStart { project: Some(project), config, worktree: !flag(args, "here"), prompt: Some(prompt), ..Default::default() }
    };
    let max_cost = match s(args, "max-cost") {
        Some(v) => {
            let dollars: f64 = v.trim_start_matches('$').parse().map_err(|_| CliError::usage(format!("“{v}” is not an amount.")))?;
            (dollars * 1e6) as u64
        }
        None => 0,
    };
    let limits = TeamLimits {
        max_workers: int(args, "max-workers").unwrap_or(4).clamp(1, 16) as u32,
        max_failures: int(args, "max-failures").unwrap_or(3) as u32,
        max_cost_micros: max_cost,
        ..TeamLimits::default()
    };
    let Reply::Thread(id) = team(conn, TeamCommand::Start { start, limits, worker_config: None }).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    Ok(Outcome::ok(
        json!({"thread": id.to_string()}),
        format!("{id}  architect\nFollow it with `orbit tail {id}`; see its crew with `orbit team status --as {id}`."),
    ))
}

async fn race(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let prompt = text(args, "prompt", ctx.origin)?;
    if prompt.trim().is_empty() {
        return Err(CliError::usage("Write the prompt to race."));
    }
    let (project, configs) = {
        let state = conn.state();
        let project = match s(args, "project") {
            Some(sel) => select::project(&state, &ctx.env, sel)?,
            None => select::containing(&state, &ctx.env.cwd)
                .ok_or_else(|| CliError::usage("Pick a project with --project (the current folder is not in one)."))?,
        };
        let providers = list(args, "provider");
        let models = list(args, "model");
        let n = (int(args, "n").unwrap_or(3) as usize).max(providers.len()).max(models.len()).clamp(2, 8);
        let mut configs = Vec::new();
        for i in 0..n {
            let mut config = state.defaults.clone();
            if let Some(p) = providers.get(i).or(providers.last()) {
                config = with_provider(&state, config, p)?;
            }
            if let Some(m) = models.get(i).or(if providers.len() > 1 { None } else { models.last() }) {
                config.model = m.clone();
            }
            configs.push(config);
        }
        (project, configs)
    };
    let command = TeamCommand::StartRace {
        project,
        prompt,
        configs,
        test_command: s(args, "test").map(str::to_owned),
        judge: flag(args, "judge"),
        base: s(args, "base").unwrap_or_default().to_owned(),
    };
    let Reply::Team(TeamReply::Race(id)) = team(conn, command).await? else {
        return Err(CliError::engine("Unexpected reply."));
    };
    // The race record follows on the event stream.
    let mut changes = conn.changes();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let race = loop {
        if let Some(r) = conn.state().races.get(&id).cloned() {
            break Some(r);
        }
        if tokio::time::timeout_at(deadline, changes.changed()).await.is_err() {
            break None;
        }
    };
    let threads: Vec<Value> = race
        .iter()
        .flat_map(|r| r.candidates.iter().map(|c| json!({"label": c.label, "thread": c.thread.to_string()})))
        .collect();
    let human = match &race {
        Some(r) => {
            let mut out = format!("{id}: {} candidates", r.candidates.len());
            for c in &r.candidates {
                out.push_str(&format!("\n{}  {}", c.label, c.thread));
            }
            out
        }
        None => id.to_string(),
    };
    Ok(Outcome::ok(json!({"race": id.to_string(), "candidates": threads}), human))
}
