//! The local control endpoint.
//!
//! Whenever an engine runs (the desktop app or `orbitd`) it also serves the
//! Orbit protocol on a random loopback port with a fresh token, and writes
//! where to find it to `<data>/endpoint.json`:
//!
//! ```json
//! {"url":"ws://127.0.0.1:53817/","token":"…","pid":4242,"version":"0.1.0","kind":"desktop"}
//! ```
//!
//! The `orbit` CLI and its MCP server read that file to connect. The file is
//! readable by its owner only (mode 0600 on Unix; on Windows it inherits the
//! per-user profile folder's ACL), it is removed on a clean exit, and a file
//! whose process is gone is treated as absent and removed by the reader.
//!
//! This is the engine's endpoint (the whole Orbit protocol, for the desktop
//! app and `orbitd` alike). The desktop app's single-instance hand-off
//! (`app.endpoint`, opening a folder or thread in the running window) is a
//! separate, app-level channel; `orbit open` goes through that one by
//! launching `orbit-desktop --thread <id>`.
//!
//! The endpoint costs nothing while no client is connected: one thread
//! parked in `accept`, no timers.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name inside the data folder.
pub const FILE: &str = "endpoint.json";

/// The contents of `endpoint.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointInfo {
    /// `ws://127.0.0.1:<port>/`.
    pub url: String,
    pub token: String,
    /// The engine's process.
    pub pid: u32,
    /// Orbit's version (the engine's crate version).
    pub version: String,
    /// `desktop` or `orbitd`.
    #[serde(default)]
    pub kind: String,
    /// The wire protocol version, so a client can refuse early.
    #[serde(default)]
    pub protocol: u16,
    /// The local transport's address (`pipe:…` or `unix:…`), when the
    /// engine serves one (the background host does). Faster than `url`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub local: String,
}

impl EndpointInfo {
    /// Where a client on this computer connects: the local transport when
    /// there is one, else the WebSocket URL.
    pub fn client_url(&self) -> &str {
        if self.local.is_empty() { &self.url } else { &self.local }
    }
}

/// The data folder a client should look in: `ORBIT_DATA` when set, else the
/// platform default (the same folder [`orbit_core::EngineOptions`] uses).
pub fn default_data_dir() -> PathBuf {
    match std::env::var_os("ORBIT_DATA").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => platform_data_dir(),
    }
}

/// `%APPDATA%\Orbit`, `~/Library/Application Support/Orbit` or
/// `$XDG_DATA_HOME/Orbit`. Kept equal to the engine's default by a test.
pub fn platform_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Orbit")
}

/// Reads the endpoint of the engine running on `data_dir`. `Ok(None)` when
/// none runs: no file, or a file left by a process that is gone.
pub fn read(data_dir: &Path) -> io::Result<Option<EndpointInfo>> {
    let path = data_dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let info: EndpointInfo = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
    if !process_alive(info.pid) {
        // Left by a process that died without cleaning up. Remove it unless
        // a new engine replaced it since it was read.
        if std::fs::read_to_string(&path).is_ok_and(|again| again == text) {
            let _ = std::fs::remove_file(&path);
        }
        return Ok(None);
    }
    Ok(Some(info))
}

/// Whether a process with this id is running. One that has exited but
/// that its parent has not reaped yet (a zombie on Unix) is not.
pub fn process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
        if pid <= 0 {
            return false;
        }
        // Signal 0 checks existence and permission without sending anything.
        // SAFETY: `kill` with signal 0 has no side effects.
        let rc = unsafe { libc::kill(pid, 0) };
        let exists = rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        // `kill` also finds a zombie: it has exited, only its exit status
        // is left for its parent.
        exists && !zombie(pid)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // A process we may not query still exists.
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Whether `pid` has exited and waits to be reaped. `false` when that
/// cannot be told.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn zombie(pid: libc::pid_t) -> bool {
    // `/proc/<pid>/stat` reads `pid (comm) state …`; `comm` may itself
    // hold `)`, so the state follows the last one.
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else { return false };
    let state = stat.iter().rposition(|&b| b == b')').and_then(|close| stat.get(close + 2));
    matches!(state, Some(b'Z' | b'X'))
}

