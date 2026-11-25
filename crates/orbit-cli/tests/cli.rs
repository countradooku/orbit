//! End to end: an engine with a scripted fake agent, served on its local
//! control endpoint, driven by the CLI in-process, through the built
//! `orbit` binary, and over MCP.

use std::path::Path;
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_cli::Env;
use orbit_core::engine::Launcher;
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use serde_json::{Value, json};

/// Environment each fake session was started with, by thread.
type Seen = Arc<Mutex<Vec<(ThreadId, Vec<(String, String)>)>>>;

/// `ask` raises an approval, `question` asks a question, `slow` works until
/// stopped; anything else is answered `echo: <text>`.
fn fake_launcher(seen: Seen) -> Launcher {
    Box::new(move |_kind, spec, events| {
        seen.lock().unwrap().push((spec.thread, spec.env.clone()));
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            let finish = |emit: &dyn Fn(AgentEvent)| {
                emit(AgentEvent::TurnFinished {
                    outcome: TurnOutcome::Completed,
                    usage: Usage { input_tokens: 10, output_tokens: 5, ..Default::default() },
                    duration_ms: 3,
                })
            };
            emit(AgentEvent::Ready { session_id: "fake".into(), model: None });
            while let Ok(msg) = rx.recv().await {
                let SessionInput::Prompt { text, steer: false, .. } = msg else {
                    if matches!(msg, SessionInput::Shutdown) {
                        return;
                    }
                    continue;
                };
                emit(AgentEvent::TurnStarted { prompt: None });
                let kind = match text.as_str() {
                    "ask" => Some(RequestKind::Command { command: "cargo test".into(), cwd: ".".into(), reason: String::new() }),
                    "question" => Some(RequestKind::Question {
                        question: "Which database?".into(),
                        options: vec!["SQLite".into(), "Postgres".into()],
                        multi_select: false,
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    emit(AgentEvent::Request { provider_ref: "r1".into(), kind });
                    while let Ok(msg) = rx.recv().await {
                        if let SessionInput::Resolve { decision, .. } = msg {
                            emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: format!("decision: {decision:?}") });
                            break;
                        }
                    }
                    finish(&emit);
                } else if text == "slow" {
                    emit(AgentEvent::Delta { block: "b".into(), kind: BlockKind::Text, text: "working".into() });
                    while let Ok(msg) = rx.recv().await {
                        if matches!(msg, SessionInput::Interrupt) {
                            emit(AgentEvent::TurnFinished { outcome: TurnOutcome::Interrupted, usage: Usage::default(), duration_ms: 1 });
                            break;
                        }
                    }
                } else {
                    for piece in ["echo", ": ", &text] {
                        emit(AgentEvent::Delta { block: "b".into(), kind: BlockKind::Text, text: piece.to_owned() });
                    }
                    emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: format!("echo: {text}") });
                    finish(&emit);
                }
            }
        });
        SessionHandle { input }
    })
}

struct Harness {
    engine: EngineHandle,
    endpoint: orbit_net::endpoint::LocalEndpoint,
    seen: Seen,
    data: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.endpoint.close();
        self.engine.shutdown();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Process::new("git").args(args).current_dir(dir).output().unwrap().status.success();
    assert!(ok, "git {args:?}");
}

