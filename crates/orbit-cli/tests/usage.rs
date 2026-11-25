//! `orbit usage …` end to end: an engine with a fake agent on its local
//! control endpoint, driven by the CLI in-process.

use crossfire::{BlockingTxTrait, mpsc};
use orbit_cli::Env;
use orbit_core::engine::Launcher;
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use serde_json::Value;

/// Answers every prompt, reporting tokens and cost.
fn launcher() -> Launcher {
    Box::new(|_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: "fake".into(), model: None });
            while let Ok(msg) = rx.recv().await {
                match msg {
                    SessionInput::Prompt { text, steer: false, .. } => {
                        emit(AgentEvent::TurnStarted { prompt: None });
                        emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: format!("echo: {text}") });
                        emit(AgentEvent::TurnFinished {
                            outcome: TurnOutcome::Completed,
                            usage: Usage { input_tokens: 1_000, output_tokens: 200, cost_micros: 25_000, ..Default::default() },
                            duration_ms: 3,
                        });
                    }
                    SessionInput::Shutdown => return,
                    _ => {}
                }
            }
        });
        SessionHandle { input }
    })
}

struct Harness {
    engine: EngineHandle,
    endpoint: orbit_net::endpoint::LocalEndpoint,
    data: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.endpoint.close();
        self.engine.shutdown();
    }
}

impl Harness {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let engine =
            EngineHandle::start_with(EngineOptions { data_dir: data.path().to_owned(), probe_providers: false }, launcher()).unwrap();
        let endpoint = orbit_net::endpoint::start(engine.clone(), data.path(), "test").unwrap();
        Self { engine, endpoint, data }
    }

    fn run(&self, argv: &[&str]) -> (i32, String) {
        let env = Env { data: Some(self.data.path().to_owned()), cwd: self.data.path().to_owned(), ..Env::default() };
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = orbit_cli::run(&argv, env, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap() + &String::from_utf8(err).unwrap())
    }

    fn json(&self, argv: &[&str]) -> Value {
        let mut argv = argv.to_vec();
        argv.push("--json");
        let (code, out) = self.run(&argv);
        assert_eq!(code, 0, "orbit {argv:?}: {out}");
        let v: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
        v["result"].clone()
    }
}

#[test]
fn usage_shows_limits_stats_and_exports_turns() {
    let h = Harness::new();
    h.json(&["new", "count my tokens", "--wait"]);
    h.json(&["new", "and these", "--wait"]);

    let stats = h.json(&["usage", "stats"]);
    assert_eq!(stats["totals"]["turns"], 2);
    assert_eq!(stats["totals"]["tokens"], 2_400);
    assert_eq!(stats["totals"]["cost_usd"], 0.05);
    assert_eq!(stats["by"], "model");
    let by_day = h.json(&["usage", "stats", "--by", "day", "--since", "3d"]);
    assert_eq!(by_day["groups"].as_array().unwrap().len(), 4, "every day of the range: {by_day}");

    let (code, csv) = h.run(&["usage", "export", "--all"]);
    assert_eq!(code, 0, "{csv}");
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[0].starts_with("date,time,thread_id,thread,"));
    assert!(lines[1].contains(",No project,Claude Code,Claude Code,") && lines[1].ends_with(",1000,200,0,0,0.025000"), "{csv}");
    let file = h.data.path().join("usage.csv");
    let exported = h.json(&["usage", "export", "--out", file.to_str().unwrap()]);
    assert_eq!(exported["rows"], 2);
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);

    // No agent is installed here, so no account has limits to show.
    let limits = h.json(&["usage"]);
    assert_eq!(limits["accounts"].as_array().unwrap().len(), 0);
    assert_eq!(limits["settings"]["failover"], "ask");
    let (_, text) = h.run(&["usage"]);
    assert!(text.contains("When an account reaches its limit: ask."), "{text}");

    let policy = h.json(&["usage", "policy", "auto", "--alerts", "off"]);
    assert_eq!(policy["failover"], "auto");
    assert_eq!(policy["alert_80"], false);
    assert_eq!(h.json(&["usage"])["settings"]["failover"], "auto");

    // Moving a thread that is not limited is refused with the reason.
    let (code, out) = h.run(&["usage", "failover", "latest", "codex"]);
    assert_ne!(code, 0);
    assert!(out.contains("No account") || out.contains("usage limit"), "{out}");
}
