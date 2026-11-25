//! The per-user background host: finding, starting and controlling it.
//!
//! On a desktop the engine runs in `orbitd --local`, started by the app and
//! reached over the local transport ([`crate::transport`]). The host owns
//! the data folder (the store's lock makes it the only one), publishes
//! `endpoint.json` with `kind: "host"`, and decides when to exit from the
//! windows attached to it and [`KeepRunning`] (`<data>/host.json`).
//!
//! * [`LocalHost`] is the app's side: it starts the host when none runs,
//!   connects, starts it again after it exits (an update restart), and
//!   reports a host too old to talk to ([`HostLink::Outdated`]).
//! * The **control request** is a JSON frame instead of `Hello` on any of
//!   the host's transports. Its format never changes incompatibly, so an
//!   app of any later version can ask an older host what it is doing and
//!   tell it to restart, which the binary protocol cannot do across
//!   versions.
//! * [`HostControl`] is the host's side: the windows attached, holds and
//!   restart requests, shared by the connections and the lifetime loop.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orbit_proto::{HostActivity, KeepRunning, PROTOCOL_VERSION, RestartWhen};
use serde::{Deserialize, Serialize};

use crate::endpoint::{self, EndpointInfo};

/// The host's binary name, next to the app's.
pub const BINARY: &str = if cfg!(windows) { "orbitd.exe" } else { "orbitd" };
/// The preference file in the data folder.
pub const POLICY_FILE: &str = "host.json";
/// The host's log in the data folder (local mode), beside the app's
/// (`logs/orbit-desktop.log`).
pub const LOG_FILE: &str = "logs/orbitd.log";
/// How long a host may take to publish its endpoint after it was started.
const START_TIMEOUT: Duration = Duration::from_secs(10);

// ── Preference ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct PolicyFile {
    #[serde(default)]
    keep_running: String,
}

/// Whether the host keeps agents running after the app closes. A missing
/// or unreadable file means the default ([`KeepRunning::WhileWorking`]).
pub fn read_policy(data_dir: &Path) -> KeepRunning {
    std::fs::read_to_string(data_dir.join(POLICY_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<PolicyFile>(&text).ok())
        .and_then(|file| KeepRunning::from_key(&file.keep_running))
        .unwrap_or_default()
}

/// Saves the preference; the host reads it each time it decides.
pub fn write_policy(data_dir: &Path, policy: KeepRunning) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(POLICY_FILE);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(&PolicyFile { keep_running: policy.key().to_owned() }).map_err(io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

// ── Control requests ─────────────────────────────────────────────────────

/// What the host answers a control request with. JSON, and only ever
/// extended: every field has a default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostStatus {
    pub pid: u32,
    pub version: String,
    pub protocol: u16,
    pub started_ms: u64,
    pub windows: u32,
    /// Threads with a running turn.
    pub working: u32,
    pub busy_terminals: u32,
    pub terminals: u32,
    pub sleeping_terminals: u32,
    pub next_automation_ms: Option<u64>,
    pub restart_pending: bool,
}

impl HostStatus {
    pub fn from_activity(a: &HostActivity) -> Self {
        Self {
            pid: a.pid,
            version: a.version.clone(),
            protocol: a.protocol,
            started_ms: a.started_ms,
            windows: a.windows,
            working: a.working.len() as u32,
            busy_terminals: a.busy_terminals,
            terminals: a.terminals,
            sleeping_terminals: a.sleeping_terminals,
            next_automation_ms: a.next_automation_ms,
            restart_pending: a.restart_pending,
        }
    }

    /// "2 agents are working", or `None` when nothing runs.
    pub fn summary(&self) -> Option<String> {
        HostActivity {
            working: vec![orbit_proto::ThreadId(0); self.working as usize],
            busy_terminals: self.busy_terminals,
            ..Default::default()
        }
        .summary()
    }
}

/// A control request: the first frame of a connection, JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub token: String,
    /// `status` or `restart`.
    pub op: String,
    /// For `restart`: `idle` (default) or `now`.
    #[serde(default)]
    pub when: String,
}

