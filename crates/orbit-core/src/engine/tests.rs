//! End-to-end engine tests against a scripted fake agent.

use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_proto::*;

use super::*;
use crate::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, SessionSpec, TurnOutcome};

mod review;

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
                            t if t.starts_with("Write a title") => {
                                // No title from this fake: threads keep the
                                // first line of their first message.
                                finish(&emit);
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
    settings: Settings,
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
        Self::open_with(path, fake_launcher())
    }

    fn open_with(path: std::path::PathBuf, launcher: Launcher) -> Self {
        let engine = EngineHandle::start_with(EngineOptions { data_dir: path, probe_providers: false }, launcher).unwrap();
        let sub = engine.subscribe(0, 0).recv().unwrap();
        let snapshot = sub.snapshot.expect("fresh clients get a snapshot");
        Self {
            engine,
            events: sub.events,
            threads: snapshot.threads.into_iter().map(|t| (t.id, t)).collect(),
            items: Default::default(),
            settings: *snapshot.settings,
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
            Event::SettingsChanged(settings) => self.settings = settings,
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
        Reply::Timeline { items, .. } => {
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
fn settings_persist_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.appearance.mode = ColorMode::Dark;
    settings.appearance.chat_width = ChatWidth::Wide;
    settings.composer.submit = SubmitKey::ModEnter;
    settings.workspace.worktree_root = dir.path().join("trees").display().to_string();
    // Sanitized on the way in: a new thread never defaults to full access.
    settings.agents.default_access = Access::Full;
    {
        let mut h = Harness::open(dir.path().to_owned());
        assert_eq!(h.settings, Settings::default());
        h.call(Command::UpdateSettings { settings: settings.clone() });
        h.wait("settings", |h| h.settings.composer.submit == SubmitKey::ModEnter);
        assert_eq!(h.settings.agents.default_access, Access::AutoEdit);
        h.engine.shutdown();
    }
    let h = Harness::open(dir.path().to_owned());
    assert_eq!(h.settings, settings.sanitized());
}

#[test]
fn a_provider_binary_override_reaches_the_session() {
    let (specs, launched) = std::sync::mpsc::channel::<SessionSpec>();
    let mut inner = fake_launcher();
    let launcher: Launcher = Box::new(move |kind, spec, events| {
        let _ = specs.send(spec.clone());
        inner(kind, spec, events)
    });
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::open_with(dir.path().to_owned(), launcher);
    let mut settings = Settings::default();
    let claude = settings.provider_mut(ProviderKind::Claude);
    claude.binary = "/opt/agents/claude".into();
    claude.env.push(EnvVar::new("CLAUDE_CONFIG_DIR", "/tmp/claude-home"));
    h.call(Command::UpdateSettings { settings });

    let id = h.new_thread();
    h.send(id, "hello");
    let spec = launched.recv_timeout(Duration::from_secs(2)).expect("a session starts");
    assert_eq!(spec.binary, Some(std::path::PathBuf::from("/opt/agents/claude")));
    assert!(spec.env.contains(&("CLAUDE_CONFIG_DIR".to_owned(), "/tmp/claude-home".to_owned())), "{:?}", spec.env);
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
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

#[test]
fn a_restart_closes_unanswerable_requests_and_sends_queued_messages() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let mut h = Harness::open(dir.path().to_owned());
        id = h.new_thread();
        h.send(id, "ask");
        h.wait("request", |h| h.thread(id).status == ThreadStatus::Waiting);
        h.send(id, "after the restart");
        h.wait("queued", |h| h.thread(id).queued == 1);
        h.engine.shutdown();
    }
    let mut h = Harness::open(dir.path().to_owned());
    // The request is closed (its session is gone) and the queued message runs.
    h.wait("queued message ran", |h| {
        h.thread(id).pending.is_empty() && h.thread(id).queued == 0 && h.thread(id).status == ThreadStatus::Idle && h.thread(id).turns == 2
    });
    let Reply::Timeline { items, .. } = h.call(Command::OpenThread { id }) else { panic!() };
    assert!(items.iter().any(|i| matches!(&i.body, ItemBody::Request(r) if matches!(r.resolution, Some(Decision::Deny { .. })))));
    assert!(items.iter().any(|i| matches!(&i.body, ItemBody::Assistant { text } if text == "echo: after the restart")));
}

#[test]
fn archived_threads_refuse_messages() {
    let h = Harness::new();
    let id = h.new_thread();
    h.call(Command::UpdateThread { id, patch: ThreadPatch { archived: Some(true), ..Default::default() } });
    let result = h.engine.call(Command::Send { thread: id, text: "hi".into(), attachments: vec![], steer: false }).recv().unwrap();
    assert!(result.unwrap_err().contains("archived"));
}

#[test]
fn switching_provider_starts_a_fresh_session() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "hello");
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    assert!(h.thread(id).session.is_some());
    let config = ThreadConfig { provider: ProviderKind::Codex, ..Default::default() };
    h.call(Command::UpdateThread { id, patch: ThreadPatch { config: Some(config), ..Default::default() } });
    h.wait("session cleared and the context handed off", |h| {
        h.thread(id).session.is_none()
            && h.thread(id).config.provider == ProviderKind::Codex
            && h.items.values().any(|i| matches!(&i.body, ItemBody::Handoff(x) if x.from == "Claude Code" && x.to == "Codex"))
    });
    // The new session's first prompt carries the earlier conversation.
    h.send(id, "next");
    h.wait("the handoff reaches the new agent", |h| {
        h.texts(2).iter().any(|t| {
            t.starts_with("agent: echo: [Context handed off from Claude Code]")
                && t.contains("User: hello")
                && t.ends_with("The user's new message:\n\nnext")
        })
    });
    // Only once.
    h.send(id, "again");
    h.wait("a plain third prompt", |h| h.texts(3).iter().any(|t| t == "agent: echo: again"));
}

