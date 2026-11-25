//! Codex adapter against a scripted fake `codex app-server`
//! (`tests/support/fake_codex.rs`). Each test copies the fake into its own
//! temp dir as `codex[.exe]` with a script, drives the adapter through the
//! public seam, and checks both the events and what the adapter sent.

use std::path::PathBuf;
use std::time::Duration;

use orbit_core::provider::{self, AgentEvent, BlockKind, SessionHandle, SessionInput, SessionSpec, TurnOutcome};
use orbit_proto::*;
use sonic_rs::{JsonValueMutTrait, JsonValueTrait, Value, json};

type Events = crossfire::AsyncRx<crossfire::mpsc::List<(ThreadId, AgentEvent)>>;

const WAIT: Duration = Duration::from_secs(15);

struct Fake {
    dir: tempfile::TempDir,
    exe: PathBuf,
}

impl Fake {
    fn new(runs: Vec<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) { "codex.exe" } else { "codex" });
        std::fs::copy(env!("CARGO_BIN_EXE_orbit-fake-codex"), &exe).unwrap();
        std::fs::write(dir.path().join("fake-codex.json"), sonic_rs::to_string(&json!({"runs": runs})).unwrap()).unwrap();
        Self { dir, exe }
    }

    /// Everything the adapter sent to run `run`, in order.
    fn received(&self, run: usize) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.path().join(format!("fake-codex.run{run}.jsonl"))).unwrap_or_default();
        text.lines().map(|l| sonic_rs::from_str(l).unwrap()).collect()
    }

    /// Params of every request named `method` sent to run `run`.
    fn requests(&self, run: usize, method: &str) -> Vec<Value> {
        self.received(run)
            .into_iter()
            .filter(|m| m.get("method").and_then(|v| v.as_str()) == Some(method))
            .map(|m| m.get("params").cloned().unwrap_or_default())
            .collect()
    }

    /// The result the adapter sent for server request `id`.
    fn response(&self, run: usize, id: u64) -> Option<Value> {
        self.received(run)
            .into_iter()
            .find(|m| m.get("method").is_none() && m.get("id").and_then(|v| v.as_u64()) == Some(id))
            .and_then(|m| m.get("result").cloned())
    }

    fn spec(&self, config: ThreadConfig) -> SessionSpec {
        SessionSpec {
            thread: ThreadId::new(),
            cwd: self.dir.path().to_owned(),
            config,
            resume: None,
            resume_at: None,
            binary: Some(self.exe.clone()),
            args: Vec::new(),
            env: vec![("ORBIT_PORT_BASE".into(), "41000".into())],
            extra_dirs: Vec::new(),
            guard: Default::default(),
            mcp: Vec::new(),
        }
    }

    fn start(&self, spec: SessionSpec) -> (SessionHandle, Events) {
        let (tx, rx) = crossfire::mpsc::unbounded_async();
        (provider::start(ProviderKind::Codex, spec, tx), rx)
    }
}

fn codex_config() -> ThreadConfig {
    ThreadConfig {
        provider: ProviderKind::Codex,
        options: vec![OptionValue { id: "effort".into(), value: OptionSetting::Choice("high".into()) }],
        ..Default::default()
    }
}

fn model(slug: &str, name: &str, default: bool) -> Value {
    json!({
        "id": slug, "model": slug, "displayName": name, "description": format!("{name} description"),
        "hidden": false, "isDefault": default, "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": [
            {"reasoningEffort": "low", "description": "Fastest"},
            {"reasoningEffort": "medium", "description": "Balanced"},
            {"reasoningEffort": "high", "description": "Thorough"},
            {"reasoningEffort": "xhigh", "description": "Most thorough"}
        ],
        "serviceTiers": [{"id": "fast", "name": "Fast", "description": "Lower latency, more credits"}],
        "inputModalities": ["text", "image"], "upgrade": null
    })
}

