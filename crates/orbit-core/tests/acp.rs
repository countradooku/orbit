//! ACP adapter against a scripted fake agent (`tests/support/fake_agent.rs`).

#[path = "support/agent.rs"]
mod support;

use orbit_core::provider::{self, AgentEvent, BlockKind, Launch, SessionInput, TurnOutcome};
use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};
use support::*;

fn initialize(load: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {"loadSession": load, "promptCapabilities": {"image": true, "embeddedContext": true}},
        "authMethods": [{"id": "oauth-personal", "name": "Sign in with Google"}],
        "agentInfo": {"name": "fake-gemini", "version": "0.9.1"}
    })
}

fn session(id: &str) -> Value {
    json!({
        "sessionId": id,
        "modes": {"currentModeId": "default", "availableModes": [
            {"id": "default", "name": "Default"}, {"id": "autoEdit", "name": "Auto edit"}, {"id": "plan", "name": "Plan"}
        ]},
        "models": {"currentModelId": "gemini-pro", "availableModels": [
            {"modelId": "gemini-pro", "name": "Gemini Pro"}, {"modelId": "gemini-flash", "name": "Gemini Flash"}
        ]}
    })
}

fn update(session: &str, update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}})
}

fn acp_config() -> ThreadConfig {
    ThreadConfig { provider: ProviderKind::Acp, instance: "gemini".into(), model: "gemini-flash".into(), ..Default::default() }
}

