//! The background host as the app uses it: the real `orbitd --local`
//! process, started through `orbit_net::host::LocalHost`, reached over the
//! named pipe or Unix socket, with a scripted agent
//! (`ORBIT_HOST_FAKE_AGENT_MS`) so turns take a known time.
//!
//! Every host here runs on its own temporary data folder and is stopped by
//! the test that started it.

use std::path::Path;
use std::time::{Duration, Instant};

use orbit_net::host::{HostError, HostLink, LocalHost};
use orbit_net::state::ClientState;
use orbit_net::{Backend, ConnectionStatus, Feed};
use orbit_proto::*;

/// How long a fake turn streams.
const TURN: Duration = Duration::from_millis(2500);
/// How long an idle, windowless host waits before it exits.
const GRACE: Duration = Duration::from_millis(400);

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: set once, before any host is started, to the same values
        // for every test in this binary.
        unsafe {
            std::env::set_var("ORBIT_HOST_FAKE_AGENT_MS", TURN.as_millis().to_string());
            std::env::set_var("ORBIT_HOST_GRACE_MS", GRACE.as_millis().to_string());
        }
    });
}

fn binary() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_orbitd").into()
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

/// The app's side of a connection: a client, its feed and its state.
struct App {
    backend: Backend,
    feed: Feed,
    state: ClientState,
}

impl App {
    fn open(host: &LocalHost) -> Self {
        let client = host.connect().unwrap();
        client.order_replies(true);
        let backend = Backend::Remote(client);
        let feed = backend.feed();
        let mut app = Self { backend, feed, state: ClientState::default() };
        app.until("connected", |s| s.connection == Some(ConnectionStatus::Connected));
        assert_eq!(app.call(Command::Host(HostCommand::AttachUi)), Ok(Reply::Done));
        app
    }

    /// Applies updates until `done` holds.
    fn until(&mut self, what: &str, done: impl Fn(&ClientState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(&self.state) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump(Duration::from_millis(50));
        }
    }

    fn pump(&mut self, wait: Duration) -> Vec<(TermId, Vec<u8>)> {
        let batch = block_on(async { tokio::time::timeout(wait, self.feed.next_batch()).await });
        match batch {
            Ok(Some(batch)) => self.state.apply(batch).terminals,
            Ok(None) => panic!("the feed ended"),
            Err(_) => Vec::new(),
        }
    }