/// Answers for every setup request a session makes.
fn handlers() -> Value {
    json!({
        "initialize": {"codexHome": "/home/u/.codex", "platformFamily": "windows", "platformOs": "windows",
                       "userAgent": "codex_cli_rs/0.170.2 (Windows 11; x86_64) orbit"},
        "model/list": {"data": [model("gpt-6.1-sol", "GPT-6.1 Sol", true), model("gpt-6-luna", "GPT-6 Luna", false)], "nextCursor": null},
        "account/read": {"account": {"type": "chatgpt", "email": "dev@example.com", "planType": "plus"}, "requiresOpenaiAuth": true},
        "account/rateLimits/read": {"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1900000000}, "secondary": null}},
        "collaborationMode/list": {"data": [
            {"name": "Plan", "mode": "plan", "model": null, "reasoning_effort": null},
            {"name": "Default", "mode": "default", "model": null, "reasoning_effort": null}
        ]},
        "skills/list": {"data": [{"cwd": "/w", "errors": [], "skills": [
            {"name": "review", "description": "Review the working tree", "enabled": true, "path": "/s/review", "scope": "user"},
            {"name": "off", "description": "Disabled", "enabled": false, "path": "/s/off", "scope": "user"}
        ]}]},
        "config/read": {"config": {"model_reasoning_summary": "detailed"}, "origins": {}},
        "thread/start": thread("thr_1"),
        "thread/resume": thread("thr_1")
    })
}

fn thread(id: &str) -> Value {
    json!({
        "thread": {"id": id, "sessionId": "s", "preview": "", "cliVersion": "0.170.2", "createdAt": 1, "updatedAt": 1,
                   "cwd": "/w", "ephemeral": false, "modelProvider": "openai", "projectId": null, "source": "appServer",
                   "status": {"type": "idle"}, "turns": []},
        "model": "gpt-6.1-sol", "modelProvider": "openai", "cwd": "/w",
        "approvalPolicy": "untrusted", "approvalsReviewer": "user", "sandbox": {"type": "workspaceWrite"}
    })
}

fn run(steps: Vec<Value>) -> Value {
    run_with(handlers(), steps)
}

fn run_with(handlers: Value, steps: Vec<Value>) -> Value {
    json!({"handlers": handlers, "steps": steps})
}

fn expect(method: &str, result: Value) -> Value {
    json!({"expect": method, "result": result})
}

fn send(method: &str, params: Value) -> Value {
    json!({"send": {"method": method, "params": params}})
}

fn server_request(id: u64, method: &str, params: Value) -> Value {
    json!({"send": {"id": id, "method": method, "params": params}})
}

fn turn_start(id: &str) -> Value {
    expect("turn/start", json!({"turn": {"id": id, "items": [], "status": "inProgress", "startedAt": null}}))
}

fn turn_started(id: &str) -> Value {
    send("turn/started", json!({"threadId": "thr_1", "turn": {"id": id, "items": [], "status": "inProgress"}}))
}

fn turn_completed(id: &str, status: &str) -> Value {
    send("turn/completed", json!({"threadId": "thr_1", "turn": {"id": id, "items": [], "status": status, "durationMs": 1234}}))
}

fn prompt(uuid: &str, text: &str, steer: bool) -> SessionInput {
    SessionInput::Prompt { uuid: uuid.into(), text: text.into(), attachments: Vec::new(), steer }
}

async fn next(rx: &Events) -> AgentEvent {
    match tokio::time::timeout(WAIT, rx.recv()).await {
        Ok(Ok((_, event))) => event,
        Ok(Err(_)) => panic!("event channel closed"),
        Err(_) => panic!("no event within {WAIT:?}"),
    }
}

/// Collects events until one matches, returning all of them (match last).
async fn until(rx: &Events, mut pred: impl FnMut(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    loop {
        let event = next(rx).await;
        let done = pred(&event);
        assert!(!matches!(event, AgentEvent::Exited { .. }) || done, "session exited early: {event:?}\nafter {seen:#?}");
        seen.push(event);
        if done {
            return seen;
        }
    }
}

async fn ready(rx: &Events) -> String {
    let events = until(rx, |e| matches!(e, AgentEvent::Ready { .. })).await;
    let Some(AgentEvent::Ready { session_id, .. }) = events.last() else { unreachable!() };
    session_id.clone()
}

async fn turn_finished(rx: &Events) -> (Vec<AgentEvent>, TurnOutcome, Usage) {
    let events = until(rx, |e| matches!(e, AgentEvent::TurnFinished { .. })).await;
    let Some(AgentEvent::TurnFinished { outcome, usage, .. }) = events.last().cloned() else { unreachable!() };
    (events, outcome, usage)
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut v = value;
    for key in path {
        v = v.get(*key)?;
    }
    v.as_str()
}

// ---------------------------------------------------------------------------
// Probe

#[tokio::test]
async fn probe_reports_a_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    let info = provider::probe(provider::Launch::builtin(ProviderKind::Codex, Some(dir.path().join("nope").join("codex.exe")), Vec::new())).await;
    assert!(!info.installed);
    assert_eq!(info.kind, ProviderKind::Codex);
    assert!(info.message.as_deref().unwrap().contains("not found"), "{:?}", info.message);
}

#[tokio::test]
async fn probe_reads_version_models_account_and_modes_from_the_server() {
    let mut h = handlers();
    // Two pages: the probe must follow the cursor.
    let pages = json!({"!seq": [
        {"data": [model("gpt-6.1-sol", "GPT-6.1 Sol", true)], "nextCursor": "page-2"},
        {"data": [model("gpt-6-luna", "GPT-6 Luna", false), {"id": "secret", "model": "secret", "displayName": "S", "description": "",
                  "hidden": true, "isDefault": false, "supportedReasoningEfforts": []}], "nextCursor": null}
    ]});
    h.as_object_mut().unwrap().insert("model/list", pages);
    let fake = Fake::new(vec![run_with(h, vec![])]);
    let started = std::time::Instant::now();
    let info = provider::probe(provider::Launch::builtin(ProviderKind::Codex, Some(fake.exe.clone()), Vec::new())).await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(info.installed, "{info:?}");
    assert_eq!(info.version.as_deref(), Some("0.170.2"));
    assert_eq!(info.signed_in, Some(true));
    assert_eq!(info.account.as_deref(), Some("dev@example.com (ChatGPT Plus)"));
    assert_eq!(info.message, None);
    let slugs: Vec<&str> = info.models.iter().map(|m| m.slug.as_str()).collect();
    assert_eq!(slugs, ["gpt-6.1-sol", "gpt-6-luna"]);
    let sol = &info.models[0];
    assert!(sol.is_default);
    assert_eq!(sol.name, "GPT-6.1 Sol");
    let OptionKind::Select { choices } = &sol.options[0].kind else { panic!() };
    assert_eq!(choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Low", "Medium", "High", "Extra high"]);
    assert_eq!(choices[3].description.as_deref(), Some("Most thorough"));
    assert_eq!(info.modes.iter().map(|m| (m.mode, m.label.as_str())).collect::<Vec<_>>(), [(Mode::Plan, "Plan"), (Mode::Build, "Default")]);

    let sent = fake.received(0);
    assert_eq!(str_at(&sent[0], &["method"]), Some("initialize"));
    assert_eq!(sent[0].get("capabilities").or(sent[0].get("params").and_then(|p| p.get("capabilities"))).and_then(|c| c.get("experimentalApi")).and_then(|v| v.as_bool()), Some(true));
    assert_eq!(str_at(&sent[1], &["method"]), Some("initialized"));
    assert!(sent[1].get("id").is_none());
    let lists = fake.requests(0, "model/list");
    assert_eq!(lists.len(), 2);
    assert_eq!(str_at(&lists[1], &["cursor"]), Some("page-2"));
}

#[tokio::test]
async fn probe_reports_a_signed_out_account() {
    let mut h = handlers();
    h.as_object_mut().unwrap().insert("account/read", json!({"account": null, "requiresOpenaiAuth": true}));
    let fake = Fake::new(vec![run_with(h, vec![])]);
    let info = provider::probe(provider::Launch::builtin(ProviderKind::Codex, Some(fake.exe.clone()), Vec::new())).await;
    assert!(info.installed);
    assert_eq!(info.signed_in, Some(false));
    assert!(info.message.as_deref().unwrap().contains("codex login"));
}

#[tokio::test]
async fn probe_reports_a_server_that_dies_during_the_handshake() {
    let fake = Fake::new(vec![json!({"handlers": {}, "steps": [{"exit": 2, "stderr": "error: unrecognized subcommand 'app-server'"}]})]);
    let info = provider::probe(provider::Launch::builtin(ProviderKind::Codex, Some(fake.exe.clone()), Vec::new())).await;
    assert!(info.installed);
    assert!(info.message.as_deref().unwrap().contains("unrecognized subcommand"), "{:?}", info.message);
}

// ---------------------------------------------------------------------------
// Sessions

#[tokio::test]
async fn turn_streams_reasoning_text_plan_and_usage() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        json!({"raw": "this line is not JSON"}),
        send("item/started", json!({"threadId": "thr_1", "turnId": "turn_1", "startedAtMs": 1, "item": {"type": "reasoning", "id": "rs_1", "summary": [], "content": []}})),
        send("item/reasoning/summaryTextDelta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "rs_1", "summaryIndex": 0, "delta": "Looking at "})),
        send("item/reasoning/summaryTextDelta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "rs_1", "summaryIndex": 0, "delta": "the code"})),
        send("item/reasoning/summaryTextDelta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "rs_1", "summaryIndex": 1, "delta": "Then tests"})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 2, "item": {"type": "reasoning", "id": "rs_1", "summary": ["Looking at the code", "Then tests"], "content": []}})),
        send("turn/plan/updated", json!({"threadId": "thr_1", "turnId": "turn_1", "plan": [
            {"step": "Read code", "status": "completed"}, {"step": "Fix bug", "status": "inProgress"}, {"step": "Test", "status": "pending"}]})),
        send("item/agentMessage/delta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "msg_1", "delta": "Hel"})),
        send("item/agentMessage/delta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "msg_1", "delta": "lo"})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 3, "item": {"type": "agentMessage", "id": "msg_1", "text": "Hello", "phase": "final_answer"}})),
        send("thread/tokenUsage/updated", json!({"threadId": "thr_1", "turnId": "turn_1", "tokenUsage": {
            "total": {"inputTokens": 5000, "cachedInputTokens": 1000, "outputTokens": 300, "reasoningOutputTokens": 100, "totalTokens": 5300},
            "last": {"inputTokens": 5000, "cachedInputTokens": 1000, "outputTokens": 300, "reasoningOutputTokens": 100, "totalTokens": 5300},
            "modelContextWindow": 272000}})),
        send("thread/name/updated", json!({"threadId": "thr_1", "threadName": "Fix the parser"})),
        turn_completed("turn_1", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    assert!(handle.send(prompt("u1", "Fix the bug", false)));
    let (events, outcome, usage) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);

    let ready = events.iter().find_map(|e| match e {
        AgentEvent::Ready { session_id, model } => Some((session_id.clone(), model.clone())),
        _ => None,
    });
    assert_eq!(ready, Some(("thr_1".into(), Some("gpt-6.1-sol".into()))));
    let catalog = events.iter().find_map(|e| if let AgentEvent::Catalog(info) = e { Some(info.clone()) } else { None }).expect("catalog");
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(catalog.modes.len(), 2);
    assert_eq!(catalog.slash_commands.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["review"]);
    assert!(events.contains(&AgentEvent::Quota { window: "5h".into(), utilization: Some(0.12), resets_ms: 1_900_000_000_000, rejected: false }));

    let turn: Vec<&AgentEvent> = events
        .iter()
        .skip_while(|e| !matches!(e, AgentEvent::TurnStarted { .. }))
        .filter(|e| !matches!(e, AgentEvent::Catalog(_) | AgentEvent::Quota { .. }))
        .collect();
    let r = |t: &str| AgentEvent::Delta { block: "rs_1".into(), kind: BlockKind::Reasoning, text: t.into() };
    let m = |t: &str| AgentEvent::Delta { block: "msg_1".into(), kind: BlockKind::Text, text: t.into() };
    let expected = vec![
        AgentEvent::TurnStarted { prompt: Some("u1".into()) },
        r("Looking at "),
        r("the code"),
        r("\n\nThen tests"),
        AgentEvent::BlockDone { block: "rs_1".into(), kind: BlockKind::Reasoning, text: "Looking at the code\n\nThen tests".into() },
        AgentEvent::Plan(vec![
            PlanStep { text: "Read code".into(), state: StepState::Done },
            PlanStep { text: "Fix bug".into(), state: StepState::Active },
            PlanStep { text: "Test".into(), state: StepState::Pending },
        ]),
        m("Hel"),
        m("lo"),
        AgentEvent::BlockDone { block: "msg_1".into(), kind: BlockKind::Text, text: "Hello".into() },
        AgentEvent::Usage(usage),
        AgentEvent::TitleChanged("Fix the parser".into()),
        AgentEvent::Checkpoint { provider_message: "turn_1".into() },
        AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, usage, duration_ms: 1234 },
    ];
    assert_eq!(turn, expected.iter().collect::<Vec<_>>());
    assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (4000, 1000, 300));
    assert_eq!((usage.context_tokens, usage.context_window), (5300, 272_000));

    // What the adapter sent.
    let start = &fake.requests(0, "thread/start")[0];
    assert_eq!(str_at(start, &["sandbox"]), Some("workspace-write"));
    assert_eq!(str_at(start, &["approvalPolicy"]), Some("untrusted"));
    assert!(start.get("model").is_none(), "no model configured: Codex picks its default");
    let turn = &fake.requests(0, "turn/start")[0];
    assert_eq!(str_at(turn, &["threadId"]), Some("thr_1"));
    assert_eq!(turn.get("input").and_then(|i| i.get(0)).and_then(|i| i.get("text")).and_then(|t| t.as_str()), Some("Fix the bug"));
    assert_eq!(str_at(turn, &["approvalPolicy"]), Some("untrusted"));
    assert_eq!(str_at(turn, &["approvalsReviewer"]), Some("user"));
    assert_eq!(str_at(turn, &["sandboxPolicy", "type"]), Some("workspaceWrite"));
    assert_eq!(str_at(turn, &["effort"]), Some("high"));
    assert_eq!(str_at(turn, &["summary"]), Some("detailed"), "the user's summary setting is kept");
    assert_eq!(str_at(turn, &["collaborationMode", "mode"]), Some("default"));
    assert_eq!(str_at(turn, &["collaborationMode", "settings", "model"]), Some("gpt-6.1-sol"));
    assert_eq!(str_at(turn, &["collaborationMode", "settings", "reasoning_effort"]), Some("high"));
    assert!(fake.received(0).iter().all(|m| m.get("jsonrpc").is_none()));
    handle.send(SessionInput::Shutdown);
    assert_eq!(until(&rx, |e| matches!(e, AgentEvent::Exited { .. })).await.last(), Some(&AgentEvent::Exited { error: None }));
}

#[tokio::test]
async fn command_approval_round_trip_and_streamed_output() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        send("item/started", json!({"threadId": "thr_1", "turnId": "turn_1", "startedAtMs": 1, "item": {
            "type": "commandExecution", "id": "cmd_1", "command": "cargo test -p core", "cwd": "/w",
            "commandActions": [{"type": "unknown", "command": "cargo test -p core"}], "status": "inProgress"}})),
        server_request(100, "item/commandExecution/requestApproval", json!({
            "threadId": "thr_1", "turnId": "turn_1", "itemId": "cmd_1", "startedAtMs": 1,
            "command": "cargo test -p core", "cwd": "/w", "reason": "Needs to build outside the sandbox"})),
        json!({"awaitResponse": 100}),
        send("serverRequest/resolved", json!({"threadId": "thr_1", "requestId": 100})),
        send("item/commandExecution/outputDelta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "cmd_1", "delta": "running 3 tests\n"})),
        send("item/commandExecution/outputDelta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "cmd_1", "delta": "ok\n"})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 2, "item": {
            "type": "commandExecution", "id": "cmd_1", "command": "cargo test -p core", "cwd": "/w", "commandActions": [],
            "status": "completed", "exitCode": 0, "aggregatedOutput": "running 3 tests\nok\n"}})),
        turn_completed("turn_1", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Run the tests", false));
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    let started = events.iter().find_map(|e| if let AgentEvent::ToolStarted(c) = e { Some(c.clone()) } else { None }).unwrap();
    assert_eq!((started.call_id.as_str(), started.kind, started.title.as_str()), ("cmd_1", ToolKind::Command, "cargo test -p core"));
    let Some(AgentEvent::Request { provider_ref, kind }) = events.last().cloned() else { unreachable!() };
    assert_eq!(
        kind,
        RequestKind::Command { command: "cargo test -p core".into(), cwd: "/w".into(), reason: "Needs to build outside the sandbox".into() }
    );
    handle.send(SessionInput::Resolve { provider_ref: provider_ref.clone(), decision: Decision::AllowForSession });
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    // Answered, so `serverRequest/resolved` withdraws nothing.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::RequestWithdrawn { .. })));
    let output: Vec<&str> = events.iter().filter_map(|e| if let AgentEvent::ToolOutput { text, .. } = e { Some(text.as_str()) } else { None }).collect();
    assert_eq!(output, ["running 3 tests\n", "ok\n"]);
    assert!(events.contains(&AgentEvent::ToolFinished {
        call_id: "cmd_1".into(),
        output: "running 3 tests\nok\n".into(),
        is_error: false,
        exit_code: Some(0),
        files: vec![],
    }));
    assert_eq!(str_at(&fake.response(0, 100).unwrap(), &["decision"]), Some("acceptForSession"));
}

#[tokio::test]
async fn file_change_approval_previews_the_diff_and_denial_feedback_steers() {
    let changes = json!([{"path": "src/lib.rs", "kind": {"type": "update", "move_path": null},
                          "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n-old\n+new\n+line\n"}]);
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        send("item/started", json!({"threadId": "thr_1", "turnId": "turn_1", "startedAtMs": 1, "item": {
            "type": "fileChange", "id": "fc_1", "changes": changes, "status": "inProgress"}})),
        server_request(101, "item/fileChange/requestApproval", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "fc_1", "startedAtMs": 1})),
        json!({"awaitResponse": 101}),
        expect("turn/steer", json!({"turnId": "turn_1"})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 2, "item": {
            "type": "fileChange", "id": "fc_1", "changes": changes, "status": "declined"}})),
        turn_completed("turn_1", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Edit lib", false));
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    let started = events.iter().find_map(|e| if let AgentEvent::ToolStarted(c) = e { Some(c.clone()) } else { None }).unwrap();
    assert_eq!(started.kind, ToolKind::Edit);
    assert_eq!(started.files, vec![FileChange { path: "src/lib.rs".into(), kind: ChangeKind::Modified, additions: 2, deletions: 1 }]);
    let Some(AgentEvent::Request { provider_ref, kind: RequestKind::FileChange { path, diff } }) = events.last().cloned() else {
        panic!("{:?}", events.last())
    };
    assert_eq!(path, "src/lib.rs");
    assert!(diff.contains("+new\n+line"), "{diff}");
    handle.send(SessionInput::Resolve { provider_ref, decision: Decision::Deny { feedback: "Keep the old name".into() } });
    let (events, _, _) = turn_finished(&rx).await;
    let finished = events.iter().find_map(|e| if let AgentEvent::ToolFinished { is_error, files, .. } = e { Some((*is_error, files.len())) } else { None });
    assert_eq!(finished, Some((true, 1)));
    // An internal steer emits no PromptAccepted for a prompt the engine never sent.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::PromptAccepted { .. })));
    assert_eq!(str_at(&fake.response(0, 101).unwrap(), &["decision"]), Some("decline"));
    let steer = &fake.requests(0, "turn/steer")[0];
    assert_eq!(str_at(steer, &["expectedTurnId"]), Some("turn_1"));
    assert_eq!(steer.get("input").and_then(|i| i.get(0)).and_then(|i| i.get("text")).and_then(|t| t.as_str()), Some("Keep the old name"));
}

