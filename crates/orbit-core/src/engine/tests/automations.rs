//! Automations against the fake agent, with the clock in the test's hands:
//! scheduled runs, missed runs, restarts, forge events, chaining, the
//! concurrency cap and timeouts.

use std::time::Duration;

use crossfire::{AsyncRx, mpsc};
use orbit_proto::*;

use super::fake_launcher;
use crate::engine::automations::{MAX_CONCURRENT, Observed};
use crate::engine::{Internal, Orchestrator};
use crate::provider::AgentEvent;
use crate::store::Store;

const MIN: u64 = 60_000;
/// 2026-11-25 00:00 UTC.
const T0: u64 = 1_795_564_800_000;

struct Rig {
    o: Orchestrator,
    agent_rx: AsyncRx<mpsc::List<(ThreadId, AgentEvent)>>,
    internal_rx: AsyncRx<mpsc::List<Internal>>,
    dir: std::path::PathBuf,
    _keep: Option<tempfile::TempDir>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Self::open(dir.path().to_owned(), T0);
        rig._keep = Some(dir);
        rig
    }

    /// Opens (or reopens) the engine on `dir` at time `now`.
    fn open(dir: std::path::PathBuf, now: u64) -> Self {
        let store = Store::open(&dir).unwrap();
        let (agent_tx, agent_rx) = mpsc::unbounded_async();
        let (internal_tx, internal_rx) = mpsc::unbounded_async();
        let (term_tx, _term_rx) = mpsc::bounded_blocking_async(crate::terminal::OUTPUT_CAPACITY);
        let (exit_tx, _exit_rx) = mpsc::unbounded_async();
        let mut o = Orchestrator::new(store, agent_tx, internal_tx, fake_launcher(), term_tx, exit_tx);
        o.automations.clock = Some(now);
        o.recover();
        Self { o, agent_rx, internal_rx, dir, _keep: None }
    }

    /// Closes the engine and opens it again at `now`.
    fn restart(self, now: u64) -> Self {
        let Rig { mut o, dir, _keep, .. } = self;
        o.shutdown();
        drop(o);
        let mut rig = Self::open(dir, now);
        rig._keep = _keep;
        rig
    }

    async fn pump(&mut self) {
        for _ in 0..30 {
            tokio::task::yield_now().await;
            while let Ok((thread, event)) = self.agent_rx.try_recv() {
                self.o.handle_agent(thread, event);
            }
            while let Ok(msg) = self.internal_rx.try_recv() {
                self.o.handle_internal(msg);
            }
        }
    }

    /// Pumps until `done` holds, with real time for background tasks.
    async fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(self) {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            self.pump().await;
            self.o.tick();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Sets the clock and ticks.
    async fn at(&mut self, now: u64) {
        self.o.automations.clock = Some(now);
        self.o.tick();
        self.pump().await;
    }

    fn save(&mut self, spec: AutomationSpec) -> AutomationId {
        match self.o.handle_command(Command::Automation(AutomationCommand::Save { id: None, spec: Box::new(spec) })) {
            Ok(Reply::Automation(AutomationReply::Saved(id))) => id,
            other => panic!("{other:?}"),
        }
    }

    fn automation(&self, id: AutomationId) -> Automation {
        self.o.store.automations().get(id).cloned().unwrap()
    }

    /// Runs newest first.
    fn runs(&self, id: AutomationId) -> Vec<AutomationRun> {
        self.o.store.automations().runs(id).cloned().collect()
    }

    fn project(&mut self) -> ProjectId {
        let path = self.dir.join("project");
        std::fs::create_dir_all(&path).unwrap();
        match self.o.handle_command(Command::AddProject { path: path.display().to_string() }) {
            Ok(Reply::Project(id)) => id,
            other => panic!("{other:?}"),
        }
    }
}

fn run(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(test());
}

fn spec(prompt: &str, trigger: Trigger) -> AutomationSpec {
    AutomationSpec {
        name: "Test".into(),
        target: RunTarget::Project,
        prompt: prompt.into(),
        triggers: vec![trigger],
        ..Default::default()
    }
}

fn every(minutes: u32) -> Trigger {
    Trigger::Interval { minutes }
}

fn finished(r: &Rig, id: AutomationId) -> bool {
    r.runs(id).first().is_some_and(|run| !run.status.is_active())
}

fn pr(number: u64, failed: u16) -> PullRequest {
    PullRequest {
        number,
        title: format!("Change {number}"),
        author: "me".into(),
        state: PrState::Open,
        head: format!("feature-{number}"),
        url: format!("https://example.com/pr/{number}"),
        checks: ChecksSummary { passed: 3, failed, pending: 0, other: 0 },
        ..Default::default()
    }
}

