//! Automations in the engine: [`Command::Automation`], the scheduler on the
//! 500 ms tick, forge and thread events, and the life of a run.
//!
//! A run goes: due (a schedule came up, an event matched, or "Run now") →
//! pending (persisted in the automation's state, waiting for one of
//! [`MAX_CONCURRENT`] slots) → checks (async, off the engine thread) →
//! running (a thread, watched on the tick) → finished, with its summary,
//! tokens and whether it changed files. Everything that decides timing is
//! in `crate::automations` and takes the time as an argument.
//!
//! Cost: with no automations the tick returns after one map check. With
//! some, it peeks at the heap top; runs are watched only while they run;
//! a project's forge is looked at every two minutes only while one of its
//! automations has a forge event trigger, through the forge's cache.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator, Outcome};
use crate::automations::schedule::{self, Schedule};
use crate::automations::scheduler::{self, Scheduler};
use crate::automations::{self as auto, template};
use crate::forge::{Forge, ForgeConfig};
use crate::git::Git;

/// Runs going at once across all automations; more wait their turn.
pub(crate) const MAX_CONCURRENT: usize = 3;
/// How often a project's forge is looked at for event triggers.
pub(crate) const SWEEP_EVERY_MS: u64 = 120_000;
/// A check command gets this long.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const SUMMARY_CHARS: usize = 600;
/// Event keys remembered per automation.
const KEEP_SEEN: usize = 256;
const MINUTE_MS: u64 = 60_000;

/// The automation engine's memory that is not persisted.
#[derive(Default)]
pub(crate) struct AutomationsState {
    scheduler: Scheduler,
    /// Pending runs in start order.
    queue: VecDeque<(AutomationId, RunId)>,
    /// Runs that hold a slot: checking or running.
    active: HashMap<RunId, Active>,
    /// Threads whose turns other automations follow: (turns, busy) when
    /// last seen.
    watched: HashMap<ThreadId, (u32, bool)>,
    /// When each project's forge was last looked at.
    swept: HashMap<ProjectId, u64>,
    sweeping: std::collections::HashSet<ProjectId>,
    /// The next time the tick looks for forge work.
    next_sweep_ms: u64,
    /// Stands in for the clock in tests.
    pub(crate) clock: Option<u64>,
}

struct Active {
    automation: AutomationId,
    thread: Option<ThreadId>,
    turn: u32,
    tokens_before: u64,
    started_ms: u64,
    deadline_ms: Option<u64>,
    reason: RunReason,
    due_ms: u64,
}

/// What the sweep of one project's forge found.
#[derive(Default)]
pub(crate) struct Observed {
    pub open: Option<Vec<PullRequest>>,
    pub mine: Option<Vec<PullRequest>>,
    pub account: Option<String>,
    /// Open issues per watched label.
    pub issues: Vec<(String, Vec<Issue>)>,
    /// Remote branch tips.
    pub branches: Vec<(String, String)>,
    pub watched: Vec<PullRequestDetail>,
}

/// What a sweep has to fetch.
#[derive(Default)]
struct SweepPlan {
    open: bool,
    mine: bool,
    account: bool,
    labels: Vec<String>,
    branches: Vec<String>,
    watch: Vec<u64>,
}

impl Orchestrator {
    fn auto_now(&self) -> u64 {
        self.automations.clock.unwrap_or_else(now_ms)
    }

    fn automation(&self, id: AutomationId) -> Option<Automation> {
        self.store.automations().get(id).cloned()
    }

    fn put_automation(&mut self, mut automation: Automation) {
        if self.store.automations().get(automation.id) == Some(&automation) {
            return;
        }
        automation.updated_ms = self.auto_now();
        self.emit(Event::AutomationUpserted(Box::new(automation)));
    }

    fn put_run(&mut self, run: AutomationRun) {
        self.emit(Event::AutomationRun(Box::new(run)));
    }

    // ── Commands ─────────────────────────────────────────────────────────

    pub(crate) fn automation_command(&mut self, command: AutomationCommand) -> CommandResult {
        match command {
            AutomationCommand::Save { id, spec } => self.save_automation(id, *spec),
            AutomationCommand::Delete { id } => {
                if self.store.automations().get(id).is_none() {
                    return Err("That automation no longer exists.".into());
                }
                self.automations.queue.retain(|(a, _)| *a != id);
                self.emit(Event::AutomationRemoved(id));
                self.rebuild_watched();
                Ok(Reply::Done)
            }
            AutomationCommand::SetPaused { id, paused } => {
                let mut a = self.automation(id).ok_or("That automation no longer exists.")?;
                if a.paused == paused {
                    return Ok(Reply::Done);
                }
                a.paused = paused;
                if paused {
                    // Runs not started yet are dropped with the pause.
                    let dropped: Vec<RunId> = a.state.pending.drain(..).map(|p| p.id).collect();
                    self.automations.queue.retain(|(_, r)| !dropped.contains(r));
                } else {
                    // Resuming starts from now: nothing missed while paused runs.
                    a.state.next_ms = next_times(&a.spec, self.auto_now());
                    a.state.note.clear();
                }
                self.put_automation(a.clone());
                if !paused {
                    self.arm_all(&a);
                    self.automations.next_sweep_ms = 0;
                }
                Ok(Reply::Done)
            }
            AutomationCommand::RunNow { id } => {
                self.automation(id).ok_or("That automation no longer exists.")?;
                let run = self.fire(id, RunReason::Manual, self.auto_now(), Vec::new()).ok_or("The run couldn't start.")?;
                Ok(Reply::Automation(AutomationReply::Run(run)))
            }
            AutomationCommand::History { id, limit } => {
                self.store.automations().get(id).ok_or("That automation no longer exists.")?;
                let take = if limit == 0 { usize::MAX } else { limit as usize };
                let runs = self.store.automations().runs(id).take(take).cloned().collect();
                Ok(Reply::Automation(AutomationReply::Runs(runs)))
            }
            AutomationCommand::Recipe { id, project, config } => {
                let recipe = auto::recipes::find(&id).ok_or_else(|| format!("There is no recipe “{id}”."))?;
                Ok(Reply::Automation(AutomationReply::Spec(Box::new(recipe.spec(project, &config)))))
            }
        }
    }