#[tokio::test]
async fn steer_then_interrupt_keeps_the_process_alive() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        expect("turn/steer", json!({"turnId": "turn_1"})),
        expect("turn/interrupt", json!({})),
        turn_completed("turn_1", "interrupted"),
        turn_start("turn_2"),
        turn_started("turn_2"),
        turn_completed("turn_2", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Start", false));
    until(&rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    // Wait for the turn id to be known by giving the server a beat.
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.send(prompt("s1", "Also check docs", true));
    until(&rx, |e| *e == AgentEvent::PromptAccepted { uuid: "s1".into() }).await;
    handle.send(SessionInput::Interrupt);
    let (_, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Interrupted);
    let interrupt = &fake.requests(0, "turn/interrupt")[0];
    assert_eq!((str_at(interrupt, &["threadId"]), str_at(interrupt, &["turnId"])), (Some("thr_1"), Some("turn_1")));
    // Same process, next turn.
    handle.send(prompt("u2", "Continue", false));
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(events.contains(&AgentEvent::TurnStarted { prompt: Some("u2".into()) }));
    assert_eq!(std::fs::read_to_string(fake.dir.path().join("fake-codex.count")).unwrap(), "1");
}

#[tokio::test]
async fn steer_falls_back_to_a_queued_turn_when_refused() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        json!({"expect": "turn/steer", "error": {"code": -32601, "message": "method not found"}}),
        turn_completed("turn_1", "completed"),
        turn_start("turn_2"),
        turn_completed("turn_2", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Start", false));
    until(&rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.send(prompt("s1", "Steer me", true));
    turn_finished(&rx).await;
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(events.contains(&AgentEvent::TurnStarted { prompt: Some("s1".into()) }));
    let second = &fake.requests(0, "turn/start")[1];
    assert_eq!(second.get("input").and_then(|i| i.get(0)).and_then(|i| i.get("text")).and_then(|t| t.as_str()), Some("Steer me"));
}

#[tokio::test]
async fn usage_limit_ends_the_turn_rate_limited_with_the_reset_time() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        turn_started("turn_1"),
        send("error", json!({"threadId": "thr_1", "turnId": "turn_1", "willRetry": true,
            "error": {"message": "Reconnecting... 1/5", "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": null}}}})),
        send("account/rateLimits/updated", json!({"rateLimits": {
            "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1900000000},
            "secondary": {"usedPercent": 64, "windowDurationMins": 10080, "resetsAt": 1900400000},
            "rateLimitReachedType": "rate_limit_reached"}})),
        send("error", json!({"threadId": "thr_1", "turnId": "turn_1", "willRetry": false,
            "error": {"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"}})),
        send("turn/completed", json!({"threadId": "thr_1", "turn": {"id": "turn_1", "items": [], "status": "failed",
            "error": {"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"}}})),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Go", false));
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::RateLimited { resets_ms: 1_900_000_000_000 });
    assert!(events.contains(&AgentEvent::Quota { window: "5h".into(), utilization: Some(1.0), resets_ms: 1_900_000_000_000, rejected: true }));
    assert!(events.contains(&AgentEvent::Quota { window: "1w".into(), utilization: Some(0.64), resets_ms: 1_900_400_000_000, rejected: false }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { level: NoticeLevel::Warning, text } if text.contains("Retrying"))));
}

