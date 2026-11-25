//! Remote hosts end to end, without a remote machine: this test binary is
//! also a fake `ssh` and a fake `scp` (it has its own `main`, see
//! `Cargo.toml`'s `harness = false`). The fakes treat a temporary folder as
//! the remote home and run the remote command on this computer, with a
//! small POSIX-shell subset (`&&`, `||`, `> file`, `~`, single quotes and
//! the `mkdir`, `chmod`, `mv`, `cat` and `uname` the client sends), so the
//! real client code (`orbit_net::remote`) drives the real `orbitd`:
//!
//! * the stdio transport round-trips through `orbitd --local --stdio`, and
//!   the host it starts outlives the connection;
//! * a thread runs on a "remote" host reached through the fake `ssh`;
//! * a host without `orbitd` is probed, installed to with the fake `scp`,
//!   and then connects;
//! * SSH failures become fixes.
//!
//! Every host runs on its own temporary data folder and is stopped by the
//! test that started it. Nothing is downloaded.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orbit_net::remote::{FailureKind, HostEntry, Platform, Probe, Remote, Tools};
use orbit_net::state::ClientState;
use orbit_net::{Backend, ConnectionStatus, Feed, RemoteClient};
use orbit_proto::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--fake-ssh") => std::process::exit(fake_ssh(&args[2..])),
        Some("--fake-scp") => std::process::exit(fake_scp(&args[2..])),
        _ => {}
    }
    // SAFETY: set before any other thread exists.
    unsafe {
        std::env::set_var("ORBIT_HOST_FAKE_AGENT_MS", "300");
        std::env::set_var("ORBIT_HOST_GRACE_MS", "400");
    }
    let filters: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with('-')).collect();
    let tests: [(&str, fn()); 4] = [
        ("stdio_round_trips_and_the_host_outlives_the_connection", stdio_round_trips_and_the_host_outlives_the_connection),
        ("a_thread_runs_on_a_host_reached_over_ssh", a_thread_runs_on_a_host_reached_over_ssh),
        ("a_host_without_orbitd_is_installed_to_with_scp", a_host_without_orbitd_is_installed_to_with_scp),
        ("ssh_failures_say_what_to_do", ssh_failures_say_what_to_do),
    ];
    let mut ran = 0;
    for (name, test) in tests {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        let started = Instant::now();
        test();
        ran += 1;
        println!("test {name} ... ok ({:?})", started.elapsed());
    }
    println!("\ntest result: ok. {ran} passed");
}

// ── The fakes ────────────────────────────────────────────────────────────

/// `--home <dir> [--uname <text>] [--fail <stderr>] <ssh options> <target> <command…>`
fn fake_ssh(args: &[String]) -> i32 {
    let mut home = PathBuf::new();
    let mut uname = "Linux x86_64".to_owned();
    let mut it = args.iter().peekable();
    let mut fail = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--home" => home = PathBuf::from(it.next().unwrap()),
            "--uname" => uname = it.next().unwrap().clone(),
            "--fail" => fail = Some(it.next().unwrap().clone()),
            "-o" | "-p" | "-i" => {
                it.next();
            }
            "-T" | "-t" | "--" => {}
            _ => break, // the target
        }
    }
    if let Some(message) = fail {
        eprintln!("{message}");
        return 255;
    }
    // ssh joins the command's words with spaces for the remote shell.
    let line = it.cloned().collect::<Vec<_>>().join(" ");
    Shell { home, uname }.run(&line)
}

/// `--home <dir> <scp options> -- <local file> <target>:<path under home>`
fn fake_scp(args: &[String]) -> i32 {
    let mut home = PathBuf::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--home" => home = PathBuf::from(it.next().unwrap()),
            "-o" | "-P" | "-i" | "--uname" | "--fail" => {
                it.next();
            }
            "--" => {}
            _ => rest.push(arg.clone()),
        }
    }
    let [from, to] = rest.as_slice() else {
        eprintln!("scp: usage");
        return 1;
    };
    let path = to.split_once(':').map(|(_, p)| p).unwrap_or(to);
    match std::fs::copy(from, home.join(path)) {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("scp: {path}: {err}");
            1
        }
    }
}

/// Enough of a POSIX shell for the commands Orbit sends.
struct Shell {
    home: PathBuf,
    uname: String,
}

