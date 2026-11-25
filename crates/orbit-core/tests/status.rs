//! Status evidence end to end, with real processes: an agent dying while a
//! child holds its stdout, the hook invoker reaching the endpoint, and an
//! agent in an Orbit terminal reporting through the hooks Orbit injected.

#[path = "support/agent.rs"]
mod support;

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use orbit_core::provider::{AgentEvent, SessionInput, TurnOutcome};
use orbit_core::status::hook::{self, HookMessage};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use sonic_rs::json;
use support::*;

fn fake_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orbit-fake-agent"))
}

#[tokio::test]
async fn an_agent_that_dies_while_a_child_holds_its_stdout_still_ends_the_turn() {
    let fake = Fake::new(
        "pi",
        json!({"protocol": "pi", "handlers": {
            "get_available_models": {"models": []},
            "get_state": {"sessionFile": "/s.jsonl", "isStreaming": false},
            "get_session_stats": {}
          }, "steps": [
            {"expect": "prompt", "result": {}},
            {"send": {"type": "agent_start"}},
            // A build the agent started; it outlives the agent and keeps
            // stdout open for a minute.
            {"spawnOrphan": 60000},
            {"exit": 9, "stderr": "killed by the OOM killer"}
        ]}),
    );
    let config = ThreadConfig { provider: ProviderKind::Pi, ..Default::default() };
    let (session, rx) = fake.start(ProviderKind::Pi, fake.spec(config));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "build".into(), attachments: vec![], steer: false });
    let started = Instant::now();
    // `until` panics after 15 s; stdout stays open for 60.
    let events = until(&rx, |e| matches!(e, AgentEvent::Exited { .. })).await;
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    let failed = events.iter().find_map(|e| match e {
        AgentEvent::TurnFinished { outcome: TurnOutcome::Failed { message }, .. } => Some(message.clone()),
        _ => None,
    });
    let message = failed.expect("the turn failed");
    assert!(message.contains("OOM"), "{message}");
}