#[test]
fn a_scheduled_run_starts_a_thread_and_records_its_result() {
    run(async || {
        let mut r = Rig::new();
        let id = r.save(spec("Summarize {date}", every(30)));
        let next = r.automation(id).state.next_ms[0];
        assert_eq!(next, T0 + 30 * MIN);
        r.at(next - 1).await;
        assert!(r.runs(id).is_empty(), "nothing before its time");
        r.at(next + 50_000).await;
        r.until("the run to finish", |r| finished(r, id)).await;
        let run = r.runs(id)[0].clone();
        assert_eq!(run.status, RunStatus::Succeeded, "{run:?}");
        assert_eq!(run.reason, RunReason::Schedule);
        let thread = r.o.store.thread(run.thread.unwrap()).unwrap().clone();
        assert!(thread.title.starts_with("Test · "), "{}", thread.title);
        assert_eq!(run.summary, format!("echo: Summarize {}", schedule_date(next + 50_000)));
        assert_eq!(run.tokens, 15);
        assert!(run.duration_ms() < 60_000);
        // Intervals count from the run.
        assert_eq!(r.automation(id).state.next_ms[0], next + 50_000 + 30 * MIN);
        assert!(r.automation(id).state.pending.is_empty());
    });
}

fn schedule_date(ms: u64) -> String {
    crate::automations::schedule::local_date_time(&jiff::tz::TimeZone::system(), ms).0
}

#[test]
fn a_long_sleep_records_missed_runs_and_does_not_run_them_all() {
    run(async || {
        let mut r = Rig::new();
        let mut s = spec("Nightly", every(10));
        s.limits.grace_minutes = 0;
        let id = r.save(s);
        // Asleep for 35 minutes: three occurrences passed, the newest 5 late.
        r.at(T0 + 45 * MIN).await;
        let runs = r.runs(id);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].status, RunStatus::Skipped(Skip::Missed { count: 4 }));
        assert!(r.o.store.threads().next().is_none(), "no run started");
        assert!(r.automation(id).state.next_ms[0] > T0 + 45 * MIN);
        // Within grace: one late run, the rest counted as missed.
        let mut s = spec("Late", every(10));
        s.limits.grace_minutes = 30;
        let late = r.save(s);
        r.at(T0 + 45 * MIN + 25 * MIN).await;
        r.until("the late run", |r| finished(r, late)).await;
        let runs = r.runs(late);
        assert_eq!(runs[0].reason, RunReason::Late, "{runs:?}");
        assert_eq!(runs[1].status, RunStatus::Skipped(Skip::Missed { count: 1 }));
    });
}

#[test]
fn a_restart_neither_loses_nor_repeats_a_run() {
    run(async || {
        let mut r = Rig::new();
        let id = r.save(spec("Hourly", Trigger::Cron { expr: "0 * * * *".into(), tz: "UTC".into() }));
        assert_eq!(r.automation(id).state.next_ms[0], T0 + 60 * MIN);
        // Closed before it was due; opened 10 minutes after: it runs once.
        let mut r = r.restart(T0 + 70 * MIN);
        r.at(T0 + 70 * MIN).await;
        r.until("the run after the restart", |r| finished(r, id)).await;
        assert_eq!(r.runs(id).len(), 1);
        assert_eq!(r.runs(id)[0].reason, RunReason::Late);
        // Restarted again right after: the same occurrence never runs again.
        let mut r = r.restart(T0 + 71 * MIN);
        r.at(T0 + 71 * MIN).await;
        r.pump().await;
        assert_eq!(r.runs(id).len(), 1, "{:?}", r.runs(id));
        assert_eq!(r.automation(id).state.next_ms[0], T0 + 120 * MIN);
        // A run that was pending when Orbit stopped starts after it.
        let mut s = spec("hang", every(5));
        s.limits.timeout_minutes = 0;
        let busy: Vec<AutomationId> = (0..MAX_CONCURRENT).map(|_| r.save(s.clone())).collect();
        for b in &busy {
            r.o.handle_command(Command::Automation(AutomationCommand::RunNow { id: *b })).unwrap();
        }
        let waiting = r.save(spec("Waits", every(600)));
        r.o.handle_command(Command::Automation(AutomationCommand::RunNow { id: waiting })).unwrap();
        r.pump().await;
        assert_eq!(r.automation(waiting).state.pending.len(), 1, "held by the cap");
        let mut r = r.restart(T0 + 72 * MIN);
        r.until("the pending run", |r| finished(r, waiting)).await;
        assert_eq!(r.runs(waiting).len(), 1);
        // The hung runs were cut off by the restart and say so.
        for b in busy {
            assert_eq!(r.runs(b)[0].status, RunStatus::Failed("Orbit closed before this run finished.".into()));
        }
    });
}

