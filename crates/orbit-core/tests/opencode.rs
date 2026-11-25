//! OpenCode adapter against a scripted fake `opencode serve` (HTTP + SSE).

#[path = "support/agent.rs"]
mod support;

use orbit_core::provider::{self, AgentEvent, BlockKind, Launch, SessionInput, TurnOutcome};
use orbit_proto::*;
use sonic_rs::{JsonValueMutTrait, Value, json};
use support::*;

fn routes() -> Value {
    json!({
        "GET /global/health": {"healthy": true, "version": "1.4.2"},
        "GET /config": {"model": "anthropic/claude-sonnet"},
        "GET /config/providers": {"providers": [{"id": "anthropic", "name": "Anthropic", "models": {
            "claude-sonnet": {"id": "claude-sonnet", "name": "Claude Sonnet", "limit": {"context": 200000}},
            "claude-haiku": {"id": "claude-haiku", "name": "Claude Haiku"}
        }}], "default": {"anthropic": "claude-sonnet"}},
        "POST /session": {"id": "ses_1", "title": "New session"},
        "POST /session/ses_1/prompt_async": {},
        "POST /session/ses_1/abort": true,
        "POST /permission/per_1/reply": true
    })
}

fn event(kind: &str, properties: Value) -> Value {
    json!({"type": kind, "properties": properties})
}

fn reply_events() -> Value {
    json!([
        event("message.updated", json!({"info": {"id": "msg_b", "role": "assistant", "sessionID": "ses_1", "time": {}}})),
        event("message.part.updated", json!({"part": {"id": "prt_1", "sessionID": "ses_1", "messageID": "msg_b", "type": "reasoning", "text": "Hm"}})),
        event("message.part.updated", json!({"part": {"id": "prt_2", "sessionID": "ses_1", "messageID": "msg_b", "type": "text", "text": "Fixing"}, "delta": "Fixing"})),
        event("message.part.updated", json!({"part": {"id": "prt_2", "sessionID": "ses_1", "messageID": "msg_b", "type": "text", "text": "Fixing it"}, "delta": " it"})),
        event("message.part.updated", json!({"part": {"id": "prt_3", "sessionID": "ses_1", "messageID": "msg_b", "type": "tool", "callID": "call_1",
            "tool": "edit", "state": {"status": "running", "input": {"filePath": "/w/a.rs"}}}})),
        event("message.part.updated", json!({"part": {"id": "prt_3", "sessionID": "ses_1", "messageID": "msg_b", "type": "tool", "callID": "call_1",
            "tool": "edit", "state": {"status": "completed", "input": {"filePath": "/w/a.rs"}, "output": "edited", "title": "a.rs"}}})),
        event("todo.updated", json!({"sessionID": "ses_1", "todos": [{"content": "Fix a.rs", "status": "completed"}, {"content": "Test", "status": "pending"}]})),
        event("message.updated", json!({"info": {"id": "msg_b", "role": "assistant", "sessionID": "ses_1", "time": {"completed": 2},
            "tokens": {"input": 900, "output": 60, "reasoning": 5, "cache": {"read": 100, "write": 0}}, "cost": 0.0125}})),
        event("session.idle", json!({"sessionID": "ses_1"}))
    ])
}

