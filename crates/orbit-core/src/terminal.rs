//! Integrated terminals: real shells on a PTY (ConPTY on Windows).
//!
//! [`Terminals`] owns every open terminal. It is source of truth for nothing
//! but the processes: the screen state lives in whatever consumes the bytes
//! (the GPUI `TerminalView`, a remote client), so the engine never parses VT.
//!
//! * **Never blocks the caller.** Each terminal runs three small threads:
//!   a reader (blocking PTY reads), a pump that coalesces output, and a waiter
//!   that reports the exit. Input goes through a fourth writer thread, so a
//!   shell that stops reading its input cannot stall the engine either.
//! * **Huge output stays cheap.** The pump forwards the first chunk after an
//!   idle period at once (keystroke echo is not delayed), then batches
//!   everything that follows into chunks of at most [`MAX_CHUNK`] bytes or
//!   [`COALESCE_WINDOW`], so `cat` of a 1 GB log is a few thousand events, not
//!   millions. The output channel is bounded: a consumer that falls behind
//!   applies backpressure to the shell instead of growing memory.
//! * **Ordered exit.** A terminal's exit is sent on the exit channel only after
//!   its last output chunk was sent on the output channel.
//! * **The consumer is a terminal.** ConPTY asks for the cursor position
//!   (`ESC [ 6 n`) when a shell starts and holds the shell's output until it
//!   is answered, as any VT emulator does (`TerminalView` answers it). A
//!   consumer that is not an emulator answers `ESC [ 1 ; 1 R` itself.
//! * **Nothing survives a close.** On Windows the shell is placed in a Job
//!   Object with `KILL_ON_JOB_CLOSE`; on Unix the shell leads its own session
//!   and process group, which receives `SIGHUP` and then `SIGKILL`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait as _, RecvTimeoutError, SendTimeoutError};
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub use orbit_proto::TermId;

/// Largest output chunk sent as one event.
pub const MAX_CHUNK: usize = 64 * 1024;
/// Longest time output is held back to be coalesced with what follows.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(8);
/// Output events buffered before the shell is made to wait (≈16 MB).
pub const OUTPUT_CAPACITY: usize = 256;

/// Output of every terminal, tagged with its id. Bounded: see [`OUTPUT_CAPACITY`].
pub type OutputSender = crossfire::MTx<crossfire::mpsc::Array<(TermId, Vec<u8>)>>;
/// Exit of every terminal with its exit code (`None` when it is unknown).
pub type ExitSender = crossfire::MTx<crossfire::mpsc::List<(TermId, Option<i32>)>>;

const READ_BUFFER: usize = 64 * 1024;
const THREAD_STACK: usize = 256 * 1024;
/// How long the waiter waits for the reader to drain after the shell exited.
/// Only reached on Unix when a background process keeps the PTY open.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// The grid size of a terminal in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl TermSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols: cols.max(1), rows: rows.max(1) }
    }

    fn pty(self) -> PtySize {
        PtySize { rows: self.rows.max(1), cols: self.cols.max(1), pixel_width: 0, pixel_height: 0 }
    }
}

impl Default for TermSize {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

/// The program a terminal runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl Shell {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new() }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable for the shell (on top of Orbit's own).
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.args
    }

    /// The user's interactive shell: on Windows `pwsh`, then Windows
    /// PowerShell, then `%COMSPEC%` (cmd); on Unix `$SHELL`, then `/bin/sh`.
    pub fn user_default() -> Self {
        #[cfg(windows)]
        {
            use crate::provider::process::resolve_binary;
            if let Some(pwsh) = resolve_binary("pwsh", None, &[]) {
                return Shell::new(pwsh).arg("-NoLogo");
            }
            if let Some(ps) = resolve_binary("powershell", None, &[]) {
                return Shell::new(ps).arg("-NoLogo");
            }
            let cmd = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
            Shell::new(cmd)
        }
        #[cfg(not(windows))]
        {
            let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty());
            Shell::new(shell.unwrap_or_else(|| "/bin/sh".into()))
        }
    }
}

/// Starts, feeds, resizes and closes terminals. Dropping it closes them all.
pub struct Terminals {
    output: OutputSender,
    exits: ExitSender,
    open: Mutex<HashMap<TermId, Terminal>>,
}

