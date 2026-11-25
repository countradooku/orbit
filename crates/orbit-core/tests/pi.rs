//! Pi adapter against a scripted fake `pi --mode rpc`.

#[path = "support/agent.rs"]
mod support;

use orbit_core::provider::{self, AgentEvent, BlockKind, Launch, SessionInput, TurnOutcome};
use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};
use support::*;

fn handlers() -> Value {
    json!({
        "get_available_models": {"models": [
            {"provider": "anthropic", "id": "claude-sonnet", "name": "Claude Sonnet", "contextWindow": 200000},
            {"provider": "openai", "id": "gpt-5", "contextWindow": 400000}
        ]},
        "get_state": {"sessionFile": "/home/u/.pi/sessions/s1.jsonl", "model": {"provider": "anthropic", "id": "claude-sonnet"},
                      "thinkingLevel": "medium", "isStreaming": false},
        "get_session_stats": {"contextUsage": {"tokens": 4200, "contextWindow": 200000}},
        "set_model": {}, "set_thinking_level": {}, "abort": {}
    })
}

fn pi_config() -> ThreadConfig {
    ThreadConfig {
        provider: ProviderKind::Pi,
        model: "openai/gpt-5".into(),
        options: vec![OptionValue { id: "thinking".into(), value: OptionSetting::Choice("high".into()) }],
        access: Access::Full,
        ..Default::default()
    }
}

fn text_delta(index: u64, delta: &str) -> Value {
    json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "contentIndex": index, "delta": delta}})
}

