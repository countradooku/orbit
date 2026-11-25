//! Quota, the usage ledger and account failover against fake accounts: a
//! fake agent whose built-in Claude account is at its limit while a second
//! account (`claude-work`) has room, and a fake limits reader.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossfire::{AsyncRx, mpsc};
use orbit_proto::*;

use crate::engine::{Internal, Launcher, Orchestrator};
use crate::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use crate::store::Store;
use crate::usage::{ReadError, Reading};

const WORK: &str = "claude-work";

/// Every session reports a signed-in catalog. On the built-in instance a
/// prompt starting with `limit` hits the five-hour limit; everything else
/// (and everything on other instances) is echoed with some usage.
fn launcher() -> Launcher {
    Box::new(|_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        let limited_account = spec.config.instance.is_empty();
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: format!("s-{}", spec.config.instance), model: None });
            emit(AgentEvent::Catalog(ProviderInfo {
                kind: ProviderKind::Claude,
                installed: true,
                signed_in: Some(true),
                models: vec![ModelInfo {
                    slug: "default".into(),
                    name: "Default".into(),
                    description: Some("Opus 5.5 · Best for complex work".into()),
                    context_window: 200_000,
                    is_default: true,
                    options: vec![],
                }],
                ..Default::default()
            }));
            let mut turn = 0;
            while let Ok(msg) = rx.recv().await {
                match msg {
                    SessionInput::Prompt { text, steer: false, .. } => {
                        turn += 1;
                        emit(AgentEvent::TurnStarted { prompt: None });
                        if limited_account && text.starts_with("limit") {
                            emit(AgentEvent::Quota { window: "five_hour".into(), utilization: Some(1.0), resets_ms: now_ms() + 3_600_000, rejected: true });
                            emit(AgentEvent::TurnFinished { outcome: TurnOutcome::RateLimited { resets_ms: now_ms() + 3_600_000 }, usage: Usage::default(), duration_ms: 1 });
                            continue;
                        }
                        emit(AgentEvent::Quota { window: "five_hour".into(), utilization: Some(0.2), resets_ms: now_ms() + 3_600_000, rejected: false });
                        emit(AgentEvent::BlockDone { block: format!("b{turn}"), kind: BlockKind::Text, text: format!("echo: {text}") });
                        emit(AgentEvent::TurnFinished {
                            outcome: TurnOutcome::Completed,
                            usage: Usage { input_tokens: 100, output_tokens: 20, cost_micros: 1_000, ..Default::default() },
                            duration_ms: 1,
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

struct Rig {
    o: Orchestrator,
    agent_rx: AsyncRx<mpsc::List<(ThreadId, AgentEvent)>>,
    internal_rx: AsyncRx<mpsc::List<Internal>>,
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (agent_tx, agent_rx) = mpsc::unbounded_async();
        let (internal_tx, internal_rx) = mpsc::unbounded_async();
        let (term_tx, _term_rx) = mpsc::bounded_blocking_async(crate::terminal::OUTPUT_CAPACITY);
        let (exit_tx, _exit_rx) = mpsc::unbounded_async();
        let mut o = Orchestrator::new(store, agent_tx, internal_tx, launcher(), term_tx, exit_tx);
        o.recover();
        let mut settings = o.store.settings().clone();
        settings.agents.instances.push(ProviderInstance {
            id: WORK.into(),
            kind: ProviderKind::Claude,
            name: "Claude (work)".into(),
            enabled: true,
            config_dir: dir.path().join("claude-work").display().to_string(),
            ..Default::default()
        });
        o.handle_command(Command::UpdateSettings { settings: Box::new(settings) }).unwrap();
        Self { o, agent_rx, internal_rx, dir }
    }

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
    }

    async fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(self) {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            self.pump().await;
            self.o.tick();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn call(&mut self, command: Command) -> Reply {
        self.o.handle_command(command).unwrap()
    }

    fn usage(&mut self, command: UsageCommand) -> Result<Reply, String> {
        self.o.handle_command(Command::Usage(command))
    }

    fn settings(&mut self, edit: impl FnOnce(&mut UsageSettings)) {
        let mut settings = self.o.usage_board().settings;
        edit(&mut settings);
        self.usage(UsageCommand::SetSettings(settings)).unwrap();
    }

    fn thread_in(&mut self, project: Option<ProjectId>, instance: &str) -> ThreadId {
        let config = ThreadConfig { instance: instance.into(), ..Default::default() };
        match self.call(Command::CreateThread { project, config, worktree: false }) {
            Reply::Thread(id) => id,
            other => panic!("{other:?}"),
        }
    }

    fn thread(&self, id: ThreadId) -> Thread {
        self.o.store.thread(id).cloned().unwrap()
    }

    fn items(&mut self, id: ThreadId) -> Vec<Item> {
        self.o.store.load_timeline(id).unwrap().items().to_vec()
    }

    fn send(&mut self, thread: ThreadId, text: &str) {
        self.call(Command::Send { thread, text: text.into(), attachments: vec![], steer: false });
    }

    async fn finish(&mut self, thread: ThreadId, text: &str) {
        let turns = self.thread(thread).turns;
        self.send(thread, text);
        self.until("the turn", |r| r.thread(thread).turns > turns && !r.thread(thread).status.is_busy()).await;
    }

    /// Both accounts have run a turn, so both are known to be installed
    /// and signed in.
    async fn warm_up(&mut self) {
        let work = self.thread_in(None, WORK);
        self.finish(work, "warm up work").await;
        let personal = self.thread_in(None, "");
        self.finish(personal, "warm up personal").await;
    }

    fn project(&mut self) -> ProjectId {
        let path = self.dir.path().join("project");
        std::fs::create_dir_all(&path).unwrap();
        match self.call(Command::AddProject { path: path.display().to_string() }) {
            Reply::Project(id) => id,
            other => panic!("{other:?}"),
        }
    }
}

fn run(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(test());
}

fn notices(items: &[Item]) -> Vec<(NoticeLevel, String)> {
    items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::Notice { level, text } => Some((*level, text.clone())),
            _ => None,
        })
        .collect()
}

fn replies(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::Assistant { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn automatic_failover_moves_the_thread_with_a_notice_and_a_handoff() {
    run(async || {
        let mut r = Rig::new();
        r.settings(|s| s.failover = FailoverPolicy::Auto);
        r.warm_up().await;
        let id = r.thread_in(None, "");
        r.finish(id, "first question").await;
        r.send(id, "limit: the long refactor");
        r.until("the move", |r| r.thread(id).config.instance == WORK && r.thread(id).status == ThreadStatus::Idle).await;

        let items = r.items(id);
        let notices = notices(&items);
        let moved = notices.iter().find(|(_, t)| t.contains("moved this thread to Claude (work)")).expect("a notice says so");
        assert_eq!(moved.0, NoticeLevel::Warning, "an automatic move stands out");
        let handoff = items.iter().find_map(|i| match &i.body {
            ItemBody::Handoff(h) => Some(h.clone()),
            _ => None,
        });
        let handoff = handoff.expect("the conversation is handed off");
        assert!(handoff.context.contains("first question"));
        assert!(!handoff.context.contains("long refactor"), "the limited turn is sent again, not summarized");
        // The limited message ran again on the work account, after the
        // handed-off context, under the same turn number.
        let last = replies(&items).pop().unwrap();
        assert!(last.contains("[Context handed off from Claude Code]"), "{last}");
        assert!(last.ends_with("limit: the long refactor"), "{last}");
        assert_eq!(r.thread(id).turns, 2);
        // The user's defaults are untouched.
        assert_eq!(r.o.store.defaults().instance, "");
    });
}

#[test]
fn ask_waits_for_the_user_and_never_moves_alone() {
    run(async || {
        let mut r = Rig::new();
        r.warm_up().await;
        let id = r.thread_in(None, "");
        r.send(id, "limit-ask");
        r.until("the limit", |r| matches!(r.thread(id).status, ThreadStatus::RateLimited { .. })).await;
        for _ in 0..5 {
            r.pump().await;
        }
        assert_eq!(r.thread(id).config.instance, "", "Ask never moves on its own");
        // The board knows the account is out and the other one has room.
        let board = r.o.usage_board();
        assert!(board.account("claude").unwrap().limited(now_ms()));
        assert!(!board.account(WORK).unwrap().limited(now_ms()));

        // The user takes the offer.
        r.usage(UsageCommand::Failover { thread: id, instance: WORK.into() }).unwrap();
        r.until("the resent turn", |r| r.thread(id).status == ThreadStatus::Idle).await;
        let items = r.items(id);
        assert!(notices(&items).iter().any(|(level, t)| *level == NoticeLevel::Info && t.contains("Moved this thread to Claude (work)")));
        assert_eq!(replies(&items).pop().as_deref(), Some("echo: limit-ask"), "no history before the limit: no handoff");
        // A thread that is not limited is not moved.
        let err = r.usage(UsageCommand::Failover { thread: id, instance: "claude".into() }).unwrap_err();
        assert!(err.contains("usage limit"), "{err}");
    });
}

#[test]
fn never_waits_and_the_queue_keeps_until_the_reset() {
    run(async || {
        let mut r = Rig::new();
        r.settings(|s| s.failover = FailoverPolicy::Never);
        r.warm_up().await;
        let id = r.thread_in(None, "");
        r.send(id, "limit-never");
        r.until("the limit", |r| matches!(r.thread(id).status, ThreadStatus::RateLimited { .. })).await;
        r.send(id, "queued behind the limit");
        for _ in 0..5 {
            r.pump().await;
            r.o.tick();
        }
        let thread = r.thread(id);
        assert!(matches!(thread.status, ThreadStatus::RateLimited { .. }));
        assert_eq!(thread.queued, 1, "the queue waits for the reset");
        assert_eq!(thread.config.instance, "");
    });
}

#[test]
fn a_pinned_project_starts_on_its_account_and_stays_there() {
    run(async || {
        let mut r = Rig::new();
        let project = r.project();
        r.settings(|s| {
            s.failover = FailoverPolicy::Auto;
            s.set_pin(PinTarget::Project(project), Some(WORK.into()));
        });
        r.warm_up().await;
        // New threads of the project start on the pinned account…
        let pinned = r.thread_in(Some(project), "");
        assert_eq!(r.thread(pinned).config.instance, WORK);
        assert_eq!(r.o.store.defaults().instance, "", "…without changing the defaults");
        // …and a thread of it that hits a limit is only offered a move.
        r.settings(|s| s.set_pin(PinTarget::Project(project), Some("claude".into())));
        let id = r.thread_in(Some(project), WORK);
        assert_eq!(r.thread(id).config.instance, "", "pinned to the built-in account");
        r.send(id, "limit-pinned");
        r.until("the limit", |r| matches!(r.thread(id).status, ThreadStatus::RateLimited { .. })).await;
        for _ in 0..5 {
            r.pump().await;
        }
        assert_eq!(r.thread(id).config.instance, "");
    });
}

#[test]
fn turns_land_in_the_ledger_and_its_stats() {
    run(async || {
        let mut r = Rig::new();
        r.warm_up().await;
        let id = r.thread_in(None, WORK);
        r.finish(id, "count me").await;
        let Ok(Reply::Usage(reply)) = r.usage(UsageCommand::Stats(UsageQuery { tz: "UTC".into(), ..Default::default() })) else {
            panic!()
        };
        let UsageReply::Stats(stats) = *reply else { panic!() };
        assert_eq!(stats.totals.turns, 3);
        assert_eq!(stats.totals.input_tokens, 300);
        assert_eq!(stats.totals.cost_micros, 3_000);
        let accounts: Vec<(&str, u32)> = stats.accounts.iter().map(|a| (a.key.as_str(), a.totals.turns)).collect();
        assert_eq!(accounts, [(WORK, 2), ("claude", 1)]);
        assert_eq!(stats.models[0].label, "Opus 5.5", "the default model is named from the catalog");
        assert_eq!(stats.days.len(), 1);
        let only = UsageQuery { instance: Some("claude".into()), tz: "UTC".into(), ..Default::default() };
        let Ok(Reply::Text(csv)) = r.usage(UsageCommand::Export(only)) else { panic!() };
        assert_eq!(csv.lines().count(), 2, "{csv}");
        assert!(csv.contains(",warm up personal,"), "{csv}");
        // The board lists each account's top consumers.
        let board = r.o.usage_board();
        let top = &board.account(WORK).unwrap().top;
        assert_eq!(top.first().map(|c| c.tokens), Some(120));
        // The ledger survives a restart.
        r.o.shutdown();
        let (ledger, backfill) = crate::usage::ledger::Ledger::open(r.dir.path());
        assert!(!backfill);
        assert_eq!(ledger.rows().len(), 3);
    });
}

#[test]
fn limits_are_read_only_while_a_client_is_in_front() {
    run(async || {
        let mut r = Rig::new();
        r.warm_up().await;
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        r.o.usage_state().reader = Some(Arc::new(move |kind, launch| {
            counter.fetch_add(1, Ordering::SeqCst);
            let work = launch.instance == WORK;
            Box::pin(async move {
                assert_eq!(kind, ProviderKind::Claude);
                if work {
                    return Err(ReadError::no_plan("Uses an API key."));
                }
                Ok(Reading {
                    windows: vec![
                        QuotaWindow { window: "five_hour".into(), utilization: Some(0.42), resets_ms: now_ms() + 3_600_000, rejected: false },
                        QuotaWindow { window: "seven_day".into(), utilization: Some(0.1), resets_ms: now_ms() + 86_400_000, rejected: false },
                    ],
                    plan: Some("Max 20x".into()),
                })
            })
        }));
        for _ in 0..10 {
            r.o.tick();
            r.pump().await;
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0, "nothing is read while every window is in the background");

        r.usage(UsageCommand::Seen).unwrap();
        r.until("both accounts read", |_| reads.load(Ordering::SeqCst) == 2).await;
        r.until("the board", |r| r.o.usage_board().account("claude").is_some_and(|a| a.source == QuotaSource::Provider)).await;
        let board = r.o.usage_board();
        let personal = board.account("claude").unwrap();
        assert_eq!(personal.plan.as_deref(), Some("Max 20x"));
        assert_eq!(personal.windows.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5-hour", "Weekly"]);
        assert_eq!(personal.tightest().unwrap().window.utilization, Some(0.42));
        let work = board.account(WORK).unwrap();
        assert_eq!(work.status, QuotaStatus::NoPlan("Uses an API key.".into()));

        // In front again soon after: fresh readings are not read again.
        r.usage(UsageCommand::Seen).unwrap();
        for _ in 0..5 {
            r.o.tick();
            r.pump().await;
        }
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        // An explicit refresh reads now.
        r.usage(UsageCommand::Refresh { instance: Some("claude".into()) }).unwrap();
        r.until("the refresh", |_| reads.load(Ordering::SeqCst) == 3).await;

        // Readings and settings survive a restart.
        r.settings(|s| s.alert_80 = false);
        r.o.shutdown();
        let Rig { o, dir, .. } = r;
        drop(o);
        let store = Store::open(dir.path()).unwrap();
        let (agent_tx, _a) = mpsc::unbounded_async();
        let (internal_tx, _i) = mpsc::unbounded_async();
        let (term_tx, _t) = mpsc::bounded_blocking_async(crate::terminal::OUTPUT_CAPACITY);
        let (exit_tx, _e) = mpsc::unbounded_async();
        let mut o = Orchestrator::new(store, agent_tx, internal_tx, launcher(), term_tx, exit_tx);
        o.usage_state();
        assert!(!o.usage_board().settings.alert_80);
        assert_eq!(o.usage_board().account("claude").map(|a| a.windows.len()), Some(2), "both windows are remembered");
    });
}
