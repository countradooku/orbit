//! Agent teams (an architect with a crew of workers) and races (one prompt,
//! several candidates, compared side by side).
//!
//! The engine keeps these records in their own files in the data folder
//! (`teams/`), outside the meta log, with their own schema version; on the
//! wire they travel in [`crate::Snapshot::teams`], as ephemeral
//! [`crate::Event::Team`] updates, and through [`TeamCommand`].

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::*;
use crate::wire::{MergeInfo, ThreadStart};

/// Every team and race, as a client first sees them.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct TeamBoard {
    pub teams: Vec<Team>,
    pub races: Vec<Race>,
}

// ── Teams ───────────────────────────────────────────────────────────────

/// Spawning limits and the circuit breaker's thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct TeamLimits {
    /// Workers of this team working at once.
    pub max_workers: u32,
    /// Failed workers before spawning stops.
    pub max_failures: u32,
    /// Spend of the whole team (architect included) before spawning stops,
    /// in millionths of a dollar; 0 means no limit.
    pub max_cost_micros: u64,
    /// How deep workers may start sub-workers (1: only the architect spawns).
    pub max_depth: u32,
}

impl Default for TeamLimits {
    fn default() -> Self {
        Self { max_workers: 4, max_failures: 3, max_cost_micros: 0, max_depth: 2 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Team {
    pub id: TeamId,
    /// The lead thread that plans and spawns.
    pub architect: ThreadId,
    pub project: Option<ProjectId>,
    pub limits: TeamLimits,
    /// The config new workers get unless the spawner names one.
    pub worker_config: Option<ThreadConfig>,
    pub workers: Vec<Worker>,
    pub notes: Vec<Note>,
    pub mail: Vec<Mail>,
    /// Set when spawning stopped for failures or cost; the user resumes.
    pub breaker: Option<Breaker>,
    /// Files two working members changed both, compared against the
    /// architect's branch.
    pub overlaps: Vec<FileOverlap>,
    pub created_ms: u64,
}

impl Team {
    pub fn worker(&self, key: &str) -> Option<&Worker> {
        self.workers.iter().find(|w| w.key == key)
    }

    pub fn worker_of(&self, thread: ThreadId) -> Option<&Worker> {
        self.workers.iter().find(|w| w.thread == Some(thread))
    }

    /// Whether `thread` is the architect or one of the workers.
    pub fn has_member(&self, thread: ThreadId) -> bool {
        self.architect == thread || self.worker_of(thread).is_some()
    }

    /// How many workers are in each bucket of the crew tally.
    pub fn tally(&self) -> Tally {
        let mut t = Tally::default();
        for w in &self.workers {
            match &w.state {
                WorkerState::Waiting(_) => t.waiting += 1,
                WorkerState::Working => t.working += 1,
                WorkerState::Done | WorkerState::Merged => t.done += 1,
                WorkerState::Failed { .. } => t.failed += 1,
                WorkerState::Stopped => t.stopped += 1,
            }
        }
        t
    }

    /// Notes newest first, at most `limit`, and how many more there are.
    pub fn latest_notes(&self, limit: usize) -> (Vec<&Note>, usize) {
        let shown: Vec<&Note> = self.notes.iter().rev().take(limit).collect();
        let more = self.notes.len() - shown.len();
        (shown, more)
    }
}

/// Crew counts for the header ("3 working · 2 done · 1 waiting").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub working: u32,
    pub done: u32,
    pub waiting: u32,
    pub failed: u32,
    pub stopped: u32,
}

impl Tally {
    pub fn total(&self) -> u32 {
        self.working + self.done + self.waiting + self.failed + self.stopped
    }
}

/// One task handed to a worker.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Worker {
    /// Short handle unique in the team (`w1`, `w2`…); what agents use.
    pub key: String,
    pub title: String,
    /// The task as the spawner wrote it.
    pub brief: String,
    /// Who spawned it: the architect, or a worker (a sub-worker).
    pub parent: ThreadId,
    /// Set once the worker started; `None` while it waits.
    pub thread: Option<ThreadId>,
    pub config: ThreadConfig,
    /// Keys of workers that must finish first.
    pub depends_on: Vec<String>,
    /// Paths (files or folders, `/`-separated) the worker expects to edit.
    pub scope: Vec<String>,
    pub state: WorkerState,
    /// The worker's last reply when it finished.
    pub summary: String,
    /// The last merge of its branch into the architect's, or its preview.
    pub merge: Option<MergeInfo>,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum WorkerState {
    Waiting(WaitReason),
    Working,
    Done,
    Failed { message: String },
    /// Its branch is merged into the architect's.
    Merged,
    Stopped,
}

impl Default for WorkerState {
    fn default() -> Self {
        WorkerState::Waiting(WaitReason::NoSlot { running: 0, limit: 0, global: false })
    }
}

impl WorkerState {
    pub fn is_finished(&self) -> bool {
        matches!(self, WorkerState::Done | WorkerState::Merged | WorkerState::Failed { .. } | WorkerState::Stopped)
    }

    /// Finished with work to show (a dependency is satisfied).
    pub fn succeeded(&self) -> bool {
        matches!(self, WorkerState::Done | WorkerState::Merged)
    }
}

/// Why a worker has not started.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum WaitReason {
    /// Workers it depends on are not done.
    Dependency { on: Vec<String> },
    /// Every slot is taken: the team's (`global: false`) or Orbit's.
    NoSlot { running: u32, limit: u32, global: bool },
    /// A working teammate has edited, or plans to edit, the same path.
    FileScope { path: String, with: String },
    /// The circuit breaker stopped spawning.
    Breaker,
    /// A dependency failed or was stopped; it will not start on its own.
    DependencyFailed { key: String },
}

impl WaitReason {
    /// One line of interface copy.
    pub fn describe(&self) -> String {
        match self {
            WaitReason::Dependency { on } => format!("Waiting on {}", on.join(", ")),
            WaitReason::NoSlot { limit, global: false, .. } => format!("No free slot ({limit} working)"),
            WaitReason::NoSlot { limit, global: true, .. } => format!("No free slot ({limit} across Orbit)"),
            WaitReason::FileScope { path, with } => format!("{with} is editing {path}"),
            WaitReason::Breaker => "Paused by the circuit breaker".into(),
            WaitReason::DependencyFailed { key } => format!("{key} failed"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Note {
    pub id: u64,
    pub author: ThreadId,
    /// `architect`, a worker key, or `you`.
    pub author_label: String,
    pub text: String,
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum MailKind {
    Message,
    Question,
    Handoff,
    Status,
    Done,
}

impl MailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MailKind::Message => "message",
            MailKind::Question => "question",
            MailKind::Handoff => "handoff",
            MailKind::Status => "status",
            MailKind::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "message" => MailKind::Message,
            "question" => MailKind::Question,
            "handoff" => MailKind::Handoff,
            "status" => MailKind::Status,
            "done" => MailKind::Done,
            _ => return None,
        })
    }
}

/// A message between members, stored until delivered at a turn boundary.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Mail {
    pub id: u64,
    /// `None`: from the user or from Orbit itself.
    pub from: Option<ThreadId>,
    pub from_label: String,
    pub to: ThreadId,
    pub kind: MailKind,
    pub text: String,
    /// Steer a working recipient now instead of waiting for its turn to end
    /// (at most once a minute per recipient; otherwise delivered normally).
    pub urgent: bool,
    pub sent_ms: u64,
    pub delivered_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Breaker {
    pub reason: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct FileOverlap {
    pub path: String,
    pub threads: Vec<ThreadId>,
}

// ── Races ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Race {
    pub id: RaceId,
    pub project: ProjectId,
    pub prompt: String,
    /// The branch every candidate starts from and the winner merges into.
    pub base: String,
    /// Run in each worktree when its candidate finishes.
    pub test_command: Option<String>,
    pub candidates: Vec<Candidate>,
    pub judge: JudgeState,
    pub state: RaceState,
    pub created_ms: u64,
}

impl Race {
    pub fn candidate(&self, thread: ThreadId) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.thread == thread)
    }