impl Harness {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "t@example.invalid"]);
        git(repo.path(), &["config", "user.name", "T"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-q", "-m", "init"]);
        let seen = Seen::default();
        let engine = EngineHandle::start_with(
            EngineOptions { data_dir: data.path().to_owned(), probe_providers: false },
            fake_launcher(seen.clone()),
        )
        .unwrap();
        let endpoint = orbit_net::endpoint::start(engine.clone(), data.path(), "test").unwrap();
        Self { engine, endpoint, seen, data, repo }
    }

    fn env(&self) -> Env {
        Env { data: Some(self.data.path().to_owned()), cwd: self.repo.path().to_owned(), ..Env::default() }
    }

    /// Runs the CLI in-process.
    fn run_with(&self, env: Env, args: &str) -> (i32, String, String) {
        let argv = split(args);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = orbit_cli::run(&argv, env, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn run(&self, args: &str) -> (i32, String, String) {
        self.run_with(self.env(), args)
    }

    /// Runs and parses `--json` output; asserts the exit code.
    fn json(&self, args: &str, code: i32) -> Value {
        let (got, out, err) = self.run(&format!("{args} --json"));
        assert_eq!(got, code, "orbit {args}\nstdout: {out}\nstderr: {err}");
        let v: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
        if code == 0 || v["ok"] == true { v["result"].clone() } else { v["error"].clone() }
    }

    fn add_project(&self) -> String {
        let r = self.json(&format!("projects add {}", quote(self.repo.path())), 0);
        r["project"]["id"].as_str().unwrap().to_owned()
    }
}

fn quote(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

/// Splits on spaces, keeping "double-quoted" runs together.
fn split(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

#[test]
fn threads_flow_from_new_to_wait_log_and_diff() {
    let h = Harness::new();
    let project = h.add_project();
    assert_eq!(h.json("projects", 0)["projects"][0]["id"], project.as_str());

    // `new --wait` returns the agent's answer.
    let r = h.json("new \"hello there\" --wait --title Greeter", 0);
    let id = r["thread"].as_str().unwrap().to_owned();
    assert_eq!(r["threads"][0]["state"], "idle");
    assert_eq!(r["threads"][0]["last_message"], "echo: hello there");
    let thread = &h.json("threads", 0)["threads"][0];
    assert_eq!(thread["title"], "Greeter");
    assert_eq!(thread["project"]["id"], project.as_str(), "project from the current folder");

    // The engine told the agent who it is.
    let seen = h.seen.lock().unwrap().clone();
    let env = &seen.iter().find(|(t, _)| t.to_string() == id).unwrap().1;
    assert!(env.contains(&("ORBIT_THREAD".to_owned(), id.clone())));
    assert!(env.iter().any(|(k, v)| k == "ORBIT_DATA" && Path::new(v) == h.data.path()));

    // send + wait by title, log, status.
    assert_eq!(h.run("send Greeter again").0, 0);
    let w = h.json(&format!("wait {id}"), 0);
    assert_eq!(w["threads"][0]["last_message"], "echo: again");
    let log = h.json(&format!("log {id}"), 0);
    let kinds: Vec<&str> = log["items"].as_array().unwrap().iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user", "assistant", "turn_end", "user", "assistant", "turn_end"]);
    assert_eq!(log["thread"]["unread"], true, "reading through the CLI leaves it unread");
    let (code, text, _) = h.run(&format!("log {id} --limit 2"));
    assert_eq!(code, 0);
    assert!(text.contains("echo: again") && !text.contains("hello there"), "{text}");
    let status = h.json(&format!("status {id}"), 0);
    assert_eq!(status["turns"], 2);
    assert_eq!(status["last_message"], "echo: again");
    let overview = h.json("status", 0);
    assert_eq!(overview["counts"]["idle"], 1);
    assert_eq!(overview["engine"]["kind"], "test");

    // Search finds it; diff runs (no changes by the fake agent).
    let hits = h.json("search again", 0);
    assert_eq!(hits["hits"][0]["thread"], id.as_str());
    assert_eq!(h.json(&format!("diff {id} --stat"), 0)["files"], json!([]));
    assert_eq!(h.json(&format!("diff {id} --since-turn 1 --stat"), 0)["scope"], "since turn 1");
    let digest = h.json(&format!("export {id}"), 0);
    assert!(digest["digest"].as_str().unwrap().contains("hello there"), "{digest}");
    let files = h.json("files READ", 0);
    assert_eq!(files["files"][0]["path"], "README.md");
    h.json(&format!("draft {id} --text \"half a thought\""), 0);

    // settle hides it from the default list; --all shows it.
    h.json(&format!("settle {id}"), 0);
    assert_eq!(h.json("threads", 0)["total"], 0);
    assert_eq!(h.json("threads --all", 0)["total"], 1);
    h.json(&format!("archive {id}"), 0);
    let e = h.json(&format!("send {id} nope"), 1);
    assert!(e["message"].as_str().unwrap().contains("archived"), "{e}");
}

#[test]
fn requests_are_answered_and_wait_reports_needs_you() {
    let h = Harness::new();
    let r = h.json("new ask", 0);
    let id = r["thread"].as_str().unwrap().to_owned();
    assert_eq!(r["details"]["title"], "ask");
    // wait stops with 6 and shows the request.
    let w = h.json(&format!("wait {id} --timeout 10s"), 6);
    assert_eq!(w["threads"][0]["state"], "needs-you");
    assert_eq!(w["threads"][0]["pending"][0]["kind"], "command");
    assert_eq!(w["threads"][0]["result"], "needs_you");
    assert_eq!(h.json("threads --needs-you", 0)["total"], 1);
    // `answer` refuses an approval; `approve` takes it.
    let e = h.json(&format!("answer {id} yes"), 4);
    assert_eq!(e["code"], "not_found");
    let a = h.json(&format!("approve {id}"), 0);
    assert_eq!(a["decision"]["decision"], "allow");
    let w = h.json(&format!("wait {id}"), 0);
    assert_eq!(w["threads"][0]["last_message"], "decision: Allow");

    // Questions take answers.
    h.json(&format!("send {id} question"), 0);
    h.json(&format!("wait {id} --until needs-you"), 0);
    h.json(&format!("answer {id} SQLite"), 0);
    let w = h.json(&format!("wait {id}"), 0);
    assert!(w["threads"][0]["last_message"].as_str().unwrap().contains("SQLite"));
    // Nothing pending now.
    assert_eq!(h.json(&format!("deny {id}"), 4)["code"], "not_found");
}

#[test]
fn stop_timeout_and_selectors() {
    let h = Harness::new();
    let id = h.json("new slow", 0)["thread"].as_str().unwrap().to_owned();
    let started = Instant::now();
    let w = h.json(&format!("wait {id} --timeout 1s"), 5);
    assert_eq!(w["timed_out"], true);
    assert!(started.elapsed() < Duration::from_secs(5));
    // --until idle keeps waiting; stop ends the turn.
    h.json(&format!("stop {id}"), 0);
    assert_eq!(h.json(&format!("wait {id} --until idle --timeout 10s"), 0)["threads"][0]["state"], "idle");
    // needs-you on a thread that finished without asking: 8.
    assert_eq!(h.json(&format!("wait {id} --until needs-you --timeout 10s"), 8)["threads"][0]["result"], "idle");
    // Unknown and ambiguous selectors.
    assert_eq!(h.json("status nothing-like-this", 4)["code"], "not_found");
    h.json("new slow", 0);
    let e = h.json("stop slow", 4);
    assert!(e["message"].as_str().unwrap().contains("matches 2 threads"), "{e}");
    // Bad arguments are usage errors, in JSON too.
    assert_eq!(h.json("threads --status asleep", 2)["code"], "usage");
}

#[test]
fn worktree_children_are_linked_to_their_parent() {
    let h = Harness::new();
    h.add_project();
    let parent = h.json("new \"lead the work\" --wait", 0)["thread"].as_str().unwrap().to_owned();
    // A thread started from inside the parent (ORBIT_THREAD) is its child.
    let mut env = h.env();
    env.thread = Some(parent.clone());
    let (code, out, err) = h.run_with(env.clone(), "new \"do part one\" --worktree --wait --json");
    assert_eq!(code, 0, "{out}{err}");
    let child: Value = serde_json::from_str(out.trim()).unwrap();
    let child_id = child["result"]["thread"].as_str().unwrap().to_owned();
    assert_eq!(child["result"]["threads"][0]["last_message"], "echo: do part one");
    let status = h.json(&format!("status {child_id}"), 0);
    assert_eq!(status["parent"], parent.as_str());
    let worktree = status["worktree"].as_str().expect("a worktree");
    assert!(Path::new(worktree).is_dir());
    assert!(status["branch"].as_str().unwrap().starts_with("orbit/"));
    assert_eq!(h.json(&format!("threads --parent {parent}"), 0)["total"], 1);
    let (_, out, _) = h.run_with(env, "threads --parent self --json");
    assert!(out.contains(&child_id));
    // A dry-run merge previews without changing anything.
    let preview = h.json(&format!("merge {child_id} --dry-run --into main"), 0);
    assert_eq!((preview["target"].as_str(), preview["dry_run"].as_bool()), (Some("main"), Some(true)));
    // The parent's detail lists its children.
    assert_eq!(h.json(&format!("status {parent}"), 0)["children"][0]["id"], child_id.as_str());
}

#[test]
fn terminal_run_streams_output_and_passes_the_exit_code() {
    let h = Harness::new();
    let (code, out, err) = h.run("terminal run echo orbit-terminal-ok");
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("orbit-terminal-ok"), "{out:?}");
    // `exit 3` means the same in PowerShell, cmd and POSIX shells.
    assert_eq!(h.run("terminal run \"exit 3\"").0, 3);
    // Flags after the command belong to the command, so --json goes first.
    let (code, out, _) = h.run("--json terminal run --detach echo later");
    assert_eq!(code, 0);
    let r = serde_json::from_str::<Value>(&out).unwrap()["result"].clone();
    let term = r["terminal"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = h.json(&format!("terminal read {term}"), 0);
        if read["output"].as_str().unwrap().contains("later") {
            break;
        }
        assert!(Instant::now() < deadline, "{read}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(h.json("terminal list", 0)["terminals"].as_array().unwrap().iter().any(|t| t["id"] == term.as_str()));
}

#[test]
fn open_asks_the_desktop_to_focus() {
    let h = Harness::new();
    let id = h.json("new hi", 0)["thread"].as_str().unwrap().to_owned();
    let sub = h.engine.subscribe(0, 0).recv().unwrap();
    assert_eq!(h.run(&format!("open {id}")).0, 0);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), sub.events.recv()).await.expect("in time").unwrap();
            if let Event::FocusRequested { thread } = env.event {
                assert_eq!(thread.map(|t| t.to_string()), Some(id));
                break;
            }
        }
    });
}

