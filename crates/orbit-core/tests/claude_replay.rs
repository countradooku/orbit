//! Replays recorded Claude Code stream-json sessions through the parser and
//! checks the normalized event sequence and the control replies.
//!
//! `write_approval`, `ask_user_question`, `interrupt`, `steer`, `text_turn`
//! and `bash_thinking` are real frames from Claude Code 2.1.326 (account and
//! user name redacted). The rest are hand-written in the same shapes for
//! cases a cheap live run does not produce.

use std::path::Path;

use orbit_core::provider::claude::parser::{Output, Parser};
use orbit_core::provider::{AgentEvent, BlockKind, TurnOutcome};
use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueTrait};

const REAL_CWD: &str = r"C:\Users\user\AppData\Local\Temp\claude\f8a231006ecde0e2\18e3462a-fa62-46ee-99b0-90e6f7a28e81\scratchpad\probe2\work";

struct Replay {
    parser: Parser,
    events: Vec<AgentEvent>,
    replies: Vec<String>,
    plan_approved: bool,
}

/// The session id each recording ran under (Orbit passes it with
/// `--session-id`, so `system/init` confirms it instead of changing it).
fn session_of(name: &str) -> &'static str {
    match name {
        "write_approval.jsonl" | "ask_user_question.jsonl" | "interrupt.jsonl" | "steer.jsonl" => {
            "4dcf48d2-39b0-4cb7-b795-1b08baf4ac6f"
        }
        "text_turn.jsonl" => "819293fb-2131-4778-973d-981cea5dfe65",
        "bash_thinking.jsonl" => "c5f36395-e184-4126-bc82-0d4ff6be6db8",
        _ => "s-1",
    }
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Feeds every line; `decide` answers requests as they appear.
fn replay_with(
    name: &str,
    cwd: &str,
    access: Access,
    prepare: impl FnOnce(&mut Parser),
    mut decide: impl FnMut(&str, &RequestKind) -> Option<Decision>,
) -> Replay {
    let mut parser = Parser::new(cwd.into(), session_of(name).into(), access, None);
    prepare(&mut parser);
    let mut r = Replay { parser, events: Vec::new(), replies: Vec::new(), plan_approved: false };
    let mut out = Output::default();
    for line in fixture(name).lines().filter(|l| !l.trim().is_empty()) {
        out.clear();
        r.parser.feed(line.as_bytes(), &mut out);
        let produced = std::mem::take(&mut out.events);
        r.replies.append(&mut out.replies);
        for event in produced {
            let answer = match &event {
                AgentEvent::Request { provider_ref, kind } => decide(provider_ref, kind).map(|d| (provider_ref.clone(), d)),
                _ => None,
            };
            r.events.push(event);
            if let Some((provider_ref, decision)) = answer {
                out.clear();
                assert!(r.parser.resolve(&provider_ref, &decision, &mut out), "unknown request {provider_ref}");
                r.events.append(&mut out.events);
                r.replies.append(&mut out.replies);
                r.plan_approved |= out.plan_approved;
            }
        }
    }
    r
}

fn replay(name: &str, cwd: &str) -> Replay {
    replay_with(name, cwd, Access::Ask, |_| {}, |_, _| None)
}

