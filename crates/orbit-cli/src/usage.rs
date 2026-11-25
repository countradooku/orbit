//! `orbit usage …`: plan limits per account, token and cost stats, CSV
//! export, and moving a thread that hit its limit to another account
//! ([`UsageCommand`]).

use std::fmt::Write as _;

use orbit_net::state::ClientState;
use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{Ctx, Outcome, duration};
use crate::render::ago_or_in;
use crate::select;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn unexpected() -> CliError {
    CliError::engine("Unexpected reply.")
}

async fn call(conn: &Conn, command: UsageCommand) -> Result<Reply, CliError> {
    conn.call(Command::Usage(command)).await
}

async fn board(conn: &Conn) -> Result<QuotaBoard, CliError> {
    match call(conn, UsageCommand::Board).await? {
        Reply::Usage(reply) => match *reply {
            UsageReply::Board(board) => Ok(board),
            _ => Err(unexpected()),
        },
        _ => Err(unexpected()),
    }
}

/// A provider instance by id or display name (case-insensitive).
fn account(state: &ClientState, selector: &str) -> Result<String, CliError> {
    let sel = selector.trim();
    let found: Vec<&ProviderInfo> = state
        .providers
        .iter()
        .filter(|p| p.instance_id().eq_ignore_ascii_case(sel) || p.display_name().eq_ignore_ascii_case(sel))
        .collect();
    match found.as_slice() {
        [one] => Ok(one.instance_id().to_owned()),
        [] => Err(CliError::not_found(format!("No account “{sel}”."))
            .hint("`orbit usage` lists accounts; `orbit status` lists every agent.")),
        _ => Err(CliError::not_found(format!("“{sel}” names more than one account; use its id."))),
    }
}

fn policy_word(policy: FailoverPolicy) -> &'static str {
    match policy {
        FailoverPolicy::Ask => "ask",
        FailoverPolicy::Auto => "auto",
        FailoverPolicy::Never => "never",
    }
}

fn percent(share: f32) -> u32 {
    (share * 100.0).round() as u32
}

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn dollars(micros: u64) -> String {
    format!("${:.2}", micros as f64 / 1e6)
}

/// `1h 40m`, `3d 2h`.
fn span(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "under a minute".into(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, minutes % 1440 / 60),
    }
}

fn window_json(w: &QuotaWindowState) -> Value {
    json!({
        "window": w.window.window,
        "label": w.label,
        "used_percent": w.window.utilization.map(percent),
        "limited": w.window.rejected,
        "resets_at_ms": (w.window.resets_ms > 0).then_some(w.window.resets_ms),
        "burn_percent_per_hour": w.burn_per_hour.map(|b| (b * 1000.0).round() / 10.0),
        "runs_out_at_ms": w.runs_out_ms,
    })
}

fn status_text(status: &QuotaStatus) -> Option<&str> {
    match status {
        QuotaStatus::NoPlan(m) | QuotaStatus::Unavailable(m) => Some(m),
        _ => None,
    }
}

