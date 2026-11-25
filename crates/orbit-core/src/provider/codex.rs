//! Codex adapter: drives `codex app-server`, the JSON-RPC server the Codex
//! IDE extension uses (newline-delimited JSON, no `jsonrpc` field).
//!
//! # One app-server per thread session
//!
//! T3 Code shares one app-server across every thread of an instance, so when
//! it dies or wedges every open thread breaks at once (T3 Code #16502), all
//! threads share one 32-request handler cap (§10.10), and per-thread state
//! (environment, ports) has to be smuggled through config overrides. Orbit
//! runs one app-server per thread session instead:
//!
//! * **Crash isolation.** A crash or hang takes down one thread. The session
//!   supervises its process: on an unexpected exit or a request that stops
//!   answering it fails the running turn with the reason (exit status and
//!   stderr tail), withdraws open approvals, restarts with backoff (at most
//!   3 times per 5 minutes) and resumes the same Codex thread with
//!   `thread/resume`. Prompts sent meanwhile are queued, not lost.
//! * **Per-thread environment.** The worktree's `cwd`, `PORT` range and
//!   user overrides are the process environment, inherited by every shell
//!   and MCP server Codex starts, and the whole tree dies with the session
//!   (Job Object / process group, see [`super::process`]).
//! * **Cost.** The app-server is a native Rust binary: startup is one
//!   process spawn plus a handshake, tens of milliseconds, overlapped with
//!   the catalog reads. The per-thread resident cost is dominated by what a
//!   Codex thread holds anyway (history, MCP connections), which a shared
//!   server would hold per thread too, so sharing saves little memory while
//!   coupling every thread's fate. Sessions end with their thread (the
//!   engine sends `Shutdown`), so idle threads hold no process.
//!
//! # Protocol notes
//!
//! * Everything the composer offers comes from the server at runtime:
//!   models and reasoning efforts from `model/list` (paginated), service
//!   tiers from the model entries, interaction modes from
//!   `collaborationMode/list`, skills from `skills/list`. Nothing is
//!   hardcoded; a server without `collaborationMode/list` simply offers no
//!   plan mode.
//! * Access levels map to `approvalPolicy` / sandbox per turn (see
//!   [`map::policy`]); a thread never defaults to full access.
//! * Steering uses `turn/steer` with the active turn id as precondition; if
//!   the server lacks it or refuses (turn ended, not steerable) the prompt
//!   is queued as the next turn. Stop is `turn/interrupt`, never a kill.
//! * The Codex thread id is the session id; `resume` resumes it and
//!   `resume_at` (a turn id from [`super::AgentEvent::Checkpoint`]) forks it
//!   at that turn, leaving the original intact.

mod map;
mod rpc;
mod session;
mod wire;

use std::path::{Path, PathBuf};
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{LazyValue, json};

use super::process::resolve_binary;
use super::{EventSender, SessionHandle, SessionSpec};
use rpc::Connection;
use wire::*;

/// The probe never takes longer than this.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    let (input, rx) = crossfire::mpsc::bounded_blocking_async(64);
    tokio::spawn(session::run(spec, events, rx));
    SessionHandle { input }
}

/// Detects Codex: installed, version, sign-in, models and modes. Bounded by
/// [`PROBE_TIMEOUT`] end to end.
pub async fn probe(binary: Option<PathBuf>, env: Vec<(String, String)>) -> ProviderInfo {
    let Some(path) = resolve(binary.as_deref()) else {
        return ProviderInfo {
            binary: binary.as_ref().map(|b| b.display().to_string()),
            message: Some(not_found_message(binary.as_deref())),
            ..provider_info(None)
        };
    };
    match tokio::time::timeout(PROBE_TIMEOUT, probe_at(&path, &env)).await {
        Ok(info) => info,
        Err(_) => ProviderInfo {
            installed: true,
            message: Some(format!("Codex didn't answer within {} seconds.", PROBE_TIMEOUT.as_secs())),
            ..provider_info(Some(&path))
        },
    }
}

/// The one place a Codex [`ProviderInfo`] is built from scratch; callers
/// override fields with struct-update syntax.
fn provider_info(binary: Option<&Path>) -> ProviderInfo {
    ProviderInfo {
        kind: ProviderKind::Codex,
        installed: false,
        version: None,
        signed_in: None,
        account: None,
        binary: binary.map(|b| b.display().to_string()),
        models: Vec::new(),
        modes: Vec::new(),
        slash_commands: Vec::new(),
        message: None,
        quota: Vec::new(),
    }
}

