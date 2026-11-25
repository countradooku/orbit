//! Drives the real Claude Code CLI end to end. Costs a few cents (haiku, tiny
//! prompts), so it is ignored by default:
//!
//! ```sh
//! cargo test -p orbit-core --test claude_live -- --ignored --nocapture
//! ```

use std::time::Duration;

use orbit_core::provider::{self, AgentEvent, SessionInput, SessionSpec, TurnOutcome};
use orbit_proto::*;

type Events = crossfire::AsyncRx<crossfire::mpsc::List<(ThreadId, AgentEvent)>>;

/// Waits for the first event `pick` accepts, logging everything seen.
async fn wait_for<T>(rx: &Events, what: &str, secs: u64, mut pick: impl FnMut(&AgentEvent) -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let (_, event) = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("event channel closed");
        match &event {
            AgentEvent::Delta { .. } | AgentEvent::Usage(_) => {}
            AgentEvent::Catalog(info) => eprintln!("  Catalog: {} models", info.models.len()),
            other => eprintln!("  {other:?}"),
        }
        if let AgentEvent::Exited { error } = &event {
            panic!("session exited while waiting for {what}: {error:?}");
        }
        if let Some(found) = pick(&event) {
            return found;
        }
    }
}

fn prompt(text: &str) -> SessionInput {
    SessionInput::Prompt { uuid: uuid::Uuid::new_v4().to_string(), text: text.into(), attachments: Vec::new(), steer: false }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs the real Claude Code CLI (costs a few cents)"]
async fn real_cli_initialize_prompt_approval_and_interrupt() {
    // Probe: initialize only, no prompt.
    let info = provider::probe(ProviderKind::Claude, None, Vec::new()).await;
    eprintln!("probe: installed={} version={:?} signed_in={:?} account={:?}", info.installed, info.version, info.signed_in, info.account);
    assert!(info.installed, "{:?}", info.message);
    assert_eq!(info.signed_in, Some(true), "{:?}", info.message);
    assert!(info.version.is_some());
    assert!(info.models.iter().any(|m| m.slug == "haiku"), "models come from initialize");
    assert!(!info.slash_commands.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = crossfire::mpsc::unbounded_async::<(ThreadId, AgentEvent)>();
    let spec = SessionSpec {
        thread: ThreadId::new(),
        cwd: dir.path().to_owned(),
        config: ThreadConfig { model: "haiku".into(), access: Access::Ask, ..Default::default() },
        resume: None,
        resume_at: None,
        binary: None,
        env: Vec::new(),
        extra_dirs: Vec::new(),
    };
    let session = provider::start(ProviderKind::Claude, spec, tx);
    let session_id = wait_for(&rx, "Ready", 60, |e| match e {
        AgentEvent::Ready { session_id, .. } => Some(session_id.clone()),
        _ => None,
    })
    .await;
    eprintln!("session {session_id}");

    // 1. A prompt that needs permission: approve the file write.
    assert!(session.send(prompt(
        "Use the Write tool to create a file named orbit.txt containing exactly: ok. Then reply with only the word done."
    )));
    let request = wait_for(&rx, "the write approval", 90, |e| match e {
        AgentEvent::Request { provider_ref, kind: RequestKind::FileChange { path, diff } } => {
            assert!(path.ends_with("orbit.txt"), "{path}");
            assert!(diff.contains("+ok"), "{diff}");
            Some(provider_ref.clone())
        }
        AgentEvent::Request { kind, .. } => panic!("unexpected request {kind:?}"),
        _ => None,
    })
    .await;
    assert!(session.send(SessionInput::Resolve { provider_ref: request, decision: Decision::Allow }));
    let outcome = wait_for(&rx, "the first turn to finish", 90, |e| match e {
        AgentEvent::TurnFinished { outcome, usage, .. } => {
            assert!(usage.cost_micros > 0 && usage.context_tokens > 0 && usage.context_window > 0, "{usage:?}");
            Some(outcome.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(std::fs::read_to_string(dir.path().join("orbit.txt")).unwrap().trim(), "ok");

    // 2. Interrupt a long command. The process must survive Stop.
    let command = if cfg!(windows) { "ping -n 30 127.0.0.1" } else { "sleep 30" };
    assert!(session.send(prompt(&format!("Run the shell command `{command}` with the Bash tool, then reply done."))));
    let request = wait_for(&rx, "the command approval", 90, |e| match e {
        AgentEvent::Request { provider_ref, kind: RequestKind::Command { command: c, .. } } => {
            assert!(c.contains("127.0.0.1") || c.contains("sleep"), "{c}");
            Some(provider_ref.clone())
        }
        _ => None,
    })
    .await;
    assert!(session.send(SessionInput::Resolve { provider_ref: request, decision: Decision::Allow }));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(session.send(SessionInput::Interrupt));
    let mut restarted = false;
    let outcome = wait_for(&rx, "the interrupted turn", 30, |e| match e {
        AgentEvent::TurnFinished { outcome, .. } => Some(outcome.clone()),
        AgentEvent::Ready { .. } => {
            restarted = true;
            None
        }
        _ => None,
    })
    .await;
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(!restarted, "Stop must not restart the CLI");

    // 3. Same process, next turn.
    assert!(session.send(prompt("Reply with only the word alive.")));
    let mut text = String::new();
    let mut restarted = false;
    let outcome = wait_for(&rx, "the turn after the interrupt", 90, |e| match e {
        AgentEvent::BlockDone { text: t, .. } => {
            text.push_str(t);
            None
        }
        AgentEvent::Ready { .. } => {
            restarted = true;
            None
        }
        AgentEvent::TurnFinished { outcome, .. } => Some(outcome.clone()),
        _ => None,
    })
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(!restarted, "the process lived through the interrupt");
    assert!(text.to_lowercase().contains("alive"), "{text}");

    assert!(session.send(SessionInput::Shutdown));
    let error = wait_for_exit(&rx).await;
    assert_eq!(error, None);
}

async fn wait_for_exit(rx: &Events) -> Option<String> {
    loop {
        let (_, event) = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await.expect("exit").expect("open");
        if let AgentEvent::Exited { error } = event {
            return error;
        }
    }
}