    fn save_automation(&mut self, id: Option<AutomationId>, mut spec: AutomationSpec) -> CommandResult {
        spec.name = spec.name.trim().to_owned();
        auto::validate(&spec)?;
        if let Some(project) = spec.project
            && self.store.project(project).is_none()
        {
            return Err("That project no longer exists.".into());
        }
        if let RunTarget::Thread(thread) = spec.target
            && self.store.thread(thread).is_none()
        {
            return Err("The thread to continue no longer exists.".into());
        }
        let now = self.auto_now();
        let automation = match id {
            Some(id) => {
                let mut a = self.automation(id).ok_or("That automation no longer exists.")?;
                if a.spec.triggers != spec.triggers || a.spec.project != spec.project {
                    a.state.next_ms = next_times(&spec, now);
                    a.state.seen.clear();
                }
                a.state.note.clear();
                a.spec = spec;
                a
            }
            None => Automation {
                id: AutomationId::new(),
                state: AutomationState { next_ms: next_times(&spec, now), ..Default::default() },
                spec,
                paused: false,
                created_ms: now,
                updated_ms: now,
            },
        };
        let id = automation.id;
        self.put_automation(automation.clone());
        self.arm_all(&automation);
        self.rebuild_watched();
        self.automations.next_sweep_ms = 0;
        if let Some(project) = automation.spec.project {
            self.automations.swept.remove(&project);
        }
        Ok(Reply::Automation(AutomationReply::Saved(id)))
    }

    fn arm_all(&mut self, a: &Automation) {
        if a.paused {
            return;
        }
        for (index, trigger) in a.spec.triggers.iter().enumerate() {
            let due = a.state.next_ms.get(index).copied().unwrap_or(0);
            if due > 0
                && let Ok(Some(schedule)) = Schedule::of(trigger)
            {
                self.automations.scheduler.arm(a.id, index, due, schedule.min_gap_ms());
            }
        }
    }

    /// Re-arms schedules, re-queues pending runs and settles runs a restart
    /// cut off. Call once after startup.
    pub(crate) fn automations_recover(&mut self) {
        let now = self.auto_now();
        let automations: Vec<Automation> = self.store.automations().iter().cloned().collect();
        let mut pending: Vec<(RunId, AutomationId)> = Vec::new();
        for mut a in automations {
            // Schedules added before their time was known.
            let fresh = next_times(&a.spec, now);
            let mut changed = false;
            if a.state.next_ms.len() != a.spec.triggers.len() {
                a.state.next_ms = fresh;
                changed = true;
            } else {
                for (i, t) in a.spec.triggers.iter().enumerate() {
                    if t.is_schedule() && a.state.next_ms[i] == 0 && fresh[i] > 0 && a.state.note.is_empty() {
                        a.state.next_ms[i] = fresh[i];
                        changed = true;
                    }
                }
            }
            if changed {
                self.put_automation(a.clone());
            }
            self.arm_all(&a);
            pending.extend(a.state.pending.iter().map(|p| (p.id, a.id)));
        }
        pending.sort();
        self.automations.queue = pending.into_iter().map(|(r, a)| (a, r)).collect();
        for run in self.store.automations().active_runs() {
            self.settle_interrupted_run(run);
        }
        self.rebuild_watched();
        self.pump_runs();
    }

    /// A run that was going when Orbit stopped: if its thread finished the
    /// turn cleanly it succeeded, otherwise it was cut off.
    fn settle_interrupted_run(&mut self, run: AutomationRun) {
        let finished = run.thread.and_then(|t| self.store.thread(t).cloned()).filter(|t| {
            t.turns > run.turn && t.queued == 0 && matches!(t.status, ThreadStatus::Idle) && self.turn_ended_cleanly(t.id, t.turns)
        });
        let active = Active {
            automation: run.automation,
            thread: run.thread,
            turn: run.turn,
            // A new thread's usage is all the run's; a continued one's is
            // not known any more.
            tokens_before: match run.turn {
                0 => 0,
                _ => run.thread.and_then(|t| self.store.thread(t)).map(|t| tokens(&t.usage)).unwrap_or(0),
            },
            started_ms: run.started_ms,
            deadline_ms: None,
            reason: run.reason.clone(),
            due_ms: run.due_ms,
        };
        self.automations.active.insert(run.id, active);
        let status = if finished.is_some() {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed("Orbit closed before this run finished.".into())
        };
        self.finish_run(run.id, status);
    }

    fn turn_ended_cleanly(&mut self, thread: ThreadId, turn: u32) -> bool {
        let was_loaded = self.store.timeline(thread).is_some();
        let clean = self.store.load_timeline(thread).ok().is_some_and(|timeline| {
            timeline.items().iter().rev().any(|i| matches!(&i.body, ItemBody::TurnEnd(s) if i.turn == turn && !s.interrupted))
        });
        if !was_loaded && !self.open.contains(&thread) {
            self.store.unload_timeline(thread);
        }
        clean
    }

    // ── The tick ─────────────────────────────────────────────────────────

    pub(crate) fn automations_tick(&mut self) {
        if self.store.automations().is_empty() && self.automations.active.is_empty() {
            return;
        }
        let now = self.auto_now();
        while let Some((id, index, due)) = self.automations.scheduler.pop_due(now) {
            self.trigger_due(id, index, due, now);
        }
        if !self.automations.active.is_empty() {
            self.watch_runs(now);
        }
        if !self.automations.watched.is_empty() {
            self.watch_threads();
        }
        if now >= self.automations.next_sweep_ms {
            self.automations.next_sweep_ms = now + 10_000;
            self.sweep_forges(now);
        }
    }

