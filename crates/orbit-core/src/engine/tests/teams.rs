//! Agent teams and races end to end, against a scripted fake agent in real
//! git repositories: dependency waits, slots and file scopes, mail at turn
//! boundaries and urgent steering, notes in briefs, the circuit breaker,
//! merging workers, a race of three with tests, a judge and keep-winner,
//! and taking a hunk from one candidate into another.
//!
//! The fake's behavior is chosen by markers in its prompt:
//! `#hold:<name>` works until [`release`] is called with that name,
//! `#fail` fails the turn, `#write:<file>:<text>` writes a file, `#cost`
//! reports $0.60 of spend, `#race` writes `answer.txt` holding its model's
//! name; team mail is acknowledged and a judge names candidate B.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_proto::*;

use super::{Harness, git};
use crate::engine::Launcher;
use crate::provider::{AgentEvent, BlockKind, McpServer, SessionHandle, SessionInput, TurnOutcome};

/// Every prompt a fake session received: thread, text, steered.
static PROMPTS: Mutex<Vec<(ThreadId, String, bool)>> = Mutex::new(Vec::new());
/// The MCP servers each session was started with.
static SPECS: Mutex<Vec<(ThreadId, Vec<McpServer>)>> = Mutex::new(Vec::new());
static RELEASED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn release(name: &str) {
    RELEASED.lock().unwrap().get_or_insert_with(HashSet::new).insert(name.to_owned());
}

fn released(name: &str) -> bool {
    RELEASED.lock().unwrap().as_ref().is_some_and(|s| s.contains(name))
}

fn prompts(thread: ThreadId) -> Vec<(String, bool)> {
    PROMPTS.lock().unwrap().iter().filter(|(t, ..)| *t == thread).map(|(_, p, s)| (p.clone(), *s)).collect()
}

fn marker<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let at = text.find(tag)? + tag.len();
    Some(text[at..].split_whitespace().next().unwrap_or(""))
}

fn team_launcher() -> Launcher {
    // Team members get the `orbit` MCP server only when the command line
    // exists; any file stands in for it here.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("ORBIT_CLI", std::env::current_exe().unwrap()) });
    Box::new(|_kind, spec, events| {
        SPECS.lock().unwrap().push((spec.thread, spec.mcp.clone()));
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            let reply = |text: String| {
                emit(AgentEvent::BlockDone { block: format!("b{}", next_raw_id()), kind: BlockKind::Text, text });
            };
            let finish = |cost: u64| {
                emit(AgentEvent::TurnFinished {
                    outcome: TurnOutcome::Completed,
                    usage: Usage { input_tokens: 100, output_tokens: 40, cost_micros: cost, ..Default::default() },
                    duration_ms: 1200,
                });
            };
            emit(AgentEvent::Ready { session_id: format!("s-{thread}"), model: None });
            while let Ok(msg) = rx.recv().await {
                let text = match msg {
                    SessionInput::Prompt { text, steer, .. } => {
                        PROMPTS.lock().unwrap().push((thread, text.clone(), steer));
                        if steer {
                            continue;
                        }
                        text
                    }
                    SessionInput::Shutdown => return,
                    _ => continue,
                };
                emit(AgentEvent::TurnStarted { prompt: None });
                if text.contains("You judge a race") {
                    reply("Winner: B\nReason: Only B passes its tests.".into());
                    finish(0);
                } else if text.starts_with("<orbit-team-mail") {
                    reply("Got the team mail.".into());
                    finish(0);
                } else if let Some(name) = marker(&text, "#hold:") {
                    let name = name.to_owned();
                    emit(AgentEvent::Delta { block: "h".into(), kind: BlockKind::Text, text: "working".into() });
                    let mut interrupted = false;
                    while !released(&name) {
                        match rx.try_recv() {
                            Ok(SessionInput::Prompt { text, steer, .. }) => PROMPTS.lock().unwrap().push((thread, text, steer)),
                            Ok(SessionInput::Interrupt) => {
                                interrupted = true;
                                break;
                            }
                            Ok(SessionInput::Shutdown) => return,
                            _ => tokio::time::sleep(Duration::from_millis(10)).await,
                        }
                    }
                    if interrupted {
                        emit(AgentEvent::TurnFinished { outcome: TurnOutcome::Interrupted, usage: Usage::default(), duration_ms: 1 });
                    } else {
                        emit(AgentEvent::BlockDone { block: "h".into(), kind: BlockKind::Text, text: format!("Finished {name}.") });
                        finish(0);
                    }
                } else if text.contains("#fail") {
                    emit(AgentEvent::TurnFinished {
                        outcome: TurnOutcome::Failed { message: "The model is overloaded.".into() },
                        usage: Usage::default(),
                        duration_ms: 1,
                    });
                } else if let Some(spec_text) = marker(&text, "#write:") {
                    let (file, body) = spec_text.split_once(':').unwrap_or((spec_text, "x"));
                    std::fs::write(spec.cwd.join(file), format!("{body}\n")).unwrap();
                    reply(format!("Wrote {file}."));
                    finish(0);
                } else if text.contains("#race") {
                    std::fs::write(spec.cwd.join("answer.txt"), format!("{}\n", spec.config.model)).unwrap();
                    reply(format!("Answered as {}.", spec.config.model));
                    finish(250_000);
                } else {
                    reply(format!("echo: {}", text.lines().last().unwrap_or_default()));
                    finish(if text.contains("#cost") { 600_000 } else { 0 });
                }
            }
        });
        SessionHandle { input }
    })
}