#[tokio::test]
async fn failed_turn_reports_the_message_and_details() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        send("turn/completed", json!({"threadId": "thr_1", "turn": {"id": "turn_1", "items": [], "status": "failed",
            "error": {"message": "Context window exceeded", "additionalDetails": "Start a new thread", "codexErrorInfo": "contextWindowExceeded"}}})),
        json!({"expect": "turn/start", "error": {"code": -32600, "message": "thread is busy"}}),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Go", false));
    let (_, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Failed { message: "Context window exceeded\nStart a new thread".into() });
    handle.send(prompt("u2", "Again", false));
    let (_, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Failed { message: "Codex couldn't start the turn: thread is busy".into() });
}

#[tokio::test]
async fn crash_mid_turn_fails_the_turn_restarts_and_resumes_the_thread() {
    let fake = Fake::new(vec![
        run(vec![
            turn_start("turn_1"),
            turn_started("turn_1"),
            send("item/agentMessage/delta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "msg_1", "delta": "Work"})),
            server_request(7, "item/commandExecution/requestApproval", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "c", "startedAtMs": 1, "command": "rm -rf build"})),
            json!({"sleepMs": 100}),
            json!({"exit": 101, "stderr": "thread 'main' panicked at core/src/codex.rs: boom"}),
        ]),
        run(vec![
            expect("thread/resume", thread("thr_1")),
            turn_start("turn_2"),
            turn_completed("turn_2", "completed"),
        ]),
    ]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Go", false));
    let (events, outcome, _) = turn_finished(&rx).await;
    let TurnOutcome::Failed { message } = outcome else { panic!("{outcome:?}") };
    assert!(message.contains("boom"), "{message}");
    let request_ref = events.iter().find_map(|e| if let AgentEvent::Request { provider_ref, .. } = e { Some(provider_ref.clone()) } else { None }).unwrap();
    let events = until(&rx, |e| matches!(e, AgentEvent::Ready { .. })).await;
    assert!(events.contains(&AgentEvent::RequestWithdrawn { provider_ref: request_ref }), "{events:#?}");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { level: NoticeLevel::Warning, text } if text.contains("Restarting"))));
    assert_eq!(events.last(), Some(&AgentEvent::Ready { session_id: "thr_1".into(), model: Some("gpt-6.1-sol".into()) }));
    let resume = &fake.requests(1, "thread/resume")[0];
    assert_eq!(str_at(resume, &["threadId"]), Some("thr_1"));
    handle.send(prompt("u2", "Again", false));
    let (_, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
}