/// Whether `pid` has exited and waits to be reaped. `false` when that
/// cannot be told.
#[cfg(target_os = "macos")]
fn zombie(pid: libc::pid_t) -> bool {
    // `sysctl kern.proc.pid.<pid>` fills a `struct kinfo_proc` (648 bytes),
    // which `libc` does not declare. Its first member, `struct extern_proc`,
    // has the `char p_stat` at byte 36 and the `pid_t p_pid` at byte 40 on
    // every 64-bit macOS; the pid is checked to be sure of the layout.
    const P_STAT: usize = 36;
    const P_PID: usize = 40;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; 1024];
    let mut len = buf.len();
    // SAFETY: `buf` is writable for `len` bytes; sysctl writes at most that
    // and reports how many it wrote.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 4, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len < P_PID + 4 {
        return false;
    }
    let p_pid = libc::pid_t::from_ne_bytes(buf[P_PID..P_PID + 4].try_into().expect("four bytes"));
    p_pid == pid && u32::from(buf[P_STAT]) == libc::SZOMB
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android", target_os = "macos"))))]
fn zombie(_pid: libc::pid_t) -> bool {
    false
}

/// Writes `contents` to `path` atomically (temp file and rename) so a
/// reader never sees half a file, readable by the owner only on Unix.
pub(crate) fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(feature = "engine")]
pub use serve::{EndpointOptions, LocalEndpoint, start, start_with};

#[cfg(feature = "engine")]
mod serve {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use orbit_core::EngineHandle;

    use crate::host::HostControl;

    /// What an endpoint serves besides the WebSocket.
    #[derive(Default)]
    pub struct EndpointOptions {
        /// Answer host commands (attach, hold, restart) from this.
        pub control: Option<Arc<HostControl>>,
        /// Also serve the local transport (a named pipe or Unix socket).
        pub local: bool,
    }

    use super::{EndpointInfo, FILE, write_private};

    /// A running endpoint. Clones share it; [`LocalEndpoint::close`] (or
    /// dropping the last clone) stops it and removes `endpoint.json`.
    #[derive(Clone)]
    pub struct LocalEndpoint {
        inner: Arc<Inner>,
    }

    struct Inner {
        info: EndpointInfo,
        path: PathBuf,
        stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl LocalEndpoint {
        pub fn info(&self) -> &EndpointInfo {
            &self.inner.info
        }

        /// Stops serving and removes `endpoint.json` if it is still ours.
        /// Idempotent.
        pub fn close(&self) {
            self.inner.close();
        }
    }

    impl Inner {
        fn close(&self) {
            let Some(stop) = self.stop.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
            let _ = stop.send(());
            // Another engine may have taken the folder over since; leave its
            // file alone.
            if super::read_raw(&self.path).is_some_and(|info| info.pid == self.info.pid && info.url == self.info.url) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// Starts serving `engine` on a random loopback port with a fresh token
    /// and publishes it in `<data_dir>/endpoint.json`. `kind` names the host
    /// (`desktop`, `orbitd`).
    pub fn start(engine: EngineHandle, data_dir: &Path, kind: &str) -> io::Result<LocalEndpoint> {
        start_with(engine, data_dir, kind, EndpointOptions::default())
    }

    /// [`start`], serving the local transport and host commands as
    /// `options` say (the background host).
    pub fn start_with(engine: EngineHandle, data_dir: &Path, kind: &str, options: EndpointOptions) -> io::Result<LocalEndpoint> {
        let listener = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let token = new_token()?;
        let info = EndpointInfo {
            url: format!("ws://{addr}/"),
            token: token.clone(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            kind: kind.to_owned(),
            protocol: orbit_proto::PROTOCOL_VERSION,
            local: if options.local { crate::transport::new_address(data_dir)? } else { String::new() },
        };
        let local = info.local.clone();
        let control = options.control;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        // The local listener is bound inside the runtime; its result comes
        // back before the endpoint is published, so a client never finds an
        // address nobody listens on.
        let (bound_tx, bound_rx) = std::sync::mpsc::channel::<io::Result<()>>();
        std::thread::Builder::new().name("orbit-endpoint".into()).stack_size(512 * 1024).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = bound_tx.send(Err(err));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        let _ = bound_tx.send(Err(err));
                        return;
                    }
                };
                let local_listener = if local.is_empty() {
                    None
                } else {
                    match crate::transport::LocalListener::bind(&local) {
                        Ok(l) => Some(l),
                        Err(err) => {
                            let _ = bound_tx.send(Err(err));
                            return;
                        }
                    }
                };
                let _ = bound_tx.send(Ok(()));
                let ws = crate::server::serve_ws(engine.clone(), listener, token.clone(), control.clone());
                let local = async move {
                    match local_listener {
                        Some(l) => crate::server::serve_local(engine, l, token, control).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    result = ws => {
                        if let Err(err) = result {
                            log::warn!("control endpoint stopped: {err}");
                        }
                    }
                    result = local => {
                        if let Err(err) = result {
                            log::warn!("local endpoint stopped: {err}");
                        }
                    }
                    _ = stop_rx => {}
                }
            });
        })?;
        bound_rx.recv().map_err(|_| io::Error::other("the endpoint thread stopped"))??;
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(FILE);
        let json = serde_json::to_string(&info).map_err(io::Error::other)?;
        let endpoint = LocalEndpoint { inner: Arc::new(Inner { info, path: path.clone(), stop: Mutex::new(Some(stop_tx)) }) };
        // On failure `endpoint` drops here and stops the server.
        write_private(&path, &json)?;
        Ok(endpoint)
    }

    /// 128 bits from the OS's secure random source, hex encoded.
    fn new_token() -> io::Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The file as written, without the liveness check.