/// The teams and races clients see, kept from the event stream.
#[derive(Default)]
struct Board {
    teams: HashMap<TeamId, Team>,
    races: HashMap<RaceId, Race>,
}

struct Rig {
    h: Harness,
    board: Board,
    repo: tempfile::TempDir,
    project: ProjectId,
}

impl Rig {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let dir = tempfile::tempdir().unwrap();
        let mut h = Harness::open_with(dir.path().to_owned(), team_launcher());
        h._dir = Some(dir);
        let Reply::Project(project) = h.call(Command::AddProject { path: repo.path().display().to_string() }) else { panic!() };
        Self { h, board: Board::default(), repo, project }
    }

    fn pump(&mut self) -> bool {
        match self.h.events.try_recv() {
            Ok(env) => {
                match &env.event {
                    Event::Team(TeamEvent::Team(t)) => {
                        self.board.teams.insert(t.id, (**t).clone());
                    }
                    Event::Team(TeamEvent::Race(r)) => {
                        self.board.races.insert(r.id, (**r).clone());
                    }
                    _ => {}
                }
                self.h.apply(env.event);
                true
            }
            Err(_) => false,
        }
    }

    fn wait<T>(&mut self, what: &str, f: impl Fn(&Rig) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            if Instant::now() >= deadline {
                let states: Vec<_> = self
                    .board
                    .teams
                    .values()
                    .flat_map(|t| t.workers.iter().map(|w| (w.key.clone(), w.state.clone(), w.thread.map(|t| self.h.thread(t).status))))
                    .collect();
                panic!("timed out waiting for {what}: {states:?}");
            }
            if !self.pump() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Applies events for a while, for checks that something did not happen.
    fn idle_for(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            if !self.pump() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn call(&self, command: TeamCommand) -> Result<Reply, String> {
        self.h.engine.call(Command::Team(command)).recv().unwrap()
    }

    fn start_team(&mut self, limits: TeamLimits) -> (TeamId, ThreadId) {
        let start = ThreadStart {
            project: Some(self.project),
            worktree: true,
            prompt: Some("Lead the team".into()),
            config: ThreadConfig { access: Access::AutoEdit, ..Default::default() },
            ..Default::default()
        };
        let Reply::Thread(architect) = self.call(TeamCommand::Start { start, limits, worker_config: None }).unwrap() else {
            panic!()
        };
        let team = self.wait("the team", |r| r.board.teams.values().find(|t| t.architect == architect).map(|t| t.id));
        self.wait("the architect's first turn", |r| {
            let t = r.h.thread(architect);
            (t.turns == 1 && t.status == ThreadStatus::Idle && t.workspace.branch.is_some()).then_some(())
        });
        (team, architect)
    }

    fn spawn(&self, actor: ThreadId, title: &str, brief: &str, deps: &[&str], scope: &[&str]) -> String {
        let reply = self.call(TeamCommand::SpawnWorker {
            actor,
            title: title.into(),
            brief: brief.into(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            config: None,
        });
        match reply {
            Ok(Reply::Team(TeamReply::Worker { key, .. })) => key,
            other => panic!("{other:?}"),
        }
    }

    fn worker(&self, team: TeamId, key: &str) -> Worker {
        self.board.teams.get(&team).and_then(|t| t.worker(key)).cloned().unwrap_or_default()
    }

    fn state(&self, team: TeamId, key: &str) -> WorkerState {
        self.worker(team, key).state
    }
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    std::fs::write(dir.join("expected.txt"), "m2\n").unwrap();
    std::fs::write(dir.join("answer.txt"), "none\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n")
}

#[test]
fn workers_wait_on_dependencies_and_their_summaries_reach_the_architect() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits::default());
    // The architect heard about the team with its first message, which the
    // timeline shows as the user wrote it.
    let first = prompts(architect);
    assert!(first[0].0.starts_with("<orbit-team role=\"architect\""), "{}", first[0].0);
    assert!(first[0].0.ends_with("Lead the team"));
    assert!(r.h.texts(1).contains(&"user: Lead the team".to_owned()), "{:?}", r.h.texts(1));
    let arch_mcp = SPECS.lock().unwrap().iter().find(|(t, _)| *t == architect).map(|(_, m)| m.clone()).unwrap();
    assert_eq!(arch_mcp[0].args, ["mcp"]);
    assert!(arch_mcp[0].env.contains(&("ORBIT_THREAD".into(), architect.to_string())));
    assert!(arch_mcp[0].config_file.as_ref().is_some_and(|f| f.is_file()), "Claude reads its MCP config from a file");

    let w1 = r.spawn(architect, "API", "Build the API #hold:deps-api", &[], &[]);
    let w2 = r.spawn(architect, "UI", "Build the UI on the API", &["w1"], &[]);
    assert_eq!((w1.as_str(), w2.as_str()), ("w1", "w2"));
    r.wait("w1 to work and w2 to wait on it", |r| {
        (r.state(team, "w1") == WorkerState::Working
            && r.state(team, "w2") == WorkerState::Waiting(WaitReason::Dependency { on: vec!["w1".into()] }))
        .then_some(())
    });
    let w1_thread = r.worker(team, "w1").thread.unwrap();
    // A child thread in its own worktree, off the architect's branch.
    r.wait("w1's worktree", |r| r.h.thread(w1_thread).workspace.worktree.clone());
    let child = r.h.thread(w1_thread);
    assert_eq!(child.parent, Some(architect));
    assert_ne!(child.workspace.worktree, r.h.thread(architect).workspace.worktree);
    let brief = r.wait("w1's brief", |_| prompts(w1_thread).first().map(|p| p.0.clone()));
    assert!(brief.starts_with("<orbit-team role=\"worker\" key=\"w1\""), "{brief}");
    assert!(brief.contains("# API (w1)"), "{brief}");

    release("deps-api");
    r.wait("w1 to finish and w2 to start", |r| {
        (r.state(team, "w1") == WorkerState::Done && r.state(team, "w2") == WorkerState::Working).then_some(())
    });
    assert_eq!(r.worker(team, "w1").summary, "Finished deps-api.");
    let w2_thread = r.worker(team, "w2").thread.unwrap();
    let brief = r.wait("w2's brief", |_| prompts(w2_thread).first().map(|p| p.0.clone()));
    assert!(brief.contains("- w1 (API): Finished deps-api."), "w2 builds on w1's summary: {brief}");
    r.wait("w2 to finish", |r| (r.state(team, "w2") == WorkerState::Done).then_some(()));

    // Both summaries reach the architect as mail between its turns.
    let mail = r.wait("the architect's mail", |_| {
        let all: String = prompts(architect).iter().filter(|p| p.0.starts_with("<orbit-team-mail")).map(|p| p.0.clone()).collect();
        (all.contains("w1 finished") && all.contains("w2 finished")).then_some(all)
    });
    assert!(mail.contains("kind=\"done\""), "{mail}");
    assert!(mail.contains("Finished deps-api."), "{mail}");
    r.wait("the mail marked delivered", |r| r.board.teams[&team].mail.iter().all(|m| m.delivered_ms.is_some()).then_some(()));
}