/// One line per event; consecutive deltas of a block are merged so the
/// sequence stays readable, with their count.
fn sigs(events: &[AgentEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(String, BlockKind, String, usize)> = None;
    let flush = |run: &mut Option<(String, BlockKind, String, usize)>, out: &mut Vec<String>| {
        if let Some((block, kind, text, n)) = run.take() {
            out.push(format!("Delta {kind:?} {block} x{n} {text:?}"));
        }
    };
    for e in events {
        if let AgentEvent::Delta { block, kind, text } = e {
            match &mut run {
                Some((b, k, t, n)) if b == block && k == kind => {
                    t.push_str(text);
                    *n += 1;
                }
                _ => {
                    flush(&mut run, &mut out);
                    run = Some((block.clone(), *kind, text.clone(), 1));
                }
            }
            continue;
        }
        flush(&mut run, &mut out);
        out.push(sig(e));
    }
    flush(&mut run, &mut out);
    out
}

fn sig(e: &AgentEvent) -> String {
    match e {
        AgentEvent::Ready { session_id, model } => format!("Ready {session_id} {model:?}"),
        AgentEvent::Catalog(i) => format!("Catalog models={} commands={}", i.models.len(), i.slash_commands.len()),
        AgentEvent::TurnStarted { prompt } => format!("TurnStarted {prompt:?}"),
        AgentEvent::PromptAccepted { uuid } => format!("PromptAccepted {uuid}"),
        AgentEvent::Delta { .. } => unreachable!(),
        AgentEvent::BlockDone { block, kind, text } => format!("BlockDone {kind:?} {block} {text:?}"),
        AgentEvent::ToolStarted(c) => {
            let files: Vec<String> =
                c.files.iter().map(|f| format!("{:?}+{}-{}", f.kind, f.additions, f.deletions)).collect();
            format!("ToolStarted {} {} {:?} {:?} input={} files={files:?}", c.call_id, c.name, c.kind, c.title, !c.input.is_empty())
        }
        AgentEvent::ToolOutput { call_id, text } => format!("ToolOutput {call_id} {text:?}"),
        AgentEvent::ToolFinished { call_id, is_error, exit_code, files, .. } => {
            let files: Vec<String> =
                files.iter().map(|f| format!("{:?}+{}-{}", f.kind, f.additions, f.deletions)).collect();
            format!("ToolFinished {call_id} error={is_error} exit={exit_code:?} files={files:?}")
        }
        AgentEvent::Plan(steps) => {
            let s: Vec<String> = steps.iter().map(|s| format!("{:?}:{}", s.state, s.text)).collect();
            format!("Plan {s:?}")
        }
        AgentEvent::Request { provider_ref, kind } => format!("Request {provider_ref} {}", request_sig(kind)),
        AgentEvent::RequestWithdrawn { provider_ref } => format!("Withdrawn {provider_ref}"),
        AgentEvent::Usage(u) => format!("Usage ctx={} window={}", u.context_tokens, u.context_window),
        AgentEvent::Quota { window, utilization, resets_ms, rejected } => {
            format!("Quota {window} {utilization:?} {resets_ms} rejected={rejected}")
        }
        AgentEvent::Notice { level, text } => format!("Notice {level:?} {text}"),
        AgentEvent::Checkpoint { provider_message } => format!("Checkpoint {provider_message}"),
        AgentEvent::TurnFinished { outcome, usage, .. } => format!(
            "TurnFinished {outcome:?} cost={} ctx={} window={} in={} out={} cr={} cw={}",
            usage.cost_micros,
            usage.context_tokens,
            usage.context_window,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens
        ),
        AgentEvent::TitleChanged(t) => format!("Title {t}"),
        AgentEvent::Exited { error } => format!("Exited {error:?}"),
    }
}

fn request_sig(kind: &RequestKind) -> String {
    match kind {
        RequestKind::Command { command, reason, .. } => format!("Command {command:?} ({reason})"),
        RequestKind::FileChange { path, .. } => format!("FileChange {path}"),
        RequestKind::Tool { name, .. } => format!("Tool {name}"),
        RequestKind::Question { question, options, multi_select } => {
            format!("Question {question:?} {options:?} multi={multi_select}")
        }
        RequestKind::Plan { plan } => format!("Plan {plan:?}"),
    }
}

fn reply(r: &str) -> sonic_rs::Value {
    let v: sonic_rs::Value = sonic_rs::from_str(r).unwrap();
    assert_eq!(v["type"].as_str(), Some("control_response"), "{r}");
    v["response"].clone()
}

fn filtered(r: &Replay) -> Vec<String> {
    sigs(&r.events).into_iter().filter(|s| !s.starts_with("Usage")).collect()
}

// ------------------------------------------------------------ real frames

#[test]
fn write_approval_streams_input_and_answers_allow() {
    let r = replay_with("write_approval.jsonl", REAL_CWD, Access::Ask, |_| {}, |_, _| Some(Decision::Allow));
    let input_json = r#"{\"file_path\": \"C:\\\\Users\\\\user\\\\AppData\\\\Local\\\\Temp\\\\claude\\\\f8a231006ecde0e2\\\\18e3462a-fa62-46ee-99b0-90e6f7a28e81\\\\scratchpad\\\\probe2\\\\work\\\\hello.txt\", \"content\": \"hi\"}"#;
    let expected_delta = format!("Delta ToolInput toolu_012fyn77ijg9PxD7RKJrETgR x18 \"{input_json}\"");
    assert_eq!(
        filtered(&r),
        vec![
            "Catalog models=14 commands=4".to_owned(),
            "Ready 4dcf48d2-39b0-4cb7-b795-1b08baf4ac6f Some(\"claude-opus-5-5\")".into(),
            "TurnStarted Some(\"bcee951a-7d24-473f-b4cb-d1d2e9ecf027\")".into(),
            "Quota seven_day Some(0.1) 1796119200000 rejected=false".into(),
            "ToolStarted toolu_012fyn77ijg9PxD7RKJrETgR Write Edit \"Write\" input=false files=[]".into(),
            expected_delta,
            "ToolStarted toolu_012fyn77ijg9PxD7RKJrETgR Write Edit \"hello.txt\" input=true files=[\"Added+1-0\"]".into(),
            "Request f1dd1c66-c4b1-44f9-be13-a464a18fe255 FileChange C:\\Users\\user\\AppData\\Local\\Temp\\claude\\f8a231006ecde0e2\\18e3462a-fa62-46ee-99b0-90e6f7a28e81\\scratchpad\\probe2\\work\\hello.txt".into(),
            "ToolFinished toolu_012fyn77ijg9PxD7RKJrETgR error=false exit=None files=[\"Added+1-0\"]".into(),
            "Delta Text msg_011ChNhVQiqn1K6S2xvoaJ1p:0 x1 \"written\"".into(),
            "BlockDone Text msg_011ChNhVQiqn1K6S2xvoaJ1p:0 \"written\"".into(),
            // The context window became known from `modelUsage`.
            "Catalog models=14 commands=4".into(),
            "Checkpoint 2f33b88e-2516-4796-92a0-9878ac5e2310".into(),
            "TurnFinished Completed cost=12128 ctx=57431 window=1000000 in=4 out=158 cr=43410 cw=57426".into(),
        ]
    );
    // The file-change request previews the new file as a diff.
    let diff = r.events.iter().find_map(|e| match e {
        AgentEvent::Request { kind: RequestKind::FileChange { diff, .. }, .. } => Some(diff.clone()),
        _ => None,
    });
    assert!(diff.unwrap().contains("+hi"));
    // The approval echoes the input and the tool use id.
    assert_eq!(r.replies.len(), 1);
    let resp = reply(&r.replies[0]);
    assert_eq!(resp["subtype"].as_str(), Some("success"));
    assert_eq!(resp["request_id"].as_str(), Some("f1dd1c66-c4b1-44f9-be13-a464a18fe255"));
    assert_eq!(resp["response"]["behavior"].as_str(), Some("allow"));
    assert_eq!(resp["response"]["updatedInput"]["content"].as_str(), Some("hi"));
    assert_eq!(resp["response"]["toolUseID"].as_str(), Some("toolu_012fyn77ijg9PxD7RKJrETgR"));
    // The catalog came from `initialize`, not from a hardcoded list.
    let catalog = r.parser.catalog.as_ref().unwrap();
    assert_eq!(catalog.info.version.as_deref(), Some("2.1.326"));
    assert_eq!(catalog.info.account.as_deref(), Some("user@example.com (Claude Ultra)"));
    let haiku = catalog.info.models.iter().find(|m| m.slug == "haiku").unwrap();
    assert_eq!(haiku.context_window, 1_000_000, "learned from modelUsage");
}

#[test]
fn allow_for_session_on_a_file_scopes_the_rule_to_that_file() {
    let r = replay_with("write_approval.jsonl", REAL_CWD, Access::Ask, |_| {}, |_, _| Some(Decision::AllowForSession));
    let resp = reply(&r.replies[0]);
    let perms = resp["response"]["updatedPermissions"].as_array().unwrap();
    assert_eq!(perms.len(), 1);
    // The CLI suggested `setMode acceptEdits`, which would silently change
    // the thread's access level; Orbit adds a rule for this file instead.
    assert_eq!(perms[0]["type"].as_str(), Some("addRules"));
    assert_eq!(perms[0]["destination"].as_str(), Some("session"));
    let rule = &perms[0]["rules"][0];
    assert_eq!(rule["toolName"].as_str(), Some("Edit"));
    assert_eq!(rule["ruleContent"].as_str(), Some("hello.txt"));
}

#[test]
fn ask_user_question_supports_several_questions_and_multi_select() {
    let r = replay_with("ask_user_question.jsonl", REAL_CWD, Access::Ask, |_| {}, |provider_ref, _| {
        Some(Decision::Answer {
            text: if provider_ref.ends_with("#0") { "Red, warm\nBlue".into() } else { "Large".into() },
        })
    });
    let s = filtered(&r);
    assert!(s.contains(&"Request 2eddbd30-1a72-4978-82cc-fcf953b177a3#0 Question \"Pick colors\" [\"Red, warm\", \"Blue\"] multi=true".to_owned()), "{s:#?}");
    assert!(s.contains(&"Request 2eddbd30-1a72-4978-82cc-fcf953b177a3#1 Question \"Pick size\" [\"Small\", \"Large\"] multi=false".to_owned()));
    assert!(s.contains(&"ToolStarted toolu_01VCqgtptj2sZpmzZbwMLtza AskUserQuestion Other \"AskUserQuestion\" input=true files=[]".to_owned()));
    assert_eq!(s.last().unwrap(), "TurnFinished Completed cost=1920 ctx=60577 window=1000000 in=4 out=241 cr=117681 cw=3110");
    // One reply, sent once both questions are answered; the multi-select
    // answer is an array, so a label containing a comma stays intact.
    assert_eq!(r.replies.len(), 1);
    let resp = reply(&r.replies[0]);
    let answers = &resp["response"]["updatedInput"]["answers"];
    let colors: Vec<&str> = answers["Pick colors"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(colors, ["Red, warm", "Blue"]);
    assert_eq!(answers["Pick size"].as_str(), Some("Large"));
    assert_eq!(resp["response"]["updatedInput"]["questions"].as_array().unwrap().len(), 2);
}

#[test]
fn declining_one_question_denies_the_tool_and_withdraws_the_rest() {
    let r = replay_with("ask_user_question.jsonl", REAL_CWD, Access::Ask, |_| {}, |provider_ref, _| {
        provider_ref.ends_with("#0").then(|| Decision::Deny { feedback: "Not now".into() })
    });
    let resp = reply(&r.replies[0]);
    assert_eq!(resp["response"]["behavior"].as_str(), Some("deny"));
    assert!(resp["response"]["message"].as_str().unwrap().contains("Not now"));
    assert_eq!(resp["response"]["toolUseID"].as_str(), Some("toolu_01VCqgtptj2sZpmzZbwMLtza"));
    assert!(sigs(&r.events).contains(&"Withdrawn 2eddbd30-1a72-4978-82cc-fcf953b177a3#1".to_owned()));
}

#[test]
fn interrupt_ends_the_turn_as_interrupted_and_session_rules_stay_scoped() {
    let r = replay_with(
        "interrupt.jsonl",
        REAL_CWD,
        Access::Ask,
        |p| p.interrupting = false,
        |_, _| Some(Decision::AllowForSession),
    );
    let s = filtered(&r);
    assert!(
        s.contains(&"Request 28db071b-5d61-4f22-b9f3-14f636f58e18 Command \"ping -n 30 127.0.0.1\" (Ping localhost 30 times)".to_owned()),
        "{s:#?}"
    );
    // Live `set_model` / `set_permission_mode` / `apply_flag_settings`
    // succeeded: no notices.
    assert!(!s.iter().any(|l| l.starts_with("Notice")), "{s:#?}");
    assert_eq!(r.parser.permission_mode.as_deref(), Some("acceptEdits"));
    assert!(s.contains(&"ToolFinished toolu_012skLztSeSRaNpo8QsrzPxM error=true exit=None files=[]".to_owned()));
    assert_eq!(s[s.len() - 2], "Checkpoint cfcc6a89-0a46-41a2-99e2-ae1d32580098");
    assert!(s.last().unwrap().starts_with("TurnFinished Interrupted"), "{s:#?}");
    // The CLI suggested a localSettings rule (a file on disk) for exactly
    // this command; Orbit keeps the scope and moves it to the session.
    let resp = reply(&r.replies[0]);
    let perm = &resp["response"]["updatedPermissions"][0];
    assert_eq!(perm["destination"].as_str(), Some("session"));
    assert_eq!(perm["rules"][0]["toolName"].as_str(), Some("Bash"));
    assert_eq!(perm["rules"][0]["ruleContent"].as_str(), Some("ping -n 30 127.0.0.1"));
}

#[test]
fn readonly_threads_deny_side_effects_without_asking() {
    let r = replay_with("interrupt.jsonl", REAL_CWD, Access::Readonly, |_| {}, |_, _| panic!("no request in readonly"));
    assert!(!r.events.iter().any(|e| matches!(e, AgentEvent::Request { .. })));
    let resp = reply(&r.replies[0]);
    assert_eq!(resp["response"]["behavior"].as_str(), Some("deny"));
    assert!(resp["response"]["message"].as_str().unwrap().contains("read-only"));
}

#[test]
fn a_steer_that_misses_the_turn_runs_as_the_next_turn() {
    let r = replay("steer.jsonl", REAL_CWD);
    let s: Vec<String> = filtered(&r).into_iter().filter(|l| !l.starts_with("Quota")).collect();
    assert_eq!(s[2], "TurnStarted Some(\"29932a0b-17bb-4d46-8b7c-43ffd832a901\")");
    assert!(s[3].starts_with("Delta Text msg_011ChNhW88z46Wqg8Eyh6Rxz:0 x27 \"one\\ntwo"), "{}", s[3]);
    assert!(s[4].ends_with("thirty-nine\\nforty\""), "{}", s[4]);
    // The steer was queued (`priority: next`), but the turn had no tool
    // boundary, so the CLI ran it as its own turn right after.
    assert_eq!(&s[5..], [
        "Catalog models=14 commands=4",
        "Checkpoint 25d34f34-eb17-454c-9102-720520986a4b",
        "TurnFinished Completed cost=784 ctx=61231 window=1000000 in=2 out=193 cr=60630 cw=406",
        "TurnStarted Some(\"26eef58a-ed31-43c2-ad4c-54dc7815b0ec\")",
        "Delta Text msg_011ChNhWEBrepWGNadvDnfqj:0 x1 \"steered\"",
        "BlockDone Text msg_011ChNhWEBrepWGNadvDnfqj:0 \"steered\"",
        "Checkpoint f76678e9-75c7-42a5-a57f-900e049eaaae",
        "TurnFinished Completed cost=664 ctx=61277 window=1000000 in=2 out=4 cr=61011 cw=260",
    ]);
}

#[test]
fn a_plain_text_turn() {
    let r = replay(
        "text_turn.jsonl",
        r"C:\Users\user\AppData\Local\Temp\claude\f8a231006ecde0e2\18e3462a-fa62-46ee-99b0-90e6f7a28e81\scratchpad\claude-probe",
    );
    assert_eq!(sigs(&r.events), [
        // Recorded without a prompt uuid, so the turn has none.
        "TurnStarted None",
        "Usage ctx=41571 window=0",
        "Delta Text msg_011ChNgbi2xD3Wa1aShFj8TT:0 x1 \"ok\"",
        "BlockDone Text msg_011ChNgbi2xD3Wa1aShFj8TT:0 \"ok\"",
        "Usage ctx=41571 window=0",
        "Quota seven_day Some(0.1) 1796119200000 rejected=false",
        "Checkpoint d4b52a27-5bee-40f0-97aa-656a338ec316",
        "TurnFinished Completed cost=8315 ctx=41571 window=1000000 in=2 out=4 cr=0 cw=41565",
    ]);
}

#[test]
fn an_auto_approved_command_with_empty_thinking() {
    let r = replay(
        "bash_thinking.jsonl",
        r"C:\Users\USER~1\AppData\Local\Temp\claude\f8a231006ecde0e2\18e3462a-fa62-46ee-99b0-90e6f7a28e81\scratchpad\claude-probe",
    );
    let s = filtered(&r);
    assert!(s.contains(&"ToolStarted toolu_01PdbZdQeieP73BgteC7AEsX Bash Command \"echo orbit-probe\" input=true files=[]".to_owned()), "{s:#?}");
    assert!(s.contains(&"ToolFinished toolu_01PdbZdQeieP73BgteC7AEsX error=false exit=Some(0) files=[]".to_owned()));
    // The thinking block carried only a signature: nothing to show.
    assert!(!s.iter().any(|l| l.contains("Reasoning")), "{s:#?}");
    assert!(s.contains(&"BlockDone Text msg_011ChNgdwmzUgwYJWb6ttTbr:1 \"Done. The command printed `orbit-probe`.\"".to_owned()));
    let output = r.events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished { output, .. } => Some(output.clone()),
        _ => None,
    });
    assert!(output.unwrap().starts_with("orbit-probe"));
}