#[cfg(feature = "engine")]
fn read_raw(path: &Path) -> Option<EndpointInfo> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_means_no_engine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
    }

    #[test]
    fn stale_file_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Spawn and reap a short-lived process: its pid is then dead.
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/C", "exit"]).spawn().unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap();
        let info = EndpointInfo {
            url: "ws://127.0.0.1:1/".into(),
            token: "t".into(),
            pid,
            version: "0".into(),
            kind: "desktop".into(),
            protocol: 1,
            local: String::new(),
        };
        write_private(&dir.path().join(FILE), &serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
        assert!(!dir.path().join(FILE).exists(), "the stale file is cleaned up");
        // The same file naming a live process is read back.
        let live = EndpointInfo { pid: std::process::id(), ..info };
        write_private(&dir.path().join(FILE), &serde_json::to_string(&live).unwrap()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(live));
    }

    /// A child that exited but is not reaped yet still answers
    /// `kill(pid, 0)`; it is not running.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn an_exited_unreaped_child_is_not_alive() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while process_alive(pid) {
            assert!(std::time::Instant::now() < deadline, "an exited child still counts as running");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // It is a zombie, not gone: that is the case `kill` alone gets wrong.
        // SAFETY: signal 0 sends nothing.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0, "not reaped yet");
        child.wait().unwrap();
        assert!(!process_alive(pid));
        // A running child is alive.
        let mut sleeper = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(process_alive(sleeper.id()));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{not json").unwrap();
        assert!(read(dir.path()).is_err());
    }

    #[cfg(feature = "engine")]
    #[test]
    fn default_folder_matches_the_engine() {
        assert_eq!(platform_data_dir(), orbit_core::EngineOptions::default_data_dir());
    }

    #[cfg(feature = "engine")]
    #[test]
    fn endpoint_is_published_served_and_removed() {
        use crate::{Backend, ConnectionStatus, RemoteClient, Update};
        let dir = tempfile::tempdir().unwrap();
        let engine = orbit_core::EngineHandle::start_with(
            orbit_core::EngineOptions { data_dir: dir.path().to_owned(), probe_providers: false },
            Box::new(|_, _, _| {
                let (input, _rx) = crossfire::mpsc::bounded_blocking(1);
                orbit_core::provider::SessionHandle { input }
            }),
        )
        .unwrap();
        let endpoint = start(engine.clone(), dir.path(), "test").unwrap();
        let info = read(dir.path()).unwrap().expect("published");
        assert_eq!(&info, endpoint.info());
        assert!(info.url.starts_with("ws://127.0.0.1:"));
        assert_eq!(info.token.len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let backend = Backend::Remote(RemoteClient::connect(info.url.clone(), info.token.clone()).unwrap());
        let mut feed = backend.feed();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            loop {
                let batch = tokio::time::timeout(std::time::Duration::from_secs(5), feed.next_batch())
                    .await
                    .expect("connected in time")
                    .expect("feed open");
                if batch.iter().any(|u| matches!(u, Update::Status(ConnectionStatus::Connected))) {
                    break;
                }
            }
        });
        backend.shutdown();

        let clone = endpoint.clone();
        drop(endpoint);
        assert!(dir.path().join(FILE).exists(), "a clone keeps it alive");
        clone.close();
        assert!(!dir.path().join(FILE).exists());
        engine.shutdown();
    }
}