/// A launcher that answers every prompt with `answer(prompt)` and records
/// each session's spec.
fn scripted_launcher(
    answer: fn(&str) -> String,
    specs: std::sync::Arc<std::sync::Mutex<Vec<SessionSpec>>>,
) -> Launcher {
    Box::new(move |_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        specs.lock().unwrap().push(spec);
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: format!("s-{}", thread.0), model: None });
            while let Ok(msg) = rx.recv().await {
                match msg {
                    SessionInput::Prompt { text, .. } => {
                        emit(AgentEvent::TurnStarted { prompt: Some("p".into()) });
                        emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: answer(&text) });
                        finish(&emit);
                    }
                    SessionInput::Shutdown => return,
                    _ => {}
                }
            }
        });
        SessionHandle { input }
    })
}

fn titles(prompt: &str) -> String {
    if prompt.starts_with("Write a title") { "Title: \"Fix the login bug.\"".into() } else { format!("echo: {prompt}") }
}

#[test]
fn the_first_turn_earns_a_generated_title_unless_renamed() {
    let specs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::open_with(dir.path().to_owned(), scripted_launcher(titles, specs.clone()));
    let id = h.new_thread();
    h.send(id, "the login form throws on submit");
    h.wait("a generated title", |h| h.thread(id).title == "Fix the login bug");
    // The title run was a throwaway read-only session, not the thread's.
    let specs = specs.lock().unwrap();
    assert!(specs.iter().any(|s| s.thread != id && s.config.access == Access::Readonly));
    drop(specs);

    // A thread the user renamed before the turn ended keeps its name.
    let other = h.new_thread();
    h.send(other, "second thread");
    h.call(Command::UpdateThread { id: other, patch: ThreadPatch { title: Some("Mine".into()), ..Default::default() } });
    h.wait("turn", |h| h.thread(other).turns == 1 && h.thread(other).status == ThreadStatus::Idle);
    std::thread::sleep(Duration::from_millis(100));
    h.wait("events", |_| true);
    while let Ok(env) = h.events.try_recv() {
        h.apply(env.event);
    }
    assert_eq!(h.thread(other).title, "Mine");
}

#[test]
fn generate_text_writes_commit_messages_with_the_threads_agent() {
    let specs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_with(dir.path().to_owned(), scripted_launcher(|p| format!("```\n{}\n```", p.lines().next().unwrap_or("")), specs));
    let id = h.new_thread();
    let reply = h.call(Command::GenerateText { thread: Some(id), task: TextTask::CommitMessage, input: "diff --git a/x b/x".into() });
    let Reply::Text(text) = reply else { panic!("{reply:?}") };
    assert!(text.starts_with("Write a git commit message"), "{text}");
    assert!(!text.contains("```"), "fences are stripped: {text}");
}

#[test]
fn clear_starts_a_fresh_session_in_the_same_thread() {
    let mut h = Harness::new();
    let id = h.new_thread();
    h.send(id, "hello");
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    assert!(h.thread(id).session.is_some());
    h.send(id, "  /clear ");
    h.wait("fresh session", |h| {
        h.thread(id).session.is_none()
            && h.items.values().any(|i| matches!(&i.body, ItemBody::Notice { text, .. } if text.starts_with("New session")))
    });
    assert!(!h.items.values().any(|i| matches!(&i.body, ItemBody::User(m) if m.text.contains("/clear"))), "/clear never reaches the agent");
    assert_eq!(h.thread(id).turns, 1, "no turn was started");
    h.send(id, "after");
    h.wait("the next message starts a new session", |h| h.thread(id).session.is_some() && h.thread(id).turns == 2);
}

#[test]
fn a_second_account_runs_with_its_own_config_dir_and_hands_off() {
    let specs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::open_with(dir.path().to_owned(), scripted_launcher(|p| format!("echo: {p}"), specs.clone()));
    let mut settings = h.settings.clone();
    settings.provider_mut(ProviderKind::Claude).env.push(EnvVar::new("HTTPS_PROXY", "http://proxy"));
    settings.agents.instances.push(ProviderInstance {
        id: "claude-work".into(),
        kind: ProviderKind::Claude,
        name: "Claude (work)".into(),
        enabled: true,
        config_dir: "/home/u/.claude-work".into(),
        ..Default::default()
    });
    settings.agents.generate_titles = false;
    h.call(Command::UpdateSettings { settings });
    let id = h.new_thread();
    h.send(id, "hello");
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    let work = ThreadConfig { instance: "claude-work".into(), ..Default::default() };
    h.call(Command::UpdateThread { id, patch: ThreadPatch { config: Some(work), ..Default::default() } });
    h.wait("handoff to the work account", |h| {
        h.items.values().any(|i| matches!(&i.body, ItemBody::Handoff(x) if x.to == "Claude (work)"))
    });
    h.send(id, "next");
    h.wait("second turn", |h| h.thread(id).turns == 2 && h.thread(id).status == ThreadStatus::Idle);
    let specs = specs.lock().unwrap();
    let last = specs.iter().rev().find(|s| s.thread == id).unwrap();
    assert_eq!(last.config.instance_id(), "claude-work");
    assert!(last.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/home/u/.claude-work".into())), "{:?}", last.env);
    assert!(last.env.contains(&("HTTPS_PROXY".into(), "http://proxy".into())), "the runtime's settings apply too");
    assert!(last.resume.is_none(), "another account cannot resume the first one's session");
    let first = specs.iter().find(|s| s.thread == id).unwrap();
    assert!(!first.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
}
