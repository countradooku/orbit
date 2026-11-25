//! Child processes for agent runtimes.
//!
//! * No console window ever flashes on Windows (`CREATE_NO_WINDOW`; T3 Code
//!   #2537).
//! * The whole process tree dies with the session: on Windows every child is
//!   placed in a Job Object with `KILL_ON_JOB_CLOSE`, so MCP servers and
//!   shells the agent started cannot outlive it, even if Orbit crashes. On
//!   Unix the child leads its own process group, which is killed on drop.
//! * stderr is always drained and its tail kept for error messages (T3 Code
//!   dropped stderr on Windows and lost every startup diagnostic).
//! * The exit is watched on its own, not inferred from stdout closing: a
//!   tool the agent started can inherit stdout and keep it open long after
//!   the agent died, which used to leave a turn "working" forever.
//!   [`JsonlReader::until_exit`] ends the stream shortly after the process
//!   exits, whoever still holds the pipe.
//! * Binaries resolve through PATH and PATHEXT plus each runtime's known
//!   install locations, so `.cmd` shims and WinGet installs both work.
//! * On Unix a GUI launch (Finder, Dock, a desktop launcher) never ran the
//!   user's shell profile, so PATH lacks Homebrew, nvm, `~/.local/bin`…
//!   Lookups also search those directories and the login shell's PATH, and
//!   agents inherit the same extended PATH so `#!/usr/bin/env node` shims run.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};

/// Bytes of stderr kept for diagnostics.
const STDERR_TAIL: usize = 8 * 1024;
/// How long output may still arrive after the process exited (what it
/// wrote just before exiting is still in the pipe). After that the stream
/// ends even if a leftover child holds the pipe open.
pub const EXIT_DRAIN: Duration = Duration::from_millis(750);

/// How a process ended: its exit status, or why it could not be read.
pub type ExitStatus = Result<std::process::ExitStatus, String>;

/// Watches one process's exit. Cheap to clone; every clone sees the exit.
#[derive(Clone)]
pub struct ExitWatch(watch::Receiver<Option<(ExitStatus, tokio::time::Instant)>>);

impl ExitWatch {
    /// The exit status, once the process has exited.
    pub fn status(&self) -> Option<ExitStatus> {
        self.0.borrow().as_ref().map(|(s, _)| s.clone())
    }

    pub fn has_exited(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// When the process exited, by the monotonic clock.
    fn exited_at(&self) -> Option<tokio::time::Instant> {
        self.0.borrow().as_ref().map(|(_, at)| *at)
    }

    /// Resolves when the process has exited (at once if it already has).
    pub async fn exited(&mut self) {
        // An error means the watcher is gone, which only happens after it
        // published the exit or when the runtime shuts down.
        let _ = self.0.wait_for(Option::is_some).await;
    }

    /// Resolves [`EXIT_DRAIN`] after the process exited: by then whatever
    /// it wrote before exiting has been read.
    pub async fn drained(&mut self) {
        self.exited().await;
        if let Some(at) = self.exited_at() {
            tokio::time::sleep_until(at + EXIT_DRAIN).await;
        }
    }
}

/// A running agent process. Dropping it kills the process tree.
pub struct AgentProcess {
    pid: Option<u32>,
    /// Asks the watcher task to kill the child; dropping it does too.
    kill: Option<oneshot::Sender<()>>,
    exit: ExitWatch,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl AgentProcess {
    pub fn spawn(
        program: &Path,
        args: &[impl AsRef<OsStr>],
        cwd: &Path,
        env: &[(String, String)],
        env_remove: &[&str],
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in env_remove {
            cmd.env_remove(key);
        }
        // Before `env`, so a caller can still override it.
        #[cfg(unix)]
        cmd.env("PATH", unix_path::child_path());
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn()?;
        #[cfg(windows)]
        let job = child.raw_handle().and_then(|h| job::Job::assign(h).ok());

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log::debug!("agent stderr: {line}");
                    let mut tail = tail.lock().unwrap();
                    tail.extend(line.as_bytes());
                    tail.push_back(b'\n');
                    while tail.len() > STDERR_TAIL {
                        tail.pop_front();
                    }
                }
            });
        }
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        // The watcher owns the child: it reaps it the moment it exits (no
        // polling; tokio is woken by the OS) and publishes the status.
        let (kill, kill_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = watch::channel(None);
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                // An explicit kill, or the process handle was dropped.
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let _ = exit_tx.send(Some((status.map_err(|e| e.to_string()), tokio::time::Instant::now())));
        });
        Ok(Self {
            pid,
            kill: Some(kill),
            exit: ExitWatch(exit_rx),
            stdin,
            stdout,
            stderr_tail,
            #[cfg(windows)]
            _job: job,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.pid
    }

    /// A watch on the process's exit, independent of its pipes.
    pub fn exit_watch(&self) -> ExitWatch {
        self.exit.clone()
    }

    /// The last lines the process wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap();
        let (a, b) = tail.as_slices();
        let mut bytes = Vec::with_capacity(a.len() + b.len());
        bytes.extend_from_slice(a);
        bytes.extend_from_slice(b);
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut exit = self.exit.clone();
        exit.exited().await;
        match exit.status() {
            Some(status) => status.map_err(std::io::Error::other),
            None => Err(std::io::Error::other("the process watcher stopped")),
        }
    }

    /// Kills the process tree now.
    pub fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid
            && !self.exit.has_exited()
        {
            // SAFETY: plain syscall; a negative pid targets the process group.
            unsafe {
                libc_kill(-(pid as i32), 9);
            }
        }
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Writes newline-delimited JSON to a child's stdin.
pub struct JsonlWriter {
    stdin: ChildStdin,
    buf: Vec<u8>,
}

