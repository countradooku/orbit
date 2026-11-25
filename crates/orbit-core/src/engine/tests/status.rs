//! The honest status engine against every way a status used to lie. The
//! orchestrator is driven directly, with time passed in, so silences of
//! minutes and sleeps of hours take no time to test.

use std::time::{Duration, Instant};

use crossfire::{AsyncRx, BlockingTxTrait as _, MTx, mpsc};
use orbit_proto::*;

use super::fake_launcher;
use crate::engine::{EventRx, Internal, Orchestrator};
use crate::provider::{AgentEvent, BlockKind};
use crate::status::hook::HookMessage;
use crate::status::term::{HookCall, TermWatch};
use crate::status::{ClockWatch, silence_threshold};
use crate::store::Store;

struct Rig {
    o: Orchestrator,
    agent_rx: AsyncRx<mpsc::List<(ThreadId, AgentEvent)>>,
    agent_tx: MTx<mpsc::List<(ThreadId, AgentEvent)>>,
    internal_rx: AsyncRx<mpsc::List<Internal>>,
    events: EventRx,
    seen: Vec<Event>,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (agent_tx, agent_rx) = mpsc::unbounded_async();
        let (internal_tx, internal_rx) = mpsc::unbounded_async();
        let (term_tx, _term_rx) = mpsc::bounded_blocking_async(crate::terminal::OUTPUT_CAPACITY);
        let (exit_tx, _exit_rx) = mpsc::unbounded_async();
        let mut o = Orchestrator::new(store, agent_tx.clone(), internal_tx, fake_launcher(), term_tx, exit_tx);
        let (_, _, events) = o.subscribe(0, 0);
        Self { o, agent_rx, agent_tx, internal_rx, events, seen: Vec::new(), _dir: dir }
    }

    /// Lets the fake agents run and applies what they reported.
    async fn pump(&mut self) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
            while let Ok((thread, event)) = self.agent_rx.try_recv() {
                self.o.handle_agent(thread, event);
            }
            while let Ok(msg) = self.internal_rx.try_recv() {
                self.o.handle_internal(msg);
            }
        }
        while let Ok(env) = self.events.try_recv() {
            self.seen.push(env.event);
        }
    }

    fn thread(&mut self) -> ThreadId {
        match self.o.handle_command(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }) {
            Ok(Reply::Thread(id)) => id,
            other => panic!("{other:?}"),
        }
    }

    async fn send(&mut self, thread: ThreadId, text: &str) {
        self.o.handle_command(Command::Send { thread, text: text.into(), attachments: vec![], steer: false }).unwrap();
        self.pump().await;
    }

    /// One tick at `after` from now, as if time had passed smoothly.
    async fn tick_after(&mut self, after: Duration) {
        self.o.status.clock = ClockWatch::default();
        self.o.status_tick_at(Instant::now() + after, now_ms() + after.as_millis() as u64);
        self.pump().await;
    }

    fn status(&self, thread: ThreadId) -> ThreadStatus {
        self.o.store.thread(thread).unwrap().status.clone()
    }

    fn live(&self, thread: ThreadId) -> Option<ThreadLive> {
        self.seen.iter().rev().find_map(|e| match e {
            Event::ThreadLive { thread: t, live } if *t == thread => Some(live.clone()),
            _ => None,
        })
    }
}

fn run(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(test());
}

#[test]
fn a_silent_turn_shows_no_output_instead_of_working() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "hang").await;
        assert_eq!(r.status(id), ThreadStatus::Running);
        let threshold = silence_threshold(ProviderKind::Claude);
        r.tick_after(threshold - Duration::from_secs(5)).await;
        assert!(r.live(id).is_none_or(|l| l.liveness == Liveness::Live));
        r.tick_after(threshold + Duration::from_secs(1)).await;
        let live = r.live(id).expect("a live update");
        assert!(matches!(live.liveness, Liveness::Quiet { tool: None, .. }), "{live:?}");
        let thread = r.o.store.thread(id).unwrap().clone();
        assert_eq!(Glyph::of(&thread, Some(&live)), Glyph::Quiet);
        // The snapshot carries it to clients that connect now.
        assert!(r.o.snapshot().live.iter().any(|(t, l)| *t == id && matches!(l.liveness, Liveness::Quiet { .. })));
        // The agent speaks again: live, and the glyph is plain Working.
        r.agent_tx.send((id, AgentEvent::Delta { block: "b".into(), kind: BlockKind::Text, text: "hi".into() })).unwrap();
        r.pump().await;
        assert_eq!(r.live(id).unwrap().liveness, Liveness::Live);
    });
}