// ------------------------------------------------------------ hand-written

#[test]
fn tools_plans_subagents_and_withdrawn_requests() {
    let r = replay_with("tools_plan_subagent.jsonl", "/repo", Access::Ask, |_| {}, |provider_ref, _| match provider_ref {
        "req-edit-2" => Some(Decision::AllowForSession),
        "req-plan" => Some(Decision::Deny { feedback: "Also update the docs".into() }),
        _ => None,
    });
    assert_eq!(filtered(&r), [
        "TurnStarted Some(\"p-1\")",
        "Delta Reasoning msg_A:0 x2 \"Need to fix the typo.\"",
        "BlockDone Reasoning msg_A:0 \"Need to fix the typo.\"",
        "ToolStarted toolu_edit Edit Edit \"Edit\" input=false files=[]",
        "Delta ToolInput toolu_edit x2 \"{\\\"file_path\\\": \\\"/repo/src/lib.rs\\\", \\\"old_string\\\": \\\"teh\\\", \\\"new_string\\\": \\\"the\\\"}\"",
        "ToolStarted toolu_edit Edit Edit \"src/lib.rs\" input=true files=[\"Modified+1-1\"]",
        "Request req-edit FileChange /repo/src/lib.rs",
        "Withdrawn req-edit",
        "Request req-edit-2 FileChange /repo/src/lib.rs",
        "ToolFinished toolu_edit error=false exit=None files=[\"Modified+1-1\"]",
        // Not streamed: the snapshot starts and completes the card.
        "ToolStarted toolu_todo TodoWrite Todo \"Update plan\" input=false files=[]",
        "ToolStarted toolu_todo TodoWrite Todo \"Update plan\" input=true files=[]",
        "Plan [\"Done:Fix typo\", \"Active:Running tests\", \"Pending:Ship\"]",
        "ToolFinished toolu_todo error=false exit=None files=[]",
        "ToolStarted toolu_task Task Task \"Task\" input=false files=[]",
        "ToolStarted toolu_task Task Task \"Run the tests (general-purpose)\" input=true files=[]",
        // A subagent's step is progress of its task; its text is not the reply.
        "ToolOutput toolu_task \"Bash: cargo test\\n\"",
        "ToolFinished toolu_task error=false exit=None files=[]",
        "ToolStarted toolu_plan ExitPlanMode Other \"ExitPlanMode\" input=false files=[]",
        "Delta ToolInput toolu_plan x1 \"{\\\"plan\\\": \\\"1. Fix it\\\\n2. Test\\\"}\"",
        "ToolStarted toolu_plan ExitPlanMode Other \"ExitPlanMode\" input=true files=[]",
        "Request req-plan Plan \"1. Fix it\\n2. Test\"",
        "ToolFinished toolu_plan error=true exit=None files=[]",
        "Delta Text msg_D:0 x2 \"Updated the plan.\"",
        "BlockDone Text msg_D:0 \"Updated the plan.\"",
        "Title Fix the typo",
        "Checkpoint a-6",
        "TurnFinished Completed cost=50000 ctx=5257 window=200000 in=25 out=72 cr=4750 cw=410",
    ]);
    assert!(!r.plan_approved);
    assert_eq!(r.replies.len(), 2);
    let edit = reply(&r.replies[0]);
    let rule = &edit["response"]["updatedPermissions"][0]["rules"][0];
    assert_eq!(rule["toolName"].as_str(), Some("Edit"));
    assert_eq!(rule["ruleContent"].as_str(), Some("src/lib.rs"));
    let plan = reply(&r.replies[1]);
    assert_eq!(plan["response"]["behavior"].as_str(), Some("deny"));
    assert!(plan["response"]["message"].as_str().unwrap().contains("Also update the docs"));
    assert_eq!(r.parser.pending_requests(), 0);
}