impl ControlRequest {
    pub fn restart_when(&self) -> RestartWhen {
        if self.when == "now" { RestartWhen::Now } else { RestartWhen::WhenIdle }
    }
}

/// Whether a connection's first frame is a control request.
pub fn is_control(frame: &[u8]) -> bool {
    frame.first() == Some(&b'{')
}

/// Sends a control request to the host at `info` and reads its status.
/// Blocking; works with any host version that has control requests.
pub fn control(info: &EndpointInfo, op: &str, when: Option<RestartWhen>) -> io::Result<HostStatus> {
    let request = ControlRequest {
        token: info.token.clone(),
        op: op.to_owned(),
        when: match when {
            Some(RestartWhen::Now) => "now".into(),
            Some(RestartWhen::WhenIdle) => "idle".into(),
            None => String::new(),
        },
    };
    let body = serde_json::to_vec(&request).map_err(io::Error::other)?;
    let target = info.client_url().to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let reply = runtime.block_on(async move {
        tokio::time::timeout(Duration::from_secs(3), crate::client::exchange_once(&target, &body))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the host did not answer"))?
    })?;
    if let Ok(error) = serde_json::from_slice::<ControlError>(&reply)
        && !error.error.is_empty()
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, error.error));
    }
    serde_json::from_slice(&reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct ControlError {
    #[serde(default)]
    pub(crate) error: String,
}

// ── Finding and starting the host ────────────────────────────────────────

/// The host binary: `ORBIT_HOST_BIN` when set, else `orbitd` next to this
/// executable. `None` when it is not there.
pub fn locate_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ORBIT_HOST_BIN").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(BINARY);
    path.is_file().then_some(path)
}

/// Starts `orbitd --local` on `data_dir`, detached from this process: its
/// own process group (and session on Unix), no console, outside this
/// process's job on Windows, so closing the app or its terminal leaves it
/// running.
pub fn spawn(binary: &Path, data_dir: &Path) -> io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(binary);
    cmd.arg("--local").arg("--data").arg(data_dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // The host must not connect to a remote host itself.
    cmd.env_remove("ORBIT_HOST").env_remove("ORBIT_TOKEN");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // A job that forbids breaking away (some terminals, CI): start
            // inside it instead.
            Err(err) if err.raw_os_error() == Some(5) => {
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
                cmd.spawn()
            }
            other => other,
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe and touches nothing else.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()
    }
    #[cfg(not(any(unix, windows)))]
    cmd.spawn()
}

/// Why the host could not be reached.
#[derive(Debug)]
pub enum HostError {
    /// There is no host binary next to the app.
    Missing,
    /// Starting it failed.
    Spawn(io::Error),
    /// It started and exited; the end of its log says why.
    Exited(String),
    /// It did not publish its endpoint in time.
    Timeout,
    /// A host of another protocol version runs; it must restart before
    /// this app can talk to it.
    Outdated(Box<EndpointInfo>),
    Io(io::Error),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Missing => write!(f, "the background host ({BINARY}) is not installed next to Orbit"),
            HostError::Spawn(err) => write!(f, "the background host couldn’t start: {err}"),
            HostError::Exited(why) => write!(f, "the background host stopped right after starting: {why}"),
            HostError::Timeout => write!(f, "the background host didn’t answer in time"),
            HostError::Outdated(info) => {
                write!(f, "the background host is from another version of Orbit ({}); restart it to connect", info.version)
            }
            HostError::Io(err) => write!(f, "{err}"),
        }
    }
}

