//! Agent Client Protocol adapter: one ACP agent process per thread session,
//! JSON-RPC 2.0 over stdio (newline-delimited).
//!
//! Any ACP agent works: Gemini CLI, Qwen Code, Grok, Kiro, Cursor's
//! `cursor-agent`, Goose and the agents in the ACP registry. An instance
//! names the command and arguments ([`orbit_proto::ProviderInstance`]);
//! [`super::presets`] lists the known ones.
//!
//! * **Handshake.** `initialize` with protocol version 1. Orbit advertises
//!   no file system or terminal capabilities: the agent uses its own, inside
//!   the thread's worktree and process tree, so there is no second,
//!   client-side sandbox to keep consistent.
//! * **Sessions.** `session/new`; an existing session is reopened with
//!   `session/resume` when the agent advertises it, else `session/load`
//!   (whose history replay is ignored: the timeline already has it), else a
//!   new session with a notice. An agent that asks to authenticate is
//!   authenticated with its own browser/agent method; methods that need a
//!   terminal or an API key produce a clear message instead.
//! * **Models, modes, options** come from the session (`configOptions`, or
//!   the older `models` / `modes`). An agent without a model list still
//!   shows one "Default model" entry so it can be picked.
//! * **Cancel is acknowledged, not assumed** (T3 Code §3c bug 1): Stop sends
//!   `session/cancel`, answers every open permission request `cancelled`
//!   (as the spec requires) and keeps the turn open until the agent's
//!   `session/prompt` returns `stopReason: "cancelled"`. Only if it never
//!   does within 10 seconds is the process killed.
//! * **"Allow for session" never silently degrades** (T3 Code bug): it uses
//!   the agent's `allow_always` option when there is one; otherwise the
//!   adapter remembers the grant itself and answers identical requests for
//!   the rest of the session.
//! * **Changed files are always named** (T3 Code #42025): paths come from
//!   diffs, `locations` and `rawInput`, merged across updates.
//! * ACP has no steering: a steer waits for the running turn and is sent as
//!   the next prompt.

pub(crate) mod map;
mod session;

use std::path::{Path, PathBuf};
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};

use super::jsonrpc::{Pipe, common_install_dirs, resolve_command};
use super::{EventSender, SessionHandle, SessionSpec};

/// The probe never takes longer than this. ACP agents launched through
/// `npx` can take a while on a cold cache.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The ACP protocol version Orbit speaks.
pub const PROTOCOL_VERSION: u64 = 1;

pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    let (input, rx) = crossfire::mpsc::bounded_blocking_async(64);
    tokio::spawn(session::run(spec, events, rx));
    SessionHandle { input }
}

pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "clientInfo": {"name": "orbit", "title": "Orbit", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// The agent's binary; `binary` is a path or a command name.
pub(crate) fn resolve(binary: Option<&Path>) -> Option<PathBuf> {
    binary.and_then(|b| resolve_command(Some(b), "", &common_install_dirs()))
}

fn not_found(binary: Option<&Path>) -> String {
    match binary {
        Some(b) => format!("`{}` was not found. Install the agent, or set its path in Settings.", b.display()),
        None => "This agent has no command configured. Set one in Settings.".to_owned(),
    }
}

/// Detects an ACP agent: starts it, reads `initialize`, and stops it. It
/// never creates a session (which some agents would keep in their history).
pub async fn probe(binary: Option<PathBuf>, args: &[String], env: &[(String, String)]) -> ProviderInfo {
    let base = ProviderInfo {
        kind: ProviderKind::Acp,
        binary: binary.as_ref().map(|b| b.display().to_string()),
        models: vec![map::default_model(Vec::new())],
        modes: vec![ModeInfo { mode: Mode::Build, label: "Build".into(), description: None }],
        ..Default::default()
    };
    let Some(path) = resolve(binary.as_deref()) else {
        return ProviderInfo { models: Vec::new(), message: Some(not_found(binary.as_deref())), ..base };
    };
    let base = ProviderInfo { installed: true, binary: Some(path.display().to_string()), ..base };
    let cwd = std::env::temp_dir();
    let (mut pipe, mut lines) = match Pipe::spawn("The agent", &path, args, &cwd, env, true) {
        Ok(pipe) => pipe,
        Err(err) => return ProviderInfo { message: Some(format!("Couldn't start the agent: {err}")), ..base },
    };
    let id = pipe.request("initialize", &initialize_params());
    let exchange = async {
        while let Some(line) = lines.recv().await {
            let Ok(message) = sonic_rs::from_slice::<Value>(&line) else { continue };
            if message.get("method").is_some() {
                if let Some(raw) = message.get("id") {
                    let raw = sonic_rs::to_string(raw).unwrap_or_default();
                    pipe.respond_error(&raw, -32601, "Orbit is only probing");
                }
                continue;
            }
            if message.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Some(message);
            }
        }
        None
    };
    let answer = tokio::time::timeout(PROBE_TIMEOUT, exchange).await;
    match answer {
        Ok(Some(message)) => {
            if let Some(error) = message.get("error") {
                let text = error.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                return ProviderInfo { message: Some(format!("The agent rejected the handshake: {text}")), ..base };
            }
            let result = message.get("result").cloned().unwrap_or_default();
            let version = result.get("agentInfo").and_then(|i| map::str_of(i, "version"));
            ProviderInfo { version, ..base }
        }
        Ok(None) => {
            let reason = pipe.exit_reason().await;
            ProviderInfo { message: Some(reason), ..base }
        }
        Err(_) => ProviderInfo {
            message: Some(format!("The agent didn't answer within {} seconds.", PROBE_TIMEOUT.as_secs())),
            ..base
        },
    }
}