    fn trigger_due(&mut self, id: AutomationId, index: usize, due: u64, now: u64) {
        let Some(mut a) = self.automation(id) else { return };
        if a.paused || a.state.next_ms.get(index) != Some(&due) {
            return; // edited, paused or already handled since it was armed
        }
        let Some(Ok(Some(schedule))) = a.spec.triggers.get(index).map(Schedule::of) else { return };
        let plan = scheduler::plan(&schedule, due, now, a.spec.limits.grace_minutes as u64 * MINUTE_MS);
        a.state.next_ms[index] = plan.next_ms;
        if plan.next_ms == 0 && a.spec.triggers.iter().all(|t| t.is_schedule()) {
            a.state.note = "Its schedule has no more runs.".into();
        }
        // Persisted before anything starts: a restart from here on neither
        // repeats this occurrence nor loses it (it is pending).
        self.put_automation(a.clone());
        if plan.next_ms > 0 {
            self.automations.scheduler.arm(id, index, plan.next_ms, schedule.min_gap_ms());
        }
        if plan.missed > 0 {
            self.record_skip(id, due, Skip::Missed { count: plan.missed });
        }
        if let Some((at, late)) = plan.fire {
            self.fire(id, if late { RunReason::Late } else { RunReason::Schedule }, at, Vec::new());
        }
    }

    /// A run is due: checks the limits, then queues it. Returns its id.
    pub(crate) fn fire(&mut self, id: AutomationId, reason: RunReason, due_ms: u64, vars: Vec<(String, String)>) -> Option<RunId> {
        let mut a = self.automation(id)?;
        let now = self.auto_now();
        let manual = reason == RunReason::Manual;
        let today = schedule::local_day(&jiff::tz::TimeZone::system(), now);
        if a.state.day != today {
            a.state.day = today;
            a.state.day_runs = 0;
        }
        if !manual {
            if a.paused {
                return None;
            }
            let max = a.spec.limits.max_runs_per_day;
            if max > 0 && a.state.day_runs >= max {
                self.put_automation(a);
                self.record_skip(id, due_ms, Skip::DailyLimit);
                return None;
            }
            if a.spec.checks.skip_if_running && self.is_running(&a) {
                self.record_skip(id, due_ms, Skip::StillRunning);
                return None;
            }
        }
        let run = RunId::new();
        a.state.day_runs += 1;
        a.state.last_fire_ms = now;
        a.state.pending.push(PendingRun { id: run, reason, due_ms, vars });
        self.put_automation(a);
        self.automations.queue.push_back((id, run));
        self.pump_runs();
        Some(run)
    }

    fn is_running(&self, a: &Automation) -> bool {
        !a.state.pending.is_empty() || self.automations.active.values().any(|r| r.automation == a.id)
    }

    /// Records a skipped occurrence; identical skips in a row share one
    /// record.
    fn record_skip(&mut self, id: AutomationId, due_ms: u64, skip: Skip) {
        let now = self.auto_now();
        let last = self.store.automations().runs(id).next().cloned();
        if let Some(mut run) = last
            && let RunStatus::Skipped(previous) = &run.status
            && same_skip(previous, &skip)
        {
            run.repeats += 1;
            run.due_ms = due_ms;
            run.finished_ms = now;
            if let (RunStatus::Skipped(Skip::Missed { count }), Skip::Missed { count: more }) = (&mut run.status, &skip) {
                *count += more;
            }
            self.put_run(run);
            return;
        }
        let run = AutomationRun {
            id: RunId::new(),
            automation: id,
            reason: RunReason::Schedule,
            status: RunStatus::Skipped(skip),
            due_ms,
            finished_ms: now,
            ..Default::default()
        };
        self.put_run(run);
    }

    /// Starts pending runs while slots are free.
    fn pump_runs(&mut self) {
        while self.automations.active.len() < MAX_CONCURRENT {
            let Some((id, run)) = self.automations.queue.pop_front() else { break };
            self.begin_run(id, run);
        }
    }

    fn begin_run(&mut self, id: AutomationId, run: RunId) {
        let Some(mut a) = self.automation(id) else { return };
        let Some(index) = a.state.pending.iter().position(|p| p.id == run) else { return };
        let pending = a.state.pending[index].clone();
        let manual = pending.reason == RunReason::Manual;
        if a.paused && !manual {
            a.state.pending.remove(index);
            self.put_automation(a);
            return;
        }
        let now = self.auto_now();
        self.automations.active.insert(
            run,
            Active {
                automation: id,
                thread: None,
                turn: 0,
                tokens_before: 0,
                started_ms: now,
                deadline_ms: None,
                reason: pending.reason.clone(),
                due_ms: pending.due_ms,
            },
        );
        let project = a.spec.project.and_then(|p| self.store.project(p)).map(|p| PathBuf::from(&p.path));
        if a.spec.project.is_some() && project.is_none() {
            return self.finish_run(run, RunStatus::Skipped(Skip::Unavailable("The project was removed.".into())));
        }
        if let RunTarget::Thread(thread) = a.spec.target
            && self.store.thread(thread).is_none_or(|t| t.archived)
        {
            return self.finish_run(run, RunStatus::Skipped(Skip::Unavailable("The thread to continue is gone or archived.".into())));
        }
        let checks = a.spec.checks.clone();
        let needed = !manual && (checks.changes_since_last_run || checks.clean_repo || !checks.command.trim().is_empty());
        match (needed, project) {
            (true, Some(path)) => {
                let (git, tx) = (self.git.clone(), self.internal_tx.clone());
                let last = a.state.fingerprint.clone();
                tokio::spawn(async move {
                    let result = run_checks(&git, &path, &checks, &last).await;
                    let _ = tx.send(Internal::Run(Box::new(move |o| o.checks_done(run, result))));
                });
            }
            _ => self.launch(run),
        }
    }