#[test]
fn slots_and_file_scopes_explain_why_a_worker_waits() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits { max_workers: 2, ..Default::default() });
    r.spawn(architect, "Users API", "#hold:scope-a", &[], &["src/api"]);
    r.spawn(architect, "Users model", "Edit the users file", &[], &["src/api/users.rs"]);
    r.spawn(architect, "Docs", "#hold:scope-c", &[], &["docs"]);
    r.wait("w2 to wait on w1's scope", |r| {
        (r.state(team, "w2") == WorkerState::Waiting(WaitReason::FileScope { path: "src/api/users.rs".into(), with: "w1".into() })
            && r.state(team, "w3") == WorkerState::Working)
        .then_some(())
    });
    assert_eq!(WaitReason::FileScope { path: "src/api/users.rs".into(), with: "w1".into() }.describe(), "w1 is editing src/api/users.rs");
    r.spawn(architect, "Tests", "#hold:scope-d", &[], &[]);
    r.wait("w4 to wait for a slot", |r| {
        matches!(r.state(team, "w4"), WorkerState::Waiting(WaitReason::NoSlot { limit: 2, global: false, .. })).then_some(())
    });
    release("scope-a");
    r.wait("w2 to start once w1 is done", |r| {
        (r.state(team, "w1") == WorkerState::Done && r.state(team, "w2") != WorkerState::Waiting(WaitReason::FileScope {
            path: "src/api/users.rs".into(),
            with: "w1".into(),
        }))
        .then_some(())
    });
    release("scope-c");
    release("scope-d");
    r.wait("everyone done", |r| {
        ["w1", "w2", "w3", "w4"].iter().all(|k| r.state(team, k) == WorkerState::Done).then_some(())
    });
}