#[tokio::test]
async fn the_hook_invoker_reaches_the_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = hook::endpoint(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HookMessage>();
    hook::serve(&endpoint, move |msg| {
        let _ = tx.send(msg);
    })
    .unwrap();

    let run = |args: &[&str], stdin: &str, token: Option<&str>| {
        let mut cmd = std::process::Command::new(fake_exe());
        cmd.args(args).env("ORBIT_FAKE_HOOK", "1").env(hook::ENV_ENDPOINT, &endpoint).stdin(std::process::Stdio::piped());
        match token {
            Some(token) => cmd.env(hook::ENV_TOKEN, token),
            None => cmd.env_remove(hook::ENV_TOKEN),
        };
        let stdin = stdin.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut child = cmd.spawn().unwrap();
            child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            let status = child.wait().unwrap();
            assert!(status.success(), "the invoker must always exit 0");
        })
    };
    let next = async |rx: &mut tokio::sync::mpsc::UnboundedReceiver<HookMessage>| {
        tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("no hook call").unwrap()
    };

    // Claude Code: payload on stdin, a big tool input skipped cheaply.
    let payload = format!(
        r#"{{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash","tool_input":{{"content":"{}"}}}}"#,
        "x".repeat(300_000)
    );
    run(&["claude"], &payload, Some("tok-1")).await.unwrap();
    let msg = next(&mut rx).await;
    assert_eq!(msg.token, "tok-1");
    assert_eq!((msg.call.agent.as_str(), msg.call.event.as_str()), ("claude", "Notification"));
    assert_eq!(msg.call.kind, "permission_prompt");

    // Codex's notify: the JSON is the last argument.
    run(&["codex", r#"{"type":"agent-turn-complete","last-assistant-message":"Done"}"#], "", Some("tok-2")).await.unwrap();
    let msg = next(&mut rx).await;
    assert_eq!((msg.token.as_str(), msg.call.agent.as_str(), msg.call.event.as_str()), ("tok-2", "codex", "agent-turn-complete"));

    // Outside an Orbit terminal (no token) it does nothing, quietly.
    run(&["claude"], r#"{"hook_event_name":"Stop"}"#, None).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err());
}

#[tokio::test]
async fn the_hook_invoker_never_hangs_when_orbit_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let status = tokio::task::spawn_blocking({
        let endpoint = hook::endpoint(dir.path());
        move || {
            let mut child = std::process::Command::new(fake_exe())
                .args(["claude"])
                .env("ORBIT_FAKE_HOOK", "1")
                .env(hook::ENV_ENDPOINT, endpoint)
                .env(hook::ENV_TOKEN, "t")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"Stop"}"#).unwrap();
            child.wait().unwrap()
        }
    })
    .await
    .unwrap();
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

/// Claude Code typed by hand in an Orbit terminal reports through the hooks
/// Orbit injected into that terminal only.
#[test]
fn an_agent_typed_in_a_terminal_reports_through_injected_hooks() {
    hook::set_invoker(Some(fake_exe()));
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let launcher: orbit_core::engine::Launcher = Box::new(orbit_core::provider::start);
    let engine = EngineHandle::start_with(EngineOptions { data_dir: data.path().to_owned(), probe_providers: false }, launcher).unwrap();
    let sub = engine.subscribe(0, 0).recv().unwrap();
    let Ok(Reply::Terminal(term)) = engine.call(Command::OpenTerminal { cwd: dir.path().display().to_string(), cols: 120, rows: 30 }).recv().unwrap()
    else {
        panic!("no terminal")
    };
    let shell = orbit_core::terminal::Shell::user_default();
    let stem = shell.program().file_stem().unwrap().to_string_lossy().to_ascii_lowercase();
    let exe = fake_exe().display().to_string();
    let command = match stem.as_str() {
        "pwsh" | "powershell" => format!("& '{exe}' --pretend-claude\r"),
        "cmd" => format!("\"{exe}\" --pretend-claude\r"),
        _ => format!("'{exe}' --pretend-claude\r"),
    };
    // Afterwards: is `claude` wrapped in this terminal? The marker is built
    // at run time so the echoed command line never matches it.
    let probe = match stem.as_str() {
        "pwsh" | "powershell" => Some(" 'WR'+'AP:' + (Get-Command claude -CommandType Function -ErrorAction SilentlyContinue).Name\r"),
        "cmd" => None,
        _ => Some(" echo \"WR\"\"AP:$(command -v claude)\"\r"),
    };

    let mut output = Vec::new();
    let mut states: Vec<Option<(AgentState, Evidence)>> = Vec::new();
    let (mut typed, mut answered, mut exited, mut probed) = (false, false, false, false);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let Ok(env) = sub.events.try_recv() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        match env.event {
            Event::TerminalOutput { term: t, data } if t == term => {
                let from = output.len().saturating_sub(4);
                output.extend_from_slice(&data);
                if memchr::memmem::find(&output[from..], b"\x1b[6n").is_some() {
                    engine.send(Command::TerminalInput { term, data: b"\x1b[1;1R".to_vec() });
                }
                if !typed {
                    typed = true;
                    engine.send(Command::TerminalInput { term, data: command.clone().into_bytes() });
                }
                let text = String::from_utf8_lossy(&output);
                if !answered && text.contains("PRETEND: Do you want to proceed?") {
                    answered = true;
                    engine.send(Command::TerminalInput { term, data: b"y\r".to_vec() });
                }
                if answered && !exited && text.contains("PRETEND: done") {
                    exited = true;
                    engine.send(Command::TerminalInput { term, data: b"\r".to_vec() });
                }
                // Probe once the shell is back at its prompt.
                let prompt_back = text.split("PRETEND: bye").nth(1).is_some_and(|rest| rest.contains(['>', '$', '%', '#']));
                if let Some(probe) = probe
                    && !probed
                    && prompt_back
                    && states.last() == Some(&None)
                {
                    probed = true;
                    engine.send(Command::TerminalInput { term, data: probe.as_bytes().to_vec() });
                }
                if probed && text.split("WRAP:").nth(1).is_some_and(|rest| rest.contains('\n')) {
                    break;
                }
            }
            Event::TerminalAgent { term: t, agent } if t == term => {
                states.push(agent.map(|a| (a.state, a.evidence)));
                if states.last() == Some(&None) && exited && probe.is_none() {
                    break;
                }
            }
            _ => {}
        }
    }
    engine.call(Command::CloseTerminal { term }).recv().unwrap().unwrap();
    engine.shutdown();
    hook::set_invoker(None);
    let text = String::from_utf8_lossy(&output).into_owned();
    let hooked: Vec<AgentState> = states.iter().flatten().filter(|(_, e)| *e == Evidence::Hook).map(|(s, _)| *s).collect();
    assert!(hooked.contains(&AgentState::Working), "states {states:?}\noutput:\n{text}");
    assert!(hooked.contains(&AgentState::NeedsYou(NeedKind::Permission)), "states {states:?}");
    assert!(hooked.contains(&AgentState::Idle), "states {states:?}");
    assert_eq!(states.last(), Some(&None), "SessionEnd ends the agent: {states:?}");
    if probe.is_some() && !cfg!(target_os = "macos") {
        // (macOS login shells may reorder PATH; see docs/STATUS.md.)
        let wrapped = text.split("WRAP:").nth(1).unwrap_or_default().lines().next().unwrap_or_default().to_owned();
        assert!(wrapped.contains("claude"), "claude is not wrapped in the terminal: {wrapped:?}\noutput:\n{text}");
    }
}