#[tokio::test]
async fn a_turn_streams_text_tools_plans_and_usage() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "session/new": session("s-1"), "session/set_model": {}},
          "steps": [
            {"expect": "session/prompt", "reply": false},
            {"send": update("s-1", json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Thinking"}}))},
            {"send": update("s-1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Let me "}}))},
            {"send": update("s-1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "edit."}}))},
            {"send": update("s-1", json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit main.rs", "kind": "edit",
                                          "status": "pending", "rawInput": {"file_path": "/w/main.rs"}}))},
            {"send": update("s-1", json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                                          "content": [{"type": "diff", "path": "/w/main.rs", "oldText": "a\n", "newText": "a\nb\n"}]}))},
            {"send": update("s-1", json!({"sessionUpdate": "plan", "entries": [{"content": "Edit", "priority": "high", "status": "completed"}]}))},
            {"send": update("s-1", json!({"sessionUpdate": "usage_update", "used": 5000, "size": 1000000}))},
            {"send": update("s-1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}))},
            {"send": update("s-1", json!({"sessionUpdate": "session_info_update", "title": "Edit main"}))},
            {"respond": "session/prompt", "result": {"stopReason": "end_turn", "usage": {"inputTokens": 100, "outputTokens": 20, "totalTokens": 120}}}
          ]}),
    );
    let mut spec = fake.spec(acp_config());
    spec.args = vec!["--experimental-acp".into()];
    let (session, rx) = fake.start(ProviderKind::Acp, spec);
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "edit main".into(), attachments: vec![], steer: false });
    let events = until(&rx, finished).await;

    assert!(events.iter().any(|e| matches!(e, AgentEvent::Ready { session_id, model } if session_id == "s-1" && model.as_deref() == Some("gemini-pro"))));
    let catalog = events.iter().find_map(|e| match e { AgentEvent::Catalog(info) => Some(info.clone()), _ => None }).unwrap();
    assert_eq!(catalog.models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(), ["gemini-pro", "gemini-flash"]);
    assert_eq!(catalog.modes.len(), 2, "plan mode is offered");
    assert_eq!(catalog.version.as_deref(), Some("0.9.1"));
    assert!(events.contains(&AgentEvent::TurnStarted { prompt: Some("u1".into()) }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::BlockDone { kind: BlockKind::Reasoning, text, .. } if text == "Thinking")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::BlockDone { kind: BlockKind::Text, text, .. } if text == "Let me edit.")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::BlockDone { kind: BlockKind::Text, text, .. } if text == "Done.")));
    let finished_tool = events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished { call_id, files, is_error, .. } if call_id == "c1" => Some((files.clone(), *is_error)),
        _ => None,
    });
    let (files, is_error) = finished_tool.unwrap();
    assert!(!is_error);
    assert_eq!((files[0].path.as_str(), files[0].additions, files[0].deletions), ("/w/main.rs", 1, 0));
    assert!(events.contains(&AgentEvent::Plan(vec![PlanStep { text: "Edit".into(), state: StepState::Done }])));
    assert!(events.contains(&AgentEvent::TitleChanged("Edit main".into())));
    let AgentEvent::TurnFinished { outcome, usage, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(*outcome, TurnOutcome::Completed);
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.context_tokens, usage.context_window), (100, 20, 5000, 1_000_000));

    // The model was applied, the prompt was JSON-RPC 2.0 text, and the
    // instance's arguments reached the agent.
    let set = fake.sent("method", "session/set_model");
    assert_eq!(str_at(&set[0], &["params", "modelId"]), Some("gemini-flash"));
    let prompt = &fake.sent("method", "session/prompt")[0];
    assert_eq!(str_at(prompt, &["params", "prompt", "0", "text"]), Some("edit main"));
    assert_eq!(fake.args(), ["--experimental-acp"]);
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn permissions_ask_and_remember_session_grants_without_an_allow_always_option() {
    let permission = |id: u64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": "s-1",
            "toolCall": {"toolCallId": format!("c{id}"), "title": "Run tests", "kind": "execute", "rawInput": {"command": "cargo test"}},
            "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]
        }})
    };
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "session/new": session("s-1")},
          "steps": [
            {"expect": "session/prompt", "reply": false},
            {"send": permission(900)},
            {"awaitResponse": 900},
            {"send": permission(901)},
            {"awaitResponse": 901},
            {"respond": "session/prompt", "result": {"stopReason": "end_turn"}}
          ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Acp, fake.spec(ThreadConfig { provider: ProviderKind::Acp, ..Default::default() }));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "test".into(), attachments: vec![], steer: false });
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    let AgentEvent::Request { provider_ref, kind } = events.last().unwrap().clone() else { panic!() };
    assert_eq!(kind, RequestKind::Command { command: "cargo test".into(), cwd: fake.dir.path().display().to_string(), reason: "Run tests".into() });
    session.send(SessionInput::Resolve { provider_ref, decision: Decision::AllowForSession });
    let events = until(&rx, finished).await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Request { .. })), "the second identical request is granted for the session");
    let answers: Vec<Value> = fake.received().into_iter().filter(|m| m.get("method").is_none() && m.get("result").is_some()).collect();
    assert_eq!(answers.len(), 2);
    for answer in answers {
        assert_eq!(str_at(&answer, &["result", "outcome", "optionId"]), Some("yes"));
    }
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn stop_waits_for_the_agent_to_acknowledge_and_withdraws_requests() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "session/new": session("s-1")},
          "steps": [
            {"expect": "session/prompt", "reply": false},
            {"send": {"jsonrpc": "2.0", "id": 77, "method": "session/request_permission", "params": {
                "sessionId": "s-1", "toolCall": {"toolCallId": "c", "title": "Write", "kind": "edit"},
                "options": [{"optionId": "ok", "kind": "allow_once"}]}}},
            {"expectNotify": "session/cancel"},
            {"awaitResponse": 77},
            {"sleepMs": 50},
            {"respond": "session/prompt", "result": {"stopReason": "cancelled"}}
          ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Acp, fake.spec(ThreadConfig { provider: ProviderKind::Acp, ..Default::default() }));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "go".into(), attachments: vec![], steer: false });
    until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    session.send(SessionInput::Interrupt);
    let events = until(&rx, finished).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::RequestWithdrawn { .. })));
    assert!(matches!(events.last(), Some(AgentEvent::TurnFinished { outcome: TurnOutcome::Interrupted, .. })));
    let answer = fake.received().into_iter().find(|m| m.get("id").and_then(|v| v.as_u64()) == Some(77) && m.get("method").is_none()).unwrap();
    assert_eq!(str_at(&answer, &["result", "outcome", "outcome"]), Some("cancelled"));
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn a_thread_reloads_its_session_and_ignores_the_replay() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(true)},
          "steps": [
            {"expect": "session/load", "reply": false},
            {"send": update("s-old", json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "old question"}}))},
            {"send": update("s-old", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old answer"}}))},
            {"respond": "session/load", "result": {}},
            {"expect": "session/prompt", "result": {"stopReason": "end_turn"}}
          ]}),
    );
    let mut spec = fake.spec(ThreadConfig { provider: ProviderKind::Acp, ..Default::default() });
    spec.resume = Some("s-old".into());
    let (session, rx) = fake.start(ProviderKind::Acp, spec);
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "again".into(), attachments: vec![], steer: false });
    let events = until(&rx, finished).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Ready { session_id, .. } if session_id == "s-old")));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Delta { .. } | AgentEvent::BlockDone { .. })), "{events:#?}");
    let catalog = events.iter().find_map(|e| match e { AgentEvent::Catalog(info) => Some(info.clone()), _ => None }).unwrap();
    assert_eq!(catalog.models[0].name, "Default model", "an agent without a model list stays pickable");
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn an_agent_that_needs_a_sign_in_is_authenticated_first() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "authenticate": {}},
          "steps": [
            {"expect": "session/new", "error": {"code": -32000, "message": "Authentication required"}},
            {"expect": "authenticate", "result": {}},
            {"expect": "session/new", "result": session("s-2")}
          ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Acp, fake.spec(ThreadConfig { provider: ProviderKind::Acp, ..Default::default() }));
    until(&rx, |e| matches!(e, AgentEvent::Ready { .. })).await;
    let auth = fake.sent("method", "authenticate");
    assert_eq!(str_at(&auth[0], &["params", "methodId"]), Some("oauth-personal"));
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn plan_mode_switches_the_agents_mode_and_back() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "session/new": session("s-1"), "session/set_mode": {}}, "steps": []}),
    );
    let plan = ThreadConfig { provider: ProviderKind::Acp, mode: Mode::Plan, ..Default::default() };
    let (session, rx) = fake.start(ProviderKind::Acp, fake.spec(plan.clone()));
    until(&rx, |e| matches!(e, AgentEvent::Ready { .. })).await;
    session.send(SessionInput::Configure(ThreadConfig { mode: Mode::Build, ..plan }));
    logged(&fake, |m| m.iter().filter(|m| m.get("method").and_then(|v| v.as_str()) == Some("session/set_mode")).count() == 2).await;
    let modes: Vec<String> = fake
        .sent("method", "session/set_mode")
        .iter()
        .map(|m| str_at(m, &["params", "modeId"]).unwrap().to_owned())
        .collect();
    assert_eq!(modes, ["plan", "default"]);
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn probe_reads_the_handshake_without_opening_a_session() {
    let fake = Fake::new("gemini", json!({"protocol": "acp", "handlers": {"initialize": initialize(false)}, "steps": []}));
    let instance = ProviderInstance {
        id: "gemini".into(),
        kind: ProviderKind::Acp,
        name: "Gemini CLI".into(),
        enabled: true,
        binary: fake.exe.display().to_string(),
        args: vec!["--experimental-acp".into()],
        ..Default::default()
    };
    let info = provider::probe(Launch::of(&instance, &ProviderSettings::new(ProviderKind::Acp))).await;
    assert!(info.installed, "{info:?}");
    assert_eq!((info.instance.as_str(), info.name.as_str(), info.version.as_deref()), ("gemini", "Gemini CLI", Some("0.9.1")));
    assert_eq!(info.models.len(), 1);
    assert!(fake.sent("method", "session/new").is_empty());

    let missing = ProviderInstance { binary: "definitely-not-an-agent-xyz".into(), ..instance };
    let info = provider::probe(Launch::of(&missing, &ProviderSettings::new(ProviderKind::Acp))).await;
    assert!(!info.installed);
}

