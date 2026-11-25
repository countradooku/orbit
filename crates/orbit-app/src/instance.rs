//! One Orbit per data folder. A second launch hands its arguments to the
//! running Orbit, which comes to the front, and exits.
//!
//! The first launch takes `<data>/app.lock` (an OS file lock, released when
//! the process ends, even on a crash) and listens on a local endpoint:
//!
//! * macOS and Linux: a Unix socket, `<data>/orbit.sock` (only the user can
//!   reach it; a loopback TCP port when the path is too long for a socket);
//! * Windows: a loopback TCP port.
//!
//! `<data>/app.endpoint` names the endpoint, the running process and a random
//! token the second launch must present, so another local user or program
//! cannot drive the window. Messages are one JSON line each way.
//!
//! On Windows the second launch, which owns the foreground because the user
//! just started it, lets the running process take the foreground
//! (`AllowSetForegroundWindow`) before handing over.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde::{Deserialize, Serialize};

const LOCK_FILE: &str = "app.lock";
const ENDPOINT_FILE: &str = "app.endpoint";
#[cfg(unix)]
const SOCKET_FILE: &str = "orbit.sock";
const TIMEOUT: Duration = Duration::from_secs(3);

/// What a launch asks of the running Orbit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Where the launch ran, to resolve relative paths.
    pub cwd: String,
}

#[derive(Serialize, Deserialize)]
struct Message {
    token: String,
    launch: Launch,
}

/// This process owns the data folder.
pub struct Instance {
    _lock: File,
    endpoint_file: PathBuf,
    /// Launches handed over by later processes.
    pub launches: UnboundedReceiver<Launch>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint_file);
    }
}

/// Removes this process's hand-off endpoint file on quit: the instance
/// itself lives in a window task that is not dropped when the app exits.
pub fn remove_endpoint_file(data_dir: &Path) {
    let _ = std::fs::remove_file(data_dir.join(ENDPOINT_FILE));
}

pub enum Claim {
    /// Run the app; hand-offs arrive on `launches`.
    Primary(Instance),
    /// Another Orbit took the launch; exit.
    Forwarded,
}

/// The launch of this process.
pub fn this_launch() -> Launch {
    Launch {
        args: std::env::args().skip(1).collect(),
        cwd: std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
    }
}

/// Becomes the data folder's Orbit, or hands `launch` to the one running.
pub fn claim(data_dir: &Path, launch: &Launch) -> io::Result<Claim> {
    std::fs::create_dir_all(data_dir)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join(LOCK_FILE))?;
    if lock.try_lock().is_err() {
        forward(data_dir, launch)?;
        return Ok(Claim::Forwarded);
    }
    let (tx, rx) = unbounded();
    let token = token();
    let endpoint = listen(data_dir, token.clone(), tx)?;
    let endpoint_file = data_dir.join(ENDPOINT_FILE);
    let tmp = endpoint_file.with_extension("tmp");
    std::fs::write(&tmp, format!("{endpoint}\n{token}\n{}\n", std::process::id()))?;
    std::fs::rename(&tmp, &endpoint_file)?;
    Ok(Claim::Primary(Instance { _lock: lock, endpoint_file, launches: rx }))
}

/// 128 random bits from the standard library's per-process random keys.
fn token() -> String {
    let a = std::collections::hash_map::RandomState::new().build_hasher();
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    b.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

/// Starts the listener thread; returns how to reach it (`tcp <addr>` or
/// `unix <path>`).
fn listen(data_dir: &Path, token: String, tx: UnboundedSender<Launch>) -> io::Result<String> {
    #[cfg(unix)]
    {
        let path = data_dir.join(SOCKET_FILE);
        // Socket paths are limited to about 100 bytes.
        if path.as_os_str().len() < 100 {
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            let endpoint = format!("unix {}", path.display());
            spawn_accept(move || listener.accept().map(|(s, _)| s), token, tx);
            return Ok(endpoint);
        }
    }
    let _ = data_dir;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let endpoint = format!("tcp {}", listener.local_addr()?);
    spawn_accept(move || listener.accept().map(|(s, _)| s), token, tx);
    Ok(endpoint)
}

trait Stream: io::Read + io::Write + Send + 'static {
    fn set_timeouts(&self, timeout: Duration);
}

impl Stream for std::net::TcpStream {
    fn set_timeouts(&self, timeout: Duration) {
        let _ = self.set_read_timeout(Some(timeout));
        let _ = self.set_write_timeout(Some(timeout));
    }
}

#[cfg(unix)]
impl Stream for std::os::unix::net::UnixStream {
    fn set_timeouts(&self, timeout: Duration) {
        let _ = self.set_read_timeout(Some(timeout));
        let _ = self.set_write_timeout(Some(timeout));
    }
}

