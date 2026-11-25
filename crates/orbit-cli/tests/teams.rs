//! An agent team end to end: the architect is a scripted agent that drives
//! the real `orbit mcp` binary Orbit injected into its session, spawning
//! three workers (the third depends on the other two), posting a note,
//! and merging a worker once its summary arrives as team mail.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_cli::Env;
use orbit_core::engine::Launcher;
use orbit_core::provider::{AgentEvent, BlockKind, McpServer, SessionHandle, SessionInput, TurnOutcome};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use serde_json::{Value, json};

/// What the architect's MCP client saw: tool names, then each result.
static ARCHITECT: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static RELEASED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A minimal MCP client over the injected server's stdio.
struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(server: &McpServer) -> Self {
        let mut child = Process::new(&server.command)
            .args(&server.args)
            .envs(server.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self { child, stdin, stdout, next: 1 };
        mcp.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fake", "version": "1"}}));
        writeln!(mcp.stdin, "{}", json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).unwrap();
        mcp
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        writeln!(self.stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        reply["result"].clone()
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}));
        assert_eq!(result["isError"], false, "{tool}: {result}");
        result["structuredContent"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The architect plans through MCP; workers echo, or hold on `#hold:<name>`
/// until the test releases them.
fn launcher() -> Launcher {
    Box::new(|_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            let finish = |text: String| {
                emit(AgentEvent::BlockDone { block: format!("b{}", next_raw_id()), kind: BlockKind::Text, text });
                emit(AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, usage: Usage::default(), duration_ms: 5 });
            };
            emit(AgentEvent::Ready { session_id: format!("s-{thread}"), model: None });
            while let Ok(msg) = rx.recv().await {
                let SessionInput::Prompt { text, steer: false, .. } = msg else {
                    if matches!(msg, SessionInput::Shutdown) {
                        return;
                    }
                    continue;
                };
                emit(AgentEvent::TurnStarted { prompt: None });
                if text.contains("role=\"architect\"") {
                    // Plan: three workers, the third building on the first two.
                    let server = spec.mcp.first().cloned().expect("the architect's session has the Orbit MCP server");
                    let done = tokio::task::spawn_blocking(move || {
                        let mut mcp = Mcp::start(&server);
                        let tools = mcp.request("tools/list", json!({}));
                        let names: Vec<Value> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].clone()).collect();
                        let mut seen = ARCHITECT.lock().unwrap();
                        seen.push(Value::Array(names));
                        seen.push(mcp.call("team_post_note", json!({"text": "Users are {id, name}."})));
                        seen.push(mcp.call("team_spawn_worker", json!({"title": "Users API", "brief": "Add the API #hold:api", "scope": "src/api"})));
                        seen.push(mcp.call("team_spawn_worker", json!({"title": "Users page", "brief": "Add the page #hold:page", "scope": "web"})));
                        seen.push(mcp.call(
                            "team_spawn_worker",
                            json!({"title": "End-to-end test", "brief": "Test the flow", "depends_on": "w1,w2"}),
                        ));
                        seen.push(mcp.call("team_status", json!({})));
                    });
                    done.await.unwrap();
                    finish("Spawned three workers.".into());
                } else if text.starts_with("<orbit-team-mail") {
                    let server = spec.mcp.first().cloned().unwrap();
                    let merge_w1 = text.contains("w1 finished");
                    tokio::task::spawn_blocking(move || {
                        if merge_w1 {
                            let mut mcp = Mcp::start(&server);
                            let merged = mcp.call("team_merge_worker", json!({"worker": "w1"}));
                            ARCHITECT.lock().unwrap().push(merged);
                        }
                    })
                    .await
                    .unwrap();
                    finish("Read the mail.".into());
                } else if let Some(at) = text.find("#hold:") {
                    let name: String = text[at + 6..].split_whitespace().next().unwrap_or("").to_owned();
                    while !RELEASED.lock().unwrap().contains(&name) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    std::fs::write(spec.cwd.join(format!("{name}.txt")), "done\n").unwrap();
                    finish(format!("Built {name}."));
                } else {
                    finish("Tested the flow.".into());
                }
            }
        });
        SessionHandle { input }
    })
}

fn git(dir: &Path, args: &[&str]) {
    assert!(Process::new("git").args(args).current_dir(dir).output().unwrap().status.success(), "git {args:?}");
}

