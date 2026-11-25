//! Automations: agent runs started by a schedule or an event.
//!
//! An [`Automation`] is logged in the meta log as
//! [`Event::AutomationUpserted`](crate::Event::AutomationUpserted), and every
//! run as [`Event::AutomationRun`](crate::Event::AutomationRun). Both are
//! logged types: follow the rules in [`crate::legacy`] before changing them.
//! Schedules are kept as text (a cron expression or a recurrence rule) and
//! parsed by the engine, so what a schedule can say can grow without a
//! schema change.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::ThreadConfig;

/// What the user set up. The engine owns [`AutomationState`].
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct AutomationSpec {
    pub name: String,
    pub project: Option<ProjectId>,
    pub target: RunTarget,
    /// Provider, instance, model, effort (an option), mode and access.
    pub config: ThreadConfig,
    /// Sent as the run's message after its variables are filled in:
    /// `{date}`, `{time}`, `{branch}`, `{pr}`, `{issue}`, `{event}`,
    /// `{last_run_summary}`, `{project}` and `{name}`.
    pub prompt: String,
    /// Any of them starts a run.
    pub triggers: Vec<Trigger>,
    pub checks: PreChecks,
    pub limits: Limits,
    pub on_finish: OnFinish,
    /// The recipe it was made from, if any (its id).
    pub recipe: String,
}

/// Where a run works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum RunTarget {
    /// A fresh worktree on a new branch for every run.
    #[default]
    Worktree,
    /// The project's own folder.
    Project,
    /// Continue one thread: every run is a new message in it.
    Thread(ThreadId),
    /// The branch of the pull request the triggering event is about (a
    /// worktree for it, or the thread that already has it); a fresh
    /// worktree when the run has no pull request.
    PullRequest,
}

/// What starts a run.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Trigger {
    /// A five-field cron expression (`0 9 * * 1-5`) in an IANA time zone
    /// (empty: the system's).
    Cron { expr: String, tz: String },
    /// A recurrence rule in the iCalendar style:
    /// `FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0`.
    Rule { rule: String, tz: String },
    /// Every `minutes`, counted from the previous run.
    Interval { minutes: u32 },
    /// Something happened on the forge or in Orbit.
    Event(EventTrigger),
}