#[test]
fn mail_waits_for_a_turn_boundary_and_urgent_mail_steers_at_most_once_a_minute() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits::default());
    r.spawn(architect, "Long job", "#hold:mail-long", &[], &[]);
    r.wait("w1 to work", |r| r.worker(team, "w1").thread.filter(|t| !prompts(*t).is_empty()));
    let w1 = r.worker(team, "w1").thread.unwrap();
    let message = |text: &str, urgent: bool| TeamCommand::Message {
        actor: Some(architect),
        team: None,
        to: "w1".into(),
        kind: MailKind::Message,
        text: text.into(),
        urgent,
    };
    r.call(message("Use port 4000.", false)).unwrap();
    r.idle_for(1500);
    assert_eq!(prompts(w1).len(), 1, "ordinary mail never interrupts a turn");
    r.call(message("Stop touching main.rs!", true)).unwrap();
    r.wait("the urgent steer", |_| prompts(w1).iter().find(|p| p.1).map(|p| p.0.clone()));
    let steer = prompts(w1).into_iter().find(|p| p.1).unwrap().0;
    assert!(steer.contains("Stop touching main.rs!") && steer.contains("urgent=\"true\""), "{steer}");
    assert!(!steer.contains("port 4000"), "only urgent mail steers");
    r.call(message("Also: rename the flag!", true)).unwrap();
    r.idle_for(1500);
    assert_eq!(prompts(w1).iter().filter(|p| p.1).count(), 1, "one steer a minute");

    release("mail-long");
    let delivered = r.wait("the rest at the turn boundary", |_| {
        prompts(w1).into_iter().find(|p| !p.1 && p.0.starts_with("<orbit-team-mail")).map(|p| p.0)
    });
    assert!(delivered.contains("count=\"2\""), "{delivered}");
    assert!(delivered.contains("Use port 4000.") && delivered.contains("Also: rename the flag!"), "{delivered}");
    assert!(delivered.contains("from=\"architect\""));
    r.wait("w1 done after reading its mail", |r| (r.state(team, "w1") == WorkerState::Done).then_some(()));
    assert_eq!(r.worker(team, "w1").summary, "Got the team mail.");
    // Members may reach only their own team.
    let other = r.call(TeamCommand::Message { actor: Some(w1), team: None, to: "w9".into(), kind: MailKind::Message, text: "hi".into(), urgent: false });
    assert!(other.unwrap_err().contains("no one called"));
}

