//! End-to-end engine tests against a scripted fake agent.

use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_proto::*;

use super::*;
use crate::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};

/// A fake agent whose behavior is chosen by the prompt text:
/// `ask` raises an approval, `crash` exits mid-turn, `slow` streams until
/// interrupted, anything else answers `echo: <text>`.
fn fake_launcher() -> Launcher {
    Box::new(|_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        let spec = spec.clone();
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: "fake-session".into(), model: None });
            let mut turn = 0;
            while let Ok(msg) = rx.recv().await {
                match msg {
                    SessionInput::Prompt { text, steer: false, .. } => {
                        turn += 1;
                        let block = format!("t{turn}");
                        emit(AgentEvent::TurnStarted { prompt: Some("p".into()) });
                        match text.as_str() {
                            "ask" => {
                                emit(AgentEvent::Request {
                                    provider_ref: "req-1".into(),
                                    kind: RequestKind::Command {
                                        command: "cargo test".into(),
                                        cwd: ".".into(),
                                        reason: String::new(),
                                    },
                                });
                                // Wait for the answer.
                                while let Ok(msg) = rx.recv().await {
                                    if let SessionInput::Resolve { decision, .. } = msg {
                                        let text = format!("decision: {decision:?}");
                                        emit(AgentEvent::BlockDone { block: block.clone(), kind: BlockKind::Text, text });
                                        break;
                                    }
                                }
                                finish(&emit);
                            }
                            "write" => {
                                std::fs::write(spec.cwd.join("written-by-agent.txt"), "hello
").unwrap();
                                emit(AgentEvent::BlockDone { block: block.clone(), kind: BlockKind::Text, text: "wrote it".into() });
                                finish(&emit);
                            }
                            "crash" => {
                                emit(AgentEvent::Exited { error: Some("boom".into()) });
                                return;
                            }
                            "slow" => {
                                emit(AgentEvent::Delta { block: block.clone(), kind: BlockKind::Text, text: "working".into() });
                                while let Ok(msg) = rx.recv().await {
                                    if matches!(msg, SessionInput::Interrupt) {
                                        emit(AgentEvent::TurnFinished {
                                            outcome: TurnOutcome::Interrupted,
                                            usage: Usage::default(),
                                            duration_ms: 1,
                                        });
                                        break;
                                    }
                                }
                            }
                            _ => {
                                for piece in ["echo", ": ", &text] {
                                    emit(AgentEvent::Delta { block: block.clone(), kind: BlockKind::Text, text: piece.to_string() });
                                }
                                emit(AgentEvent::BlockDone { block: block.clone(), kind: BlockKind::Text, text: format!("echo: {text}") });
                                emit(AgentEvent::Checkpoint { provider_message: format!("msg-{turn}") });
                                finish(&emit);
                            }
                        }
                    }
                    SessionInput::Shutdown => return,
                    _ => {}
                }
            }
        });
        SessionHandle { input }
    })
}

fn finish(emit: &impl Fn(AgentEvent)) {
    emit(AgentEvent::TurnFinished {
        outcome: TurnOutcome::Completed,
        usage: Usage { input_tokens: 10, output_tokens: 5, context_tokens: 1000, context_window: 200_000, ..Default::default() },
        duration_ms: 3,
    });
}