impl JsonlWriter {
    pub fn new(stdin: ChildStdin) -> Self {
        Self { stdin, buf: Vec::with_capacity(4096) }
    }

    /// Writes one JSON value followed by `\n` and flushes.
    pub async fn send<T: serde::Serialize + ?Sized>(&mut self, value: &T) -> std::io::Result<()> {
        self.buf.clear();
        sonic_rs::to_writer(&mut self.buf, value).map_err(std::io::Error::other)?;
        self.buf.push(b'\n');
        self.stdin.write_all(&self.buf).await?;
        self.stdin.flush().await
    }

    /// Writes a pre-serialized JSON line.
    pub async fn send_raw(&mut self, json: &str) -> std::io::Result<()> {
        self.stdin.write_all(json.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }
}

/// Reads newline-delimited JSON lines from a child's stdout, reusing one
/// buffer. Lines can be many megabytes (tool results with file contents).
pub struct JsonlReader<R = ChildStdout> {
    reader: BufReader<R>,
    line: Vec<u8>,
    exit: Option<ExitWatch>,
}

impl<R: tokio::io::AsyncRead + Unpin> JsonlReader<R> {
    pub fn new(stdout: R) -> Self {
        Self { reader: BufReader::with_capacity(64 * 1024, stdout), line: Vec::with_capacity(16 * 1024), exit: None }
    }

    /// Ends the stream [`EXIT_DRAIN`] after the process exits, even when a
    /// process it started still holds stdout open.
    pub fn until_exit(mut self, exit: ExitWatch) -> Self {
        self.exit = Some(exit);
        self
    }

