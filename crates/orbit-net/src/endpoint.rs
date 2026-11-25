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
//! whose process is gone is treated as absent.
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
        return Ok(None);
    }
    Ok(Some(info))
}

/// Whether a process with this id exists.
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
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

#[cfg_attr(not(any(test, feature = "engine")), allow(dead_code))]
/// Writes `contents` to `path` atomically (temp file and rename) so a
/// reader never sees half a file, readable by the owner only on Unix.
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
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
pub use serve::{LocalEndpoint, start};

#[cfg(feature = "engine")]
mod serve {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use orbit_core::EngineHandle;

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
        };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::Builder::new().name("orbit-endpoint".into()).stack_size(512 * 1024).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build() {
                Ok(runtime) => runtime,
                Err(err) => return log::warn!("control endpoint: {err}"),
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => return log::warn!("control endpoint: {err}"),
                };
                tokio::select! {
                    result = crate::server::serve_on(engine, listener, token) => {
                        if let Err(err) = result {
                            log::warn!("control endpoint stopped: {err}");
                        }
                    }
                    _ = stop_rx => {}
                }
            });
        })?;
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
        };
        write_private(&dir.path().join(FILE), &serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
        // The same file naming a live process is read back.
        let live = EndpointInfo { pid: std::process::id(), ..info };
        write_private(&dir.path().join(FILE), &serde_json::to_string(&live).unwrap()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(live));
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