    fn checks_done(&mut self, run: RunId, result: Result<(), Skip>) {
        if !self.automations.active.contains_key(&run) {
            return;
        }
        match result {
            Ok(()) => self.launch(run),
            Err(skip) => self.finish_run(run, RunStatus::Skipped(skip)),
        }
    }

    /// Takes the run out of the automation's pending list.
    fn take_pending(&mut self, id: AutomationId, run: RunId) -> Option<PendingRun> {
        let mut a = self.automation(id)?;
        let index = a.state.pending.iter().position(|p| p.id == run)?;
        let pending = a.state.pending.remove(index);
        self.put_automation(a);
        Some(pending)
    }

    /// Sends the prompt: to a new thread, the thread it continues, or the
    /// pull request's thread.
    fn launch(&mut self, run: RunId) {
        let Some(id) = self.automations.active.get(&run).map(|r| r.automation) else { return };
        let Some(a) = self.automation(id) else { return self.drop_active(run) };
        let pending = self.take_pending(id, run).unwrap_or_default();
        let now = self.auto_now();
        let prompt = self.render_prompt(&a, &pending.vars, now);
        let (date, time) = schedule::local_date_time(&jiff::tz::TimeZone::system(), now);
        let title = format!("{} · {} {}", a.spec.name, date, time);
        let pr_number = var(&pending.vars, "pr_number").and_then(|n| n.parse::<u64>().ok());
        match (a.spec.target, a.spec.project, pr_number) {
            (RunTarget::Thread(thread), ..) => self.attach_and_send(run, thread, prompt),
            (RunTarget::PullRequest, Some(project), Some(number)) => {
                match self.forge(ForgeCommand::Checkout { project, number, config: a.spec.config.clone() }) {
                    Outcome::Ready(result) => self.pr_thread_ready(run, result, prompt),
                    Outcome::Deferred(future) => {
                        let tx = self.internal_tx.clone();
                        tokio::spawn(async move {
                            let result = future.await;
                            let _ = tx.send(Internal::Run(Box::new(move |o| o.pr_thread_ready(run, result, prompt))));
                        });
                    }
                }
            }
            (target, project, _) => {
                let start = ThreadStart {
                    project,
                    config: a.spec.config.clone(),
                    worktree: project.is_some() && target != RunTarget::Project,
                    parent: None,
                    title,
                    prompt: Some(prompt),
                };
                match self.cli_command(Command::StartThread(start)) {
                    Ok(Reply::Thread(thread)) => self.attach(run, thread, 0),
                    Ok(_) => self.finish_run(run, RunStatus::Failed("The thread didn't start.".into())),
                    Err(message) => self.finish_run(run, RunStatus::Failed(message)),
                }
            }
        }
    }

    fn pr_thread_ready(&mut self, run: RunId, result: CommandResult, prompt: String) {
        if !self.automations.active.contains_key(&run) {
            return;
        }
        match result {
            Ok(Reply::Thread(thread)) => self.attach_and_send(run, thread, prompt),
            Ok(_) => self.finish_run(run, RunStatus::Failed("The pull request's thread didn't start.".into())),
            Err(message) => self.finish_run(run, RunStatus::Failed(message)),
        }
    }

    fn attach_and_send(&mut self, run: RunId, thread: ThreadId, prompt: String) {
        let turn = self.store.thread(thread).map(|t| t.turns).unwrap_or(0);
        match self.send(thread, prompt, Vec::new(), false) {
            Ok(_) => self.attach(run, thread, turn),
            Err(message) => self.finish_run(run, RunStatus::Failed(message)),
        }
    }

    fn attach(&mut self, run: RunId, thread: ThreadId, turn: u32) {
        let now = self.auto_now();
        let tokens_before = self.store.thread(thread).map(|t| tokens(&t.usage)).unwrap_or(0);
        let Some(active) = self.automations.active.get_mut(&run) else { return };
        let automation = active.automation;
        let timeout = self.store.automations().get(automation).map(|a| a.spec.limits.timeout_minutes).unwrap_or(0);
        active.thread = Some(thread);
        active.turn = turn;
        active.tokens_before = tokens_before;
        active.started_ms = now;
        active.deadline_ms = (timeout > 0).then(|| now + timeout as u64 * MINUTE_MS);
        let record = AutomationRun {
            id: run,
            automation,
            reason: active.reason.clone(),
            status: RunStatus::Running,
            thread: Some(thread),
            turn,
            due_ms: active.due_ms,
            started_ms: now,
            ..Default::default()
        };
        self.put_run(record);
    }

    fn drop_active(&mut self, run: RunId) {
        self.automations.active.remove(&run);
        self.pump_runs();
    }

    /// Ends runs whose thread finished, failed, vanished or ran too long.
    fn watch_runs(&mut self, now: u64) {
        let mut ended: Vec<(RunId, RunStatus)> = Vec::new();
        for (run, active) in &self.automations.active {
            let Some(thread_id) = active.thread else { continue };
            let Some(thread) = self.store.thread(thread_id) else {
                ended.push((*run, RunStatus::Failed("Its thread was deleted.".into())));
                continue;
            };
            if active.deadline_ms.is_some_and(|d| now >= d) {
                ended.push((*run, RunStatus::TimedOut));
                continue;
            }
            let checkpointing = self.runtimes.get(&thread_id).is_some_and(|r| r.checkpointing);
            if thread.status.is_busy() || thread.queued > 0 || checkpointing || thread.turns <= active.turn {
                continue;
            }
            let status = match &thread.status {
                ThreadStatus::Failed { message } => RunStatus::Failed(message.clone()),
                ThreadStatus::RateLimited { .. } => RunStatus::Failed("The agent hit its usage limit.".into()),
                _ => RunStatus::Succeeded,
            };
            ended.push((*run, status));
        }
        for (run, status) in ended {
            if status == RunStatus::TimedOut
                && let Some(thread) = self.automations.active.get(&run).and_then(|a| a.thread)
            {
                let _ = self.interrupt(thread);
            }
            self.finish_run(run, status);
        }
    }