#[tokio::test]
async fn repeated_crashes_give_up_with_the_reason() {
    let crash = || json!({"handlers": handlers(), "steps": [expect("thread/start", thread("thr_1")), {"exit": 3, "stderr": "fatal: out of memory"}]});
    let crash_resume = || json!({"handlers": handlers(), "steps": [expect("thread/resume", thread("thr_1")), {"exit": 3, "stderr": "fatal: out of memory"}]});
    let fake = Fake::new(vec![crash(), crash_resume(), crash_resume(), crash_resume()]);
    let (_handle, rx) = fake.start(fake.spec(codex_config()));
    let events = until(&rx, |e| matches!(e, AgentEvent::Exited { .. })).await;
    let Some(AgentEvent::Exited { error: Some(error) }) = events.last() else { panic!("{events:?}") };
    assert!(error.contains("out of memory") && error.contains("Stopped restarting"), "{error}");
    assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Ready { .. })).count(), 4);
}

#[tokio::test]
async fn resume_opens_the_existing_thread_and_resume_at_forks_it() {
    let fake = Fake::new(vec![run(vec![]), run_with(
        {
            let mut h = handlers();
            h.as_object_mut().unwrap().insert("thread/fork", thread("thr_fork"));
            h
        },
        vec![],
    )]);
    let mut spec = fake.spec(codex_config());
    spec.resume = Some("thr_1".into());
    let (handle, rx) = fake.start(spec.clone());
    assert_eq!(ready(&rx).await, "thr_1");
    let resume = &fake.requests(0, "thread/resume")[0];
    assert_eq!(str_at(resume, &["threadId"]), Some("thr_1"));
    assert!(fake.requests(0, "thread/start").is_empty());
    handle.send(SessionInput::Shutdown);
    until(&rx, |e| matches!(e, AgentEvent::Exited { .. })).await;

    spec.resume_at = Some("turn_3".into());
    let (_handle, rx) = fake.start(spec);
    assert_eq!(ready(&rx).await, "thr_fork");
    let fork = &fake.requests(1, "thread/fork")[0];
    assert_eq!((str_at(fork, &["threadId"]), str_at(fork, &["lastTurnId"])), (Some("thr_1"), Some("turn_3")));
}