impl Shell {
    fn run(&self, line: &str) -> i32 {
        let mut status = 0;
        let mut skip = false;
        let mut words = Vec::new();
        let tokens = self.tokens(line);
        let mut it = tokens.into_iter().peekable();
        loop {
            let token = it.next();
            match token.as_deref() {
                Some("&&") | Some("||") | None => {
                    if !skip {
                        status = self.simple(&words);
                    }
                    words.clear();
                    match token.as_deref() {
                        Some("&&") => skip = status != 0,
                        Some("||") => skip = status == 0,
                        _ => return status,
                    }
                }
                Some(_) => words.push(token.unwrap()),
            }
        }
    }

    /// Words, with single quotes and `~` expanded.
    fn tokens(&self, line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        let mut started = false;
        for c in line.chars() {
            match c {
                '\'' => {
                    quoted = !quoted;
                    started = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if started {
                        out.push(self.expand(std::mem::take(&mut word)));
                        started = false;
                    }
                }
                c => {
                    word.push(c);
                    started = true;
                }
            }
        }
        if started {
            out.push(self.expand(word));
        }
        out
    }

    fn expand(&self, word: String) -> String {
        match word.strip_prefix("~/") {
            Some(rest) => self.home.join(rest).display().to_string(),
            None => word,
        }
    }

    fn simple(&self, words: &[String]) -> i32 {
        let Some((program, args)) = words.split_first() else { return 0 };
        match program.as_str() {
            "mkdir" => {
                for dir in args.iter().filter(|a| !a.starts_with('-')) {
                    std::fs::create_dir_all(dir).unwrap();
                }
                0
            }
            "chmod" => 0,
            "mv" => {
                let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
                std::fs::rename(paths[0], paths[1]).map(|_| 0).unwrap_or(1)
            }
            "uname" => {
                println!("{}", self.uname);
                0
            }
            "cat" if args.first().map(String::as_str) == Some(">") => {
                let mut file = std::fs::File::create(&args[1]).unwrap();
                std::io::copy(&mut std::io::stdin(), &mut file).map(|_| 0).unwrap_or(1)
            }
            _ => self.exec(program, args),
        }
    }