    fn call(&mut self, command: Command) -> Result<Reply, String> {
        let mut rx = self.backend.call(command);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(result) = rx.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "no reply");
            self.pump(Duration::from_millis(10));
        }
    }

    fn activity(&mut self) -> HostActivity {
        match self.call(Command::Host(HostCommand::Activity)) {
            Ok(Reply::Host(HostReply::Activity(a))) => a,
            other => panic!("{other:?}"),
        }
    }

    /// Output of `term` until it contains `needle`, answering the cursor
    /// query a starting ConPTY shell sends.
    fn output_until(&mut self, term: TermId, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains(needle) {
            assert!(Instant::now() < deadline, "no {needle:?} in {:?}", String::from_utf8_lossy(&seen));
            for (t, data) in self.pump(Duration::from_millis(20)) {
                if t == term {
                    if memchr_find(&data, b"\x1b[6n") {
                        self.backend.send(Command::TerminalInput { term, data: b"\x1b[1;1R".to_vec() });
                    }
                    seen.extend(data);
                }
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    fn close(self) {
        self.backend.shutdown();
    }
}

fn memchr_find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn host_pid(dir: &Path) -> Option<u32> {
    orbit_net::endpoint::read(dir).ok().flatten().filter(|i| i.kind == "host").map(|i| i.pid)
}

/// Waits until the host serving `dir` has exited.
fn wait_exit(pid: u32, within: Duration) -> Duration {
    let start = Instant::now();
    while orbit_net::endpoint::process_alive(pid) {
        assert!(start.elapsed() < within, "the host (pid {pid}) is still running after {within:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    start.elapsed()
}

/// Stops a host a test left running.
fn stop(dir: &Path) {
    if let Some(info) = orbit_net::endpoint::read(dir).ok().flatten() {
        let _ = orbit_net::host::control(&info, "restart", Some(RestartWhen::Now));
        wait_exit(info.pid, Duration::from_secs(10));
    }
}

fn new_thread(app: &mut App) -> ThreadId {
    match app.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }) {
        Ok(Reply::Thread(id)) => id,
        other => panic!("{other:?}"),
    }
}

fn attach(app: &mut App, thread: ThreadId, cwd: &Path) -> (TermSession, Vec<u8>) {
    let command = HostCommand::AttachTerminal { thread, cwd: cwd.display().to_string(), cols: 100, rows: 30, wake: true };
    match app.call(Command::Host(command)) {
        Ok(Reply::Host(HostReply::Terminal { session, scrollback })) => (session, scrollback),
        other => panic!("{other:?}"),
    }
}

#[test]
fn agents_and_terminals_survive_the_app_closing() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let host = LocalHost::new(dir.path().to_owned(), Some(binary()));
    let started = Instant::now();
    host.start().unwrap();
    let start_ms = started.elapsed();
    let pid = host_pid(dir.path()).expect("a host runs");
    assert_ne!(pid, std::process::id());

    let mut app = App::open(&host);
    let thread = new_thread(&mut app);
    app.call(Command::Send { thread, text: "build it".into(), attachments: vec![], steer: false }).unwrap();
    app.until("the turn to start", |s| s.threads.get(&thread).is_some_and(|t| t.status.is_busy()));
    let (session, _) = attach(&mut app, thread, work.path());
    let term = session.term;
    app.call(Command::Host(HostCommand::TerminalReady { term })).unwrap();
    app.backend.send(Command::TerminalInput { term, data: b"echo kept-across-close\r".to_vec() });
    app.output_until(term, "kept-across-close");

    // The app closes mid-turn: the host keeps working.
    app.close();
    std::thread::sleep(GRACE * 2);
    assert!(orbit_net::endpoint::process_alive(pid), "the host exited while an agent was working");

    // The app comes back: the same host, the turn finished meanwhile, and
    // the terminal is there with what it showed.
    let mut app = App::open(&host);
    assert_eq!(host_pid(dir.path()), Some(pid));
    app.until("the turn to finish while the app was closed", |s| {
        s.threads.get(&thread).is_some_and(|t| t.turns == 1 && !t.status.is_busy())
    });
    let (again, scrollback) = attach(&mut app, thread, work.path());
    assert_eq!(again.term, term);
    assert!(String::from_utf8_lossy(&scrollback).contains("kept-across-close"), "scrollback: {:?}", String::from_utf8_lossy(&scrollback));
    app.call(Command::Host(HostCommand::TerminalReady { term })).unwrap();
    app.backend.send(Command::TerminalInput { term, data: b"echo still-alive\r".to_vec() });
    app.output_until(term, "still-alive");
    let timeline = match app.call(Command::ReadTimeline { id: thread, after: None, limit: 0 }) {
        Ok(Reply::Timeline { items, .. }) => items,
        other => panic!("{other:?}"),
    };
    assert!(
        timeline.iter().any(|i| matches!(&i.body, ItemBody::Assistant { text } if text == "done: build it")),
        "the turn's answer is in the timeline"
    );

    // Closed again with nothing working (an idle shell is not work): the
    // host exits after the grace period.
    app.close();
    let took = wait_exit(pid, Duration::from_secs(10));
    assert!(host_pid(dir.path()).is_none(), "the endpoint is removed");
    eprintln!("host start {start_ms:?}; exit {took:?} after the last window closed (grace {GRACE:?})");
}

#[test]
fn an_idle_host_exits_after_the_grace_period_and_stays_while_a_turn_runs() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let host = LocalHost::new(dir.path().to_owned(), Some(binary()));
    host.start().unwrap();
    let pid = host_pid(dir.path()).unwrap();

    // A turn runs with no window attached: the host stays until it ends.
    let mut app = App::open(&host);
    let thread = new_thread(&mut app);
    app.call(Command::Send { thread, text: "work".into(), attachments: vec![], steer: false }).unwrap();
    app.until("the turn to start", |s| s.threads.get(&thread).is_some_and(|t| t.status.is_busy()));
    let a = app.activity();
    assert_eq!((a.pid, a.windows, a.working.len()), (pid, 1, 1));
    let closed = Instant::now();
    app.close();
    let took = wait_exit(pid, TURN + GRACE + Duration::from_secs(10));
    let total = closed.elapsed();
    assert!(took >= GRACE, "the host left before the turn ended ({took:?})");
    eprintln!("exited {total:?} after the window closed, mid-turn (turn {TURN:?}, grace {GRACE:?})");

    // With nothing to do, it goes after the grace period.
    let host = LocalHost::new(dir.path().to_owned(), Some(binary()));
    host.start().unwrap();
    let pid = host_pid(dir.path()).unwrap();
    let app = App::open(&host);
    app.close();
    let took = wait_exit(pid, Duration::from_secs(10));
    assert!(took >= GRACE / 2, "it waited for the grace period ({took:?})");
    // With `always`, it stays.
    orbit_net::host::write_policy(dir.path(), KeepRunning::Always).unwrap();
    host.start().unwrap();
    let pid = host_pid(dir.path()).unwrap();
    App::open(&host).close();
    std::thread::sleep(GRACE * 3);
    assert!(orbit_net::endpoint::process_alive(pid), "`always` keeps the host running");
    stop(dir.path());
}