#[test]
fn a_quiet_turn_names_the_tool_it_is_running() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "hang").await;
        let call = ToolCall {
            call_id: "c1".into(),
            kind: ToolKind::Command,
            name: "Bash".into(),
            title: "cargo test --workspace".into(),
            input: String::new(),
            output: String::new(),
            output_blob: None,
            exit_code: None,
            files: vec![],
        };
        r.agent_tx.send((id, AgentEvent::ToolStarted(call))).unwrap();
        r.pump().await;
        r.tick_after(Duration::from_secs(600)).await;
        let live = r.live(id).unwrap();
        assert_eq!(liveness_line(&live.liveness, live.last_signal_ms + 600_000).as_deref(), Some("No output for 10m · cargo test --workspace"));
    });
}

#[test]
fn waiting_for_the_user_is_never_called_silence() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "ask").await;
        assert_eq!(r.status(id), ThreadStatus::Waiting);
        r.tick_after(Duration::from_secs(3600)).await;
        assert!(r.live(id).is_none_or(|l| l.liveness == Liveness::Live));
    });
}

#[test]
fn after_sleep_running_turns_are_unverified_until_the_agent_reports() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "hang").await;
        // Two consecutive ticks an hour apart: the machine slept.
        let t0 = Instant::now();
        r.o.status_tick_at(t0, now_ms());
        r.o.status_tick_at(t0 + Duration::from_secs(3600), now_ms() + 3_600_000);
        r.pump().await;
        let live = r.live(id).unwrap();
        let Liveness::Unverified { reason, .. } = &live.liveness else { panic!("{live:?}") };
        assert!(reason.contains("asleep for 1h"), "{reason}");
        let thread = r.o.store.thread(id).unwrap().clone();
        assert_eq!(Glyph::of(&thread, Some(&live)), Glyph::Unverified);
        r.agent_tx.send((id, AgentEvent::Usage(Usage::default()))).unwrap();
        r.pump().await;
        assert_eq!(r.live(id).unwrap().liveness, Liveness::Live);
    });
}

#[test]
fn after_sleep_overdue_snoozes_wake() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        let until = now_ms() + 60_000;
        r.o.handle_command(Command::UpdateThread { id, patch: ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() } })
            .unwrap();
        // The snooze timer counts monotonic time, which stood still.
        let t0 = Instant::now();
        r.o.status_tick_at(t0, now_ms());
        r.o.status_tick_at(t0 + Duration::from_millis(500), now_ms() + 7_200_000);
        let thread = r.o.store.thread(id).unwrap();
        assert_eq!((thread.snoozed_until, thread.woke), (None, true));
    });
}

#[test]
fn a_session_that_vanishes_without_a_word_fails_its_turn() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "vanish").await;
        assert_eq!(r.status(id), ThreadStatus::Running, "the old engine waited here forever");
        r.tick_after(Duration::from_millis(500)).await;
        assert_eq!(r.status(id), ThreadStatus::Running, "one sighting is not enough");
        r.tick_after(Duration::from_secs(1)).await;
        let ThreadStatus::Failed { message } = r.status(id) else { panic!("{:?}", r.status(id)) };
        assert!(message.contains("lost contact"), "{message}");
        // The next message starts a fresh session.
        r.send(id, "hello").await;
        r.pump().await;
        assert_eq!(r.status(id), ThreadStatus::Idle);
    });
}

#[test]
fn a_crash_during_an_approval_closes_the_request() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "ask").await;
        let item = r.o.store.thread(id).unwrap().pending[0].item;
        r.agent_tx.send((id, AgentEvent::Exited { error: Some("segfault".into()) })).unwrap();
        r.pump().await;
        let thread = r.o.store.thread(id).unwrap().clone();
        assert!(matches!(thread.status, ThreadStatus::Failed { .. }));
        assert!(thread.pending.is_empty());
        assert_eq!(r.o.item(id, item).unwrap().status, ItemStatus::Cancelled, "no live approval card for a dead agent");
        assert_eq!(Glyph::of(&thread, None), Glyph::Failed);
    });
}

#[test]
fn an_exit_outside_a_turn_settles_requests_left_behind() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.send(id, "hello").await;
        assert_eq!(r.status(id), ThreadStatus::Idle);
        // A request outside any turn (a background wake-up), then a crash.
        let kind = RequestKind::Question { question: "Continue?".into(), options: vec![], multi_select: false };
        r.agent_tx.send((id, AgentEvent::Request { provider_ref: "q1".into(), kind })).unwrap();
        r.pump().await;
        assert_eq!(r.status(id), ThreadStatus::Waiting);
        r.agent_tx.send((id, AgentEvent::Exited { error: None })).unwrap();
        r.pump().await;
        let thread = r.o.store.thread(id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle, "not waiting on an agent that is gone");
        assert!(thread.pending.is_empty());
    });
}