    fn exec(&self, program: &str, args: &[String]) -> i32 {
        let mut path = PathBuf::from(program);
        if !path.is_absolute() {
            // Only absolute programs exist on this "remote": no orbitd on PATH.
            eprintln!("sh: {program}: command not found");
            return 127;
        }
        // Windows runs only `.exe` files; the "installed" binary has none.
        if cfg!(windows) && path.extension().is_none() && path.is_file() {
            let exe = path.with_extension("exe");
            // A running copy can't be replaced; it is the same file then.
            if std::fs::copy(&path, &exe).is_err() && !exe.is_file() {
                eprintln!("sh: {program}: cannot execute");
                return 126;
            }
            path = exe;
        }
        if !path.is_file() {
            eprintln!("sh: {program}: No such file or directory");
            return 127;
        }
        match std::process::Command::new(&path).args(args).env("ORBIT_DATA", self.home.join("orbit-data")).status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(err) => {
                eprintln!("sh: {program}: cannot execute: {err}");
                126
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn orbitd() -> PathBuf {
    env!("CARGO_BIN_EXE_orbitd").into()
}

fn tools(home: &Path, extra: &[&str]) -> Tools {
    let me = std::env::current_exe().unwrap();
    let fake = |flag: &str| -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![me.clone().into(), flag.into(), "--home".into(), home.into()];
        argv.extend(extra.iter().map(OsString::from));
        argv
    };
    Tools { ssh: fake("--fake-ssh"), scp: fake("--fake-scp"), wsl: vec!["wsl.exe".into()] }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

/// A window's side of a connection.
struct App {
    backend: Backend,
    feed: Feed,
    state: ClientState,
}

impl App {
    fn open(client: RemoteClient) -> Self {
        client.order_replies(true);
        let backend = Backend::Remote(client);
        let feed = backend.feed();
        let mut app = Self { backend, feed, state: ClientState::default() };
        app.until("connected", |s| s.connection == Some(ConnectionStatus::Connected));
        assert_eq!(app.call(Command::Host(HostCommand::AttachUi)), Ok(Reply::Done));
        app
    }

    fn until(&mut self, what: &str, done: impl Fn(&ClientState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(&self.state) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; connection {:?}", self.state.connection);
            let batch = block_on(async { tokio::time::timeout(Duration::from_millis(50), self.feed.next_batch()).await });
            match batch {
                Ok(Some(batch)) => {
                    self.state.apply(batch);
                }
                Ok(None) => panic!("the feed ended"),
                Err(_) => {}
            }
        }
    }

    fn call(&mut self, command: Command) -> Result<Reply, String> {
        let mut rx = self.backend.call(command);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(result) = rx.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "no reply");
            let batch = block_on(async { tokio::time::timeout(Duration::from_millis(10), self.feed.next_batch()).await });
            if let Ok(Some(batch)) = batch {
                self.state.apply(batch);
            }
        }
    }

    fn close(self) {
        self.backend.shutdown();
    }
}

fn host_pid(data: &Path) -> Option<u32> {
    orbit_net::endpoint::read(data).ok().flatten().map(|i| i.pid)
}

/// Stops the host serving `data`.
fn stop(data: &Path) {
    if let Some(info) = orbit_net::endpoint::read(data).ok().flatten() {
        let _ = orbit_net::host::control(&info, "restart", Some(RestartWhen::Now));
        let start = Instant::now();
        while orbit_net::endpoint::process_alive(info.pid) {
            assert!(start.elapsed() < Duration::from_secs(10), "the host didn't stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn run_turn(app: &mut App, text: &str) -> ThreadId {
    let thread = match app.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }) {
        Ok(Reply::Thread(id)) => id,
        other => panic!("{other:?}"),
    };
    app.call(Command::Send { thread, text: text.into(), attachments: vec![], steer: false }).unwrap();
    app.until("the turn to finish", |s| s.threads.get(&thread).is_some_and(|t| t.turns == 1 && !t.status.is_busy()));
    let items = match app.call(Command::ReadTimeline { id: thread, after: None, limit: 0 }) {
        Ok(Reply::Timeline { items, .. }) => items,
        other => panic!("{other:?}"),
    };
    let answer = format!("done: {text}");
    assert!(
        items.iter().any(|i| matches!(&i.body, ItemBody::Assistant { text } if *text == answer)),
        "the turn's answer is in the timeline"
    );
    thread
}

/// Puts this build's `orbitd` where the install puts it.
fn preinstall(home: &Path) {
    let bin = home.join(".orbit/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(orbitd(), bin.join("orbitd")).unwrap();
}

// ── Tests ────────────────────────────────────────────────────────────────

fn stdio_round_trips_and_the_host_outlives_the_connection() {
    let data = tempfile::tempdir().unwrap();
    let argv = [orbitd().display().to_string(), "--local".into(), "--stdio".into(), "--data".into(), data.path().display().to_string()];
    let url = orbit_net::transport::exec_url(&argv);
    let mut app = App::open(RemoteClient::connect(url.clone(), String::new()).unwrap());
    let pid = host_pid(data.path()).expect("the relay started a host");
    // Round trips through stdin/stdout, the relay and the host's pipe.
    let started = Instant::now();
    for _ in 0..20 {
        assert_eq!(app.call(Command::Host(HostCommand::Ping)), Ok(Reply::Done));
    }
    let per_call = started.elapsed() / 20;
    // A folder anywhere on the host, for the project picker.
    let listing = match app.call(Command::Host(HostCommand::ListDir { path: data.path().display().to_string() })) {
        Ok(Reply::Host(HostReply::Dir(listing))) => listing,
        other => panic!("{other:?}"),
    };
    assert!(listing.entries.iter().any(|e| e.name == "endpoint.json" && !e.is_dir), "{listing:?}");
    assert!(listing.parent.is_some());
    assert_eq!(listing.os, std::env::consts::OS);
    let thread = run_turn(&mut app, "over stdio");
    app.close();

    // The connection is gone; the host stays, and the next one attaches.
    std::thread::sleep(Duration::from_millis(200));
    assert!(orbit_net::endpoint::process_alive(pid), "the host exited with the connection");
    let app = App::open(RemoteClient::connect(url, String::new()).unwrap());
    assert_eq!(host_pid(data.path()), Some(pid), "the same host");
    assert!(app.state.threads.contains_key(&thread), "its threads are there");
    app.close();
    stop(data.path());
    eprintln!("  stdio round trip {per_call:?} per call (debug build)");
}

fn a_thread_runs_on_a_host_reached_over_ssh() {
    let home = tempfile::tempdir().unwrap();
    preinstall(home.path());
    let tools = tools(home.path(), &[]);
    let entry = HostEntry::ssh("dev@box", Some(2222), "");
    let remote = Remote::new(&entry, &tools);
    let info = match remote.probe().unwrap() {
        Probe::Ready(info) => info,
        other => panic!("{other:?}"),
    };
    assert_eq!(info.protocol, PROTOCOL_VERSION);
    let url = remote.connect_url(&info).unwrap();
    let mut app = App::open(RemoteClient::connect(url.clone(), String::new()).unwrap());
    let data = home.path().join("orbit-data");
    let pid = host_pid(&data).expect("a host runs in the remote's data folder");
    let thread = run_turn(&mut app, "on the remote");
    app.close();
    std::thread::sleep(Duration::from_millis(200));
    assert!(orbit_net::endpoint::process_alive(pid), "the remote host outlives the SSH session");
    // Reconnecting attaches to it.
    let app = App::open(RemoteClient::connect(url, String::new()).unwrap());
    assert!(app.state.threads.contains_key(&thread));
    assert_eq!(host_pid(&data), Some(pid));
    app.close();
    stop(&data);
}

fn a_host_without_orbitd_is_installed_to_with_scp() {
    let home = tempfile::tempdir().unwrap();
    let tools = tools(home.path(), &["--uname", "Linux aarch64"]);
    let entry = HostEntry::parse_ssh("box", "").unwrap();
    let remote = Remote::new(&entry, &tools);
    let probe = remote.probe().unwrap();
    assert_eq!(probe, Probe::Missing { platform: Some(Platform { os: "linux".into(), arch: "aarch64".into() }) });
    assert_eq!(probe.platform().unwrap().triple().as_deref(), Some("aarch64-unknown-linux-gnu"));
    // A file that is not orbitd is refused once copied.
    let junk = home.path().join("junk");
    std::fs::write(&junk, b"not a program").unwrap();
    assert!(remote.install(&junk).is_err());
    // This build's orbitd stands in for the right platform's.
    let info = remote.install(&orbitd()).unwrap();
    assert_eq!(info.protocol, PROTOCOL_VERSION);
    assert!(home.path().join(".orbit/bin/orbitd").is_file(), "copied with scp and moved into place");
    assert!(!home.path().join(".orbit/bin/orbitd.new").exists());
    assert!(matches!(remote.probe().unwrap(), Probe::Ready(_)));
    // And it connects.
    let app = App::open(RemoteClient::connect(remote.connect_url(&info).unwrap(), String::new()).unwrap());
    app.close();
    stop(&home.path().join("orbit-data"));
}

fn ssh_failures_say_what_to_do() {
    let home = tempfile::tempdir().unwrap();
    let entry = HostEntry::ssh("dev@box", None, "");
    let tools = tools(home.path(), &["--fail", "dev@box: Permission denied (publickey,password)."]);
    let failure = Remote::new(&entry, &tools).probe().unwrap_err();
    assert_eq!(failure.kind, FailureKind::Auth);
    assert_eq!(failure.run_once.as_deref(), Some("ssh dev@box"));
    let tools = self::tools(home.path(), &["--fail", "Host key verification failed."]);
    assert_eq!(Remote::new(&entry, &tools).probe().unwrap_err().kind, FailureKind::HostKey);
    // A missing ssh is said plainly.
    let none = Tools { ssh: vec![home.path().join("no-such-ssh").into()], ..tools };
    assert_eq!(Remote::new(&entry, &none).probe().unwrap_err().kind, FailureKind::NoTool);
    // A connection that fails shows the reason in its status, and keeps
    // trying with backoff.
    let failing = self::tools(home.path(), &["--fail", "ssh: connect to host box port 22: Connection refused"]);
    let url = Remote::new(&entry, &failing).connect_url(&orbit_net::remote::ProbeInfo { exe: "/x/orbitd".into(), ..Default::default() }).unwrap();
    let client = RemoteClient::connect(url, String::new()).unwrap();
    let backend = Backend::Remote(client);
    let mut feed = backend.feed();
    let mut state = ClientState::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "no failure status");
        if let Ok(Some(batch)) = block_on(async { tokio::time::timeout(Duration::from_millis(50), feed.next_batch()).await }) {
            state.apply(batch);
        }
        if let Some(ConnectionStatus::Disconnected { reason }) = &state.connection {
            assert!(reason.contains("refused"), "{reason}");
            break;
        }
    }
    backend.shutdown();
}