#[test]
fn approving_a_plan_is_reported_to_the_session() {
    let r = replay_with("tools_plan_subagent.jsonl", "/repo", Access::AutoEdit, |_| {}, |provider_ref, _| {
        (provider_ref == "req-plan").then_some(Decision::Allow)
    });
    assert!(r.plan_approved);
    let resp = reply(r.replies.last().unwrap());
    assert_eq!(resp["response"]["behavior"].as_str(), Some("allow"));
    let perm = &resp["response"]["updatedPermissions"][0];
    assert_eq!(perm["type"].as_str(), Some("setMode"));
    assert_eq!(perm["mode"].as_str(), Some("acceptEdits"), "the thread's access level");
    assert_eq!(perm["destination"].as_str(), Some("session"));
}

#[test]
fn rate_limits_end_the_turn_with_the_reset_time() {
    let r = replay("rate_limited.jsonl", "/repo");
    assert_eq!(filtered(&r), [
        "TurnStarted Some(\"p-9\")",
        "Quota five_hour Some(1.0) 1796119200000 rejected=true",
        "Quota seven_day Some(0.4) 1796500000000 rejected=false",
        "BlockDone Text m-syn:0 \"You have hit your limit\"",
        "TurnFinished RateLimited { resets_ms: 1796119200000 } cost=0 ctx=0 window=200000 in=0 out=0 cr=0 cw=0",
    ]);
}