#[tokio::test]
async fn a_turn_runs_over_http_and_events() {
    let fake = Fake::new("opencode", json!({"protocol": "opencode", "routes": routes(), "on": {"POST /session/ses_1/prompt_async": reply_events()}}));
    let config = ThreadConfig { provider: ProviderKind::OpenCode, model: "anthropic/claude-haiku".into(), mode: Mode::Plan, ..Default::default() };
    let (session, rx) = fake.start(ProviderKind::OpenCode, fake.spec(config));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "fix a.rs".into(), attachments: vec![], steer: false });
    let events = until(&rx, finished).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Ready { session_id, .. } if session_id == "ses_1")));
    let catalog = events.iter().find_map(|e| match e { AgentEvent::Catalog(info) => Some(info.clone()), _ => None }).unwrap();
    assert_eq!(catalog.version.as_deref(), Some("1.4.2"));
    assert!(catalog.models.iter().any(|m| m.slug == "anthropic/claude-sonnet" && m.is_default && m.context_window == 200_000));
    let text: String = events.iter().filter_map(|e| match e { AgentEvent::Delta { kind: BlockKind::Text, text, .. } => Some(text.as_str()), _ => None }).collect();
    assert_eq!(text, "Fixing it");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { call_id, files, .. } if call_id == "call_1" && files[0].path == "/w/a.rs")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Plan(steps) if steps.len() == 2)));
    let AgentEvent::TurnFinished { outcome, usage, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(*outcome, TurnOutcome::Completed);
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost_micros), (900, 65, 12_500));

    let prompt = &fake.sent("route", "POST /session/ses_1/prompt_async")[0];
    assert_eq!(str_at(prompt, &["body", "agent"]), Some("plan"), "plan mode is OpenCode's plan agent");
    assert_eq!(str_at(prompt, &["body", "model", "providerID"]), Some("anthropic"));
    assert_eq!(str_at(prompt, &["body", "model", "modelID"]), Some("claude-haiku"));
    assert_eq!(str_at(prompt, &["body", "parts", "0", "text"]), Some("fix a.rs"));
    assert!(str_at(prompt, &["body", "messageID"]).unwrap().starts_with("msg_"));
    let created = &fake.sent("route", "POST /session")[0];
    assert_eq!(str_at(created, &["body", "permission", "0", "action"]), Some("ask"), "Ask access asks before edits");
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn permissions_are_answered_over_http() {
    let asked = json!([event("permission.asked", json!({"id": "per_1", "sessionID": "ses_1", "permission": "bash",
        "patterns": ["npm test"], "metadata": {"command": "npm test"}, "title": "Run npm test"}))]);
    let mut on = json!({"POST /session/ses_1/prompt_async": asked});
    on.as_object_mut().unwrap().insert("POST /permission/per_1/reply", json!([event("session.idle", json!({"sessionID": "ses_1"}))]));
    let fake = Fake::new("opencode", json!({"protocol": "opencode", "routes": routes(), "on": on}));
    let (session, rx) = fake.start(ProviderKind::OpenCode, fake.spec(ThreadConfig { provider: ProviderKind::OpenCode, ..Default::default() }));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "test".into(), attachments: vec![], steer: false });
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    let AgentEvent::Request { provider_ref, kind } = events.last().unwrap().clone() else { panic!() };
    assert!(matches!(kind, RequestKind::Command { ref command, .. } if command == "npm test"), "{kind:?}");
    session.send(SessionInput::Resolve { provider_ref, decision: Decision::AllowForSession });
    until(&rx, finished).await;
    let reply = &fake.sent("route", "POST /permission/per_1/reply")[0];
    assert_eq!(str_at(reply, &["body", "reply"]), Some("always"));
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn stop_aborts_and_waits_for_idle() {
    let started = json!([event("message.updated", json!({"info": {"id": "msg_b", "role": "assistant", "sessionID": "ses_1", "time": {}}}))]);
    let aborted = json!([
        event("session.error", json!({"sessionID": "ses_1", "error": {"name": "MessageAbortedError", "data": {"message": "Aborted"}}})),
        event("session.idle", json!({"sessionID": "ses_1"}))
    ]);
    let fake = Fake::new(
        "opencode",
        json!({"protocol": "opencode", "routes": routes(), "on": {"POST /session/ses_1/prompt_async": started, "POST /session/ses_1/abort": aborted}}),
    );
    let (session, rx) = fake.start(ProviderKind::OpenCode, fake.spec(ThreadConfig { provider: ProviderKind::OpenCode, ..Default::default() }));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "go".into(), attachments: vec![], steer: false });
    until(&rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    session.send(SessionInput::Interrupt);
    let events = until(&rx, finished).await;
    assert!(matches!(events.last(), Some(AgentEvent::TurnFinished { outcome: TurnOutcome::Interrupted, .. })), "{events:#?}");
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn probe_starts_a_server_and_reads_models() {
    let fake = Fake::new("opencode", json!({"protocol": "opencode", "routes": routes()}));
    let info = provider::probe(Launch::builtin(ProviderKind::OpenCode, Some(fake.exe.clone()), Vec::new())).await;
    assert!(info.installed, "{info:?}");
    assert_eq!(info.version.as_deref(), Some("1.4.2"));
    assert_eq!(info.models.len(), 2);
    assert_eq!(info.modes.len(), 2);
}