/// The parts of one terminal the manager keeps.
struct Terminal {
    /// Taken (closing the pseudoconsole) when the shell exits.
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    input: crossfire::MTx<crossfire::mpsc::List<Vec<u8>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    closed: Arc<AtomicBool>,
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Terminals {
    /// Output chunks go to `output`, exits to `exits`. Either receiver may be
    /// dropped; a terminal whose output nobody reads is drained and discarded
    /// once it is closed.
    pub fn new(output: OutputSender, exits: ExitSender) -> Self {
        Self { output, exits, open: Mutex::new(HashMap::new()) }
    }

    /// Starts `shell` (the user's default when `None`) in `cwd`.
    pub fn open(&self, cwd: &Path, size: TermSize, shell: Option<Shell>) -> std::io::Result<TermId> {
        let shell = shell.unwrap_or_else(Shell::user_default);
        if !cwd.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the working directory {} does not exist", cwd.display()),
            ));
        }
        let pair = native_pty_system().openpty(size.pty()).map_err(std::io::Error::other)?;

        let mut cmd = CommandBuilder::new(&shell.program);
        cmd.args(&shell.args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Orbit");
        for (key, value) in &shell.env {
            cmd.env(key, value);
        }
        let mut child = pair.slave.spawn_command(cmd).map_err(std::io::Error::other)?;
        // The slave end belongs to the shell now; keeping it would keep the
        // PTY open after the shell exits (Unix never reaches EOF).
        drop(pair.slave);

        #[cfg(windows)]
        let job = child.as_raw_handle().and_then(|h| job::Job::assign(h).ok());
        #[cfg(unix)]
        let pid = child.process_id();
        let killer = child.clone_killer();

        let reader = pair.master.try_clone_reader().map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let closed = Arc::new(AtomicBool::new(false));
        let id = TermId::new();

        // reader -> pump: raw reads, bounded so a stalled pump stalls the PTY.
        let (raw_tx, raw_rx) = crossfire::spsc::bounded_blocking::<Vec<u8>>(64);
        spawn(format!("term-read-{id}"), move || read_loop(reader, raw_tx))?;

        // pump -> waiter: "all output was forwarded".
        let (drained_tx, drained_rx) = crossfire::spsc::bounded_blocking::<()>(1);
        {
            let output = self.output.clone();
            let closed = closed.clone();
            spawn(format!("term-pump-{id}"), move || {
                pump_loop(id, raw_rx, &output, &closed);
                let _ = drained_tx.try_send(());
            })?;
        }

        {
            let exits = self.exits.clone();
            let master = master.clone();
            spawn(format!("term-wait-{id}"), move || {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                // Closing the pseudoconsole is what ends the reader on Windows.
                // The reader is still draining, so ConPTY cannot deadlock here.
                drop(master.lock().take());
                let _ = drained_rx.recv_timeout(DRAIN_GRACE);
                let _ = exits.send((id, code));
            })?;
        }

        let (input_tx, input_rx) = crossfire::mpsc::unbounded_blocking::<Vec<u8>>();
        spawn(format!("term-write-{id}"), move || write_loop(writer, input_rx))?;

        self.open.lock().insert(
            id,
            Terminal {
                master,
                input: input_tx,
                killer,
                closed,
                #[cfg(unix)]
                pid,
                #[cfg(windows)]
                _job: job,
            },
        );
        Ok(id)
    }

    /// Queues `bytes` for the shell's input. Never blocks. Returns `false`
    /// when the terminal is not open.
    pub fn input(&self, id: TermId, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return self.is_open(id);
        }
        let open = self.open.lock();
        open.get(&id).is_some_and(|t| t.input.send(bytes.to_vec()).is_ok())
    }

    /// Changes the grid size the shell sees.
    pub fn resize(&self, id: TermId, size: TermSize) -> std::io::Result<()> {
        let open = self.open.lock();
        let term = open.get(&id).ok_or_else(|| not_open(id))?;
        let master = term.master.lock();
        match master.as_ref() {
            Some(master) => master.resize(size.pty()).map_err(std::io::Error::other),
            None => Err(not_open(id)),
        }
    }

    /// The grid size the shell sees.
    pub fn size(&self, id: TermId) -> Option<TermSize> {
        let open = self.open.lock();
        let master = open.get(&id)?.master.lock();
        let size = master.as_ref()?.get_size().ok()?;
        Some(TermSize::new(size.cols, size.rows))
    }

    /// Kills the shell and everything it started. Its exit is still reported
    /// on the exit channel. Returns `false` when the terminal was not open.
    pub fn close(&self, id: TermId) -> bool {
        let term = self.open.lock().remove(&id);
        match term {
            Some(term) => {
                term.kill();
                true
            }
            None => false,
        }
    }

    /// Whether `id` is open (closed terminals and exited shells are not).
    pub fn is_open(&self, id: TermId) -> bool {
        self.open.lock().get(&id).is_some_and(|t| t.master.lock().is_some())
    }

    /// Forgets a terminal whose shell exited by itself. Call it when the exit
    /// notification arrives; [`Terminals::close`] does the same and kills.
    pub fn remove_exited(&self, id: TermId) {
        let mut open = self.open.lock();
        if open.get(&id).is_some_and(|t| t.master.lock().is_none()) {
            open.remove(&id);
        }
    }

    /// Ids of the terminals that are open, in creation order.
    pub fn ids(&self) -> Vec<TermId> {
        let mut ids: Vec<TermId> = self.open.lock().keys().copied().collect();
        ids.sort();
        ids
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        for (_, term) in self.open.lock().drain() {
            term.kill();
        }
    }
}

