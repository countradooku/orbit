//! `orbit automation …`: scheduled and event-triggered agent runs
//! ([`AutomationCommand`]).

use std::fmt::Write as _;
use std::time::Duration;

use orbit_net::state::ClientState;
use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{self, Ctx, Outcome, duration};
use crate::render::ago_or_in;
use crate::select;
use crate::spec::exit;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn int(args: &Map<String, Value>, name: &str) -> Option<u64> {
    args.get(name).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn unexpected() -> CliError {
    CliError::engine("Unexpected reply.")
}

async fn call(conn: &Conn, command: AutomationCommand) -> Result<Reply, CliError> {
    conn.call(Command::Automation(command)).await
}

/// An automation by id, exact name, or a unique part of its name.
pub fn automation(state: &ClientState, selector: &str) -> Result<AutomationId, CliError> {
    let sel = selector.trim();
    if let Ok(id) = sel.parse::<AutomationId>()
        && state.automation(id).is_some()
    {
        return Ok(id);
    }
    if let Some(a) = state.automations.iter().find(|a| a.spec.name.eq_ignore_ascii_case(sel)) {
        return Ok(a.id);
    }
    let lower = sel.to_lowercase();
    let matches: Vec<&Automation> = state
        .automations
        .iter()
        .filter(|a| a.spec.name.to_lowercase().contains(&lower) || a.id.to_string().contains(&lower))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id),
        [] => Err(CliError::not_found(format!("No automation matches “{sel}”.")).hint("See them with `orbit automation list`.")),
        many => Err(CliError::not_found(format!(
            "“{sel}” matches {} automations: {}.",
            many.len(),
            many.iter().map(|a| a.spec.name.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn word(debug: impl std::fmt::Debug) -> String {
    let text = format!("{debug:?}");
    let text = text.split(['(', ' ', '{']).next().unwrap_or_default();
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// A trigger in words, as it was written (`cron 0 9 * * 1-5`).
pub fn trigger_text(state: &ClientState, trigger: &Trigger) -> String {
    let zone = |tz: &str| if tz.is_empty() { String::new() } else { format!(" ({tz})") };
    match trigger {
        Trigger::Cron { expr, tz } => format!("cron {expr}{}", zone(tz)),
        Trigger::Rule { rule, tz } => format!("rule {rule}{}", zone(tz)),
        Trigger::Interval { minutes } => match minutes {
            m if m % 60 == 0 => format!("every {}h", m / 60),
            m => format!("every {m}m"),
        },
        Trigger::Event(event) => match event {
            EventTrigger::ChecksFailed { mine: true } => "when checks fail on my pull requests".into(),
            EventTrigger::ChecksFailed { mine: false } => "when checks fail on a pull request".into(),
            EventTrigger::ReviewRequested => "when my review is requested".into(),
            EventTrigger::IssueLabeled { label } => format!("on new issues labeled {label}"),
            EventTrigger::BranchPushed { branch } => format!("when {branch} is pushed"),
            EventTrigger::WatchPr { number } => format!("watching #{number} until it merges"),
            EventTrigger::Finished { source, when } => {
                let name = match source {
                    ChainSource::Automation(id) => {
                        state.automation(*id).map(|a| format!("“{}”", a.spec.name)).unwrap_or_else(|| id.to_string())
                    }
                    ChainSource::Thread(id) => {
                        state.threads.get(id).map(|t| format!("thread “{}”", crate::json::title(t))).unwrap_or_else(|| id.to_string())
                    }
                };
                let when = match when {
                    ChainWhen::Succeeded => "succeeds",
                    ChainWhen::Failed => "fails",
                    ChainWhen::Either => "finishes",
                };
                format!("after {name} {when}")
            }
        },
    }
}

fn status_text(status: &RunStatus) -> String {
    match status {
        RunStatus::Running => "running".into(),
        RunStatus::Succeeded => "succeeded".into(),
        RunStatus::Failed(message) => format!("failed: {message}"),
        RunStatus::TimedOut => "timed out".into(),
        RunStatus::Cancelled => "cancelled".into(),
        RunStatus::Skipped(skip) => format!("skipped: {}", skip_text(skip)),
    }
}

fn skip_text(skip: &Skip) -> String {
    match skip {
        Skip::Missed { count: 1 } => "missed while Orbit was closed or asleep".into(),
        Skip::Missed { count } => format!("missed {count} runs while Orbit was closed or asleep"),
        Skip::StillRunning => "the previous run was still going".into(),
        Skip::DailyLimit => "the daily limit was reached".into(),
        Skip::NoChanges => "nothing changed since the last run".into(),
        Skip::Dirty => "the project had uncommitted changes".into(),
        Skip::CheckFailed(message) => format!("the check failed: {message}"),
        Skip::Unavailable(message) => message.clone(),
    }
}

fn reason_text(reason: &RunReason) -> String {
    match reason {
        RunReason::Schedule => "schedule".into(),
        RunReason::Late => "schedule (late)".into(),
        RunReason::Manual => "run now".into(),
        RunReason::Event(what) => what.clone(),
    }
}

pub fn run_json(run: &AutomationRun) -> Value {
    let (status, detail) = match &run.status {
        RunStatus::Failed(message) => ("failed", Some(message.clone())),
        RunStatus::Skipped(skip) => ("skipped", Some(skip_text(skip))),
        RunStatus::Running => ("running", None),
        RunStatus::Succeeded => ("succeeded", None),
        RunStatus::TimedOut => ("timed_out", None),
        RunStatus::Cancelled => ("cancelled", None),
    };
    json!({
        "id": run.id.to_string(),
        "status": status,
        "detail": detail,
        "reason": reason_text(&run.reason),
        "thread": run.thread.map(|t| t.to_string()),
        "due_ms": run.due_ms,
        "started_ms": run.started_ms,
        "finished_ms": run.finished_ms,
        "duration_ms": run.duration_ms(),
        "tokens": run.tokens,
        "changed": run.changed,
        "summary": run.summary,
        "pr_url": (!run.pr_url.is_empty()).then_some(&run.pr_url),
        "repeats": run.repeats,
    })
}

pub fn automation_json(state: &ClientState, a: &Automation) -> Value {
    let project = a.spec.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
    let target = match a.spec.target {
        RunTarget::Worktree => json!("worktree"),
        RunTarget::Project => json!("project"),
        RunTarget::PullRequest => json!("pr"),
        RunTarget::Thread(t) => json!({"thread": t.to_string()}),
    };
    let last = state.runs(a.id).first();
    json!({
        "id": a.id.to_string(),
        "name": a.spec.name,
        "project": project.map(|p| p.name.clone()),
        "paused": a.paused,
        "target": target,
        "triggers": a.spec.triggers.iter().map(|t| trigger_text(state, t)).collect::<Vec<_>>(),
        "next_run_ms": a.next_run_ms(),
        "pending": a.state.pending.len(),
        "note": (!a.state.note.is_empty()).then_some(&a.state.note),
        "last_run": last.map(run_json),
        "prompt": a.spec.prompt,
        "agent": {
            "provider": a.spec.config.instance_id(),
            "model": a.spec.config.model,
            "effort": crate::json::effort(&a.spec.config),
            "mode": crate::json::mode_id(a.spec.config.mode),
            "access": crate::json::access_id(a.spec.config.access),
        },
        "checks": {
            "only_if_changed": a.spec.checks.changes_since_last_run,
            "only_if_clean": a.spec.checks.clean_repo,
            "command": a.spec.checks.command,
            "skip_if_running": a.spec.checks.skip_if_running,
        },
        "limits": {
            "max_runs_per_day": a.spec.limits.max_runs_per_day,
            "timeout_minutes": a.spec.limits.timeout_minutes,
            "grace_minutes": a.spec.limits.grace_minutes,
        },
        "on_finish": {
            "notify": word(a.spec.on_finish.notify),
            "settle_if_unchanged": a.spec.on_finish.settle_if_unchanged,
            "open_pr": a.spec.on_finish.open_pr,
        },
        "recipe": (!a.spec.recipe.is_empty()).then_some(&a.spec.recipe),
    })
}

fn line(state: &ClientState, a: &Automation) -> String {
    let when = if a.paused {
        "paused".to_owned()
    } else if !a.state.pending.is_empty() {
        "starting".to_owned()
    } else {
        match a.next_run_ms() {
            Some(t) => ago_or_in(t),
            None => "on events".to_owned(),
        }
    };
    let last = match state.runs(a.id).first() {
        Some(run) if run.status == RunStatus::Running => "running now".to_owned(),
        Some(run) => format!("last {} {}", status_glyph(&run.status), ago_or_in(run.finished_ms.max(run.due_ms))),
        None => "never ran".to_owned(),
    };
    let triggers: Vec<String> = a.spec.triggers.iter().map(|t| trigger_text(state, t)).collect();
    format!("{}  {:<28} {:<10} {:<16} {}", a.id, crate::json::one_line(&a.spec.name, 28), when, last, triggers.join("; "))
}

fn status_glyph(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "…",
        RunStatus::Succeeded => "✓",
        RunStatus::Failed(_) | RunStatus::TimedOut => "✗",
        RunStatus::Skipped(_) | RunStatus::Cancelled => "–",
    }
}

fn history_text(runs: &[AutomationRun]) -> String {
    if runs.is_empty() {
        return "No runs yet.".into();
    }
    let mut out = String::new();
    for run in runs {
        let when = ago_or_in(if run.started_ms > 0 { run.started_ms } else { run.finished_ms.max(run.due_ms) });
        let repeats = if run.repeats > 0 { format!(" ×{}", run.repeats + 1) } else { String::new() };
        let _ = write!(out, "{} {:<8} {}{repeats} · {}", status_glyph(&run.status), when, status_text(&run.status), reason_text(&run.reason));
        if run.duration_ms() > 0 {
            let _ = write!(out, " · {}s", run.duration_ms() / 1000);
        }
        if run.tokens > 0 {
            let _ = write!(out, " · {} tokens", run.tokens);
        }
        if let Some(thread) = run.thread {
            let _ = write!(out, " · {thread}");
        }
        out.push('\n');
        if !run.summary.is_empty() {
            let _ = writeln!(out, "    {}", crate::json::one_line(&run.summary, 110));
        }
        if !run.pr_url.is_empty() {
            let _ = writeln!(out, "    {}", run.pr_url);
        }
    }
    out.trim_end().to_owned()
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let pick = |args: &Map<String, Value>| -> Result<AutomationId, CliError> {
        automation(&conn.state(), s(args, "automation").unwrap_or_default())
    };
    match path {
        ["automation", "list"] => {
            let state = conn.state();
            let project = s(args, "project").map(|p| select::project(&state, &ctx.env, p)).transpose()?;
            let list: Vec<&Automation> =
                state.automations.iter().filter(|a| project.is_none() || a.spec.project == project).collect();
            let value: Vec<Value> = list.iter().map(|a| automation_json(&state, a)).collect();
            let human = if list.is_empty() {
                "No automations yet. Create one with `orbit automation create`, or start from a recipe: \
                 `orbit help automation create`."
                    .to_owned()
            } else {
                list.iter().map(|a| line(&state, a)).collect::<Vec<_>>().join("\n")
            };
            Ok(Outcome::ok(json!(value), human))
        }
        ["automation", "show"] => {
            let id = pick(args)?;
            let state = conn.state();
            let a = state.automation(id).ok_or_else(|| CliError::not_found("That automation is gone."))?;
            let mut value = automation_json(&state, a);
            let runs: Vec<AutomationRun> = state.runs(id).iter().take(5).cloned().collect();
            value["runs"] = json!(runs.iter().map(run_json).collect::<Vec<_>>());
            let mut human = format!("{}  {}\n", a.id, a.spec.name);
            let _ = writeln!(human, "  {}", a.spec.triggers.iter().map(|t| trigger_text(&state, t)).collect::<Vec<_>>().join("; "));
            if a.paused {
                let _ = writeln!(human, "  Paused{}", if a.state.note.is_empty() { String::new() } else { format!(": {}", a.state.note) });
            } else if let Some(next) = a.next_run_ms() {
                let _ = writeln!(human, "  Next run {}", ago_or_in(next));
            }
            let _ = writeln!(human, "\n{}\n", a.spec.prompt.trim());
            human.push_str(&history_text(&runs));
            Ok(Outcome::ok(value, human))
        }
        ["automation", "create"] => create(ctx, conn, args).await,
        ["automation", "run"] => {
            let id = pick(args)?;
            let Reply::Automation(AutomationReply::Run(run)) = call(conn, AutomationCommand::RunNow { id }).await? else {
                return Err(unexpected());
            };
            if !flag(args, "wait") {
                return Ok(Outcome::ok(
                    json!({"run": run.to_string()}),
                    format!("Started {run}. See it with `orbit automation history {id}`."),
                ));
            }
            let timeout = duration(s(args, "timeout").unwrap_or("10m"))?;
            wait_for_run(conn, id, run, timeout).await
        }
        ["automation", "pause"] | ["automation", "resume"] => {
            let id = pick(args)?;
            let paused = path[1] == "pause";
            call(conn, AutomationCommand::SetPaused { id, paused }).await?;
            let verb = if paused { "Paused" } else { "Resumed" };
            Ok(Outcome::ok(json!({"id": id.to_string(), "paused": paused}), format!("{verb} {id}.")))
        }
        ["automation", "delete"] => {
            let id = pick(args)?;
            call(conn, AutomationCommand::Delete { id }).await?;
            Ok(Outcome::ok(json!({"id": id.to_string(), "deleted": true}), format!("Deleted {id} and its history.")))
        }
        ["automation", "history"] => {
            let id = pick(args)?;
            let limit = int(args, "limit").unwrap_or(20) as u32;
            let Reply::Automation(AutomationReply::Runs(runs)) = call(conn, AutomationCommand::History { id, limit }).await? else {
                return Err(unexpected());
            };
            Ok(Outcome::ok(json!(runs.iter().map(run_json).collect::<Vec<_>>()), history_text(&runs)))
        }
        _ => Err(CliError::usage("Unknown automation command.")),
    }
}

/// Waits for a run to end, then reports it (exit `failed` if it failed).
async fn wait_for_run(conn: &Conn, id: AutomationId, run: RunId, timeout: Option<Duration>) -> Result<Outcome, CliError> {
    let mut changes = conn.changes();
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    loop {
        let found = {
            let state = conn.state();
            let pending = state.automation(id).is_some_and(|a| a.state.pending.iter().any(|p| p.id == run));
            state.runs(id).iter().find(|r| r.id == run).cloned().filter(|r| !pending && !r.status.is_active())
        };
        if let Some(record) = found {
            let mut outcome = Outcome::ok(run_json(&record), history_text(std::slice::from_ref(&record)));
            if record.status.is_failure() {
                outcome.exit = exit::FAILED;
            }
            return Ok(outcome);
        }
        let wait = changes.changed();
        match deadline {
            Some(at) => {
                if tokio::time::timeout_at(at, wait).await.is_err() {
                    return Err(CliError::new(exit::TIMEOUT, format!("{run} is still going.")));
                }
            }
            None => {
                let _ = wait.await;
            }
        }
    }
}

fn minutes(args: &Map<String, Value>, name: &str, default: &str) -> Result<u32, CliError> {
    Ok(duration(s(args, name).unwrap_or(default))?.map(|d| d.as_secs().div_ceil(60) as u32).unwrap_or(0))
}

async fn create(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<Outcome, CliError> {
    let prompt = exec::text(args, "prompt", ctx.origin)?;
    let (project, config, thread, after) = {
        let state = conn.state();
        let project = match s(args, "project") {
            Some(sel) => Some(select::project(&state, &ctx.env, sel)?),
            None => select::containing(&state, &ctx.env.cwd),
        };
        let config = exec::agent_config(&state, state.defaults.clone(), args)?;
        let thread = s(args, "thread").map(|t| select::thread(&state, &ctx.env, t)).transpose()?;
        let after = s(args, "after").map(|a| automation(&state, a)).transpose()?;
        (project, config, thread, after)
    };
    let mut spec = match s(args, "recipe") {
        Some(id) => {
            let command = AutomationCommand::Recipe { id: id.to_owned(), project, config: config.clone() };
            let Reply::Automation(AutomationReply::Spec(spec)) = call(conn, command).await? else { return Err(unexpected()) };
            *spec
        }
        None => AutomationSpec { project, config: config.clone(), ..Default::default() },
    };
    // Options given on the command line win over the recipe's.
    let given = |name: &str| args.get(name).is_some_and(|v| !v.is_null() && v != &json!(false) && v != &json!(""));
    if !prompt.trim().is_empty() {
        spec.prompt = prompt.trim().to_owned();
    }
    if let Some(name) = s(args, "name") {
        spec.name = name.to_owned();
    } else if spec.name.is_empty() {
        spec.name = crate::json::one_line(&spec.prompt, 40);
    }
    if s(args, "recipe").is_some() {
        // The recipe chose its mode and access; options still win.
        if given("mode") {
            spec.config.mode = config.mode;
        }
        if given("access") {
            spec.config.access = config.access;
        }
    } else if !given("access") {
        // Nobody is there to approve edits while it runs.
        spec.config.access = Access::AutoEdit;
    }
    let tz = s(args, "tz").unwrap_or_default().to_owned();
    let mut triggers = Vec::new();
    if let Some(expr) = s(args, "cron") {
        triggers.push(Trigger::Cron { expr: expr.to_owned(), tz: tz.clone() });
    }
    if let Some(rule) = s(args, "rule") {
        triggers.push(Trigger::Rule { rule: rule.to_owned(), tz: tz.clone() });
    }
    if let Some(every) = s(args, "every") {
        let minutes = duration(every)?.map(|d| d.as_secs().div_ceil(60) as u32).unwrap_or(0);
        triggers.push(Trigger::Interval { minutes });
    }
    if let Some(on) = s(args, "on") {
        let need = |name: &str| s(args, name).map(str::to_owned).ok_or_else(|| CliError::usage(format!("--on {on} needs --{name}.")));
        let event = match on {
            "checks-failed" => EventTrigger::ChecksFailed { mine: false },
            "my-checks-failed" => EventTrigger::ChecksFailed { mine: true },
            "review-requested" => EventTrigger::ReviewRequested,
            "issue-labeled" => EventTrigger::IssueLabeled { label: need("label")? },
            "branch-pushed" => EventTrigger::BranchPushed { branch: need("branch")? },
            "watch-pr" => EventTrigger::WatchPr {
                number: int(args, "pr").ok_or_else(|| CliError::usage("--on watch-pr needs --pr."))?,
            },
            "after" => EventTrigger::Finished {
                source: ChainSource::Automation(after.ok_or_else(|| CliError::usage("--on after needs --after."))?),
                when: match s(args, "when").unwrap_or("succeeded") {
                    "failed" => ChainWhen::Failed,
                    "finished" => ChainWhen::Either,
                    _ => ChainWhen::Succeeded,
                },
            },
            other => return Err(CliError::usage(format!("Unknown event “{other}”."))),
        };
        triggers.push(Trigger::Event(event));
    }
    if !triggers.is_empty() {
        spec.triggers = triggers;
    } else {
        // A recipe's placeholders filled from the options.
        for trigger in &mut spec.triggers {
            match trigger {
                Trigger::Event(EventTrigger::IssueLabeled { label }) if given("label") => *label = s(args, "label").unwrap_or_default().into(),
                Trigger::Event(EventTrigger::BranchPushed { branch }) if given("branch") => {
                    *branch = s(args, "branch").unwrap_or_default().into()
                }
                Trigger::Event(EventTrigger::WatchPr { number }) => *number = int(args, "pr").unwrap_or(*number),
                Trigger::Cron { tz: zone, .. } | Trigger::Rule { tz: zone, .. } if !tz.is_empty() => *zone = tz.clone(),
                _ => {}
            }
        }
    }
    if spec.triggers.is_empty() {
        return Err(CliError::usage("Say when it runs: --cron, --rule, --every or --on."));
    }
    if let Some(thread) = thread {
        spec.target = RunTarget::Thread(thread);
    } else if s(args, "recipe").is_none() || given("target") {
        spec.target = match s(args, "target").unwrap_or("worktree") {
            "project" => RunTarget::Project,
            "pr" => RunTarget::PullRequest,
            _ => RunTarget::Worktree,
        };
    }
    if given("only-if-changed") {
        spec.checks.changes_since_last_run = true;
    }
    if given("only-if-clean") {
        spec.checks.clean_repo = true;
    }
    if let Some(check) = s(args, "check") {
        spec.checks.command = check.to_owned();
    }
    if given("overlap") {
        spec.checks.skip_if_running = false;
    }
    if let Some(max) = int(args, "max-per-day") {
        spec.limits.max_runs_per_day = max as u32;
    }
    if s(args, "recipe").is_none() || given("timeout") {
        spec.limits.timeout_minutes = minutes(args, "timeout", "60m")?;
    }
    if s(args, "recipe").is_none() || given("grace") {
        spec.limits.grace_minutes = minutes(args, "grace", "30m")?;
    }
    if s(args, "recipe").is_none() || given("notify") {
        spec.on_finish.notify = match s(args, "notify").unwrap_or("always") {
            "failures" => Notify::Failures,
            "never" => Notify::Never,
            _ => Notify::Always,
        };
    }
    if given("settle-if-unchanged") {
        spec.on_finish.settle_if_unchanged = true;
    }
    if given("open-pr") {
        spec.on_finish.open_pr = true;
    }
    let Reply::Automation(AutomationReply::Saved(id)) = call(conn, AutomationCommand::Save { id: None, spec: Box::new(spec) }).await? else {
        return Err(unexpected());
    };
    if flag(args, "paused") {
        call(conn, AutomationCommand::SetPaused { id, paused: true }).await?;
    }
    // The record arrives as an event; give it a moment.
    let mut changes = conn.changes();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        {
            let state = conn.state();
            if let Some(a) = state.automation(id) {
                let value = automation_json(&state, a);
                let when = match a.next_run_ms() {
                    Some(next) => format!("First run {}.", ago_or_in(next)),
                    None if a.paused => "It is paused.".to_owned(),
                    None => "It runs when its event happens.".to_owned(),
                };
                return Ok(Outcome::ok(value, format!("Created {id}  {}\n{when} Run it now with `orbit automation run {id}`.", a.spec.name)));
            }
        }
        if tokio::time::timeout_at(deadline, changes.changed()).await.is_err() {
            return Ok(Outcome::ok(json!({"id": id.to_string()}), format!("Created {id}.")));
        }
    }
}