#[tokio::test]
async fn unresumable_thread_falls_back_to_a_new_thread_with_a_notice() {
    let mut h = handlers();
    h.as_object_mut().unwrap().insert("thread/resume", json!({"!error": {"code": -32600, "message": "no rollout found for thread id thr_gone"}}));
    h.as_object_mut().unwrap().insert("thread/start", thread("thr_new"));
    let fake = Fake::new(vec![run_with(h, vec![])]);
    let mut spec = fake.spec(codex_config());
    spec.resume = Some("thr_gone".into());
    let (_handle, rx) = fake.start(spec);
    let events = until(&rx, |e| matches!(e, AgentEvent::Ready { .. })).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { level: NoticeLevel::Warning, text } if text.contains("no rollout found"))));
    assert_eq!(events.last(), Some(&AgentEvent::Ready { session_id: "thr_new".into(), model: Some("gpt-6.1-sol".into()) }));
}

#[tokio::test]
async fn full_access_auto_accepts_and_readonly_never_escalates() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        server_request(5, "item/commandExecution/requestApproval", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "c", "startedAtMs": 1, "command": "ls"})),
        json!({"awaitResponse": 5}),
        turn_completed("turn_1", "completed"),
    ])]);
    let config = ThreadConfig { access: Access::Full, ..codex_config() };
    let (handle, rx) = fake.start(fake.spec(config));
    handle.send(prompt("u1", "Go", false));
    let (events, _, _) = turn_finished(&rx).await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Request { .. })));
    assert_eq!(str_at(&fake.response(0, 5).unwrap(), &["decision"]), Some("accept"));
    assert_eq!(str_at(&fake.requests(0, "thread/start")[0], &["sandbox"]), Some("danger-full-access"));
    assert_eq!(str_at(&fake.requests(0, "turn/start")[0], &["sandboxPolicy", "type"]), Some("dangerFullAccess"));

    // Readonly: a reconfigure applies to the next turn without a restart.
    handle.send(SessionInput::Configure(ThreadConfig { access: Access::Readonly, ..codex_config() }));
    handle.send(SessionInput::Shutdown);
    until(&rx, |e| matches!(e, AgentEvent::Exited { .. })).await;
    let fake = Fake::new(vec![run(vec![turn_start("turn_1"), turn_completed("turn_1", "completed")])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(SessionInput::Configure(ThreadConfig { access: Access::Readonly, ..codex_config() }));
    handle.send(prompt("u1", "Look", false));
    turn_finished(&rx).await;
    let turn = &fake.requests(0, "turn/start")[0];
    assert_eq!((str_at(turn, &["approvalPolicy"]), str_at(turn, &["sandboxPolicy", "type"])), (Some("never"), Some("readOnly")));
}

#[tokio::test]
async fn plan_mode_turn_offers_the_plan_and_approval_implements_it() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        send("item/plan/delta", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "plan_1", "delta": "1. Do it"})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 2, "item": {"type": "plan", "id": "plan_1", "text": "1. Do it"}})),
        turn_completed("turn_1", "completed"),
        turn_start("turn_2"),
        turn_completed("turn_2", "completed"),
    ])]);
    let config = ThreadConfig { mode: Mode::Plan, ..codex_config() };
    let (handle, rx) = fake.start(fake.spec(config));
    handle.send(prompt("u1", "Plan the fix", false));
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    assert!(events.contains(&AgentEvent::Delta { block: "plan_1".into(), kind: BlockKind::Text, text: "1. Do it".into() }));
    let Some(AgentEvent::Request { provider_ref, kind }) = events.last().cloned() else { unreachable!() };
    assert_eq!(kind, RequestKind::Plan { plan: "1. Do it".into() });
    handle.send(SessionInput::Resolve { provider_ref, decision: Decision::Allow });
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(events.contains(&AgentEvent::TurnStarted { prompt: None }));
    let turns = fake.requests(0, "turn/start");
    assert_eq!(str_at(&turns[0], &["collaborationMode", "mode"]), Some("plan"));
    assert_eq!(str_at(&turns[1], &["collaborationMode", "mode"]), Some("default"));
}

