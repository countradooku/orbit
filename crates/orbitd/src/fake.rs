//! A stand-in agent for the host's integration tests (`tests/host.rs`).
//!
//! With `ORBIT_HOST_FAKE_AGENT_MS=<ms>` the local host starts its engine
//! with this launcher instead of the real providers: every message is
//! answered by a turn that streams a token every 20 ms for `<ms>` and then
//! finishes with `done: <message>`. Nothing else changes, so the tests run
//! the real host process, transport, lifetime and reconnects.

use std::time::Duration;

use crossfire::{BlockingTxTrait as _, mpsc};
use orbit_core::engine::Launcher;
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use orbit_proto::Usage;

pub fn launcher_from_env() -> Option<Launcher> {
    let ms: u64 = std::env::var("ORBIT_HOST_FAKE_AGENT_MS").ok()?.trim().parse().ok()?;
    Some(launcher(Duration::from_millis(ms)))
}

fn launcher(turn: Duration) -> Launcher {
    Box::new(move |_kind, spec, events| {
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
                        let start = tokio::time::Instant::now();
                        while start.elapsed() < turn {
                            emit(AgentEvent::Delta { block: "b".into(), kind: BlockKind::Text, text: "tok ".into() });
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: format!("done: {text}") });
                        emit(AgentEvent::TurnFinished {
                            outcome: TurnOutcome::Completed,
                            usage: Usage::default(),
                            duration_ms: turn.as_millis() as u64,
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