#[test]
fn no_engine_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env { data: Some(dir.path().to_owned()), ..Env::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = orbit_cli::run(&split("threads"), env, &mut out, &mut err);
    assert_eq!(code, 3);
    assert!(String::from_utf8(err).unwrap().contains("Orbit isn’t running"));
}

// ── Through the built binary ────────────────────────────────────────────

fn binary() -> Process {
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_orbit"));
    for var in ["ORBIT_HOST", "ORBIT_TOKEN", "ORBIT_THREAD", "ORBIT_PARENT_THREAD"] {
        cmd.env_remove(var);
    }
    cmd
}

#[test]
fn the_binary_talks_to_the_endpoint_quickly() {
    let h = Harness::new();
    h.json("new \"from the binary test\" --title \"from the binary test\" --wait", 0);
    let mut times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let out = binary().args(["threads", "--json"]).env("ORBIT_DATA", h.data.path()).output().unwrap();
        times.push(started.elapsed());
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["result"]["threads"][0]["title"], "from the binary test");
    }
    times.sort();
    eprintln!("orbit threads --json: median {:?} (min {:?})", times[2], times[0]);
    let out = binary().args(["status", "latest"]).env("ORBIT_DATA", h.data.path()).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("from the binary test"));
    // Usage errors exit 2 with clap's message on stderr.
    let out = binary().args(["threads", "--status", "asleep"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    // Self-description needs no engine.
    let out = binary().args(["help", "--json"]).env("ORBIT_DATA", h.data.path()).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["result"]["commands"].as_array().unwrap().len() > 20);
}