#[tokio::test]
async fn a_team_members_orbit_tools_ride_on_its_session_only() {
    let fake = Fake::new(
        "gemini",
        json!({"protocol": "acp", "handlers": {"initialize": initialize(false), "session/new": session("s-9"), "session/set_model": {}},
          "steps": [
            {"expect": "session/prompt", "reply": false},
            {"respond": "session/prompt", "result": {"stopReason": "end_turn"}}
          ]}),
    );
    let mut spec = fake.spec(acp_config());
    spec.mcp = vec![provider::McpServer {
        name: "orbit".into(),
        command: "/opt/orbit/orbit".into(),
        args: vec!["mcp".into()],
        env: vec![("ORBIT_THREAD".into(), "thr_42".into())],
        config_file: None,
    }];
    let (session, rx) = fake.start(ProviderKind::Acp, spec);
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "plan it".into(), attachments: vec![], steer: false });
    until(&rx, finished).await;
    let opened = fake.sent("method", "session/new");
    let servers = at(&opened[0], &["params", "mcpServers"]).unwrap();
    assert_eq!(at(servers, &["0", "name"]).and_then(|v| v.as_str()), Some("orbit"));
    assert_eq!(at(servers, &["0", "command"]).and_then(|v| v.as_str()), Some("/opt/orbit/orbit"));
    assert_eq!(at(servers, &["0", "args", "0"]).and_then(|v| v.as_str()), Some("mcp"));
    assert_eq!(at(servers, &["0", "env", "0", "value"]).and_then(|v| v.as_str()), Some("thr_42"));
}