#[test]
fn a_failing_check_on_the_forge_fires_once_per_failure() {
    run(async || {
        let mut r = Rig::new();
        let project = r.project();
        let mut s = spec("Fix {pr}", Trigger::Event(EventTrigger::ChecksFailed { mine: true }));
        s.project = Some(project);
        let id = r.save(s);
        let seen = |prs: Vec<PullRequest>| Observed { mine: Some(prs), ..Default::default() };
        r.o.automations_observed(project, seen(vec![pr(42, 1), pr(43, 0)]));
        r.until("the fix run", |r| finished(r, id)).await;
        let runs = r.runs(id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].reason, RunReason::Event("Checks failed on #42".into()));
        assert!(runs[0].summary.contains("Fix #42 Change 42"), "{}", runs[0].summary);
        // The same failure seen again: nothing new.
        r.o.automations_observed(project, seen(vec![pr(42, 1), pr(43, 0)]));
        r.pump().await;
        assert_eq!(r.runs(id).len(), 1);
        // Fixed, then failing again: a new run.
        r.o.automations_observed(project, seen(vec![pr(42, 0)]));
        r.o.automations_observed(project, seen(vec![pr(42, 2)]));
        r.until("the second fix", |r| r.runs(id).len() == 2).await;
        // Other people's lists don't count for "mine".
        r.o.automations_observed(project, Observed { open: Some(vec![pr(50, 1)]), ..Default::default() });
        r.pump().await;
        assert_eq!(r.runs(id).len(), 2);
    });
}

#[test]
fn a_new_labeled_issue_fires_but_old_ones_do_not() {
    run(async || {
        let mut r = Rig::new();
        let project = r.project();
        let mut s = spec("Triage {issue}", Trigger::Event(EventTrigger::IssueLabeled { label: "bug".into() }));
        s.project = Some(project);
        let id = r.save(s);
        let issue = |key: &str| Issue {
            tracker: TrackerKind::GitHub,
            key: key.into(),
            title: "Broken".into(),
            state: IssueState::Open,
            status: String::new(),
            author: "someone".into(),
            assignee: None,
            labels: vec!["bug".into()],
            url: String::new(),
            updated_ms: 0,
            comments: 0,
            branch: None,
        };
        r.o.automations_observed(project, Observed { issues: vec![("bug".into(), vec![issue("#1")])], ..Default::default() });
        r.pump().await;
        assert!(r.runs(id).is_empty(), "issues already open are the baseline");
        r.o.automations_observed(project, Observed { issues: vec![("bug".into(), vec![issue("#1"), issue("#2")])], ..Default::default() });
        r.until("the triage run", |r| finished(r, id)).await;
        assert_eq!(r.runs(id)[0].reason, RunReason::Event("New issue #2 labeled “bug”".into()));
    });
}

#[test]
fn watching_a_pull_request_stops_when_it_merges() {
    run(async || {
        let mut r = Rig::new();
        let project = r.project();
        let mut s = spec("Babysit", Trigger::Event(EventTrigger::WatchPr { number: 7 }));
        s.project = Some(project);
        let id = r.save(s);
        let detail = |state: PrState| PullRequestDetail { pr: PullRequest { state, ..pr(7, 0) }, ..Default::default() };
        r.o.automations_observed(project, Observed { watched: vec![detail(PrState::Merged)], ..Default::default() });
        let a = r.automation(id);
        assert!(a.paused);
        assert_eq!(a.state.note, "#7 was merged, so it stopped watching.");
    });
}

#[test]
fn a_finished_run_starts_the_automations_chained_to_it() {
    run(async || {
        let mut r = Rig::new();
        let first = r.save(spec("first", every(600)));
        let mut then = spec("Then: {event}", Trigger::Event(EventTrigger::Finished { source: ChainSource::Automation(first), when: ChainWhen::Succeeded }));
        then.name = "Then".into();
        let second = r.save(then);
        let mut on_fail = spec("cleanup", Trigger::Event(EventTrigger::Finished { source: ChainSource::Automation(first), when: ChainWhen::Failed }));
        on_fail.name = "On failure".into();
        let third = r.save(on_fail);
        r.o.handle_command(Command::Automation(AutomationCommand::RunNow { id: first })).unwrap();
        r.until("the chained run", |r| finished(r, second) && finished(r, first)).await;
        assert_eq!(r.runs(first)[0].reason, RunReason::Manual);
        assert_eq!(r.runs(second)[0].reason, RunReason::Event("“Test” finished".into()));
        assert!(r.runs(second)[0].summary.contains("echo: first"), "the source's message is in the prompt");
        assert!(r.runs(third).is_empty(), "only on failure");
    });
}