#[test]
fn a_restart_request_brings_up_a_new_host_the_app_reconnects_to() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let host = LocalHost::new(dir.path().to_owned(), Some(binary()));
    host.start().unwrap();
    let first = host_pid(dir.path()).unwrap();
    let mut app = App::open(&host);
    let thread = new_thread(&mut app);
    app.call(Command::Send { thread, text: "slowly".into(), attachments: vec![], steer: false }).unwrap();
    app.until("the turn to start", |s| s.threads.get(&thread).is_some_and(|t| t.status.is_busy()));

    // The updater asks for a restart once the work is done: the host waits
    // for the turn.
    app.call(Command::Host(HostCommand::Restart { when: RestartWhen::WhenIdle })).unwrap();
    assert!(app.activity().restart_pending);
    std::thread::sleep(GRACE);
    assert!(orbit_net::endpoint::process_alive(first), "it restarted while an agent was working");
    wait_exit(first, TURN + Duration::from_secs(10));

    // The app starts the new host and catches up from a fresh snapshot.
    app.until("the connection to drop", |s| s.connection != Some(ConnectionStatus::Connected));
    app.until("the new host", |s| s.connection == Some(ConnectionStatus::Connected) && s.threads.contains_key(&thread));
    let second = host_pid(dir.path()).expect("a new host");
    assert_ne!(second, first);
    assert_eq!(app.call(Command::Host(HostCommand::AttachUi)), Ok(Reply::Done));
    assert!(app.state.threads[&thread].turns == 1, "the turn finished before the restart");
    app.close();
    stop(dir.path());
}

#[test]
fn an_app_of_another_protocol_asks_the_host_to_restart() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let host = LocalHost::new(dir.path().to_owned(), Some(binary()));
    host.start().unwrap();
    let old = host_pid(dir.path()).unwrap();
    // Make the running host look like one from an older Orbit.
    let path = dir.path().join(orbit_net::endpoint::FILE);
    let text = std::fs::read_to_string(&path).unwrap();
    let older = text.replace(&format!("\"protocol\":{PROTOCOL_VERSION}"), "\"protocol\":9");
    assert_ne!(older, text);
    std::fs::write(&path, older).unwrap();

    // A new app cannot talk to it, but can ask what it is doing…
    let newer = LocalHost::new(dir.path().to_owned(), Some(binary()));
    newer.start().unwrap();
    let HostLink::Outdated { status: Some(status), .. } = newer.link() else { panic!("{:?}", newer.link()) };
    assert_eq!(status.pid, old);
    assert_eq!(status.summary(), None, "nothing is working");
    let mut child = None;
    assert!(matches!(orbit_net::host::ensure_running(dir.path(), Some(&binary()), &mut child), Err(HostError::Outdated(_))));

    // …and restart it; the next connection starts the new one.
    newer.restart_outdated(RestartWhen::Now).unwrap();
    wait_exit(old, Duration::from_secs(10));
    let mut app = App::open(&newer);
    assert!(matches!(newer.link(), HostLink::Ready { .. }));
    assert_ne!(host_pid(dir.path()), Some(old));
    assert_eq!(app.activity().protocol, PROTOCOL_VERSION);
    app.close();
    stop(dir.path());
}

#[test]
fn without_the_host_binary_the_engine_runs_in_the_app() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-orbitd");
    let chosen = orbit_net::host::local_backend(dir.path().to_owned(), None, || {
        orbit_core::EngineHandle::start_with(
            orbit_core::EngineOptions { data_dir: dir.path().to_owned(), probe_providers: false },
            Box::new(|_, _, _| {
                let (input, _rx) = crossfire::mpsc::bounded_blocking(1);
                orbit_core::provider::SessionHandle { input }
            }),
        )
    })
    .unwrap();
    assert!(matches!(chosen.backend, Backend::Local(_)));
    assert!(chosen.host.is_none());
    let reason = chosen.fallback.expect("a visible reason");
    assert!(reason.contains("not installed"), "{reason}");
    chosen.backend.shutdown();

    // A binary that cannot run is a fallback too, not a hang.
    std::fs::write(&missing, b"not a program").unwrap();
    let host = LocalHost::new(dir.path().to_owned(), Some(missing));
    assert!(matches!(host.start(), Err(HostError::Spawn(_) | HostError::Exited(_))));
}