#[test]
fn notes_reach_new_worker_briefs_newest_first() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits::default());
    for i in 1..=7 {
        r.call(TeamCommand::PostNote { actor: Some(architect), team: None, text: format!("decision {i}") }).unwrap();
    }
    let Reply::Team(TeamReply::Notes { notes, more }) =
        r.call(TeamCommand::ReadNotes { actor: Some(architect), team: None, limit: 3 }).unwrap()
    else {
        panic!()
    };
    assert_eq!(notes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["decision 7", "decision 6", "decision 5"]);
    assert_eq!((more, notes[0].author_label.as_str()), (4, "architect"));
    r.spawn(architect, "Read the notes", "Do it", &[], &[]);
    let thread = r.wait("w1 to start", |r| r.worker(team, "w1").thread);
    let brief = r.wait("its brief", |_| prompts(thread).first().map(|p| p.0.clone()));
    assert!(brief.contains("[architect] decision 7") && brief.contains("[architect] decision 3"), "{brief}");
    assert!(!brief.contains("decision 2"), "{brief}");
    assert!(brief.contains("2 more notes"), "{brief}");
    // A worker posts too; the user can read the board.
    r.wait("w1's first turn", |r| (r.state(team, "w1") == WorkerState::Done).then_some(()));
    r.call(TeamCommand::PostNote { actor: Some(thread), team: None, text: "GET /users returns {id,name}".into() }).unwrap();
    let Reply::Team(TeamReply::Notes { notes, .. }) = r.call(TeamCommand::ReadNotes { actor: None, team: Some(team), limit: 1 }).unwrap()
    else {
        panic!()
    };
    assert_eq!((notes[0].author_label.as_str(), notes[0].text.as_str()), ("w1", "GET /users returns {id,name}"));
}

#[test]
fn the_circuit_breaker_stops_spawning_until_the_user_resumes() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits { max_failures: 2, ..Default::default() });
    r.spawn(architect, "Flaky one", "#fail", &[], &[]);
    r.spawn(architect, "Flaky two", "#fail", &[], &[]);
    r.wait("the breaker", |r| r.board.teams.get(&team).and_then(|t| t.breaker.clone()));
    assert_eq!(r.board.teams[&team].breaker.as_ref().unwrap().reason, "2 workers failed");
    assert!(matches!(r.state(team, "w1"), WorkerState::Failed { .. }));
    r.spawn(architect, "Next", "Carry on", &[], &[]);
    r.idle_for(800);
    assert_eq!(r.state(team, "w3"), WorkerState::Waiting(WaitReason::Breaker));
    r.wait("the user is told", |r| {
        r.h.items.values().any(|i| matches!(&i.body, ItemBody::Notice { level: NoticeLevel::Warning, text } if text.starts_with("Spawning paused: 2 workers failed"))).then_some(())
    });
    assert!(r.h.thread(architect).unread);
    // The architect hears about it too, and the failures, as mail.
    r.wait("the architect's mail", |_| {
        prompts(architect).iter().any(|p| p.0.contains("Orbit paused spawning") && p.0.contains("failed “Flaky")).then_some(())
    });
    r.call(TeamCommand::ResetBreaker { team, limits: Some(TeamLimits { max_failures: 5, ..Default::default() }) }).unwrap();
    r.wait("w3 to run after the reset", |r| (r.state(team, "w3") == WorkerState::Done).then_some(()));
}