/// One JSON-RPC exchange over the binary's pipes.
struct McpProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl McpProcess {
    fn start(data: &Path, thread: Option<&str>) -> Self {
        let mut cmd = binary();
        cmd.arg("mcp").env("ORBIT_DATA", data).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        if let Some(thread) = thread {
            cmd.env("ORBIT_THREAD", thread);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    fn send(&mut self, msg: Value) {
        use std::io::Write;
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        use std::io::BufRead;
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    fn call(&mut self, id: u64, tool: &str, args: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}}));
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply["result"].clone()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_over_the_binary_pipes() {
    let h = Harness::new();
    h.add_project();
    let parent = h.json("new \"I lead\" --access ask --wait", 0)["thread"].as_str().unwrap().to_owned();
    let mut mcp = McpProcess::start(h.data.path(), Some(&parent));

    mcp.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}
    }}));
    let init = mcp.recv();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "orbit");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = mcp.recv();
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["list_threads", "start_thread", "send_message", "wait", "read_thread", "get_diff", "approve_request"] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }

    // A child in a worktree, linked to the caller, with access capped.
    let started = mcp.call(3, "start_thread", json!({"prompt": "part two", "worktree": true, "access": "full"}));
    assert_eq!(started["isError"], false, "{started}");
    let child = started["structuredContent"]["thread"].as_str().unwrap().to_owned();
    assert_eq!(started["structuredContent"]["access_capped"], true);
    let waited = mcp.call(4, "wait", json!({"threads": [child.clone()], "timeout": 30}));
    assert_eq!(waited["structuredContent"]["threads"][0]["last_message"], "echo: part two", "{waited}");
    let listed = mcp.call(5, "list_threads", json!({"parent": "self"}));
    assert_eq!(listed["structuredContent"]["threads"][0]["id"], child.as_str());
    assert_eq!(listed["structuredContent"]["threads"][0]["access"], "ask");
    assert_eq!(listed["structuredContent"]["threads"][0]["parent"], parent.as_str());
    let read = mcp.call(6, "read_thread", json!({"thread": child.clone()}));
    assert_eq!(read["structuredContent"]["items"][1]["text"], "echo: part two");
    // Text content mirrors the structured result.
    let text: Value = serde_json::from_str(read["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, read["structuredContent"]);

    // Agents may not stop the user's other threads.
    let other = h.json("new unrelated", 0)["thread"].as_str().unwrap().to_owned();
    let refused = mcp.call(7, "stop_thread", json!({"thread": other}));
    assert_eq!(refused["isError"], true);
    // Bad arguments are tool errors with the reason.
    let bad = mcp.call(8, "list_threads", json!({"status": "asleep"}));
    assert_eq!(bad["isError"], true);
    assert!(bad["content"][0]["text"].as_str().unwrap().contains("status"));
    // Unknown methods and tools are protocol errors.
    mcp.send(json!({"jsonrpc": "2.0", "id": 9, "method": "nope"}));
    assert_eq!(mcp.recv()["error"]["code"], -32601);
    mcp.send(json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {"name": "nope"}}));
    assert_eq!(mcp.recv()["error"]["code"], -32602);
    mcp.send(json!({"jsonrpc": "2.0", "id": 11, "method": "ping"}));
    assert_eq!(mcp.recv()["result"], json!({}));
}

