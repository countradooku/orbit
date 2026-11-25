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
//! * Binaries resolve through PATH and PATHEXT plus each runtime's known
//!   install locations, so `.cmd` shims and WinGet installs both work.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Bytes of stderr kept for diagnostics.
const STDERR_TAIL: usize = 8 * 1024;

/// A running agent process. Dropping it kills the process tree.
pub struct AgentProcess {
    child: Child,
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
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            child,
            stderr_tail,
            #[cfg(windows)]
            _job: job,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
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
        self.child.wait().await
    }

    /// Kills the process tree now.
    pub fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SAFETY: plain syscall; a negative pid targets the process group.
            unsafe {
                libc_kill(-(pid as i32), 9);
            }
        }
        let _ = self.child.start_kill();
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
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
pub struct JsonlReader {
    reader: BufReader<ChildStdout>,
    line: Vec<u8>,
}

impl JsonlReader {
    pub fn new(stdout: ChildStdout) -> Self {
        Self { reader: BufReader::with_capacity(64 * 1024, stdout), line: Vec::with_capacity(16 * 1024) }
    }

    /// The next non-empty line, or `None` at end of stream.
    pub async fn next_line(&mut self) -> std::io::Result<Option<&[u8]>> {
        loop {
            self.line.clear();
            if self.reader.read_until(b'\n', &mut self.line).await? == 0 {
                return Ok(None);
            }
            while matches!(self.line.last(), Some(b'\n' | b'\r')) {
                self.line.pop();
            }
            if !self.line.is_empty() {
                return Ok(Some(&self.line));
            }
        }
    }
}

/// Finds an executable by name on PATH (honoring PATHEXT on Windows), then in
/// `extra` locations. Returns the first existing file.
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
    for dir in std::env::split_paths(&path_var).chain(extra.iter().cloned()) {
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