fn spawn_accept<S: Stream>(mut accept: impl FnMut() -> io::Result<S> + Send + 'static, token: String, tx: UnboundedSender<Launch>) {
    std::thread::Builder::new()
        .name("orbit-instance".into())
        .spawn(move || {
            while let Ok(stream) = accept() {
                if let Some(launch) = serve(stream, &token)
                    && tx.unbounded_send(launch).is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn the instance listener");
}

/// Reads one hand-off, checks its token, and acknowledges it.
fn serve<S: Stream>(stream: S, token: &str) -> Option<Launch> {
    stream.set_timeouts(TIMEOUT);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.by_ref().take(256 * 1024).read_line(&mut line).ok()?;
    let message: Message = sonic_rs::from_str(line.trim()).ok()?;
    let ok = message.token == token;
    let _ = reader.get_mut().write_all(if ok { b"ok\n" } else { b"denied\n" });
    ok.then_some(message.launch)
}

use std::io::Read as _;

/// Hands `launch` to the running Orbit.
fn forward(data_dir: &Path, launch: &Launch) -> io::Result<()> {
    // The running Orbit may still be writing its endpoint (two launches at
    // once): give it a moment.
    let started = std::time::Instant::now();
    let text = loop {
        match std::fs::read_to_string(data_dir.join(ENDPOINT_FILE)) {
            Ok(text) if text.lines().count() >= 3 => break text,
            _ if started.elapsed() > TIMEOUT => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "Orbit is running but has not opened its endpoint"));
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut lines = text.lines();
    let endpoint = lines.next().unwrap_or_default().to_owned();
    let token = lines.next().unwrap_or_default().to_owned();
    let pid: u32 = lines.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
    allow_foreground(pid);
    let message = sonic_rs::to_string(&Message { token, launch: launch.clone() }).map_err(io::Error::other)?;
    let reply = match endpoint.split_once(' ') {
        Some(("tcp", addr)) => {
            let addr: std::net::SocketAddr = addr.parse().map_err(io::Error::other)?;
            exchange(std::net::TcpStream::connect_timeout(&addr, TIMEOUT)?, &message)?
        }
        #[cfg(unix)]
        Some(("unix", path)) => exchange(std::os::unix::net::UnixStream::connect(path)?, &message)?,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown endpoint")),
    };
    match reply.trim() {
        "ok" => Ok(()),
        other => Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the running Orbit answered “{other}”"))),
    }
}

fn exchange<S: Stream>(mut stream: S, message: &str) -> io::Result<String> {
    stream.set_timeouts(TIMEOUT);
    stream.write_all(message.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

#[cfg(windows)]
fn allow_foreground(pid: u32) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
    }
    if pid != 0 {
        // SAFETY: a plain Win32 call with a process id; failure is harmless.
        unsafe { AllowSetForegroundWindow(pid) };
    }
}

#[cfg(not(windows))]
fn allow_foreground(_: u32) {}

/// Un-minimizes the window before it is brought to the front. GPUI's
/// activation focuses a window but leaves a minimized one in the taskbar on
/// Windows; macOS and Linux window managers restore on activation.
#[cfg(windows)]
pub fn restore_window(window: &gpui_kit::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsIconic(hwnd: isize) -> i32;
        fn ShowWindow(hwnd: isize, command: i32) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    if let RawWindowHandle::Win32(handle) = handle.as_raw() {
        let hwnd = handle.hwnd.get();
        // SAFETY: the handle is this process's live top-level window.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn restore_window(_: &gpui_kit::Window) {}

/// What a launch asks for, once its arguments are read.
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    /// Open a thread: `orbit://thread/<id>` or `--thread <id>`.
    Thread(String),
    /// Add (or select) a project folder: `orbit <folder>`.
    Folder(PathBuf),
}

/// Reads a launch's arguments. Unknown flags are ignored.
pub fn intents(launch: &Launch) -> Vec<Intent> {
    let mut out = Vec::new();
    let mut args = launch.args.iter();
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("orbit://thread/") {
            out.push(Intent::Thread(rest.trim_end_matches('/').to_owned()));
        } else if arg == "--thread" {
            if let Some(id) = args.next() {
                out.push(Intent::Thread(id.clone()));
            }
        } else if !arg.starts_with('-') {
            let path = Path::new(arg);
            let path = if path.is_absolute() { path.to_path_buf() } else { Path::new(&launch.cwd).join(path) };
            if path.is_dir() {
                out.push(Intent::Folder(path));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_reaches_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let launch = Launch { args: vec!["--thread".into(), "thr_1".into()], cwd: "/tmp".into() };
        let Claim::Primary(mut first) = claim(dir.path(), &Launch::default()).unwrap() else {
            panic!("the first launch owns the folder")
        };
        assert!(matches!(claim(dir.path(), &launch).unwrap(), Claim::Forwarded));
        let got = futures::executor::block_on(futures::StreamExt::next(&mut first.launches)).unwrap();
        assert_eq!(got, launch);

        // A wrong token is refused.
        let text = std::fs::read_to_string(dir.path().join(ENDPOINT_FILE)).unwrap();
        let bad = text.replacen(text.lines().nth(1).unwrap(), "0000", 1);
        std::fs::write(dir.path().join(ENDPOINT_FILE), bad).unwrap();
        assert!(forward(dir.path(), &launch).is_err());

        // Once the first is gone, the folder is free again.
        drop(first);
        assert!(matches!(claim(dir.path(), &Launch::default()).unwrap(), Claim::Primary(_)));
    }

    #[test]
    fn arguments_become_intents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("repo")).unwrap();
        let launch = Launch {
            args: vec!["repo".into(), "orbit://thread/thr_9/".into(), "--verbose".into(), "missing".into()],
            cwd: dir.path().display().to_string(),
        };
        assert_eq!(intents(&launch), vec![Intent::Folder(dir.path().join("repo")), Intent::Thread("thr_9".into())]);
    }
}