#[test]
fn outcomes_come_from_terminal_reason_not_error_text() {
    let r = replay("outcomes.jsonl", "/repo");
    let outcomes: Vec<TurnOutcome> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, [
        // "cancelled" in the text does not make it an interruption, and the
        // diagnostic line is dropped.
        TurnOutcome::Failed { message: "Upstream proxy cancelled the request".into() },
        TurnOutcome::Failed { message: "API Error: 500 Internal server error".into() },
        TurnOutcome::Failed { message: "Max turns".into() },
    ]);
}

#[test]
fn background_wakes_and_accepted_steers() {
    let r = replay("wake_and_steer.jsonl", "/repo");
    assert_eq!(filtered(&r), [
        "TurnStarted Some(\"p-5\")",
        "Delta Text msg_F:0 x1 \"Working\"",
        "PromptAccepted p-6",
        "BlockDone Text msg_F:0 \"Working\"",
        "Checkpoint a-7",
        "TurnFinished Completed cost=10000 ctx=10 window=200000 in=1 out=4 cr=3 cw=2",
        // The CLI started this turn by itself when background work ended.
        "TurnStarted None",
        "BlockDone Text msg_G:0 \"The background build finished.\"",
        "Checkpoint a-8",
        "TurnFinished Completed cost=2000 ctx=10 window=200000 in=1 out=4 cr=3 cw=2",
        "Notice Warning Claude Code did not accept a queued message; send it again",
    ]);
}

#[test]
fn force_finish_withdraws_open_requests() {
    // Feed up to the approval only.
    let mut parser = Parser::new(REAL_CWD.into(), session_of("write_approval.jsonl").into(), Access::Ask, None);
    let mut out = Output::default();
    for line in fixture("write_approval.jsonl").lines() {
        parser.feed(line.as_bytes(), &mut out);
        if line.contains("can_use_tool") {
            break;
        }
    }
    out.clear();
    parser.force_finish(TurnOutcome::Interrupted, &mut out);
    let s = sigs(&out.events);
    assert!(s.contains(&"Withdrawn f1dd1c66-c4b1-44f9-be13-a464a18fe255".to_owned()), "{s:#?}");
    assert!(s.last().unwrap().starts_with("TurnFinished Interrupted"), "{s:#?}");
}