/// Makes sure a host runs on `data_dir` and returns its endpoint: the one
/// running, or one started from `binary`. `child` keeps the process started
/// here: one still starting is waited for rather than started again, and
/// one that exited is reaped.
pub fn ensure_running(
    data_dir: &Path,
    binary: Option<&Path>,
    child: &mut Option<std::process::Child>,
) -> Result<EndpointInfo, HostError> {
    if let Some(info) = live_endpoint(data_dir)? {
        return Ok(info);
    }
    let started = Instant::now();
    // A host started earlier that has not served yet is waited for; one that
    // is exiting (it may still look alive for a moment) is replaced.
    let starting = child.take().and_then(|mut c| c.try_wait().ok().flatten().is_none().then_some(c));
    let (mut spawned, mut fresh) = match starting {
        Some(starting) => (starting, false),
        None => (spawn(binary.ok_or(HostError::Missing)?, data_dir).map_err(HostError::Spawn)?, true),
    };
    let mut wait = Duration::from_millis(2);
    loop {
        if let Some(info) = live_endpoint(data_dir)? {
            *child = Some(spawned);
            return Ok(info);
        }
        if let Ok(Some(status)) = spawned.try_wait() {
            // Another host may have won the data folder meanwhile.
            if let Some(info) = live_endpoint(data_dir)? {
                return Ok(info);
            }
            if !fresh {
                // That was the previous host finishing its exit: start one.
                spawned = spawn(binary.ok_or(HostError::Missing)?, data_dir).map_err(HostError::Spawn)?;
                fresh = true;
                continue;
            }
            let why = match log_tail(data_dir) {
                Some(line) => format!("{line} ({status})"),
                None => status.to_string(),
            };
            return Err(HostError::Exited(why));
        }
        if started.elapsed() > START_TIMEOUT {
            *child = Some(spawned);
            return Err(HostError::Timeout);
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(25));
    }
}

/// The endpoint of the engine serving `data_dir`, if one runs. An engine
/// of another protocol version is [`HostError::Outdated`].
fn live_endpoint(data_dir: &Path) -> Result<Option<EndpointInfo>, HostError> {
    match endpoint::read(data_dir) {
        Ok(Some(info)) if info.protocol == PROTOCOL_VERSION => Ok(Some(info)),
        Ok(Some(info)) => Err(HostError::Outdated(Box::new(info))),
        Ok(None) => Ok(None),
        // Being rewritten or removed right now (Windows refuses to open a
        // file pending deletion): look again on the next poll.
        Err(err) if matches!(err.kind(), io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied) => Ok(None),
        Err(err) => Err(HostError::Io(err)),
    }
}

/// The last meaningful line of the host's log.
fn log_tail(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(LOG_FILE)).ok()?;
    text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_owned())
}

// ── The app's side ───────────────────────────────────────────────────────

/// Where the app stands with its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostLink {
    Starting,
    Ready { version: String, pid: u32 },
    /// A host this app cannot talk to runs: it must restart first.
    /// `status` says what it is doing (when it answered); `restart` what
    /// was asked of it.
    Outdated { version: String, status: Option<HostStatus>, restart: Option<RestartWhen> },
    Failed(String),
}

/// The app's background host: starts it when needed and connects to it,
/// again after it restarts.
#[derive(Clone)]
pub struct LocalHost {
    inner: std::sync::Arc<std::sync::Mutex<Inner>>,
}

struct Inner {
    data_dir: PathBuf,
    binary: Option<PathBuf>,
    child: Option<std::process::Child>,
    link: HostLink,
    /// The outdated host's endpoint, for control requests.
    outdated: Option<EndpointInfo>,
    restart: Option<RestartWhen>,
    /// Set once the app quits: nothing is started any more.
    closed: bool,
    /// Whether the first host this app reached was already running.
    found_running: Option<bool>,
}