struct Harness {
    engine: EngineHandle,
    events: EventRx,
    threads: std::collections::HashMap<ThreadId, Thread>,
    items: std::collections::HashMap<ItemId, Item>,
    _dir: Option<tempfile::TempDir>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Self::open(dir.path().to_owned());
        h._dir = Some(dir);
        h
    }

    fn open(path: std::path::PathBuf) -> Self {
        let engine =
            EngineHandle::start_with(EngineOptions { data_dir: path, probe_providers: false }, fake_launcher()).unwrap();
        let sub = engine.subscribe(0, 0).recv().unwrap();
        let snapshot = sub.snapshot.expect("fresh clients get a snapshot");
        Self {
            engine,
            events: sub.events,
            threads: snapshot.threads.into_iter().map(|t| (t.id, t)).collect(),
            items: Default::default(),
            _dir: None,
        }
    }

    fn call(&self, command: Command) -> Reply {
        self.engine.call(command).recv().unwrap().unwrap()
    }

    fn new_thread(&self) -> ThreadId {
        match self.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }) {
            Reply::Thread(id) => id,
            other => panic!("{other:?}"),
        }
    }

    fn send(&self, thread: ThreadId, text: &str) {
        self.call(Command::Send { thread, text: text.into(), attachments: vec![], steer: false });
    }

    /// Applies events until `done` holds or two seconds pass.
    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            match self.events.try_recv() {
                Ok(env) => self.apply(env.event),
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::ThreadUpserted(t) => {
                self.threads.insert(t.id, t);
            }
            Event::ItemUpserted { item, .. } => {
                self.items.insert(item.id, item);
            }
            Event::ItemRemoved { item, .. } => {
                self.items.remove(&item);
            }
            _ => {}
        }
    }

    /// The latest state of a thread; a default until its first event lands.
    fn thread(&self, id: ThreadId) -> Thread {
        self.threads.get(&id).cloned().unwrap_or_default()
    }

    fn texts(&self, turn: u32) -> Vec<String> {
        let mut items: Vec<&Item> = self.items.values().filter(|i| i.turn == turn).collect();
        items.sort_by_key(|i| i.id);
        items
            .iter()
            .filter_map(|i| match &i.body {
                ItemBody::User(m) => Some(format!("user: {}", m.text)),
                ItemBody::Assistant { text } => Some(format!("agent: {text}")),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn a_message_streams_a_reply_and_settles_the_turn() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "hi there");
    h.wait("turn to finish", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    assert_eq!(h.texts(1), vec!["user: hi there", "agent: echo: hi there"]);
    let t = h.thread(id);
    assert_eq!(t.title, "hi there");
    assert_eq!(t.session.as_deref(), Some("fake-session"));
    assert_eq!(t.usage.output_tokens, 5);
    assert_eq!(t.usage.context_fill(), Some(0.005));
    let end = h.items.values().find_map(|i| match &i.body {
        ItemBody::TurnEnd(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(end.unwrap().provider_cursor.as_deref(), Some("msg-1"));
}

#[test]
fn messages_sent_during_a_turn_queue_and_run_next() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "slow");
    h.wait("slow turn to start", |h| h.thread(id).status == ThreadStatus::Running);
    h.send(id, "second");
    h.wait("message to queue", |h| h.thread(id).queued == 1);
    h.call(Command::Interrupt { thread: id });
    h.wait("queued turn to finish", |h| h.thread(id).turns == 2 && h.thread(id).status == ThreadStatus::Idle);
    assert_eq!(h.thread(id).queued, 0);
    assert_eq!(h.texts(2), vec!["user: second", "agent: echo: second"]);
}

#[test]
fn approvals_reach_the_inbox_and_the_answer_reaches_the_agent() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "ask");
    h.wait("request", |h| h.thread(id).status == ThreadStatus::Waiting);
    let pending = h.thread(id).pending.clone();
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].request.kind, RequestKind::Command { ref command, .. } if command == "cargo test"));
    h.call(Command::Resolve { thread: id, request: pending[0].request.id, decision: Decision::Allow });
    h.wait("turn to finish", |h| h.thread(id).status == ThreadStatus::Idle && h.thread(id).turns == 1);
    assert!(h.thread(id).pending.is_empty());
    assert_eq!(h.texts(1).last().unwrap(), "agent: decision: Allow");
    let request = h.items.values().find_map(|i| match &i.body {
        ItemBody::Request(r) => Some(r.clone()),
        _ => None,
    });
    assert_eq!(request.unwrap().resolution, Some(Decision::Allow));
}

#[test]
fn an_agent_crash_fails_the_turn_with_its_error() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "crash");
    h.wait("failure", |h| matches!(h.thread(id).status, ThreadStatus::Failed { .. }));
    assert_eq!(h.thread(id).status, ThreadStatus::Failed { message: "boom".into() });
    assert!(h.items.values().any(|i| matches!(&i.body, ItemBody::Notice { text, .. } if text == "boom")));
}