fn limits_outcome(board: &QuotaBoard) -> Outcome {
    let now = now_ms();
    let accounts: Vec<Value> = board
        .accounts
        .iter()
        .map(|a| {
            json!({
                "account": a.instance,
                "name": a.name,
                "agent": a.kind.label(),
                "plan": a.plan,
                "limited": a.limited(now),
                "status": match &a.status {
                    QuotaStatus::Unknown => "unknown",
                    QuotaStatus::Loading => "loading",
                    QuotaStatus::Ok => "ok",
                    QuotaStatus::NoPlan(_) => "no_plan",
                    QuotaStatus::Unavailable(_) => "unavailable",
                },
                "message": status_text(&a.status),
                "updated_ms": (a.updated_ms > 0).then_some(a.updated_ms),
                "windows": a.windows.iter().map(window_json).collect::<Vec<_>>(),
                "top": a.top.iter().map(|c| json!({"thread": c.thread.to_string(), "title": c.title, "tokens": c.tokens})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut human = String::new();
    if board.accounts.is_empty() {
        human.push_str("No accounts with plan limits yet: Claude Code and Codex report theirs once installed and signed in.\n");
    }
    for a in &board.accounts {
        let plan = a.plan.as_deref().map(|p| format!(" · {p}")).unwrap_or_default();
        let updated = if a.updated_ms > 0 { format!("  (updated {})", ago_or_in(a.updated_ms)) } else { String::new() };
        let _ = writeln!(human, "{}{plan}{updated}", a.name);
        if let Some(message) = status_text(&a.status) {
            let _ = writeln!(human, "  {message}");
        }
        for w in &a.windows {
            let used = if w.window.rejected { "limit reached".to_owned() } else { format!("{}%", w.window.utilization.map(percent).unwrap_or(0)) };
            let resets = if w.window.resets_ms > now { format!(", resets {}", ago_or_in(w.window.resets_ms)) } else { String::new() };
            let pace = match (w.burn_per_hour, w.runs_out_ms) {
                (Some(_), Some(at)) if at <= now => String::new(),
                (Some(rate), Some(at)) => format!(" · {}%/h, runs out in {}", percent(rate), span(at - now)),
                (Some(rate), None) if rate > 0.0 => format!(" · {}%/h, lasts until the reset", percent(rate)),
                _ => String::new(),
            };
            let _ = writeln!(human, "  {:<22} {used}{resets}{pace}", w.label);
        }
        if !a.top.is_empty() {
            let top: Vec<String> = a.top.iter().map(|c| format!("{} ({} tokens)", c.title, tokens(c.tokens))).collect();
            let _ = writeln!(human, "  Most used: {}", top.join(", "));
        }
    }
    let _ = write!(
        human,
        "When an account reaches its limit: {}. Change it with `orbit usage policy ask|auto|never`.",
        board.settings.failover.label().to_lowercase()
    );
    let settings = json!({
        "failover": policy_word(board.settings.failover),
        "alert_80": board.settings.alert_80,
        "alert_95": board.settings.alert_95,
        "poll": board.settings.poll,
        "pins": board.settings.pins.iter().map(|p| json!({
            "project": match p.target { PinTarget::Project(id) => Some(id.to_string()), _ => None },
            "automation": match p.target { PinTarget::Automation(id) => Some(id.to_string()), _ => None },
            "account": p.instance,
        })).collect::<Vec<_>>(),
    });
    Outcome::ok(json!({"accounts": accounts, "settings": settings}), human)
}

fn query(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<UsageQuery, CliError> {
    let state = conn.state();
    let since = if flag(args, "all") { None } else { duration(s(args, "since").unwrap_or("7d"))? };
    Ok(UsageQuery {
        from_ms: since.map(|d| now_ms().saturating_sub(d.as_millis() as u64)).unwrap_or(0),
        to_ms: 0,
        project: s(args, "project").map(|p| select::project(&state, &ctx.env, p)).transpose()?,
        instance: s(args, "account").map(|a| account(&state, a)).transpose()?,
        tz: std::env::var("TZ").unwrap_or_default(),
    })
}

fn totals_json(t: &UsageTotals) -> Value {
    json!({
        "turns": t.turns,
        "input_tokens": t.input_tokens,
        "output_tokens": t.output_tokens,
        "cache_read_tokens": t.cache_read_tokens,
        "cache_write_tokens": t.cache_write_tokens,
        "tokens": t.tokens(),
        "cost_usd": t.cost_micros as f64 / 1e6,
    })
}

fn group_json(g: &UsageGroup) -> Value {
    let mut v = totals_json(&g.totals);
    v["key"] = json!(g.key);
    v["label"] = json!(g.label);
    v
}

fn stats_outcome(stats: &UsageStats, by: &str) -> Outcome {
    let groups: Vec<UsageGroup> = match by {
        "day" => stats.days.iter().map(|d| UsageGroup { key: d.date.clone(), label: d.date.clone(), totals: d.totals }).collect(),
        "account" => stats.accounts.clone(),
        "project" => stats.projects.clone(),
        "thread" => stats.threads.clone(),
        _ => stats.models.clone(),
    };
    let value = json!({
        "totals": totals_json(&stats.totals),
        "by": by,
        "groups": groups.iter().map(group_json).collect::<Vec<_>>(),
    });
    let t = &stats.totals;
    let mut human = format!(
        "{} turns · {} tokens ({} in, {} out, {} cache read) · {}\n",
        t.turns,
        tokens(t.tokens()),
        tokens(t.input_tokens),
        tokens(t.output_tokens),
        tokens(t.cache_read_tokens),
        dollars(t.cost_micros)
    );
    let width = groups.iter().map(|g| g.label.chars().count()).max().unwrap_or(0).clamp(8, 40);
    for g in &groups {
        let label: String = g.label.chars().take(width).collect();
        let _ = writeln!(human, "  {label:<width$}  {:>8} tokens  {:>8}  {:>4} turns", tokens(g.totals.tokens()), dollars(g.totals.cost_micros), g.totals.turns);
    }
    if t.turns > 0 && t.cost_micros == 0 {
        human.push_str("Cost is shown as the agents report it; Codex reports none.\n");
    }
    Outcome::ok(value, human.trim_end().to_owned())
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["usage"] => Ok(limits_outcome(&board(conn).await?)),
        ["usage", "refresh"] => {
            let instance = s(args, "account").map(|a| account(&conn.state(), a)).transpose()?;
            call(conn, UsageCommand::Refresh { instance: instance.clone() }).await?;
            // Readings arrive as events; give them a moment, then show.
            let mut changes = conn.changes();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while changes.changed().await.is_ok() {
                    let state = conn.state();
                    let done = state.quota.accounts.iter().filter(|a| instance.as_deref().is_none_or(|i| a.instance == i)).all(|a| {
                        a.status != QuotaStatus::Loading && now_ms().saturating_sub(a.updated_ms) < 20_000
                            || matches!(a.status, QuotaStatus::NoPlan(_) | QuotaStatus::Unavailable(_))
                    });
                    if done {
                        break;
                    }
                }
            })
            .await;
            Ok(limits_outcome(&board(conn).await?))
        }
        ["usage", "stats"] => {
            let query = query(ctx, conn, args)?;
            let Reply::Usage(reply) = call(conn, UsageCommand::Stats(query)).await? else { return Err(unexpected()) };
            let UsageReply::Stats(stats) = *reply else { return Err(unexpected()) };
            Ok(stats_outcome(&stats, s(args, "by").unwrap_or("model")))
        }
        ["usage", "export"] => {
            let query = query(ctx, conn, args)?;
            let Reply::Text(csv) = call(conn, UsageCommand::Export(query)).await? else { return Err(unexpected()) };
            let rows = csv.lines().count().saturating_sub(1);
            match s(args, "out") {
                Some(path) => {
                    std::fs::write(path, &csv).map_err(|e| CliError::engine(format!("Couldn’t write {path}: {e}")))?;
                    Ok(Outcome::ok(json!({"file": path, "rows": rows}), format!("Wrote {rows} turns to {path}.")))
                }
                None if ctx.json() => Ok(Outcome::ok(json!({"csv": csv, "rows": rows}), String::new())),
                None => Ok(Outcome { value: json!({"rows": rows}), exit: crate::spec::exit::OK, human: Some(csv.trim_end().to_owned()) }),
            }
        }
        ["usage", "failover"] => {
            let thread = select::thread(&conn.state(), &ctx.env, s(args, "thread").unwrap_or_default())?;
            let instance = account(&conn.state(), s(args, "account").unwrap_or_default())?;
            call(conn, UsageCommand::Failover { thread, instance: instance.clone() }).await?;
            Ok(Outcome::ok(
                json!({"thread": thread.to_string(), "account": instance}),
                format!("Moved {thread} to {instance}; its limited turn runs again there. Follow it with `orbit wait {thread}`."),
            ))
        }
        ["usage", "policy"] => {
            let mut settings = board(conn).await?.settings;
            if let Some(word) = s(args, "policy") {
                settings.failover = match word {
                    "auto" => FailoverPolicy::Auto,
                    "never" => FailoverPolicy::Never,
                    _ => FailoverPolicy::Ask,
                };
            }
            if let Some(alerts) = s(args, "alerts") {
                let on = alerts == "on";
                (settings.alert_80, settings.alert_95) = (on, on);
            }
            call(conn, UsageCommand::SetSettings(settings.clone())).await?;
            Ok(Outcome::ok(
                json!({"failover": policy_word(settings.failover), "alert_80": settings.alert_80, "alert_95": settings.alert_95}),
                format!(
                    "When an account reaches its limit: {}. Alerts at 80 % and 95 %: {}.",
                    settings.failover.label().to_lowercase(),
                    if settings.alert_80 || settings.alert_95 { "on" } else { "off" }
                ),
            ))
        }
        ["usage", "pin"] => {
            let (project, automation) = {
                let state = conn.state();
                let project = s(args, "project").map(|p| select::project(&state, &ctx.env, p)).transpose()?;
                let automation = s(args, "automation").map(|a| crate::automation::automation(&state, a)).transpose()?;
                (project, automation)
            };
            let target = match (project, automation) {
                (Some(p), None) => PinTarget::Project(p),
                (None, Some(a)) => PinTarget::Automation(a),
                _ => return Err(CliError::usage("Name a project (--project) or an automation (--automation).")),
            };
            let instance = match s(args, "account") {
                Some(a) if a.eq_ignore_ascii_case("none") => None,
                Some(a) => Some(account(&conn.state(), a)?),
                None => return Err(CliError::usage("Name an account, or `none` to unpin.")),
            };
            let mut settings = board(conn).await?.settings;
            settings.set_pin(target, instance.clone());
            call(conn, UsageCommand::SetSettings(settings)).await?;
            let what = match target {
                PinTarget::Project(id) => format!("project {id}"),
                PinTarget::Automation(id) => format!("automation {id}"),
            };
            let human = match &instance {
                Some(i) => format!("Pinned {what} to {i}: its new threads start there and never move automatically."),
                None => format!("Unpinned {what}."),
            };
            Ok(Outcome::ok(json!({"pinned": what, "account": instance}), human))
        }
        _ => Err(CliError::usage("Unknown usage command.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_and_amounts_read_well() {
        assert_eq!(span(30_000), "under a minute");
        assert_eq!(span(100 * 60_000), "1h 40m");
        assert_eq!(span(3 * 86_400_000 + 7_200_000), "3d 2h");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(4_200_000), "4.2M");
        assert_eq!(dollars(1_234_567), "$1.23");
    }

    #[test]
    fn the_board_renders_windows_pace_and_messages() {
        let now = now_ms();
        let board = QuotaBoard {
            accounts: vec![
                AccountQuota {
                    instance: "claude".into(),
                    kind: ProviderKind::Claude,
                    name: "Claude Code".into(),
                    plan: Some("Max 20x".into()),
                    windows: vec![QuotaWindowState {
                        window: QuotaWindow { window: "five_hour".into(), utilization: Some(0.42), resets_ms: now + 7_500_000, rejected: false },
                        label: "5-hour".into(),
                        span_ms: 5 * 3_600_000,
                        burn_per_hour: Some(0.3),
                        runs_out_ms: Some(now + 100 * 60_000 + 30_000),
                    }],
                    status: QuotaStatus::Ok,
                    updated_ms: now - 61_000,
                    source: QuotaSource::Provider,
                    top: vec![Consumer { thread: ThreadId(1), title: "Refactor".into(), tokens: 12_000, cost_micros: 0 }],
                },
                AccountQuota {
                    instance: "codex".into(),
                    kind: ProviderKind::Codex,
                    name: "Codex".into(),
                    status: QuotaStatus::NoPlan("Uses an API key.".into()),
                    ..Default::default()
                },
            ],
            settings: UsageSettings::default(),
        };
        let out = limits_outcome(&board);
        let human = out.human.unwrap();
        assert!(human.starts_with("Claude Code · Max 20x  (updated 1m ago)"), "{human}");
        assert!(human.contains("5-hour                 42%, resets in 2h · 30%/h, runs out in 1h 40m"), "{human}");
        assert!(human.contains("Most used: Refactor (12.0k tokens)"), "{human}");
        assert!(human.contains("Codex\n  Uses an API key."), "{human}");
        assert_eq!(out.value["accounts"][0]["windows"][0]["used_percent"], 42);
        assert_eq!(out.value["settings"]["failover"], "ask");
    }
}