    /// The next non-empty line, or `None` at end of stream.
    pub async fn next_line(&mut self) -> std::io::Result<Option<&[u8]>> {
        self.line.clear();
        loop {
            // `read_until` keeps what it read when it is cancelled, so a
            // line interrupted by the exit continues where it stopped.
            let read = match self.exit.as_mut() {
                None => self.reader.read_until(b'\n', &mut self.line).await?,
                Some(exit) => match exit.exited_at() {
                    None => tokio::select! {
                        read = self.reader.read_until(b'\n', &mut self.line) => read?,
                        _ = exit.exited() => continue,
                    },
                    Some(at) => {
                        match tokio::time::timeout_at(at + EXIT_DRAIN, self.reader.read_until(b'\n', &mut self.line)).await {
                            Ok(read) => read?,
                            Err(_) => 0, // a leftover child holds the pipe
                        }
                    }
                },
            };
            if read == 0 && self.line.is_empty() {
                return Ok(None);
            }
            while matches!(self.line.last(), Some(b'\n' | b'\r')) {
                self.line.pop();
            }
            if !self.line.is_empty() {
                return Ok(Some(&self.line));
            }
            if read == 0 {
                return Ok(None);
            }
        }
    }
}

/// Finds an executable by name on PATH (honoring PATHEXT on Windows), then in
/// `extra` locations, then (Unix only) in the common user and package-manager
/// bin directories and the login shell's PATH. Returns the first existing file.
pub fn resolve_binary(name: &str, explicit: Option<&Path>, extra: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = explicit {
        let expanded = expand_home(path);
        return expanded.is_file().then_some(expanded);
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::env::split_paths(&path_var).chain(extra.iter().cloned());
    #[cfg(unix)]
    let dirs = dirs.chain(unix_path::fallback_dirs().iter().cloned());
    for dir in dirs {
        for ext in &exts {
            // Prefer a real executable over a .cmd shim: Rust can spawn
            // .cmd files only through cmd.exe, which mangles arguments.
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| path.to_owned()),
        Err(_) => path.to_owned(),
    }
}

#[cfg(unix)]
mod unix_path {
    //! The directories a terminal session would have on PATH but a GUI launch
    //! does not. Computed once per process.
    use std::ffi::OsString;
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::time::Duration;

    /// How long the login shell may take to print its PATH.
    const SHELL_TIMEOUT: Duration = Duration::from_secs(3);
    const MARKER: &str = "__ORBIT_PATH__";

    /// Searched after PATH: per-user installs, npm/Volta/Bun globals,
    /// Homebrew (Apple silicon, Intel, Linux), then the login shell's PATH
    /// (nvm, asdf, mise and anything else the profile adds).
    pub fn fallback_dirs() -> &'static [PathBuf] {
        static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
        DIRS.get_or_init(|| {
            let mut out = Vec::new();
            if let Some(home) = dirs::home_dir() {
                for dir in [".local/bin", ".npm-global/bin", ".volta/bin", ".bun/bin", ".cargo/bin"] {
                    out.push(home.join(dir));
                }
            }
            if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX").filter(|p| !p.is_empty()) {
                out.push(PathBuf::from(prefix).join("bin"));
            }
            for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"] {
                out.push(PathBuf::from(dir));
            }
            out.extend(login_shell_path());
            let mut seen = std::collections::HashSet::new();
            out.retain(|d| seen.insert(d.clone()));
            out
        })
    }

    /// PATH for agent processes: Orbit's own PATH, then every fallback
    /// directory that exists and is not on it yet.
    pub fn child_path() -> &'static OsString {
        static PATH: OnceLock<OsString> = OnceLock::new();
        PATH.get_or_init(|| {
            let current = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&current).collect();
            for dir in fallback_dirs() {
                if dir.is_dir() && !dirs.contains(dir) {
                    dirs.push(dir.clone());
                }
            }
            std::env::join_paths(dirs).unwrap_or(current)
        })
    }

    /// `$SHELL -l -i -c 'printenv PATH'`, bounded by [`SHELL_TIMEOUT`].
    /// Empty when there is no `$SHELL` or it fails. `printenv` keeps fish's
    /// list-valued `$PATH` colon-separated; the marker skips profile chatter.
    fn login_shell_path() -> Vec<PathBuf> {
        let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        let mut cmd = Command::new(&shell);
        cmd.args(["-l", "-i", "-c", &format!("echo {MARKER}; printenv PATH")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe. A new session has no
        // controlling terminal, so the interactive shell cannot take over
        // the terminal Orbit was started from; it also leads its own process
        // group, which is killed below.
        unsafe {
            cmd.pre_exec(|| {
                super::libc_setsid();
                Ok(())
            });
        }
        let Ok(mut child) = cmd.spawn() else {
            return Vec::new();
        };
        let stdout = child.stdout.take();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_string(&mut text);
            }
            let _ = tx.send(text);
        });
        let text = rx.recv_timeout(SHELL_TIMEOUT).unwrap_or_default();
        // SAFETY: plain syscall on the group the shell leads (see setsid).
        unsafe {
            super::libc_kill(-(child.id() as i32), 9);
        }
        let _ = child.wait();
        let mut lines = text.lines().skip_while(|l| l.trim() != MARKER).skip(1);
        match lines.next() {
            Some(path) => std::env::split_paths(path.trim()).filter(|p| p.is_absolute()).collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(windows)]
mod job {
    //! A Job Object that kills every process in it when the last handle
    //! closes, which happens when the session drops or Orbit exits.
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        counters: [u64; 6],
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    pub struct Job(Handle);

    // SAFETY: a job handle is a kernel object reference usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn assign(process: std::os::windows::io::RawHandle) -> std::io::Result<Self> {
            // SAFETY: standard Win32 calls with valid arguments; every failure
            // is checked and the handle is closed by Drop.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Job(job);
                let mut info = ExtendedLimit::default();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.0,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<ExtendedLimit>() as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job.0, process as Handle) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_jsonl_lines_and_skips_blank_ones() {
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd", vec!["/C", "echo first& echo.& echo second"])
        } else {
            ("sh", vec!["-c", "printf 'first\\n\\nsecond\\n'"])
        };
        let program = resolve_binary(program, None, &[]).expect("shell on PATH");
        let mut proc = AgentProcess::spawn(&program, &args, &std::env::temp_dir(), &[], &[]).unwrap();
        let mut reader = JsonlReader::new(proc.stdout.take().unwrap());
        let first = reader.next_line().await.unwrap().unwrap().to_vec();
        assert_eq!(String::from_utf8_lossy(&first).trim(), "first");
        let second = reader.next_line().await.unwrap().unwrap().to_vec();
        assert_eq!(String::from_utf8_lossy(&second).trim(), "second");
        assert!(reader.next_line().await.unwrap().is_none());
    }
}