impl LocalHost {
    pub fn new(data_dir: PathBuf, binary: Option<PathBuf>) -> Self {
        let inner = Inner {
            data_dir,
            binary,
            child: None,
            link: HostLink::Starting,
            outdated: None,
            restart: None,
            closed: false,
            found_running: None,
        };
        Self { inner: std::sync::Arc::new(std::sync::Mutex::new(inner)) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn link(&self) -> HostLink {
        self.lock().link.clone()
    }

    /// The host was running before this app started (it kept working while
    /// Orbit was closed), rather than started by it.
    pub fn found_running(&self) -> bool {
        self.lock().found_running == Some(true)
    }

    /// Finds the host or starts its process, without waiting for it to be
    /// ready (the connection does): what the app does at launch, so its
    /// window does not wait. Fails only when there is nothing to start.
    pub fn launch(&self) -> Result<(), HostError> {
        let mut inner = self.lock();
        match live_endpoint(&inner.data_dir) {
            Ok(Some(_)) | Err(HostError::Outdated(_)) => {
                inner.found_running.get_or_insert(true);
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => return Err(err),
        }
        let binary = inner.binary.clone().ok_or(HostError::Missing)?;
        let child = spawn(&binary, &inner.data_dir).map_err(HostError::Spawn)?;
        inner.child = Some(child);
        inner.found_running.get_or_insert(false);
        Ok(())
    }

    /// Finds or starts the host now and waits until it serves (blocking).
    /// An outdated host is not an error here: the app connects and shows it.
    pub fn start(&self) -> Result<(), HostError> {
        match self.resolve() {
            Ok(_) | Err(HostError::Outdated(_)) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// A client that reaches the host, and starts it again when it is gone.
    pub fn connect(&self) -> io::Result<crate::RemoteClient> {
        let host = self.clone();
        crate::RemoteClient::connect_with(Box::new(move || match host.resolve() {
            Ok(info) => Ok((info.client_url().to_owned(), info.token)),
            Err(err) => Err(err.to_string()),
        }))
    }

    /// Asks a host this app cannot talk to to restart, now or once its
    /// work is done. The app connects to the new one when it is up.
    pub fn restart_outdated(&self, when: RestartWhen) -> io::Result<()> {
        let info = self.lock().outdated.clone().ok_or_else(|| io::Error::other("no outdated host"))?;
        let status = control(&info, "restart", Some(when))?;
        let mut inner = self.lock();
        inner.restart = Some(when);
        inner.link = HostLink::Outdated { version: info.version, status: Some(status), restart: Some(when) };
        Ok(())
    }

    /// The app is quitting: start nothing more.
    pub fn close(&self) {
        self.lock().closed = true;
    }

    fn resolve(&self) -> Result<EndpointInfo, HostError> {
        let (data_dir, binary, mut child) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(HostError::Io(io::Error::other("Orbit is closing")));
            }
            (inner.data_dir.clone(), inner.binary.clone(), inner.child.take())
        };
        // Without the lock: starting the host can take a moment, and the
        // interface reads the link meanwhile.
        let had_child = child.is_some();
        let result = ensure_running(&data_dir, binary.as_deref(), &mut child);
        let mut inner = self.lock();
        let spawned = !had_child && child.is_some();
        inner.child = child;
        match result {
            Ok(info) => {
                inner.found_running.get_or_insert(!spawned);
                inner.link = HostLink::Ready { version: info.version.clone(), pid: info.pid };
                inner.outdated = None;
                inner.restart = None;
                Ok(info)
            }
            Err(HostError::Outdated(info)) => {
                let restart = inner.restart;
                inner.outdated = Some((*info).clone());
                drop(inner);
                // Ask what it is doing (without the lock: it can take a moment).
                let status = control(&info, "status", None).ok();
                let mut inner = self.lock();
                inner.link = HostLink::Outdated { version: info.version.clone(), status, restart };
                Err(HostError::Outdated(info))
            }
            Err(err) => {
                inner.link = HostLink::Failed(err.to_string());
                Err(err)
            }
        }
    }
}

/// How a desktop reaches its engine on this computer.
#[cfg(feature = "engine")]
pub struct LocalBackend {
    pub backend: crate::Backend,
    /// The background host, when the engine runs in one.
    pub host: Option<LocalHost>,
    /// Why the engine runs in this process instead (shown to the user).
    pub fallback: Option<String>,
}

/// The background host when it can be started (or is running), else the
/// engine in this process from `in_process`, with the reason.
#[cfg(feature = "engine")]
pub fn local_backend(
    data_dir: PathBuf,
    binary: Option<PathBuf>,
    in_process: impl FnOnce() -> io::Result<orbit_core::EngineHandle>,
) -> io::Result<LocalBackend> {
    let host = LocalHost::new(data_dir, binary);
    match host.launch() {
        Ok(()) => {
            let client = host.connect()?;
            Ok(LocalBackend { backend: crate::Backend::Remote(client), host: Some(host), fallback: None })
        }
        Err(err) => {
            log::warn!("no background host: {err}");
            let engine = in_process()?;
            let reason = format!("Orbit runs its engine in this window because {err}. Agents and terminals stop when Orbit closes.");
            Ok(LocalBackend { backend: crate::Backend::Local(engine), host: None, fallback: Some(reason) })
        }
    }
}

// ── The host's side ──────────────────────────────────────────────────────

#[cfg(feature = "engine")]
pub use control::HostControl;

#[cfg(feature = "engine")]
mod control {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use orbit_proto::{RestartWhen, now_ms};

    /// What the host's connections tell its lifetime loop.
    pub struct HostControl {
        windows: AtomicU32,
        hold_until_ms: AtomicU64,
        restart: Mutex<Option<RestartWhen>>,
        changed: tokio::sync::Notify,
        started_ms: u64,
    }

    impl Default for HostControl {
        fn default() -> Self {
            Self {
                windows: AtomicU32::new(0),
                hold_until_ms: AtomicU64::new(0),
                restart: Mutex::new(None),
                changed: tokio::sync::Notify::new(),
                started_ms: now_ms(),
            }
        }
    }

    /// One attached window; dropping it detaches.
    pub struct WindowGuard(std::sync::Arc<HostControl>);

    impl Drop for WindowGuard {
        fn drop(&mut self) {
            self.0.windows.fetch_sub(1, Ordering::AcqRel);
            self.0.changed.notify_one();
        }
    }

    impl HostControl {
        pub fn started_ms(&self) -> u64 {
            self.started_ms
        }

        /// App windows connected now.
        pub fn windows(&self) -> u32 {
            self.windows.load(Ordering::Acquire)
        }

        pub fn attach_window(self: &std::sync::Arc<Self>) -> WindowGuard {
            self.windows.fetch_add(1, Ordering::AcqRel);
            self.changed.notify_one();
            WindowGuard(self.clone())
        }

        /// Keep running at least `ms` from now, windows or not.
        pub fn hold(&self, ms: u64) {
            self.hold_until_ms.fetch_max(now_ms().saturating_add(ms), Ordering::AcqRel);
            self.changed.notify_one();
        }

        pub fn held(&self, now_ms: u64) -> bool {
            self.hold_until_ms.load(Ordering::Acquire) > now_ms
        }

        /// Asks for a restart; `Now` wins over `WhenIdle`.
        pub fn request_restart(&self, when: RestartWhen) {
            let mut restart = self.restart.lock().unwrap_or_else(|e| e.into_inner());
            if *restart != Some(RestartWhen::Now) {
                *restart = Some(when);
            }
            drop(restart);
            self.changed.notify_one();
        }

        pub fn restart(&self) -> Option<RestartWhen> {
            *self.restart.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Waits until windows, holds or restart requests change.
        pub async fn changed(&self) {
            self.changed.notified().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_file_round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_policy(dir.path()), KeepRunning::WhileWorking);
        for policy in KeepRunning::ALL {
            write_policy(dir.path(), policy).unwrap();
            assert_eq!(read_policy(dir.path()), policy);
        }
        std::fs::write(dir.path().join(POLICY_FILE), "{not json").unwrap();
        assert_eq!(read_policy(dir.path()), KeepRunning::WhileWorking);
    }

    #[test]
    fn a_missing_binary_is_reported_not_spawned() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = None;
        assert!(matches!(ensure_running(dir.path(), None, &mut child), Err(HostError::Missing)));
        let host = LocalHost::new(dir.path().to_owned(), None);
        assert!(matches!(host.start(), Err(HostError::Missing)));
        assert!(matches!(host.link(), HostLink::Failed(_)));
    }

    #[test]
    fn control_requests_are_told_apart_from_hello() {
        let hello = orbit_proto::encode(&orbit_proto::ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            token: "t".into(),
            epoch: 0,
            since: 0,
        });
        assert!(!is_control(&hello));
        let req = ControlRequest { token: "t".into(), op: "status".into(), when: String::new() };
        assert!(is_control(&serde_json::to_vec(&req).unwrap()));
        // Unknown fields from a later version are ignored.
        let later: HostStatus = serde_json::from_str(r#"{"pid":4,"working":2,"something_new":true}"#).unwrap();
        assert_eq!((later.pid, later.working), (4, 2));
        assert_eq!(later.summary().unwrap(), "2 agents are working");
    }
}
