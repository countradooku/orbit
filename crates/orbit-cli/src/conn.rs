//! Finding and talking to the engine.
//!
//! The local engine publishes its loopback URL and a fresh token in
//! `<data>/endpoint.json`; `--host`/`--token` (or `ORBIT_HOST`/`ORBIT_TOKEN`)
//! name another one. The connection is an [`orbit_net::RemoteClient`]; a
//! background task applies its updates to a [`ClientState`] that any number
//! of concurrent operations (MCP tool calls) read and wait on.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use orbit_net::endpoint::{self, EndpointInfo};
use orbit_net::state::ClientState;
use orbit_net::{ConnectionStatus, RemoteClient, Update};
use orbit_proto::*;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, watch};

use crate::error::CliError;

/// What the process environment says, captured once (tests build their own).
#[derive(Clone, Debug, Default)]
pub struct Env {
    /// `ORBIT_HOST` / `ORBIT_TOKEN`.
    pub host: Option<String>,
    pub token: Option<String>,
    /// `ORBIT_DATA`.
    pub data: Option<PathBuf>,
    /// `ORBIT_THREAD`: the Orbit thread this process runs in (set by the
    /// engine for every agent it starts).
    pub thread: Option<String>,
    /// `ORBIT_PARENT_THREAD`: overrides `ORBIT_THREAD` as the parent of the
    /// threads this process starts.
    pub parent: Option<String>,
    pub cwd: PathBuf,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        Self {
            host: var("ORBIT_HOST"),
            token: var("ORBIT_TOKEN"),
            data: var("ORBIT_DATA").map(PathBuf::from),
            thread: var("ORBIT_THREAD"),
            parent: var("ORBIT_PARENT_THREAD"),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }

    /// The thread acting through this process: `ORBIT_PARENT_THREAD`, else
    /// `ORBIT_THREAD`.
    pub fn caller(&self) -> Option<&str> {
        self.parent.as_deref().or(self.thread.as_deref())
    }
}

/// Where to connect.
#[derive(Clone, Debug)]
pub struct Target {
    pub url: String,
    pub token: String,
    /// Set when found through `endpoint.json`.
    pub endpoint: Option<EndpointInfo>,
    pub data_dir: PathBuf,
}

pub fn data_dir(global: &Map<String, Value>, env: &Env) -> PathBuf {
    match global.get("data").and_then(Value::as_str) {
        Some(dir) => PathBuf::from(dir),
        None => env.data.clone().unwrap_or_else(endpoint::platform_data_dir),
    }
}

/// Resolves the engine to talk to from flags, environment and
/// `endpoint.json`, in that order.
pub fn resolve(global: &Map<String, Value>, env: &Env) -> Result<Target, CliError> {
    let data_dir = data_dir(global, env);
    let flag = |name: &str| global.get(name).and_then(Value::as_str).map(str::to_owned);
    if let Some(host) = flag("host").or_else(|| env.host.clone()) {
        let token = flag("token").or_else(|| env.token.clone()).unwrap_or_default();
        return Ok(Target { url: normalize_url(&host), token, endpoint: None, data_dir });
    }
    match endpoint::read(&data_dir) {
        Ok(Some(info)) => {
            if info.protocol != 0 && info.protocol != PROTOCOL_VERSION {
                return Err(CliError::unavailable(format!(
                    "The running Orbit ({}) speaks protocol {}; this orbit speaks {}.",
                    info.version, info.protocol, PROTOCOL_VERSION
                ))
                .hint("Use the orbit that came with the running app."));
            }
            Ok(Target { url: info.url.clone(), token: info.token.clone(), endpoint: Some(info), data_dir })
        }
        Ok(None) => Err(CliError::unavailable("Orbit isn’t running.").hint(format!(
            "Start the Orbit app or `orbitd` (looked for {}), or pass --host.",
            data_dir.join(endpoint::FILE).display()
        ))),
        Err(err) => Err(CliError::unavailable(format!("Couldn’t read the engine’s endpoint: {err}"))),
    }
}

/// `host:port` → `ws://host:port/`.
fn normalize_url(host: &str) -> String {
    let host = host.trim();
    let url = if host.contains("://") { host.to_owned() } else { format!("ws://{host}") };
    if url.ends_with('/') { url } else { format!("{url}/") }
}

struct Shared {
    state: Mutex<ClientState>,
    /// Bumped after every applied batch.
    version: watch::Sender<u64>,
    /// Every event batch, for consumers that need the events themselves.
    events: broadcast::Sender<Arc<Vec<Envelope>>>,
}

/// A live connection with its client-side state.
pub struct Conn {
    client: RemoteClient,
    shared: Arc<Shared>,
    pub target: Target,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.pump.abort();
        self.client.close();
    }
}