#[tokio::test]
async fn questions_are_answered_together_and_resolved_requests_are_withdrawn() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        server_request(9, "item/tool/requestUserInput", json!({"threadId": "thr_1", "turnId": "turn_1", "itemId": "q", "isBlocking": true,
            "questions": [
                {"id": "db", "header": "Database", "question": "Which database?", "options": [{"label": "Postgres", "description": "Shared, survives restarts"}, {"label": "SQLite", "description": ""}]},
                {"id": "name", "header": "", "question": "Project name?"}
            ]})),
        json!({"awaitResponse": 9}),
        server_request(10, "mcpServer/elicitation/request", json!({"threadId": "thr_1", "serverName": "github", "mode": "url", "message": "Sign in", "url": "https://example.com", "elicitationId": "e"})),
        send("serverRequest/resolved", json!({"threadId": "thr_1", "requestId": 10})),
        turn_completed("turn_1", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Set up", false));
    let mut questions = Vec::new();
    while questions.len() < 2 {
        let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
        let Some(AgentEvent::Request { provider_ref, kind }) = events.last().cloned() else { unreachable!() };
        questions.push((provider_ref, kind));
    }
    assert_eq!(questions[0].1, RequestKind::Question { question: "Database: Which database?".into(), options: vec![QuestionOption::new("Postgres", "Shared, survives restarts"), "SQLite".into()], multi_select: false });
    assert_eq!(questions[1].1, RequestKind::Question { question: "Project name?".into(), options: vec![], multi_select: false });
    handle.send(SessionInput::Resolve { provider_ref: questions[1].0.clone(), decision: Decision::Answer { text: "orbit".into() } });
    handle.send(SessionInput::Resolve { provider_ref: questions[0].0.clone(), decision: Decision::Answer { text: "SQLite".into() } });
    let events = until(&rx, |e| matches!(e, AgentEvent::RequestWithdrawn { .. })).await;
    let elicitation = events.iter().find_map(|e| if let AgentEvent::Request { provider_ref, kind: RequestKind::Tool { name, .. } } = e { Some((provider_ref.clone(), name.clone())) } else { None }).unwrap();
    assert_eq!(elicitation.1, "github");
    assert_eq!(events.last(), Some(&AgentEvent::RequestWithdrawn { provider_ref: elicitation.0 }));
    turn_finished(&rx).await;
    let answers = fake.response(0, 9).unwrap();
    let get = |q: &str| answers.get("answers").and_then(|a| a.get(q)).and_then(|a| a.get("answers")).and_then(|a| a.get(0)).and_then(|a| a.as_str()).map(str::to_owned);
    assert_eq!((get("db").as_deref(), get("name").as_deref()), (Some("SQLite"), Some("orbit")));
}