    /// Every candidate finished and its tests (if any) ran.
    pub fn settled(&self) -> bool {
        self.candidates.iter().all(|c| c.finished.is_some() && !matches!(c.test, TestState::Pending | TestState::Running))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Candidate {
    pub thread: ThreadId,
    /// `A`, `B`, `C`…
    pub label: String,
    pub config: ThreadConfig,
    /// When its first turn ended, and how.
    pub finished: Option<CandidateEnd>,
    pub duration_ms: u64,
    pub usage: Usage,
    /// Its changes against the base.
    pub files: Vec<FileChange>,
    pub test: TestState,
}

impl Candidate {
    pub fn additions(&self) -> u32 {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn deletions(&self) -> u32 {
        self.files.iter().map(|f| f.deletions).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum CandidateEnd {
    Completed,
    Failed { message: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TestState {
    /// No test command is configured.
    #[default]
    None,
    Pending,
    Running,
    Passed { ms: u64 },
    Failed { code: Option<i32>, ms: u64, tail: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum JudgeState {
    #[default]
    Off,
    /// Asked for; runs once every candidate settled.
    Waiting,
    Running,
    Verdict { winner: ThreadId, reason: String },
    Failed { message: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum RaceState {
    #[default]
    Running,
    /// The winner was merged and the rest archived.
    Decided { winner: ThreadId },
    Cancelled,
}

// ── Commands ────────────────────────────────────────────────────────────

/// Where a team message goes: a worker key, `architect`, `parent`, `@all`
/// (every working worker) or a thread id.
pub type Recipient = String;

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TeamCommand {
    /// Starts `start` as the architect of a new team; its sessions get the
    /// Orbit MCP tools. Replies `Reply::Thread`.
    Start { start: ThreadStart, limits: TeamLimits, worker_config: Option<ThreadConfig> },
    /// A member spawns a worker. It starts in its own worktree off the
    /// architect's branch as soon as its dependencies are done, a slot is
    /// free and no teammate is editing its scope. Replies
    /// [`TeamReply::Worker`].
    SpawnWorker {
        actor: ThreadId,
        title: String,
        brief: String,
        depends_on: Vec<String>,
        scope: Vec<String>,
        config: Option<ThreadConfig>,
    },
    /// Stores mail for delivery at the recipient's next turn boundary.
    /// `actor: None` is the user. Replies `Reply::Done`.
    Message { actor: Option<ThreadId>, team: Option<TeamId>, to: Recipient, kind: MailKind, text: String, urgent: bool },
    /// Replies `Reply::Done`.
    PostNote { actor: Option<ThreadId>, team: Option<TeamId>, text: String },
    /// Newest first. Replies [`TeamReply::Notes`].
    ReadNotes { actor: Option<ThreadId>, team: Option<TeamId>, limit: u32 },
    /// The team `actor` belongs to (or `team`). Replies [`TeamReply::Team`].
    Status { actor: Option<ThreadId>, team: Option<TeamId> },
    /// Merges a finished worker's branch into the architect's worktree
    /// (dry run first; conflicts are reported, nothing changes). Replies
    /// `Reply::Merge`.
    MergeWorker { actor: Option<ThreadId>, team: Option<TeamId>, worker: String, dry_run: bool },
    /// Stops a worker's turn and keeps it from continuing. Replies `Done`.
    StopWorker { actor: Option<ThreadId>, team: Option<TeamId>, worker: String },
    /// The user resumes spawning after the circuit breaker tripped, with
    /// new limits if given. Replies `Reply::Done`.
    ResetBreaker { team: TeamId, limits: Option<TeamLimits> },

    /// Runs `prompt` once per config, each in its own worktree off `base`
    /// (empty: the project's base branch). Replies [`TeamReply::Race`].
    StartRace {
        project: ProjectId,
        prompt: String,
        configs: Vec<ThreadConfig>,
        test_command: Option<String>,
        judge: bool,
        base: String,
    },
    /// `a`'s changes against `b`'s, file by file (both working trees, as
    /// they are now). Applying a hunk of it to `a` takes `b`'s version of
    /// that hunk. Replies `Reply::Diff`.
    CompareCandidates { a: ThreadId, b: ThreadId },
    /// Applies `patch` (from `hunk_patch` of a comparison) to `thread`'s
    /// working tree; nothing changes unless all of it applies. Replies
    /// `Reply::Done`.
    ApplyPatch { thread: ThreadId, patch: String },
    /// Runs the race's tests again in every candidate. Replies `Done`.
    RunTests { race: RaceId },
    /// Asks a judge for a verdict now. Replies `Done`.
    Judge { race: RaceId },
    /// Merges `winner` into the base branch and archives the others.
    /// Replies `Reply::Merge`.
    KeepWinner { race: RaceId, winner: ThreadId },
    CancelRace { race: RaceId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TeamReply {
    Team(Box<Team>),
    Worker { team: TeamId, key: String, thread: Option<ThreadId>, state: WorkerState },
    /// Newest first; `more` older notes were left out.
    Notes { notes: Vec<Note>, more: u32 },
    Race(RaceId),
}

/// Ephemeral updates; the latest state is in the snapshot.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TeamEvent {
    Team(Box<Team>),
    TeamRemoved(TeamId),
    Race(Box<Race>),
    RaceRemoved(RaceId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tally_counts_each_state_once() {
        let w = |state| Worker { state, ..Default::default() };
        let team = Team {
            workers: vec![
                w(WorkerState::Working),
                w(WorkerState::Working),
                w(WorkerState::Done),
                w(WorkerState::Merged),
                w(WorkerState::Waiting(WaitReason::Breaker)),
                w(WorkerState::Failed { message: "x".into() }),
            ],
            ..Default::default()
        };
        let t = team.tally();
        assert_eq!((t.working, t.done, t.waiting, t.failed, t.total()), (2, 2, 1, 1, 6));
    }

    #[test]
    fn latest_notes_are_newest_first_with_a_count_of_the_rest() {
        let team = Team {
            notes: (0..7)
                .map(|i| Note { id: i, author: ThreadId(1), author_label: "w1".into(), text: format!("n{i}"), at_ms: i })
                .collect(),
            ..Default::default()
        };
        let (shown, more) = team.latest_notes(3);
        assert_eq!(shown.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["n6", "n5", "n4"]);
        assert_eq!(more, 4);
    }

    #[test]
    fn records_round_trip() {
        let race = Race {
            id: RaceId(9),
            candidates: vec![Candidate { label: "A".into(), test: TestState::Passed { ms: 3 }, ..Default::default() }],
            judge: JudgeState::Verdict { winner: ThreadId(2), reason: "Smaller diff.".into() },
            ..Default::default()
        };
        let back: Race = crate::decode(&crate::encode(&race)).unwrap();
        assert_eq!(back, race);
    }
}