#[test]
fn mcp_in_process_runs_calls_concurrently() {
    let h = Harness::new();
    let slow = h.json("new slow", 0)["thread"].as_str().unwrap().to_owned();
    let env = h.env();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let ctx = orbit_cli::exec::Ctx::new(env, Default::default(), orbit_cli::exec::Origin::Tool);
        tokio::task::spawn_local(orbit_cli::mcp::serve(ctx, server_read, server_write));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        let mut send = async |v: Value| {
            client_write.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        };
        // A long wait, then a quick list: the list must answer first.
        send(json!({"jsonrpc": "2.0", "id": "w", "method": "tools/call", "params": {"name": "wait", "arguments": {"threads": [slow], "timeout": "3s"}}})).await;
        send(json!({"jsonrpc": "2.0", "id": "l", "method": "tools/call", "params": {"name": "list_threads", "arguments": {}}})).await;
        let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], "l");
        let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second["id"], "w");
        assert_eq!(second["result"]["structuredContent"]["timed_out"], true);
        assert_eq!(second["result"]["structuredContent"]["exit"], 5);
        // Garbage is a parse error with a null id.
        send(json!("not an object")).await;
        let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["error"]["code"], -32600);
        client_write.write_all(b"{oops\n").await.unwrap();
        let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((reply["error"]["code"].clone(), reply["id"].clone()), (json!(-32700), Value::Null));
    });
}