impl Trigger {
    pub fn is_schedule(&self) -> bool {
        !matches!(self, Trigger::Event(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum EventTrigger {
    /// Checks started failing on an open pull request (`mine`: only pull
    /// requests you wrote, review or are assigned).
    ChecksFailed { mine: bool },
    /// Someone asked you to review a pull request.
    ReviewRequested,
    /// A new issue has this label.
    IssueLabeled { label: String },
    /// The remote branch moved.
    BranchPushed { branch: String },
    /// A run of another automation, or a thread's turn, ended.
    Finished { source: ChainSource, when: ChainWhen },
    /// One pull request until it is merged or closed: runs when its checks
    /// fail or a reviewer asks for changes, then the automation pauses.
    WatchPr { number: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ChainSource {
    Automation(AutomationId),
    Thread(ThreadId),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum ChainWhen {
    #[default]
    Succeeded,
    Failed,
    Either,
}

/// Checked before a scheduled or event run starts; "Run now" skips them.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PreChecks {
    /// Only when the project's files or commits changed since the last run.
    pub changes_since_last_run: bool,
    /// Only when the project has no uncommitted changes.
    pub clean_repo: bool,
    /// A shell command run in the project folder that must succeed (empty:
    /// none). It has a minute.
    pub command: String,
    /// Skip a run while the previous one is still going.
    pub skip_if_running: bool,
}

impl Default for PreChecks {
    fn default() -> Self {
        Self { changes_since_last_run: false, clean_repo: false, command: String::new(), skip_if_running: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Limits {
    /// At most this many runs start per day (0: no limit).
    pub max_runs_per_day: u32,
    /// A run still going after this long is stopped (0: no limit).
    pub timeout_minutes: u32,
    /// A run missed while the computer slept or Orbit was closed still
    /// starts if it is at most this late; otherwise it is recorded as
    /// missed.
    pub grace_minutes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_runs_per_day: 0, timeout_minutes: 60, grace_minutes: 30 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Notify {
    #[default]
    Always,
    /// Only failed or timed-out runs ask for attention.
    Failures,
    Never,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct OnFinish {
    pub notify: Notify,
    /// Settle the run's thread when it changed nothing.
    pub settle_if_unchanged: bool,
    /// Commit, push and open a pull request with the run's changes (worktree
    /// runs only).
    pub open_pr: bool,
}

/// What the engine remembers about an automation between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AutomationState {
    /// When each trigger is next due (Unix ms; 0 for events), by index in
    /// [`AutomationSpec::triggers`]. Persisted before a run starts, so a
    /// restart never fires the same occurrence twice.
    pub next_ms: Vec<u64>,
    /// When the last run was started (interval triggers count from it).
    pub last_fire_ms: u64,
    /// The local day (days since 1970-01-01) `day_runs` counts.
    pub day: u32,
    pub day_runs: u32,
    /// The project's files and commits after the last run, for
    /// [`PreChecks::changes_since_last_run`].
    pub fingerprint: String,
    /// Events already acted on (pull requests, issues, branch tips).
    pub seen: Vec<String>,
    /// Why the automation stopped by itself, or what is wrong with it.
    pub note: String,
    /// Runs that are due but have not started: waiting for a free slot
    /// (runs are capped across automations) or for their checks. Kept here
    /// so a restart neither loses nor repeats them.
    pub pending: Vec<PendingRun>,
}

/// A run that is due but has not started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct PendingRun {
    /// The id its record will have.
    pub id: RunId,
    pub reason: RunReason,
    pub due_ms: u64,
    /// Prompt variables taken from the event (`pr`, `issue`, `branch`…).
    pub vars: Vec<(String, String)>,
}

/// A user's automation and its engine state.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Automation {
    pub id: AutomationId,
    pub spec: AutomationSpec,
    pub paused: bool,
    pub state: AutomationState,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl Automation {
    /// The earliest time a schedule trigger is due, if any.
    pub fn next_run_ms(&self) -> Option<u64> {
        if self.paused {
            return None;
        }
        self.state.next_ms.iter().copied().filter(|t| *t > 0).min()
    }
}

/// Why a run started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum RunReason {
    #[default]
    Schedule,
    /// A scheduled run that started late (the computer slept, or Orbit was
    /// closed) but within the grace window.
    Late,
    Manual,
    /// An event, described (`Checks failed on #42`).
    Event(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Skip {
    /// Missed by more than the grace window, `count` times.
    Missed { count: u32 },
    StillRunning,
    DailyLimit,
    NoChanges,
    Dirty,
    /// The check command failed; its last output.
    CheckFailed(String),
    /// The project, thread or forge was not available.
    Unavailable(String),
}

/// What became of a run. Runs that are due but not started yet are
/// [`AutomationState::pending`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum RunStatus {
    #[default]
    Running,
    Succeeded,
    Failed(String),
    TimedOut,
    Skipped(Skip),
    Cancelled,
}

impl RunStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, RunStatus::Running)
    }
    pub fn is_failure(&self) -> bool {
        matches!(self, RunStatus::Failed(_) | RunStatus::TimedOut)
    }
}

/// One run of an automation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AutomationRun {
    pub id: RunId,
    pub automation: AutomationId,
    pub reason: RunReason,
    pub status: RunStatus,
    pub thread: Option<ThreadId>,
    /// The thread's turn count before the run, so its own turns are known.
    pub turn: u32,
    /// When it was due (Unix ms).
    pub due_ms: u64,
    pub started_ms: u64,
    pub finished_ms: u64,
    /// Input plus output tokens the run used.
    pub tokens: u64,
    /// Whether it changed files (`None`: not known).
    pub changed: Option<bool>,
    /// The agent's last message, shortened.
    pub summary: String,
    /// The pull request it opened.
    pub pr_url: String,
    /// Identical skips in a row are counted on one record.
    pub repeats: u32,
}

impl AutomationRun {
    pub fn duration_ms(&self) -> u64 {
        if self.finished_ms > self.started_ms && self.started_ms > 0 { self.finished_ms - self.started_ms } else { 0 }
    }
}

/// Automation requests ([`crate::Command::Automation`]).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AutomationCommand {
    /// Create (`id: None`) or replace an automation's spec. Replies
    /// [`AutomationReply::Saved`].
    Save { id: Option<AutomationId>, spec: AutomationSpec },
    /// Deletes it and its history; runs already going keep their threads.
    Delete { id: AutomationId },
    SetPaused { id: AutomationId, paused: bool },
    /// Start a run now, without pre-run checks or the daily limit. Replies
    /// [`AutomationReply::Run`].
    RunNow { id: AutomationId },
    /// The newest runs, newest first. Replies [`AutomationReply::Runs`].
    History { id: AutomationId, limit: u32 },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AutomationReply {
    Saved(AutomationId),
    Run(RunId),
    Runs(Vec<AutomationRun>),
}