fn resolve(explicit: Option<&Path>) -> Option<PathBuf> {
    resolve_binary("codex", explicit, &install_locations())
}

/// Where Codex installers put the binary when it is not on PATH.
fn install_locations() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
            dirs.push(local.join("Programs").join("codex"));
        }
        if let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) {
            dirs.push(roaming.join("npm"));
        }
    } else {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(".npm-global").join("bin"));
    }
    dirs
}

fn not_found_message(explicit: Option<&Path>) -> String {
    match explicit {
        Some(path) => format!("Codex was not found at {}. Check the path in Settings.", path.display()),
        None => "Codex is not installed. Install it with `npm install -g @openai/codex`, or set its path in Settings."
            .to_owned(),
    }
}

fn initialize_params() -> sonic_rs::Value {
    json!({
        "clientInfo": {"name": "orbit", "title": "Orbit", "version": env!("CARGO_PKG_VERSION")},
        // Plan mode (`collaborationMode`) and its listing are experimental.
        "capabilities": {"experimentalApi": true, "optOutNotificationMethods": ["turn/diff/updated"]}
    })
}

/// Initialize, then `model/list` (all pages), `account/read` and
/// `collaborationMode/list` in parallel.
async fn probe_at(path: &Path, env: &[(String, String)]) -> ProviderInfo {
    #[derive(PartialEq)]
    enum Probe {
        Initialize,
        Models,
        Account,
        Modes,
    }
    let base = provider_info(Some(path));
    let cwd = std::env::temp_dir();
    let (mut conn, mut lines) = match Connection::spawn(path, &cwd, env) {
        Ok(conn) => conn,
        Err(err) => return ProviderInfo { message: Some(format!("Couldn't start Codex: {err}")), ..base },
    };
    let mut calls = vec![(conn.request("initialize", &initialize_params()), Probe::Initialize)];
    let mut info = ProviderInfo { installed: true, ..base };
    let mut models = Vec::new();
    let mut modes = Vec::new();
    while !calls.is_empty() {
        let Some(line) = lines.recv().await else {
            let reason = conn.exit_reason().await;
            return ProviderInfo { message: Some(reason), ..info };
        };
        let Ok(envelope) = sonic_rs::from_slice::<RpcLine>(&line) else { continue };
        let Some(id) = envelope.id.as_ref() else { continue };
        if envelope.method.is_some() {
            conn.respond_error(id.as_raw_str(), METHOD_NOT_FOUND, "Orbit is only probing");
            continue;
        }
        let Ok(id) = id.as_raw_str().parse::<u64>() else { continue };
        let Some(index) = calls.iter().position(|(c, _)| *c == id) else { continue };
        let (_, call) = calls.swap_remove(index);
        let result = envelope.result.as_ref().map(LazyValue::as_raw_str);
        match call {
            Probe::Initialize => {
                if let Some(err) = envelope.error {
                    return ProviderInfo { message: Some(format!("Codex rejected the handshake: {}", err.message)), ..info };
                }
                info.version = result
                    .and_then(|r| sonic_rs::from_str::<InitializeResponse>(r).ok())
                    .and_then(|r| map::version_from_user_agent(&r.user_agent));
                conn.notify("initialized");
                calls.push((conn.request("model/list", &json!({})), Probe::Models));
                calls.push((conn.request("account/read", &json!({})), Probe::Account));
                calls.push((conn.request("collaborationMode/list", &json!({})), Probe::Modes));
            }
            Probe::Models => match result.map(sonic_rs::from_str::<ModelListResponse>) {
                Some(Ok(page)) => {
                    models.extend(page.data);
                    if let Some(cursor) = page.next_cursor.filter(|c| !c.is_empty()) {
                        calls.push((conn.request("model/list", &json!({"cursor": cursor})), Probe::Models));
                    }
                }
                _ => {
                    let reason = envelope.error.map(|e| e.message).unwrap_or_else(|| "unreadable response".into());
                    info.message.get_or_insert(format!("Couldn't list Codex models: {reason}"));
                }
            },
            Probe::Account => {
                if let Some(Ok(response)) = result.map(sonic_rs::from_str::<AccountResponse>) {
                    let message;
                    (info.signed_in, info.account, message) = map::account(&response);
                    if message.is_some() {
                        info.message = message;
                    }
                }
            }
            Probe::Modes => {
                if let Some(Ok(response)) = result.map(sonic_rs::from_str::<CollaborationModeListResponse>) {
                    modes = response.data;
                }
            }
        }
    }
    info.models = map::models(&models);
    info.modes = map::modes(&modes);
    info
}