impl Terminal {
    fn kill(mut self) {
        self.closed.store(true, Ordering::Release);
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: plain syscalls; a negative pid targets the shell's
            // process group (it called setsid, so it leads one).
            unsafe {
                libc_kill(-(pid as i32), 1); // SIGHUP: shells forward it to their jobs
                libc_kill(-(pid as i32), 9);
            }
        }
        #[cfg(windows)]
        if let Some(job) = self._job.take() {
            job.terminate();
        }
        let _ = self.killer.kill();
        // Dropping the input sender ends the writer thread.
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn not_open(id: TermId) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, format!("terminal {id} is not open"))
}

fn spawn(name: String, f: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    thread::Builder::new().name(name).stack_size(THREAD_STACK).spawn(f).map(drop)
}

fn read_loop(mut reader: Box<dyn Read + Send>, raw: crossfire::Tx<crossfire::spsc::Array<Vec<u8>>>) {
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if raw.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO on Unix when the shell side closes; a broken pipe on Windows.
            Err(_) => return,
        }
    }
}

/// Forwards reads as coalesced chunks: the first chunk after an idle window
/// leaves at once, later ones are merged for up to [`COALESCE_WINDOW`] or
/// [`MAX_CHUNK`] bytes.
fn pump_loop(
    id: TermId,
    raw: crossfire::Rx<crossfire::spsc::Array<Vec<u8>>>,
    output: &OutputSender,
    closed: &AtomicBool,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut last_flush: Option<Instant> = None;
    loop {
        if buf.is_empty() {
            match raw.recv() {
                Ok(chunk) => buf = chunk,
                Err(_) => return,
            }
        }
        let mut disconnected = false;
        while buf.len() < MAX_CHUNK {
            let now = Instant::now();
            let wait = last_flush.map_or(Duration::ZERO, |at| (at + COALESCE_WINDOW).saturating_duration_since(now));
            let next = if wait.is_zero() {
                match raw.try_recv() {
                    Ok(next) => Ok(next),
                    Err(crossfire::TryRecvError::Empty) => break,
                    Err(crossfire::TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                }
            } else {
                raw.recv_timeout(wait)
            };
            match next {
                Ok(next) => buf.extend_from_slice(&next),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        loop {
            let rest = if buf.len() > MAX_CHUNK { buf.split_off(MAX_CHUNK) } else { Vec::new() };
            let chunk = std::mem::replace(&mut buf, rest);
            if !forward(id, chunk, output, closed) {
                return;
            }
            last_flush = Some(Instant::now());
            if !disconnected || buf.is_empty() {
                break;
            }
        }
        if disconnected {
            return;
        }
    }
}

/// Sends one chunk, waiting while the consumer is behind. Gives up when the
/// consumer is gone, or when the terminal was closed and the consumer is
/// still not reading (so a closed terminal never leaks a blocked thread).
fn forward(id: TermId, mut chunk: Vec<u8>, output: &OutputSender, closed: &AtomicBool) -> bool {
    loop {
        match output.send_timeout((id, chunk), Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(SendTimeoutError::Disconnected(_)) => return false,
            Err(SendTimeoutError::Timeout((_, back))) => {
                if closed.load(Ordering::Acquire) {
                    return false;
                }
                chunk = back;
            }
        }
    }
}

fn write_loop(mut writer: Box<dyn Write + Send>, input: crossfire::Rx<crossfire::mpsc::List<Vec<u8>>>) {
    while let Ok(bytes) = input.recv() {
        if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
            return;
        }
    }
}

#[cfg(windows)]
mod job {
    //! A Job Object that kills the shell and every process it started when
    //! the terminal closes, or when Orbit exits and the handle is released.
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
    struct ExtendedLimit {
        basic: BasicLimit,
        io: [u64; 6],
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
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
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

        /// Kills every process in the job now.
        pub fn terminate(self) {
            // SAFETY: the handle is valid until Drop closes it.
            unsafe {
                TerminateJobObject(self.0, 1);
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
    use crossfire::mpsc;

    type OutputRx = crossfire::Rx<crossfire::mpsc::Array<(TermId, Vec<u8>)>>;
    type ExitRx = crossfire::Rx<crossfire::mpsc::List<(TermId, Option<i32>)>>;

    fn manager() -> (Terminals, OutputRx, ExitRx) {
        let (out_tx, out_rx) = mpsc::bounded_blocking(OUTPUT_CAPACITY);
        let (exit_tx, exit_rx) = mpsc::unbounded_blocking();
        (Terminals::new(out_tx, exit_tx), out_rx, exit_rx)
    }

    /// A plain, fast shell for tests (not the user's interactive one).
    fn test_shell() -> Shell {
        if cfg!(windows) {
            Shell::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())).arg("/Q")
        } else {
            Shell::new("/bin/sh")
        }
    }

    fn command_shell(script: &str) -> Shell {
        if cfg!(windows) { test_shell().args(["/D", "/C", script]) } else { test_shell().args(["-c", script]) }
    }

    /// Collects one terminal's output until `done` holds, answering the
    /// cursor position report ConPTY asks for at startup like a real
    /// terminal would.
    fn read_until(
        terms: &Terminals,
        rx: &OutputRx,
        id: TermId,
        timeout: Duration,
        mut done: impl FnMut(&[u8]) -> bool,
    ) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut all = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((from, bytes)) => {
                    assert_eq!(from, id);
                    let scan_from = all.len().saturating_sub(3);
                    all.extend_from_slice(&bytes);
                    if memchr::memmem::find(&all[scan_from..], b"\x1b[6n").is_some() {
                        terms.input(id, b"\x1b[1;1R");
                    }
                    if done(&all) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        all
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        memchr::memmem::find(haystack, needle.as_bytes()).is_some()
    }

    #[test]
    fn echo_round_trips() {
        let (terms, out, _exits) = manager();
        let cwd = std::env::temp_dir();
        let id = terms.open(&cwd, TermSize::new(100, 30), Some(test_shell())).unwrap();
        assert!(terms.is_open(id));
        assert!(terms.input(id, b"echo orbit-$((40+2))-ok\r"));
        if cfg!(windows) {
            // cmd has no arithmetic expansion; echo a marker it cannot fake.
            terms.input(id, b"echo orbit-%PROCESSOR_ARCHITECTURE:~0,0%42-ok\r");
        }
        let all = read_until(&terms, &out, id, Duration::from_secs(20), |all| contains(all, "orbit-42-ok"));
        assert!(contains(&all, "orbit-42-ok"), "output: {}", String::from_utf8_lossy(&all));
        assert!(terms.close(id));
        assert!(!terms.is_open(id));
        assert!(!terms.input(id, b"x"));
    }

    #[test]
    fn resize_reaches_the_shell() {
        let (terms, out, _exits) = manager();
        let id = terms.open(&std::env::temp_dir(), TermSize::new(80, 24), Some(test_shell())).unwrap();
        terms.resize(id, TermSize::new(132, 43)).unwrap();
        assert_eq!(terms.size(id), Some(TermSize::new(132, 43)));
        let query: &[u8] = if cfg!(windows) { b"mode con\r" } else { b"stty size\r" };
        terms.input(id, query);
        let all = read_until(&terms, &out, id, Duration::from_secs(20), |all| {
            if cfg!(windows) { contains(all, "132") && contains(all, "43") } else { contains(all, "43 132") }
        });
        let text = String::from_utf8_lossy(&all);
        if cfg!(windows) {
            assert!(text.contains("132") && text.contains("43"), "output: {text}");
        } else {
            assert!(text.contains("43 132"), "output: {text}");
        }
        assert!(terms.resize(TermId(1), TermSize::default()).is_err());
    }

    #[test]
    fn reports_the_exit_code_after_the_last_output() {
        let (terms, out, exits) = manager();
        let script = if cfg!(windows) { "echo last-words& exit 7" } else { "echo last-words; exit 7" };
        let id = terms.open(&std::env::temp_dir(), TermSize::default(), Some(command_shell(script))).unwrap();
        let mut all = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let (exited, code) = loop {
            assert!(Instant::now() < deadline, "no exit notification");
            if let Ok(exit) = exits.try_recv() {
                break exit;
            }
            all.extend(read_until(&terms, &out, id, Duration::from_millis(50), |_| true));
        };
        assert_eq!(exited, id);
        assert_eq!(code, Some(7));
        // Everything the shell wrote was queued before the exit was sent.
        while let Ok((_, bytes)) = out.try_recv() {
            all.extend_from_slice(&bytes);
        }
        assert!(contains(&all, "last-words"), "output: {}", String::from_utf8_lossy(&all));
        assert!(!terms.is_open(id));
        terms.remove_exited(id);
        assert!(terms.ids().is_empty());
    }

    #[test]
    fn close_kills_the_shell_and_reports_it() {
        let (terms, _out, exits) = manager();
        let id = terms.open(&std::env::temp_dir(), TermSize::default(), Some(test_shell())).unwrap();
        assert!(terms.close(id));
        let (exited, _) = exits.recv_timeout(Duration::from_secs(10)).expect("no exit after close");
        assert_eq!(exited, id);
        assert!(!terms.close(id));
    }

    #[test]
    fn missing_cwd_is_an_error() {
        let (terms, _out, _exits) = manager();
        let missing = std::env::temp_dir().join("orbit-no-such-dir-7f3a");
        let err = terms.open(&missing, TermSize::default(), Some(test_shell())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// Writes `ORBIT_TERM_FLOOD` bytes of numbered lines to stdout when run
    /// as the child of `huge_output_is_coalesced`; does nothing otherwise.
    #[test]
    fn flood_child() {
        let Some(total) = std::env::var("ORBIT_TERM_FLOOD").ok().and_then(|v| v.parse::<usize>().ok()) else {
            return;
        };
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
        let mut written = 0;
        let mut line = 0u64;
        while written < total {
            let text = format!("line {line:08} the quick brown fox jumps over the lazy dog\r\n");
            out.write_all(text.as_bytes()).unwrap();
            written += text.len();
            line += 1;
        }
        out.write_all(b"FLOOD-DONE\r\n").unwrap();
        out.flush().unwrap();
    }

    #[test]
    fn huge_output_is_coalesced() {
        const TOTAL: usize = 16 * 1024 * 1024;
        let (terms, out, exits) = manager();
        let exe = std::env::current_exe().unwrap();
        let shell = Shell::new(exe)
            .args(["--exact", "terminal::tests::flood_child", "--nocapture", "--test-threads=1"])
            .env("ORBIT_TERM_FLOOD", TOTAL.to_string());
        let start = Instant::now();
        let id = terms.open(&std::env::temp_dir(), TermSize::new(120, 40), Some(shell)).unwrap();

        let mut bytes = 0usize;
        let mut events = 0usize;
        let mut largest = 0usize;
        let mut tail: Vec<u8> = Vec::new();
        let mut done = false;
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            match out.recv_timeout(Duration::from_millis(100)) {
                Ok((_, chunk)) => {
                    events += 1;
                    bytes += chunk.len();
                    largest = largest.max(chunk.len());
                    tail.extend_from_slice(&chunk);
                    if memchr::memmem::find(&tail, b"\x1b[6n").is_some() {
                        terms.input(id, b"\x1b[1;1R");
                    }
                    if memchr::memmem::find(&tail, b"FLOOD-DONE").is_some() {
                        done = true;
                    }
                    let keep = tail.len().saturating_sub(16);
                    tail.drain(..keep);
                }
                Err(_) => {
                    if done && exits.try_recv().is_ok() {
                        break;
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        assert!(done, "the flood never finished: {bytes} bytes in {events} events");
        assert!(largest <= MAX_CHUNK, "a chunk of {largest} bytes exceeds MAX_CHUNK");
        // Coalescing bound: one event per window plus one per full chunk.
        let windows = (elapsed.as_millis() as usize / COALESCE_WINDOW.as_millis() as usize) + 1;
        let bound = windows + bytes / MAX_CHUNK + 16;
        assert!(events <= bound, "{events} events for {bytes} bytes in {elapsed:?} (bound {bound})");
        let mbps = bytes as f64 / 1e6 / elapsed.as_secs_f64();
        eprintln!("huge output: {bytes} bytes, {events} events, largest {largest}, {elapsed:?}, {mbps:.1} MB/s");
    }
}