#[tokio::test]
async fn mcp_tool_calls_and_web_searches_become_tools() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        send("item/started", json!({"threadId": "thr_1", "turnId": "turn_1", "startedAtMs": 1, "item": {
            "type": "mcpToolCall", "id": "mcp_1", "server": "github", "tool": "search_issues", "arguments": {"q": "crash"}, "status": "inProgress"}})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 2, "item": {
            "type": "mcpToolCall", "id": "mcp_1", "server": "github", "tool": "search_issues", "arguments": {"q": "crash"}, "status": "completed",
            "result": {"content": [{"type": "text", "text": "3 issues"}]}}})),
        send("item/completed", json!({"threadId": "thr_1", "turnId": "turn_1", "completedAtMs": 3, "item": {
            "type": "webSearch", "id": "ws_1", "query": "tokio select cancel safety", "action": {"type": "search", "query": "tokio select cancel safety"}}})),
        turn_completed("turn_1", "completed"),
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Look it up", false));
    let (events, _, _) = turn_finished(&rx).await;
    let started: Vec<ToolCall> = events.iter().filter_map(|e| if let AgentEvent::ToolStarted(c) = e { Some(c.clone()) } else { None }).collect();
    assert_eq!((started[0].kind, started[0].name.as_str(), started[0].input.as_str()), (ToolKind::Mcp, "mcp__github__search_issues", r#"{"q":"crash"}"#));
    assert_eq!((started[1].kind, started[1].title.as_str()), (ToolKind::Web, "tokio select cancel safety"));
    assert!(events.contains(&AgentEvent::ToolFinished { call_id: "mcp_1".into(), output: "3 issues".into(), is_error: false, exit_code: None, files: vec![] }));
}

#[tokio::test]
async fn a_missing_binary_ends_the_session_with_a_helpful_error() {
    let dir = tempfile::tempdir().unwrap();
    let spec = SessionSpec {
        thread: ThreadId::new(),
        cwd: dir.path().to_owned(),
        config: codex_config(),
        resume: None,
        resume_at: None,
        binary: Some(dir.path().join("missing").join("codex.exe")),
        args: Vec::new(),
        env: Vec::new(),
        extra_dirs: Vec::new(),
        guard: Default::default(),
        mcp: Vec::new(),
    };
    let (tx, rx) = crossfire::mpsc::unbounded_async();
    let _handle = provider::start(ProviderKind::Codex, spec, tx);
    let Ok(Ok((_, AgentEvent::Exited { error: Some(error) }))) = tokio::time::timeout(WAIT, rx.recv()).await else { panic!() };
    assert!(error.contains("not found"), "{error}");
}

#[tokio::test]
async fn an_unconfirmed_stop_closes_the_turn_after_the_timeout() {
    let fake = Fake::new(vec![run(vec![
        turn_start("turn_1"),
        expect("turn/interrupt", json!({})),
        // Codex never reports the turn as completed.
    ])]);
    let (handle, rx) = fake.start(fake.spec(codex_config()));
    handle.send(prompt("u1", "Go", false));
    until(&rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    handle.send(SessionInput::Interrupt);
    let (events, outcome, _) = turn_finished(&rx).await;
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { level: NoticeLevel::Warning, text } if text.contains("didn't confirm"))));
    assert_eq!(fake.requests(0, "turn/interrupt").len(), 1);
}