#[test]
fn spending_past_the_limit_trips_the_breaker() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits { max_cost_micros: 500_000, ..Default::default() });
    r.spawn(architect, "Expensive", "Think hard #cost", &[], &[]);
    let breaker = r.wait("the breaker", |r| r.board.teams.get(&team).and_then(|t| t.breaker.clone()));
    assert_eq!(breaker.reason, "the team spent $0.60 of its $0.50 limit");
}

#[test]
fn finished_workers_merge_into_the_architect_and_conflicts_are_reported() {
    let mut r = Rig::new();
    let (team, architect) = r.start_team(TeamLimits::default());
    r.spawn(architect, "Feature", "#write:feature.txt:hello", &[], &[]);
    r.spawn(architect, "Clash one", "#write:shared.txt:one", &[], &[]);
    r.spawn(architect, "Clash two", "#write:shared.txt:two", &[], &[]);
    r.wait("all three done", |r| ["w1", "w2", "w3"].iter().all(|k| r.state(team, k) == WorkerState::Done).then_some(()));
    let merge = |worker: &str, dry_run: bool| TeamCommand::MergeWorker { actor: Some(architect), team: None, worker: worker.into(), dry_run };
    let Reply::Merge(preview) = r.call(merge("w1", true)).unwrap() else { panic!() };
    assert!(preview.merged.is_none() && preview.conflicts.is_empty());
    assert!(preview.files.iter().any(|f| f.path == "feature.txt"), "{preview:?}");
    let arch_dir = std::path::PathBuf::from(r.h.thread(architect).workspace.worktree.unwrap());
    assert!(!arch_dir.join("feature.txt").exists(), "a dry run changes nothing");
    let Reply::Merge(done) = r.call(merge("w1", false)).unwrap() else { panic!() };
    assert!(done.merged.is_some());
    assert_eq!(read(&arch_dir.join("feature.txt")), "hello\n");
    r.wait("w1 merged", |r| (r.state(team, "w1") == WorkerState::Merged).then_some(()));

    let Reply::Merge(_) = r.call(merge("w2", false)).unwrap() else { panic!() };
    let Reply::Merge(clash) = r.call(merge("w3", false)).unwrap() else { panic!() };
    assert_eq!(clash.conflicts, ["shared.txt"]);
    assert!(clash.merged.is_none());
    assert_eq!(read(&arch_dir.join("shared.txt")), "one\n", "a conflicting merge changes nothing");
    r.wait("the conflict on record", |r| {
        (r.state(team, "w3") == WorkerState::Done && r.worker(team, "w3").merge.is_some_and(|m| m.conflicts == ["shared.txt"])).then_some(())
    });
    // Only the architect or the worker's parent may merge.
    let w2_thread = r.worker(team, "w2").thread.unwrap();
    let refused = r.call(TeamCommand::MergeWorker { actor: Some(w2_thread), team: None, worker: "w3".into(), dry_run: true });
    assert!(refused.unwrap_err().contains("Only the architect"));
}