    /// Closes a run: its record, what it changed, what happens next.
    fn finish_run(&mut self, run: RunId, status: RunStatus) {
        let Some(active) = self.automations.active.remove(&run) else { return };
        let id = active.automation;
        let now = self.auto_now();
        // Checks that skipped it: the run never started.
        if active.thread.is_none() {
            self.take_pending(id, run);
            if let RunStatus::Skipped(skip) = status {
                self.record_skip(id, active.due_ms, skip);
            } else if self.store.automations().get(id).is_some() {
                let record = AutomationRun {
                    id: run,
                    automation: id,
                    reason: active.reason,
                    status,
                    due_ms: active.due_ms,
                    started_ms: active.started_ms,
                    finished_ms: now,
                    ..Default::default()
                };
                self.put_run(record);
            }
            self.pump_runs();
            return;
        }
        let thread_id = active.thread.unwrap_or_default();
        let mut record = self.store.automations().run(id, run).cloned().unwrap_or(AutomationRun {
            id: run,
            automation: id,
            reason: active.reason.clone(),
            thread: active.thread,
            turn: active.turn,
            due_ms: active.due_ms,
            started_ms: active.started_ms,
            ..Default::default()
        });
        record.status = status.clone();
        record.finished_ms = now;
        if let Some(thread) = self.store.thread(thread_id).cloned() {
            record.tokens = tokens(&thread.usage).saturating_sub(active.tokens_before);
            let (summary, changed) = self.run_outcome(thread_id, active.turn);
            record.summary = summary;
            record.changed = changed;
        }
        let automation = self.automation(id);
        let succeeded = status == RunStatus::Succeeded;
        self.put_run(record.clone());
        if let Some(a) = &automation {
            self.after_run(a, &record, thread_id);
        }
        // Chained automations.
        let source = ChainSource::Automation(id);
        let summary = record.summary.clone();
        let name = automation.map(|a| a.spec.name).unwrap_or_default();
        self.fire_chained(source, succeeded, &name, &summary);
        self.pump_runs();
    }

    /// The agent's last message in the run, and whether its turns changed
    /// files.
    fn run_outcome(&mut self, thread: ThreadId, since_turn: u32) -> (String, Option<bool>) {
        let was_loaded = self.store.timeline(thread).is_some();
        let Ok(timeline) = self.store.load_timeline(thread) else { return (String::new(), None) };
        let mut summary = String::new();
        let mut changed = None;
        for item in timeline.items().iter().filter(|i| i.turn > since_turn) {
            match &item.body {
                ItemBody::Assistant { text } if !text.trim().is_empty() => summary = text.clone(),
                ItemBody::TurnEnd(s) => changed = Some(changed.unwrap_or(false) || !s.files.is_empty()),
                _ => {}
            }
        }
        let busy = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
        if !was_loaded && !busy && !self.open.contains(&thread) {
            self.store.unload_timeline(thread);
        }
        (shorten(summary.trim(), SUMMARY_CHARS), changed)
    }

    /// Notification, settling, the pull request and the fingerprint.
    fn after_run(&mut self, a: &Automation, record: &AutomationRun, thread: ThreadId) {
        let failed = record.status.is_failure();
        let quiet = match a.spec.on_finish.notify {
            Notify::Always => false,
            Notify::Failures => !failed,
            Notify::Never => true,
        };
        let settle = a.spec.on_finish.settle_if_unchanged && record.status == RunStatus::Succeeded && record.changed == Some(false);
        if (quiet || settle) && self.store.thread(thread).is_some() {
            let patch = ThreadPatch { unread: quiet.then_some(false), settled: settle.then_some(true), ..Default::default() };
            let _ = self.patch_thread(thread, patch);
        }
        let worktree = self.store.thread(thread).and_then(|t| t.workspace.worktree.clone());
        if a.spec.on_finish.open_pr
            && record.status == RunStatus::Succeeded
            && record.changed == Some(true)
            && let Some(path) = worktree
        {
            let (git, tx) = (self.git.clone(), self.internal_tx.clone());
            let (automation, run) = (a.id, record.id);
            let title = format!("{}: {}", a.spec.name, shorten(first_line(&record.summary), 60));
            let body = format!("{}\n\n_Opened by the Orbit automation “{}”._", record.summary, a.spec.name);
            tokio::spawn(async move {
                let path = PathBuf::from(path);
                let result = async {
                    git.commit(&path, &title, true).await.map_err(|e| e.to_string())?;
                    git.create_pull_request(&path, &title, &body, None).await.map_err(|e| e.to_string())
                }
                .await;
                let _ = tx.send(Internal::Run(Box::new(move |o| o.pr_opened(automation, run, result))));
            });
        }
        if let Some(path) = a.spec.project.and_then(|p| self.store.project(p)).map(|p| PathBuf::from(&p.path)) {
            let (git, tx, id) = (self.git.clone(), self.internal_tx.clone(), a.id);
            tokio::spawn(async move {
                let print = fingerprint(&git, &path).await;
                let _ = tx.send(Internal::Run(Box::new(move |o| {
                    if let Some(mut a) = o.automation(id) {
                        a.state.fingerprint = print;
                        o.put_automation(a);
                    }
                })));
            });
        }
    }