#[test]
fn impossible_statuses_are_repaired_on_the_tick() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        r.o.update_thread(id, |t| t.status = ThreadStatus::Running);
        r.tick_after(Duration::from_millis(500)).await;
        assert_eq!(r.status(id), ThreadStatus::Idle, "running with no turn");
        r.o.update_thread(id, |t| t.status = ThreadStatus::Waiting);
        r.tick_after(Duration::from_millis(500)).await;
        assert_eq!(r.status(id), ThreadStatus::Idle, "waiting with nothing to answer");
    });
}

#[test]
fn terminal_hooks_reach_the_thread_and_unknown_tokens_do_not() {
    run(async || {
        let mut r = Rig::new();
        let id = r.thread();
        let term = TermId(42);
        r.o.status.terminals.insert(term, TermWatch::new(term, "secret".into(), Some(id)));
        let hook = |event: &str, kind: &str| {
            Internal::Hook(HookMessage {
                token: "secret".into(),
                call: HookCall { agent: "claude".into(), event: event.into(), kind: kind.into(), ..Default::default() },
            })
        };
        r.o.handle_internal(hook("UserPromptSubmit", ""));
        r.pump().await;
        let live = r.live(id).unwrap();
        assert_eq!(live.terminals.len(), 1);
        assert_eq!((live.terminals[0].state, live.terminals[0].evidence), (AgentState::Working, Evidence::Hook));
        assert!(r.seen.iter().any(|e| matches!(e, Event::TerminalAgent { term: t, agent: Some(_) } if *t == term)));
        let thread = r.o.store.thread(id).unwrap().clone();
        assert_eq!(Glyph::of(&thread, Some(&live)), Glyph::Working);

        r.o.handle_internal(hook("Notification", "permission_prompt"));
        r.pump().await;
        assert_eq!(r.live(id).unwrap().terminals[0].state, AgentState::NeedsYou(NeedKind::Permission));

        // A forged call is ignored.
        let before = r.seen.len();
        r.o.handle_internal(Internal::Hook(HookMessage {
            token: "guess".into(),
            call: HookCall { agent: "claude".into(), event: "Stop".into(), ..Default::default() },
        }));
        r.pump().await;
        assert_eq!(r.seen.len(), before);

        // Output proves liveness; two silent minutes while working do not.
        r.o.handle_internal(hook("PreToolUse", ""));
        r.pump().await;
        r.tick_after(Duration::from_secs(121)).await;
        assert_eq!(r.live(id).unwrap().terminals[0].state, AgentState::Unverified);
        r.o.terminal_output(term, b"\x1b[2K\xe2\xa0\x8b Running".to_vec());
        r.pump().await;
        assert_eq!(r.live(id).unwrap().terminals[0].state, AgentState::Working);

        r.o.handle_internal(hook("SessionEnd", ""));
        r.pump().await;
        assert!(r.live(id).unwrap().terminals.is_empty());
        assert!(r.seen.iter().any(|e| matches!(e, Event::TerminalAgent { term: t, agent: None } if *t == term)));
    });
}

#[test]
fn terminals_bind_to_the_thread_working_in_their_folder() {
    run(async || {
        let mut r = Rig::new();
        let folder = tempfile::tempdir().unwrap();
        let Ok(Reply::Project(project)) = r.o.handle_command(Command::AddProject { path: folder.path().display().to_string() }) else {
            panic!()
        };
        let create = |r: &mut Rig| match r.o.handle_command(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree: false }) {
            Ok(Reply::Thread(id)) => id,
            other => panic!("{other:?}"),
        };
        let (a, b) = (create(&mut r), create(&mut r));
        let cwd = r.o.store.project(project).unwrap().path.clone();
        let Ok(Reply::Terminal(term)) = r.o.handle_command(Command::OpenTerminal { cwd: cwd.clone(), cols: 80, rows: 24 }) else {
            panic!()
        };
        // Two threads work there and neither is open: no guess.
        assert_eq!(r.o.status.terminals[&term].thread, None);
        r.o.handle_command(Command::BindTerminal { term, thread: Some(b) }).unwrap();
        assert_eq!(r.o.status.terminals[&term].thread, Some(b));
        r.o.handle_command(Command::CloseTerminal { term }).unwrap();
        assert!(!r.o.status.terminals.contains_key(&term));
        // With one of them open, a new terminal there belongs to it.
        r.o.handle_command(Command::OpenThread { id: a }).unwrap();
        let Ok(Reply::Terminal(term)) = r.o.handle_command(Command::OpenTerminal { cwd, cols: 80, rows: 24 }) else {
            panic!()
        };
        assert_eq!(r.o.status.terminals[&term].thread, Some(a));
        r.o.handle_command(Command::CloseTerminal { term }).unwrap();
    });
}