#[test]
fn history_survives_a_restart_and_stays_unloaded_until_opened() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let mut h = Harness::open(dir.path().to_owned());
        id = h.new_thread();
        h.send(id, "persist me");
        h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
        h.engine.shutdown();
    }
    let h = Harness::open(dir.path().to_owned());
    assert_eq!(h.thread(id).title, "persist me");
    match h.call(Command::OpenThread { id }) {
        Reply::Timeline(items) => {
            let texts: Vec<_> = items
                .iter()
                .filter_map(|i| match &i.body {
                    ItemBody::Assistant { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(texts, vec!["echo: persist me"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn editing_an_earlier_message_replaces_the_rest_of_the_thread() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "first");
    h.wait("turn 1", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    h.send(id, "second");
    h.wait("turn 2", |h| h.thread(id).turns == 2 && h.thread(id).status == ThreadStatus::Idle);
    let second = h
        .items
        .values()
        .find(|i| matches!(&i.body, ItemBody::User(m) if m.text == "second"))
        .unwrap()
        .id;
    h.call(Command::EditAndResend { thread: id, item: second, text: "rewritten".into() });
    h.wait("replacement turn", |h| {
        h.thread(id).status == ThreadStatus::Idle && h.texts(2).contains(&"agent: echo: rewritten".to_string())
    });
    assert_eq!(h.texts(2), vec!["user: rewritten", "agent: echo: rewritten"]);
    assert_eq!(h.texts(1), vec!["user: first", "agent: echo: first"]);
}

#[test]
fn search_finds_text_in_loaded_and_unloaded_threads() {
    let mut h = Harness::new();
    let a = h.new_thread();
    h.send(a, "needle in thread a");
    h.wait("turn a", |h| h.thread(a).turns == 1 && h.thread(a).status == ThreadStatus::Idle);
    h.call(Command::CloseThread { id: a });
    let b = h.new_thread();
    h.send(b, "another NEEDLE");
    h.wait("turn b", |h| h.thread(b).turns == 1 && h.thread(b).status == ThreadStatus::Idle);
    let Reply::SearchHits(hits) = h.call(Command::Search { query: "needle".into(), limit: 10 }) else { panic!() };
    let threads: std::collections::HashSet<ThreadId> = hits.iter().map(|h| h.thread).collect();
    assert!(threads.contains(&a) && threads.contains(&b), "{hits:?}");
    assert!(hits.iter().any(|h| h.excerpt.contains("needle in thread a")));
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git").args(args).current_dir(dir).status().unwrap().success();
    assert!(ok, "git {args:?}");
}

#[test]
fn a_turns_diff_shows_exactly_what_the_turn_changed() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
    std::fs::write(repo.path().join("before.txt"), "already here
").unwrap();

    let mut h = Harness::new();
    let Reply::Project(project) = h.call(Command::AddProject { path: repo.path().display().to_string() }) else { panic!() };
    let Reply::Thread(id) =
        h.call(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree: false })
    else {
        panic!()
    };
    h.send(id, "write");
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    // The snapshot after the turn lands asynchronously.
    h.wait("checkpoint", |h| {
        h.items.values().any(|i| matches!(&i.body, ItemBody::TurnEnd(s) if s.checkpoint.is_some()))
    });
    let summary = h
        .items
        .values()
        .find_map(|i| match &i.body {
            ItemBody::TurnEnd(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    let paths: Vec<&str> = summary.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["written-by-agent.txt"], "untouched pre-existing files are not the turn's");

    let Reply::Diff(patch) = h.call(Command::Diff { thread: id, scope: DiffScope::Turn(1) }) else { panic!() };
    assert!(patch.contains("written-by-agent.txt") && patch.contains("+hello"), "{patch}");
    assert!(!patch.contains("before.txt"), "{patch}");

    // Undo the turn: back to how the files were before it.
    h.call(Command::RestoreCheckpoint { thread: id, turn: 1 });
    assert!(!repo.path().join("written-by-agent.txt").exists(), "undo removes the turn's file");
    assert!(repo.path().join("before.txt").exists(), "files from before the turn stay");
    std::fs::write(repo.path().join("written-by-agent.txt"), "hello
").unwrap();

    // Revert the turn's file through its patch.
    let file = orbit_proto::diff::parse(&patch).remove(0);
    h.call(Command::RevertPatch { thread: id, patch: orbit_proto::file_patch(&file) });
    assert!(!repo.path().join("written-by-agent.txt").exists());
}

#[test]
fn terminals_round_trip_through_the_engine() {
    let mut h = Harness::new();
    let dir = tempfile::tempdir().unwrap();
    let Reply::Terminal(term) =
        h.call(Command::OpenTerminal { cwd: dir.path().display().to_string(), cols: 80, rows: 24 })
    else {
        panic!()
    };
    // ConPTY waits for a cursor-position reply before it shows output.
    h.call(Command::TerminalInput { term, data: b"[1;1R".to_vec() });
    h.call(Command::TerminalInput { term, data: b"echo orbit-term-ok
".to_vec() });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("orbit-term-ok") {
        assert!(Instant::now() < deadline, "no echo; got {:?}", String::from_utf8_lossy(&seen));
        match h.events.try_recv() {
            Ok(Envelope { event: Event::TerminalOutput { term: t, data }, .. }) if t == term => seen.extend(data),
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    h.call(Command::CloseTerminal { term });
}

#[test]
fn settle_snooze_and_wake() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.call(Command::UpdateThread { id, patch: ThreadPatch { pinned: Some(true), ..Default::default() } });
    h.call(Command::UpdateThread { id, patch: ThreadPatch { settled: Some(true), ..Default::default() } });
    h.wait("settled", |h| h.thread(id).settled_ms.is_some());
    assert!(!h.thread(id).pinned, "settling unpins");

    // A message brings a settled thread back.
    h.send(id, "hi");
    h.wait("unsettled", |h| h.thread(id).settled_ms.is_none() && h.thread(id).turns == 1);

    // A snooze ends by itself and marks the thread as woken.
    let until = now_ms() + 300;
    h.call(Command::UpdateThread { id, patch: ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() } });
    h.wait("snoozed", |h| h.thread(id).snoozed_until == Some(until));
    h.wait("woke", |h| h.thread(id).snoozed_until.is_none() && h.thread(id).woke);

    // A request wakes a snoozed thread early.
    h.call(Command::UpdateThread {
        id,
        patch: ThreadPatch { snooze_until: Some(Some(now_ms() + 3_600_000)), ..Default::default() },
    });
    h.wait("snoozed again", |h| h.thread(id).snoozed_until.is_some());
    h.send(id, "ask");
    h.wait("woken by the request", |h| h.thread(id).status == ThreadStatus::Waiting && h.thread(id).snoozed_until.is_none());
}

#[test]
fn a_worktree_thread_queues_until_its_worktree_is_ready() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("README.md"), "hi\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);

    let mut h = Harness::new();
    let Reply::Project(project) = h.call(Command::AddProject { path: repo.path().display().to_string() }) else { panic!() };
    let Reply::Thread(id) =
        h.call(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree: true })
    else {
        panic!()
    };
    // Sent before the worktree exists: queued, then run in the worktree.
    h.send(id, "write");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle) {
        assert!(Instant::now() < deadline, "worktree turn never ran: {:?}", h.thread(id).status);
        match h.events.try_recv() {
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let worktree = h.thread(id).workspace.worktree.clone().expect("worktree path");
    assert!(std::path::Path::new(&worktree).join("written-by-agent.txt").exists(), "the agent ran in the worktree");
    assert!(!repo.path().join("written-by-agent.txt").exists(), "the main checkout is untouched");
    assert!(h.thread(id).workspace.branch.as_deref().is_some_and(|b| b.starts_with("orbit/")));

    h.call(Command::DeleteThread { id });
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::path::Path::new(&worktree).exists() {
        assert!(Instant::now() < deadline, "worktree was not removed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