    fn pr_opened(&mut self, automation: AutomationId, run: RunId, result: Result<String, String>) {
        let Some(mut record) = self.store.automations().run(automation, run).cloned() else { return };
        match result {
            Ok(url) => record.pr_url = url,
            Err(message) => {
                record.summary = format!("{}\n\nCouldn't open the pull request: {message}", record.summary).trim().to_owned()
            }
        }
        self.put_run(record);
    }

    fn render_prompt(&self, a: &Automation, vars: &[(String, String)], now: u64) -> String {
        let tz = jiff::tz::TimeZone::system();
        let (date, time) = schedule::local_date_time(&tz, now);
        let project = a.spec.project.and_then(|p| self.store.project(p));
        let last = self
            .store
            .automations()
            .runs(a.id)
            .find(|r| matches!(r.status, RunStatus::Succeeded | RunStatus::Failed(_)) && !r.summary.is_empty())
            .map(|r| r.summary.clone());
        template::render(&a.spec.prompt, |name| match name {
            "date" => Some(date.clone()),
            "time" => Some(time.clone()),
            "name" => Some(a.spec.name.clone()),
            "project" => Some(project.map(|p| p.name.clone()).unwrap_or_default()),
            "last_run_summary" => Some(last.clone().unwrap_or_else(|| "(no earlier run)".into())),
            "branch" => var(vars, "branch").or_else(|| project.and_then(|p| p.base_branch.clone())).or(Some(String::new())),
            "pr" | "issue" | "event" => Some(var(vars, name).unwrap_or_default()),
            _ => None,
        })
    }

    // ── Events ───────────────────────────────────────────────────────────

    fn rebuild_watched(&mut self) {
        let mut watched = HashMap::new();
        for a in self.store.automations().iter().filter(|a| !a.paused) {
            for trigger in &a.spec.triggers {
                if let Trigger::Event(EventTrigger::Finished { source: ChainSource::Thread(t), .. }) = trigger
                    && let Some(thread) = self.store.thread(*t)
                {
                    let seen = self.automations.watched.get(t).copied().unwrap_or((thread.turns, thread.status.is_busy()));
                    watched.insert(*t, seen);
                }
            }
        }
        self.automations.watched = watched;
    }

    /// Threads other automations follow: a turn that ended fires them.
    fn watch_threads(&mut self) {
        let mut ended = Vec::new();
        for (id, seen) in self.automations.watched.iter_mut() {
            let Some(thread) = self.store.thread(*id) else { continue };
            let now = (thread.turns, thread.status.is_busy() || thread.queued > 0);
            if now != *seen {
                let finished = seen.1 && !now.1;
                *seen = now;
                if finished {
                    let ok = !matches!(thread.status, ThreadStatus::Failed { .. } | ThreadStatus::RateLimited { .. });
                    let title = if thread.title.is_empty() { "a thread".to_owned() } else { thread.title.clone() };
                    ended.push((*id, ok, title));
                }
            }
        }
        for (thread, ok, title) in ended {
            self.fire_chained(ChainSource::Thread(thread), ok, &title, "");
        }
    }