fn run(env: &Env, args: &[&str]) -> Value {
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).chain(["--json".to_owned()]).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = orbit_cli::run(&argv, env.clone(), &mut out, &mut err);
    let text = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "orbit {args:?}: {text} {}", String::from_utf8_lossy(&err));
    serde_json::from_str::<Value>(text.trim()).unwrap()["result"].clone()
}

#[test]
fn an_architect_spawns_three_workers_through_its_injected_mcp_tools() {
    // Orbit finds the `orbit` command line for the members' MCP server here.
    unsafe { std::env::set_var("ORBIT_CLI", env!("CARGO_BIN_EXE_orbit")) };
    let data = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.invalid"]);
    git(repo.path(), &["config", "user.name", "T"]);
    std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let engine =
        EngineHandle::start_with(EngineOptions { data_dir: data.path().to_owned(), probe_providers: false }, launcher()).unwrap();
    let endpoint = orbit_net::endpoint::start(engine.clone(), data.path(), "test").unwrap();
    let env = Env { data: Some(data.path().to_owned()), cwd: repo.path().to_owned(), ..Env::default() };
    run(&env, &["projects", "add", &repo.path().display().to_string()]);

    let architect = run(&env, &["team", "start", "Build the users feature", "--access", "auto-edit"])["thread"]
        .as_str()
        .unwrap()
        .to_owned();
    let status = |env: &Env| run(env, &["team", "status", "--as", &architect]);
    let wait = |what: &str, ok: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let s = status(&env);
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {s:#}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let state = |s: &Value, key: &str| s["workers"].as_array().unwrap().iter().find(|w| w["key"] == key).map(|w| w["state"].clone());

    let s = wait("three workers, the third waiting on the others", &|s| {
        s["workers"].as_array().unwrap().len() == 3 && state(s, "w1") == Some(json!("working")) && state(s, "w2") == Some(json!("working"))
    });
    assert_eq!(s["workers"][2]["state"], "waiting");
    assert_eq!(s["workers"][2]["waiting"], "Waiting on w1, w2");
    assert_eq!(s["workers"][2]["depends_on"], json!(["w1", "w2"]));
    assert_eq!(s["workers"][0]["scope"], json!(["src/api"]));
    assert_eq!(s["tally"], json!({"working": 2, "done": 0, "waiting": 1, "failed": 0}));
    {
        let seen = ARCHITECT.lock().unwrap();
        let names = seen[0].as_array().unwrap();
        for tool in ["team_spawn_worker", "team_message", "team_post_note", "team_read_notes", "team_status", "team_merge_worker"] {
            assert!(names.contains(&json!(tool)), "{tool} in {names:?}");
        }
        assert_eq!(seen[2]["worker"], "w1");
        assert_eq!(seen[4]["worker"], "w3");
        assert_eq!(seen[4]["state"], "waiting");
        assert_eq!(seen[5]["workers"].as_array().unwrap().len(), 3);
    }
    // Workers are the architect's children: they show up with `--parent`.
    let children = run(&env, &["threads", "--parent", &architect]);
    assert_eq!(children["threads"].as_array().unwrap().len(), 2, "w3 has no thread until it starts");
    let notes = run(&env, &["team", "notes", "--as", &architect]);
    assert_eq!(notes["notes"][0]["text"], "Users are {id, name}.");

    RELEASED.lock().unwrap().extend(["api".to_owned(), "page".to_owned()]);
    let s = wait("every worker done and w1 merged by the architect", &|s| {
        state(s, "w1") == Some(json!("merged")) && state(s, "w2") == Some(json!("done")) && state(s, "w3") == Some(json!("done"))
    });
    assert_eq!(s["workers"][2]["summary"], "Tested the flow.");
    let merged = ARCHITECT.lock().unwrap().last().cloned().unwrap();
    assert!(merged["merged"].is_string(), "{merged}");
    assert!(merged["files"].as_array().unwrap().iter().any(|f| f["path"] == "api.txt"), "{merged}");

    // The user can message a worker from the command line too.
    let sent = run(&env, &["team", "message", "w2", "Thanks!", "--as", &architect]);
    assert_eq!(sent["stored"], true);
    endpoint.close();
    engine.shutdown();
}