#[test]
fn a_race_of_three_runs_tests_gets_a_verdict_keeps_the_winner_and_takes_a_hunk() {
    let mut r = Rig::new();
    let configs: Vec<ThreadConfig> = ["m1", "m2", "m3"].iter().map(|m| ThreadConfig { model: (*m).into(), ..Default::default() }).collect();
    let reply = r.call(TeamCommand::StartRace {
        project: r.project,
        prompt: "Answer the question #race".into(),
        configs,
        test_command: Some("git diff --no-index --quiet answer.txt expected.txt".into()),
        judge: true,
        base: "main".into(),
    });
    let Ok(Reply::Team(TeamReply::Race(race))) = reply else { panic!("{reply:?}") };
    let settled = r.wait("every candidate tested and judged", |r| {
        let race = r.board.races.get(&race)?;
        (race.settled() && matches!(race.judge, JudgeState::Verdict { .. })).then(|| race.clone())
    });
    let by_label = |l: &str| settled.candidates.iter().find(|c| c.label == l).unwrap().clone();
    let (a, b, c) = (by_label("A"), by_label("B"), by_label("C"));
    assert!(matches!(b.test, TestState::Passed { .. }), "{:?}", b.test);
    assert!(matches!(a.test, TestState::Failed { code: Some(1), .. }), "{:?}", a.test);
    assert!(matches!(c.test, TestState::Failed { .. }));
    assert_eq!(b.finished, Some(CandidateEnd::Completed));
    assert_eq!((b.usage.cost_micros, b.duration_ms), (250_000, 1200));
    assert_eq!(b.files.iter().map(|f| (f.path.as_str(), f.additions, f.deletions)).collect::<Vec<_>>(), [("answer.txt", 1, 1)]);
    assert_eq!(settled.judge, JudgeState::Verdict { winner: b.thread, reason: "Only B passes its tests.".into() });
    assert_eq!(r.h.thread(a.thread).title, "Answer the question #race (A)");

    // A against B, file by file; taking B's hunk into A makes A's answer B's.
    let Reply::Diff(raw) = r.call(TeamCommand::CompareCandidates { a: a.thread, b: b.thread }).unwrap() else { panic!() };
    let files = orbit_proto::diff::parse(&raw);
    assert_eq!(files.len(), 1, "{raw}");
    assert_eq!(files[0].path(), "answer.txt");
    let patch = hunk_patch(&files[0], 0);
    r.call(TeamCommand::ApplyPatch { thread: a.thread, patch: patch.clone() }).unwrap();
    let a_dir = std::path::PathBuf::from(r.h.thread(a.thread).workspace.worktree.unwrap());
    assert_eq!(read(&a_dir.join("answer.txt")), "m2\n");
    assert!(r.call(TeamCommand::ApplyPatch { thread: a.thread, patch }).is_err(), "a hunk already taken no longer applies");

    let Reply::Merge(merged) = r.call(TeamCommand::KeepWinner { race, winner: b.thread }).unwrap() else { panic!() };
    assert!(merged.merged.is_some());
    git(r.repo.path(), &["checkout", "-q", "main"]);
    assert_eq!(read(&r.repo.path().join("answer.txt")), "m2\n", "the winner is on main");
    r.wait("the race decided and the rest archived", |r| {
        let race = r.board.races.get(&race)?;
        (race.state == RaceState::Decided { winner: b.thread }
            && r.h.thread(a.thread).archived
            && r.h.thread(c.thread).archived
            && !r.h.thread(b.thread).archived)
            .then_some(())
    });
}

#[test]
fn teams_and_races_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let (team, architect);
    {
        let mut r = Rig { h: Harness::open_with(dir.path().to_owned(), team_launcher()), board: Board::default(), repo, project: ProjectId(0) };
        let Reply::Project(p) = r.h.call(Command::AddProject { path: r.repo.path().display().to_string() }) else { panic!() };
        r.project = p;
        (team, architect) = r.start_team(TeamLimits::default());
        r.call(TeamCommand::PostNote { actor: Some(architect), team: None, text: "kept".into() }).unwrap();
        r.h.engine.shutdown();
    }
    let h = Harness::open_with(dir.path().to_owned(), team_launcher());
    let Reply::Team(TeamReply::Team(t)) = h.call(Command::Team(TeamCommand::Status { actor: Some(architect), team: None })) else { panic!() };
    assert_eq!((t.id, t.notes[0].text.as_str()), (team, "kept"));
}