    fn fire_chained(&mut self, source: ChainSource, succeeded: bool, name: &str, summary: &str) {
        let targets: Vec<AutomationId> = self
            .store
            .automations()
            .iter()
            .filter(|a| !a.paused)
            .filter(|a| {
                a.spec.triggers.iter().any(|t| match t {
                    Trigger::Event(EventTrigger::Finished { source: s, when }) => {
                        *s == source
                            && match when {
                                ChainWhen::Succeeded => succeeded,
                                ChainWhen::Failed => !succeeded,
                                ChainWhen::Either => true,
                            }
                    }
                    _ => false,
                })
            })
            .map(|a| a.id)
            .collect();
        let event = format!("“{name}” {}", if succeeded { "finished" } else { "failed" });
        for id in targets {
            let now = self.auto_now();
            let detail = if summary.is_empty() { event.clone() } else { format!("{event}. Its last message:

{summary}") };
            let vars = vec![("event".to_owned(), detail)];
            self.fire(id, RunReason::Event(event.clone()), now, vars);
        }
    }

    /// Starts a look at each project's forge whose automations need one.
    fn sweep_forges(&mut self, now: u64) {
        let mut plans: HashMap<ProjectId, SweepPlan> = HashMap::new();
        for a in self.store.automations().iter().filter(|a| !a.paused) {
            let Some(project) = a.spec.project else { continue };
            for trigger in &a.spec.triggers {
                let Trigger::Event(event) = trigger else { continue };
                                match event {
                    EventTrigger::ChecksFailed { mine: true } => plans.entry(project).or_default().mine = true,
                    EventTrigger::ChecksFailed { mine: false } => plans.entry(project).or_default().open = true,
                    EventTrigger::ReviewRequested => {
                        let p = plans.entry(project).or_default();
                        p.mine = true;
                        p.account = true;
                    }
                    EventTrigger::IssueLabeled { label } => plans.entry(project).or_default().labels.push(label.clone()),
                    EventTrigger::BranchPushed { branch } => {
                        plans.entry(project).or_default().branches.push(branch.clone())
                    }
                    EventTrigger::WatchPr { number } => plans.entry(project).or_default().watch.push(*number),
                    EventTrigger::Finished { .. } => {}
                }
            }
        }
        for (project, plan) in plans {
            let due = self.automations.swept.get(&project).is_none_or(|t| now >= t + SWEEP_EVERY_MS);
            if !due || self.automations.sweeping.contains(&project) {
                continue;
            }
            let Some(path) = self.store.project(project).map(|p| PathBuf::from(&p.path)) else { continue };
            self.automations.swept.insert(project, now);
            self.automations.sweeping.insert(project);
            let (service, config, git, tx) =
                (self.forge.service.clone(), self.forge_config(), self.git.clone(), self.internal_tx.clone());
            tokio::spawn(async move {
                let observed = sweep(&service, &config, &git, &path, plan).await;
                let _ = tx.send(Internal::Run(Box::new(move |o| {
                    o.automations.sweeping.remove(&project);
                    o.automations_observed(project, observed);
                })));
            });
        }
    }

    /// Acts on what a project's forge showed: each new event fires once.
    pub(crate) fn automations_observed(&mut self, project: ProjectId, observed: Observed) {
        let automations: Vec<Automation> =
            self.store.automations().iter().filter(|a| !a.paused && a.spec.project == Some(project)).cloned().collect();
        for mut a in automations {
            let mut fires: Vec<(String, Vec<(String, String)>)> = Vec::new();
            let mut stop: Option<String> = None;
            for trigger in a.spec.triggers.clone() {
                let Trigger::Event(event) = trigger else { continue };
                match event {
                    EventTrigger::ChecksFailed { mine } => {
                        let list = if mine { &observed.mine } else { &observed.open };
                        let Some(list) = list else { continue };
                        let prefix = if mine { "checks-mine:" } else { "checks:" };
                        let live: Vec<String> = list.iter().map(|p| format!("{prefix}{}", p.number)).collect();
                        a.state.seen.retain(|k| !k.starts_with(prefix) || live.contains(k));
                        for pr in list.iter().filter(|p| p.state.is_open()) {
                            let key = format!("{prefix}{}", pr.number);
                            if pr.checks.failed > 0 {
                                if !a.state.seen.contains(&key) {
                                    a.state.seen.push(key);
                                    fires.push((format!("Checks failed on #{}", pr.number), pr_vars(pr)));
                                }
                            } else {
                                // Passing or running again: the next failure counts.
                                a.state.seen.retain(|k| *k != key);
                            }
                        }
                    }
                    EventTrigger::ReviewRequested => {
                        let (Some(list), Some(account)) = (&observed.mine, &observed.account) else { continue };
                        if account.is_empty() {
                            continue;
                        }
                        let requested: Vec<&PullRequest> =
                            list.iter().filter(|p| p.state.is_open() && !p.author.eq_ignore_ascii_case(account)).collect();
                        let live: Vec<String> = requested.iter().map(|p| format!("review:{}", p.number)).collect();
                        a.state.seen.retain(|k| !k.starts_with("review:") || live.contains(k));
                        for pr in requested {
                            let key = format!("review:{}", pr.number);
                            if !a.state.seen.contains(&key) {
                                a.state.seen.push(key);
                                fires.push((format!("Review requested on #{}", pr.number), pr_vars(pr)));
                            }
                        }
                    }
                    EventTrigger::IssueLabeled { label } => {
                        let Some((_, issues)) = observed.issues.iter().find(|(l, _)| *l == label) else { continue };
                        let base = format!("base:issues:{label}");
                        let first = !a.state.seen.contains(&base);
                        if first {
                            a.state.seen.push(base);
                        }
                        for issue in issues {
                            let key = format!("issue:{}", issue.key);
                            if !a.state.seen.contains(&key) {
                                a.state.seen.push(key);
                                if !first {
                                    fires.push((format!("New issue {} labeled “{label}”", issue.key), issue_vars(issue)));
                                }
                            }
                        }
                    }
                    EventTrigger::BranchPushed { branch } => {
                        let Some((_, tip)) = observed.branches.iter().find(|(b, _)| *b == branch) else { continue };
                        let prefix = format!("push:{branch}=");
                        let key = format!("{prefix}{tip}");
                        let known = a.state.seen.iter().position(|k| k.starts_with(&prefix));
                        match known {
                            Some(i) if a.state.seen[i] == key => {}
                            Some(i) => {
                                a.state.seen[i] = key;
                                fires.push((
                                    format!("{branch} was pushed ({})", &tip[..tip.len().min(7)]),
                                    vec![("branch".into(), branch.clone())],
                                ));
                            }
                            None => a.state.seen.push(key),
                        }
                    }
                    EventTrigger::WatchPr { number } => {
                        let Some(detail) = observed.watched.iter().find(|d| d.pr.number == number) else { continue };
                        match detail.pr.state {
                            PrState::Merged => stop = Some(format!("#{number} was merged, so it stopped watching.")),
                            PrState::Closed => stop = Some(format!("#{number} was closed, so it stopped watching.")),
                            _ => {}
                        }
                        if stop.is_some() {
                            continue;
                        }
                        let key = format!("watch-checks:{number}");
                        if detail.pr.checks.failed > 0 {
                            if !a.state.seen.contains(&key) {
                                a.state.seen.push(key);
                                fires.push((format!("Checks failed on #{number}"), pr_vars(&detail.pr)));
                            }
                        } else {
                            a.state.seen.retain(|k| *k != key);
                        }
                        if let Some(review) = detail
                            .reviews
                            .iter()
                            .filter(|r| r.state == ReviewState::ChangesRequested)
                            .max_by_key(|r| r.submitted_ms)
                        {
                            let key = format!("watch-review:{number}:{}", review.submitted_ms);
                            if !a.state.seen.contains(&key) {
                                a.state.seen.push(key);
                                fires.push((format!("{} asked for changes on #{number}", review.author), pr_vars(&detail.pr)));
                            }
                        }
                    }
                    EventTrigger::Finished { .. } => {}
                }
            }
            if a.state.seen.len() > KEEP_SEEN {
                let extra = a.state.seen.len() - KEEP_SEEN;
                let mut dropped = 0;
                a.state.seen.retain(|k| {
                    if dropped < extra && !k.starts_with("base:") && !k.starts_with("push:") {
                        dropped += 1;
                        false
                    } else {
                        true
                    }
                });
            }
            if let Some(note) = &stop {
                a.paused = true;
                a.state.note = note.clone();
            }
            let id = a.id;
            self.put_automation(a);
            if stop.is_some() {
                self.rebuild_watched();
            }
            let now = self.auto_now();
            for (event, mut vars) in fires {
                vars.push(("event".into(), event.clone()));
                self.fire(id, RunReason::Event(event), now, vars);
            }
        }
    }
}

/// Each trigger's next time from `now` (0 for events and dead schedules).
pub(crate) fn next_times(spec: &AutomationSpec, now: u64) -> Vec<u64> {
    spec.triggers
        .iter()
        .map(|t| match Schedule::of(t) {
            Ok(Some(s)) => s.next_after(now).unwrap_or(0),
            _ => 0,
        })
        .collect()
}

fn same_skip(a: &Skip, b: &Skip) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
        && match (a, b) {
            (Skip::Missed { .. }, Skip::Missed { .. }) => true,
            _ => a == b,
        }
}

fn tokens(usage: &Usage) -> u64 {
    usage.input_tokens + usage.output_tokens
}

fn var(vars: &[(String, String)], name: &str) -> Option<String> {
    vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

fn pr_vars(pr: &PullRequest) -> Vec<(String, String)> {
    vec![
        ("pr".into(), format!("#{} {} ({})", pr.number, pr.title, pr.url)),
        ("pr_number".into(), pr.number.to_string()),
        ("branch".into(), pr.head.clone()),
    ]
}

fn issue_vars(issue: &Issue) -> Vec<(String, String)> {
    vec![("issue".into(), format!("{} {} ({})", issue.key, issue.title, issue.url))]
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().trim_start_matches('#').trim()
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The project's commit and working-tree contents, or empty when it is not
/// a git repository.
async fn fingerprint(git: &Git, path: &Path) -> String {
    let head = git.git(path, ["rev-parse", "HEAD"]).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned());
    let tree = git.snapshot(path).await;
    match (head, tree) {
        (Ok(head), Ok(tree)) => format!("{head}:{}", tree.as_str()),
        _ => String::new(),
    }
}

async fn run_checks(git: &Git, path: &Path, checks: &PreChecks, last: &str) -> Result<(), Skip> {
    if checks.clean_repo {
        let status = git
            .git(path, ["status", "--porcelain", "--untracked-files=normal"])
            .await
            .map_err(|e| Skip::Unavailable(format!("Couldn't read the project's status: {e}")))?;
        if !status.iter().all(u8::is_ascii_whitespace) {
            return Err(Skip::Dirty);
        }
    }
    if checks.changes_since_last_run && !last.is_empty() && fingerprint(git, path).await == last {
        return Err(Skip::NoChanges);
    }
    let command = checks.command.trim();
    if !command.is_empty() {
        let (program, args) = shell_command(command);
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).current_dir(path).stdin(std::process::Stdio::null()).kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = match tokio::time::timeout(CHECK_TIMEOUT, cmd.output()).await {
            Err(_) => return Err(Skip::CheckFailed(format!("“{command}” took longer than a minute."))),
            Ok(Err(e)) => return Err(Skip::CheckFailed(format!("Couldn't run “{command}”: {e}"))),
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&output.stdout).into_owned();
            }
            let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            let code = output.status.code().map(|c| format!(" (exit {c})")).unwrap_or_default();
            return Err(Skip::CheckFailed(format!("“{command}” failed{code}. {}", tail.join(" ")).trim().to_owned()));
        }
    }
    Ok(())
}