#[test]
fn runs_are_capped_and_time_out() {
    run(async || {
        let mut r = Rig::new();
        let mut s = spec("hang", every(600));
        s.limits.timeout_minutes = 5;
        let ids: Vec<AutomationId> = (0..MAX_CONCURRENT + 2).map(|_| r.save(s.clone())).collect();
        for id in &ids {
            r.o.handle_command(Command::Automation(AutomationCommand::RunNow { id: *id })).unwrap();
        }
        r.until("the first runs to start", |r| ids.iter().filter(|id| !r.runs(**id).is_empty()).count() == MAX_CONCURRENT).await;
        let waiting: usize = ids.iter().map(|id| r.automation(*id).state.pending.len()).sum();
        assert_eq!(waiting, 2, "the rest wait for a slot");
        // Five minutes later the hung runs time out and the others start.
        r.at(T0 + 5 * MIN + 1).await;
        r.until("the waiting runs to start", |r| ids.iter().all(|id| !r.runs(*id).is_empty())).await;
        let timed_out = ids.iter().filter(|id| r.runs(**id)[0].status == RunStatus::TimedOut).count();
        assert_eq!(timed_out, MAX_CONCURRENT);
    });
}

#[test]
fn limits_skip_runs_and_identical_skips_share_a_record() {
    run(async || {
        let mut r = Rig::new();
        let mut s = spec("hang", every(10));
        s.limits.timeout_minutes = 0;
        let id = r.save(s);
        r.at(T0 + 12 * MIN).await;
        r.until("the first run", |r| !r.runs(id).is_empty()).await;
        // Still running at the next two occurrences: skipped, counted once.
        r.at(T0 + 24 * MIN).await;
        r.at(T0 + 36 * MIN).await;
        let runs = r.runs(id);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].status, RunStatus::Skipped(Skip::StillRunning));
        assert_eq!(runs[0].repeats, 1);
        // A daily limit.
        let mut s = spec("once", every(10));
        s.limits.max_runs_per_day = 1;
        let limited = r.save(s);
        for id in [limited, limited] {
            r.o.handle_command(Command::Automation(AutomationCommand::RunNow { id })).unwrap();
        }
        r.until("the manual runs", |r| r.runs(limited).iter().filter(|x| x.status == RunStatus::Succeeded).count() == 2).await;
        r.at(T0 + 48 * MIN).await;
        assert_eq!(r.runs(limited)[0].status, RunStatus::Skipped(Skip::DailyLimit), "Run now ignores the limit; schedules don't");
    });
}

#[test]
fn pausing_drops_pending_runs_and_resuming_skips_what_passed() {
    run(async || {
        let mut r = Rig::new();
        let id = r.save(spec("Tick", every(10)));
        r.o.handle_command(Command::Automation(AutomationCommand::SetPaused { id, paused: true })).unwrap();
        r.at(T0 + 60 * MIN).await;
        assert!(r.runs(id).is_empty());
        r.o.handle_command(Command::Automation(AutomationCommand::SetPaused { id, paused: false })).unwrap();
        assert_eq!(r.automation(id).state.next_ms[0], T0 + 70 * MIN, "counted from the resume");
        r.at(T0 + 61 * MIN).await;
        assert!(r.runs(id).is_empty(), "nothing missed while paused");
        // Deleting removes it and its history.
        r.o.handle_command(Command::Automation(AutomationCommand::Delete { id })).unwrap();
        assert!(r.o.store.automations().get(id).is_none());
        assert!(r.o.snapshot().automations.is_empty());
    });
}

#[test]
fn bad_specs_are_refused_with_a_reason() {
    let mut r = Rig::new();
    let refuse = |r: &mut Rig, spec: AutomationSpec| match r.o.handle_command(Command::Automation(AutomationCommand::Save { id: None, spec: Box::new(spec) })) {
        Err(message) => message,
        other => panic!("{other:?}"),
    };
    assert!(refuse(&mut r, spec("x", Trigger::Cron { expr: "61 * * * *".into(), tz: String::new() })).contains("minute"));
    assert!(refuse(&mut r, spec("", every(5))).contains("agent should do"));
    assert!(refuse(&mut r, spec("Hi {colour}", every(5))).contains("{colour}"));
    assert!(refuse(&mut r, spec("x", Trigger::Event(EventTrigger::ReviewRequested))).contains("need a project"));
    let mut worktree = spec("x", every(5));
    worktree.target = RunTarget::Worktree;
    assert!(refuse(&mut r, worktree).contains("worktree needs a project"));
}