#[tokio::test]
async fn a_turn_streams_text_and_tools_and_reports_usage() {
    let fake = Fake::new(
        "pi",
        json!({"protocol": "pi", "handlers": handlers(), "steps": [
            {"expect": "prompt", "result": {}},
            {"send": {"type": "agent_start"}},
            {"send": {"type": "message_start", "message": {"role": "assistant"}}},
            {"send": text_delta(0, "Running ")},
            {"send": text_delta(0, "tests")},
            {"send": {"type": "message_update", "assistantMessageEvent": {"type": "text_end", "contentIndex": 0, "content": "Running tests"}}},
            {"send": {"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash", "args": {"command": "cargo test"}}},
            {"send": {"type": "tool_execution_update", "toolCallId": "t1", "partialResult": {"content": [{"type": "text", "text": "ok 1"}]}}},
            {"send": {"type": "tool_execution_update", "toolCallId": "t1", "partialResult": {"content": [{"type": "text", "text": "ok 1\nok 2"}]}}},
            {"send": {"type": "tool_execution_end", "toolCallId": "t1", "result": {"content": [{"type": "text", "text": "ok 1\nok 2\ndone"}]}, "isError": false}},
            {"send": {"type": "message_end", "message": {"role": "assistant", "stopReason": "stop",
                       "usage": {"input": 300, "output": 40, "cacheRead": 10, "cacheWrite": 0, "totalTokens": 350, "cost": {"total": 0.002}}}}},
            {"send": {"type": "agent_end"}}
        ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Pi, fake.spec(pi_config()));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "test it".into(), attachments: vec![], steer: false });
    let events = until(&rx, finished).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Ready { session_id, .. } if session_id == "/home/u/.pi/sessions/s1.jsonl")));
    let catalog = events.iter().find_map(|e| match e { AgentEvent::Catalog(info) => Some(info.clone()), _ => None }).unwrap();
    assert_eq!(catalog.models[0].slug, "anthropic/claude-sonnet");
    assert!(catalog.models[0].is_default);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::BlockDone { kind: BlockKind::Text, text, .. } if text == "Running tests")));
    let outputs: Vec<&str> = events.iter().filter_map(|e| match e { AgentEvent::ToolOutput { text, .. } => Some(text.as_str()), _ => None }).collect();
    assert_eq!(outputs, ["ok 1", "\nok 2"], "only what is new is forwarded");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { output, .. } if output.ends_with("done"))));
    let AgentEvent::TurnFinished { outcome, usage, .. } = events.last().unwrap() else { panic!() };
    assert_eq!(*outcome, TurnOutcome::Completed);
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.cost_micros), (300, 40, 2_000));

    // Model and thinking level were set live, before the prompt.
    let set = &fake.sent("type", "set_model")[0];
    assert_eq!((str_at(set, &["provider"]), str_at(set, &["modelId"])), (Some("openai"), Some("gpt-5")));
    assert_eq!(str_at(&fake.sent("type", "set_thinking_level")[0], &["level"]), Some("high"));
    assert_eq!(fake.args()[..2], ["--mode", "rpc"]);
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn steering_is_native_and_stop_waits_for_pi() {
    let fake = Fake::new(
        "pi",
        json!({"protocol": "pi", "handlers": handlers(), "steps": [
            {"expect": "prompt", "result": {}},
            {"send": {"type": "agent_start"}},
            {"expect": "prompt", "result": {}},
            {"expect": "abort", "result": {}},
            {"send": {"type": "agent_end", "aborted": true}}
        ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Pi, fake.spec(pi_config()));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "start".into(), attachments: vec![], steer: false });
    until(&rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    session.send(SessionInput::Prompt { uuid: "u2".into(), text: "also this".into(), attachments: vec![], steer: true });
    until(&rx, |e| matches!(e, AgentEvent::PromptAccepted { uuid } if uuid == "u2")).await;
    let steer = &fake.sent("type", "prompt")[1];
    assert_eq!(str_at(steer, &["streamingBehavior"]), Some("steer"));
    session.send(SessionInput::Interrupt);
    let events = until(&rx, finished).await;
    assert!(matches!(events.last(), Some(AgentEvent::TurnFinished { outcome: TurnOutcome::Interrupted, .. })));
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn extension_questions_become_requests() {
    let fake = Fake::new(
        "pi",
        json!({"protocol": "pi", "handlers": handlers(), "steps": [
            {"expect": "prompt", "result": {}},
            {"send": {"type": "extension_ui_request", "id": "q1", "method": "confirm", "title": "Allow bash?", "message": "rm -rf build"}},
            {"awaitResponse": "q1"},
            {"send": {"type": "agent_end"}}
        ]}),
    );
    let (session, rx) = fake.start(ProviderKind::Pi, fake.spec(ThreadConfig { provider: ProviderKind::Pi, ..Default::default() }));
    session.send(SessionInput::Prompt { uuid: "u1".into(), text: "clean".into(), attachments: vec![], steer: false });
    let events = until(&rx, |e| matches!(e, AgentEvent::Request { .. })).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains("without asking"))), "Pi's lack of approvals is said once");
    let AgentEvent::Request { provider_ref, kind } = events.last().unwrap().clone() else { panic!() };
    assert_eq!(kind, RequestKind::Tool { name: "Allow bash?".into(), input: "rm -rf build".into() });
    session.send(SessionInput::Resolve { provider_ref, decision: Decision::Allow });
    until(&rx, finished).await;
    let answer = &fake.sent("type", "extension_ui_response")[0];
    assert_eq!(str_at(answer, &["id"]), Some("q1"));
    assert_eq!(answer.get("confirmed").and_then(|v| v.as_bool()), Some(true));
    session.send(SessionInput::Shutdown);
}

#[tokio::test]
async fn probe_lists_models_without_a_session() {
    let fake = Fake::new("pi", json!({"protocol": "pi", "handlers": handlers(), "steps": []}));
    let info = provider::probe(Launch::builtin(ProviderKind::Pi, Some(fake.exe.clone()), Vec::new())).await;
    assert!(info.installed, "{info:?}");
    assert_eq!(info.models.len(), 2);
    assert_eq!(info.signed_in, Some(true));
    assert!(fake.args().contains(&"--no-session".to_owned()));
}