/// The user's shell, set to run one command.
fn shell_command(command: &str) -> (PathBuf, Vec<String>) {
    let shell = crate::terminal::Shell::user_default();
    let program = shell.program().to_owned();
    let name = program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let args: Vec<String> = if name == "pwsh" || name == "powershell" {
        ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command].map(String::from).to_vec()
    } else if name == "cmd" {
        ["/D", "/S", "/C", command].map(String::from).to_vec()
    } else {
        vec!["-c".into(), command.into()]
    };
    (program, args)
}

async fn sweep(service: &Forge, config: &ForgeConfig, git: &Git, path: &Path, plan: SweepPlan) -> Observed {
    let mut observed = Observed::default();
    if plan.open {
        observed.open = service.pull_requests(config, git, path, PrFilter::Open, false).await.ok().map(|l| l.items);
    }
    if plan.mine {
        observed.mine = service.pull_requests(config, git, path, PrFilter::Mine, false).await.ok().map(|l| l.items);
    }
    if plan.account
        && let ForgeAuth::Ready { account } = service.info(config, git, path).await.auth
    {
        observed.account = Some(account);
    }
    if !plan.labels.is_empty()
        && let Ok(at) = service.locate(git, path).await
    {
        let tracker = match at.repo.kind {
            ForgeKind::GitLab => TrackerKind::GitLab,
            _ => TrackerKind::GitHub,
        };
        for label in plan.labels {
            let query = match tracker {
                TrackerKind::GitHub => format!("label:\"{label}\" is:open is:issue"),
                _ => String::new(),
            };
            if let Ok(list) = service.issues(config, git, Some(path), tracker, &query, false).await {
                let issues = list
                    .items
                    .into_iter()
                    .filter(|i| i.state != IssueState::Closed && i.labels.iter().any(|l| l.eq_ignore_ascii_case(&label)))
                    .collect();
                observed.issues.push((label, issues));
            }
        }
    }
    for branch in plan.branches {
        let reference = format!("refs/heads/{branch}");
        if let Ok(out) = git.git(path, ["ls-remote", "origin", reference.as_str()]).await
            && let Some(tip) = String::from_utf8_lossy(&out).split_whitespace().next()
        {
            observed.branches.push((branch, tip.to_owned()));
        }
    }
    for number in plan.watch {
        if let Ok(detail) = service.pull_request(config, git, path, number, false).await {
            observed.watched.push(detail.value);
        }
    }
    observed
}
