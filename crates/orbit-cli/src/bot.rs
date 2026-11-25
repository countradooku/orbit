//! `orbit bot …`: the chat bots' status and who may use them
//! ([`BotCommand`]).

use std::fmt::Write as _;

use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{Ctx, Outcome};
use crate::render::ago_or_in;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

async fn bots(conn: &Conn, command: BotCommand) -> Result<Reply, CliError> {
    conn.call(Command::Bots(command)).await
}

fn platform(args: &Map<String, Value>) -> Result<Platform, CliError> {
    let raw = s(args, "platform").unwrap_or_default();
    Platform::from_id(raw).ok_or_else(|| CliError::usage(format!("“{raw}” is not telegram, slack or discord.")))
}

/// Letting people in is for the owner: refuse it from inside an agent's
/// thread, where a prompt could ask for it.
fn not_from_an_agent(ctx: &Ctx) -> Result<(), CliError> {
    if ctx.env.thread.is_some() {
        return Err(CliError::usage("Pairing can’t be approved from inside an agent’s thread. Run it in your own terminal."));
    }
    Ok(())
}

fn connection(c: &BotConnection) -> (&'static str, String) {
    match c {
        BotConnection::Off => ("off", "Off".into()),
        BotConnection::NeedsToken => ("needs_token", "Needs a token".into()),
        BotConnection::Connecting => ("connecting", "Connecting".into()),
        BotConnection::Connected => ("connected", "Connected".into()),
        BotConnection::Retrying { attempt, retry_ms } => ("retrying", format!("Retrying (attempt {attempt}, {})", ago_or_in(*retry_ms))),
        BotConnection::Rejected => ("rejected", "Token rejected".into()),
    }
}

fn user_json(u: &PairedUser) -> Value {
    json!({"platform": u.platform.id(), "user_id": u.user_id, "name": u.name, "paired_ms": u.paired_ms})
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["bot", "status"] => {
            let Reply::Bots(BotReply::State(state)) = bots(conn, BotCommand::State).await? else { return Err(unexpected()) };
            let mut human = String::new();
            let mut platforms = Vec::new();
            for p in &state.platforms {
                let (word, label) = connection(&p.connection);
                let account = if p.account.is_empty() { String::new() } else { format!(" as {}", p.account) };
                let token = if p.token_hint.is_empty() { "no token".to_owned() } else { format!("token {}", p.token_hint) };
                let _ = writeln!(human, "{:<9} {}{account}  ({token})", p.platform.label(), if p.enabled { label.clone() } else { "Off".into() });
                if let Some(error) = &p.error {
                    let _ = writeln!(human, "          last error {}: {error}", ago_or_in(p.error_ms));
                }
                platforms.push(json!({
                    "platform": p.platform.id(),
                    "enabled": p.enabled,
                    "connection": word,
                    "account": p.account,
                    "token_saved": !p.token_hint.is_empty(),
                    "error": p.error,
                    "error_ms": p.error_ms,
                }));
            }
            if state.paired.is_empty() {
                human.push_str("\nNo one is paired yet.\n");
            } else {
                human.push_str("\nPaired:\n");
                for u in &state.paired {
                    let _ = writeln!(human, "  {} · {} {} (since {})", u.name, u.platform.label(), u.user_id, ago_or_in(u.paired_ms));
                }
            }
            if !state.pending.is_empty() {
                human.push_str("\nWaiting to pair (orbit bot pair <code>):\n");
                for r in &state.pending {
                    let _ = writeln!(human, "  {}  {} · {} {} (expires {})", r.code, r.name, r.platform.label(), r.user_id, ago_or_in(r.expires_ms));
                }
            }
            let pending: Vec<Value> = state
                .pending
                .iter()
                .map(|r| json!({"platform": r.platform.id(), "user_id": r.user_id, "name": r.name, "code": r.code, "expires_ms": r.expires_ms}))
                .collect();
            Ok(Outcome::ok(
                json!({"platforms": platforms, "paired": state.paired.iter().map(user_json).collect::<Vec<_>>(), "pending": pending}),
                human.trim_end().to_owned(),
            ))
        }
        ["bot", "pair"] => {
            not_from_an_agent(ctx)?;
            let code = s(args, "code").unwrap_or_default().trim().to_owned();
            if args.get("reject").and_then(Value::as_bool).unwrap_or(false) {
                bots(conn, BotCommand::Reject { code: code.clone() }).await.map_err(not_found)?;
                return Ok(Outcome::ok(json!({"rejected": code}), format!("Turned away the request with code {code}.")));
            }
            let Reply::Bots(BotReply::Paired(user)) = bots(conn, BotCommand::Approve { code }).await.map_err(not_found)? else {
                return Err(unexpected());
            };
            let human = format!("Paired {} on {}. Their chats now drive Orbit.", user.name, user.platform.label());
            Ok(Outcome::ok(user_json(&user), human))
        }
        ["bot", "unpair"] => {
            let platform = platform(args)?;
            let user_id = s(args, "user").unwrap_or_default().to_owned();
            bots(conn, BotCommand::Revoke { platform, user_id: user_id.clone() }).await.map_err(not_found)?;
            Ok(Outcome::ok(
                json!({"platform": platform.id(), "user_id": user_id, "removed": true}),
                format!("Removed {user_id} on {}.", platform.label()),
            ))
        }
        ["bot", "allow"] => {
            not_from_an_agent(ctx)?;
            let platform = platform(args)?;
            let user_id = s(args, "user").unwrap_or_default().to_owned();
            let name = s(args, "name").unwrap_or_default().to_owned();
            let Reply::Bots(BotReply::Paired(user)) = bots(conn, BotCommand::Allow { platform, user_id, name }).await? else {
                return Err(unexpected());
            };
            let human = format!("Allowed {} on {}.", user.name, user.platform.label());
            Ok(Outcome::ok(user_json(&user), human))
        }
        _ => Err(CliError::usage("Unknown bot command.")),
    }
}

fn not_found(err: CliError) -> CliError {
    CliError::not_found(err.message)
}

fn unexpected() -> CliError {
    CliError::engine("Unexpected reply from the engine.")
}