impl Conn {
    /// Connects and waits for the engine's snapshot. Must run inside a Tokio
    /// runtime (the update pump is a task on it).
    pub async fn connect(target: Target, timeout: Duration) -> Result<Conn, CliError> {
        let client = RemoteClient::connect(target.url.clone(), target.token.clone())
            .map_err(|e| CliError::unavailable(format!("Couldn’t start the connection: {e}")))?;
        let updates = client.updates().expect("a fresh client has its update stream");
        let mut state = ClientState::default();
        let mut reset = false;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let update = match tokio::time::timeout_at(deadline, updates.recv()).await {
                Ok(Ok(update)) => update,
                Ok(Err(_)) => return Err(CliError::unavailable("The connection closed before the engine answered.")),
                Err(_) => {
                    client.close();
                    return Err(CliError::unavailable(format!("The engine at {} didn’t answer in time.", target.url)));
                }
            };
            match update {
                Update::Status(ConnectionStatus::Disconnected { reason }) => {
                    client.close();
                    let hint = if target.endpoint.is_some() {
                        "Is Orbit still running? Restart it if it stopped responding."
                    } else {
                        "Check --host and --token (or ORBIT_HOST and ORBIT_TOKEN)."
                    };
                    return Err(CliError::unavailable(reason).hint(hint));
                }
                Update::Status(ConnectionStatus::Connected) if reset => {
                    state.apply(vec![update]);
                    break;
                }
                Update::Reset(_) => {
                    reset = true;
                    state.apply(vec![update]);
                }
                other => {
                    state.apply(vec![other]);
                }
            }
        }
        let (version, _) = watch::channel(0u64);
        let (events, _) = broadcast::channel(4096);
        let shared = Arc::new(Shared { state: Mutex::new(state), version, events });
        let pump = {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut n = 0u64;
                while let Ok(first) = updates.recv().await {
                    let mut batch = vec![first];
                    while let Ok(more) = updates.try_recv() {
                        batch.push(more);
                    }
                    for update in &batch {
                        if let Update::Events(envs) = update {
                            let _ = shared.events.send(Arc::new(envs.clone()));
                        }
                    }
                    shared.state.lock().unwrap_or_else(|e| e.into_inner()).apply(batch);
                    n += 1;
                    shared.version.send_replace(n);
                }
            })
        };
        Ok(Conn { client, shared, target, pump })
    }

    /// The client-side view of the engine. Never hold it across an await.
    pub fn state(&self) -> MutexGuard<'_, ClientState> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn connected(&self) -> bool {
        matches!(self.state().connection, Some(ConnectionStatus::Connected))
    }

    /// Notified after every applied batch of updates.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.shared.version.subscribe()
    }

    /// Every event batch from now on.
    pub fn events(&self) -> broadcast::Receiver<Arc<Vec<Envelope>>> {
        self.shared.events.subscribe()
    }

    pub async fn call(&self, command: Command) -> Result<Reply, CliError> {
        match self.client.call(command).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(message)) if !self.connected() || is_connection_error(&message) => Err(CliError::unavailable(message)),
            Ok(Err(message)) => Err(CliError::engine(message)),
            Err(_) => Err(CliError::unavailable("The connection to the engine closed.")),
        }
    }
}

fn is_connection_error(message: &str) -> bool {
    ["The connection to the host", "Not connected to the host", "Still connecting to the host", "The host is shutting down"]
        .iter()
        .any(|p| message.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_shorthand_becomes_a_url() {
        assert_eq!(normalize_url("127.0.0.1:7840"), "ws://127.0.0.1:7840/");
        assert_eq!(normalize_url("ws://box:1/"), "ws://box:1/");
        assert_eq!(normalize_url("wss://box.example"), "wss://box.example/");
    }

    #[test]
    fn missing_engine_is_unavailable_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env { data: Some(dir.path().to_owned()), ..Env::default() };
        let err = resolve(&Map::new(), &env).unwrap_err();
        assert_eq!(err.exit, crate::spec::exit::UNAVAILABLE);
        assert!(err.hint.unwrap().contains("endpoint.json"));
    }

    #[test]
    fn flags_win_over_environment() {
        let env = Env { host: Some("a:1".into()), token: Some("t1".into()), ..Env::default() };
        let mut global = Map::new();
        let target = resolve(&global, &env).unwrap();
        assert_eq!((target.url.as_str(), target.token.as_str()), ("ws://a:1/", "t1"));
        global.insert("host".into(), "b:2".into());
        global.insert("token".into(), "t2".into());
        let target = resolve(&global, &env).unwrap();
        assert_eq!((target.url.as_str(), target.token.as_str()), ("ws://b:2/", "t2"));
    }
}
