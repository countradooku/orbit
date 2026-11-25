//! Source-control hosts (pull requests, checks, merges) and issue trackers.
//!
//! The engine talks to each host through the user's own CLI (`gh`, `glab`)
//! and to trackers through their APIs; these records are what every client
//! sees, whichever host produced them. Times are Unix milliseconds, 0 when
//! unknown.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::ThreadConfig;

// ── Hosts ────────────────────────────────────────────────────────────────

/// A git hosting service, detected from the repository's remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Bitbucket,
    AzureDevOps,
    Gitea,
}

impl ForgeKind {
    pub fn label(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GitHub",
            ForgeKind::GitLab => "GitLab",
            ForgeKind::Bitbucket => "Bitbucket",
            ForgeKind::AzureDevOps => "Azure DevOps",
            ForgeKind::Gitea => "Gitea",
        }
    }

    /// What the host calls a change proposal.
    pub fn review_noun(self) -> &'static str {
        match self {
            ForgeKind::GitLab => "merge request",
            _ => "pull request",
        }
    }

    /// The sigil before a change proposal's number (`#12`, `!12`).
    pub fn review_sigil(self) -> &'static str {
        match self {
            ForgeKind::GitLab => "!",
            _ => "#",
        }
    }
}

/// The hosted repository behind a project's remote.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ForgeRepo {
    pub kind: ForgeKind,
    /// `github.com`, `gitlab.example.com`.
    pub host: String,
    /// `owner/name`, or `group/subgroup/name` on GitLab.
    pub path: String,
    /// The repository's web page.
    pub web_url: String,
    /// The git remote it was read from (`origin`).
    pub remote: String,
}

/// Whether Orbit can talk to the host, and the exact fix when it cannot.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ForgeAuth {
    /// Signed in as `account` (may be empty when the CLI does not say).
    Ready { account: String },
    /// The host's CLI is not installed. `fix` says how to install it.
    NotInstalled { program: String, fix: String },
    /// The CLI is installed but not signed in to this host. `fix` is the
    /// exact command to run.
    SignedOut { program: String, fix: String },
    /// Orbit has no adapter for this host yet.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ForgeInfo {
    /// `None` when the project has no remote on a known host.
    pub repo: Option<ForgeRepo>,
    pub auth: ForgeAuth,
    /// The local branch checked out in the project, for "my PR" defaults.
    pub branch: Option<String>,
}

// ── Pull requests ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PrState {
    #[default]
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            PrState::Open => "Open",
            PrState::Draft => "Draft",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, PrState::Open | PrState::Draft)
    }
}

/// Which pull requests a list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PrFilter {
    #[default]
    Open,
    /// Open ones the signed-in user wrote, was asked to review, or is
    /// assigned.
    Mine,
    Merged,
    Closed,
}

impl PrFilter {
    pub const ALL: [PrFilter; 4] = [PrFilter::Open, PrFilter::Mine, PrFilter::Merged, PrFilter::Closed];

    pub fn label(self) -> &'static str {
        match self {
            PrFilter::Open => "Open",
            PrFilter::Mine => "Mine",
            PrFilter::Merged => "Merged",
            PrFilter::Closed => "Closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum CheckState {
    #[default]
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
    Skipped,
    /// Finished without a verdict (neutral, stale, action required).
    Neutral,
}

impl CheckState {
    pub fn is_pending(self) -> bool {
        matches!(self, CheckState::Queued | CheckState::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            CheckState::Queued => "Queued",
            CheckState::Running => "Running",
            CheckState::Passed => "Passed",
            CheckState::Failed => "Failed",
            CheckState::Cancelled => "Cancelled",
            CheckState::Skipped => "Skipped",
            CheckState::Neutral => "Neutral",
        }
    }
}

/// Counts of a pull request's checks by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ChecksSummary {
    pub passed: u16,
    pub failed: u16,
    pub pending: u16,
    /// Skipped, cancelled and neutral.
    pub other: u16,
}

impl ChecksSummary {
    pub fn total(&self) -> u16 {
        self.passed + self.failed + self.pending + self.other
    }

    pub fn add(&mut self, state: CheckState) {
        match state {
            CheckState::Passed => self.passed += 1,
            CheckState::Failed => self.failed += 1,
            CheckState::Queued | CheckState::Running => self.pending += 1,
            _ => self.other += 1,
        }
    }

    /// One state for the whole set: any failure fails it, then anything
    /// pending keeps it running. `None` when there are no checks.
    pub fn overall(&self) -> Option<CheckState> {
        if self.total() == 0 {
            None
        } else if self.failed > 0 {
            Some(CheckState::Failed)
        } else if self.pending > 0 {
            Some(CheckState::Running)
        } else if self.passed > 0 {
            Some(CheckState::Passed)
        } else {
            Some(CheckState::Neutral)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ReviewDecision {
    #[default]
    None,
    ReviewRequired,
    Approved,
    ChangesRequested,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum Mergeability {
    #[default]
    Unknown,
    /// Can be merged now.
    Clean,
    Conflicting,
    /// Branch protection blocks it (reviews or required checks missing).
    Blocked,
    /// The base moved; the branch must be updated first.
    Behind,
    /// Mergeable, but some non-required checks fail.
    Unstable,
    Draft,
}

impl Mergeability {
    pub fn label(self) -> &'static str {
        match self {
            Mergeability::Unknown => "Checking mergeability",
            Mergeability::Clean => "Ready to merge",
            Mergeability::Conflicting => "Has conflicts",
            Mergeability::Blocked => "Blocked by branch rules",
            Mergeability::Behind => "Behind the base branch",
            Mergeability::Unstable => "Mergeable with failing checks",
            Mergeability::Draft => "Draft",
        }
    }

    pub fn can_merge(self) -> bool {
        matches!(self, Mergeability::Clean | Mergeability::Unstable)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum MergeMethod {
    Merge,
    #[default]
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] = [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase];

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Merge commit",
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }
}

/// A pull (or merge) request as lists show it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub state: PrState,
    /// Source branch.
    pub head: String,
    /// Target branch.
    pub base: String,
    pub url: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub checks: ChecksSummary,
    pub review: ReviewDecision,
    pub additions: u32,
    pub deletions: u32,
    pub labels: Vec<String>,
    /// The source branch lives in a fork.
    pub cross_repo: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Check {
    pub name: String,
    /// The workflow or pipeline stage it belongs to, when there is one.
    pub group: String,
    pub state: CheckState,
    pub started_ms: u64,
    pub finished_ms: u64,
    /// Where its details and logs are.
    pub url: String,
    /// The host's job id, used to fetch failure logs; empty when the check
    /// is not a job Orbit can read (an external status).
    pub job: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    #[default]
    Commented,
    Dismissed,
    Pending,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Review {
    pub author: String,
    pub state: ReviewState,
    /// Markdown.
    pub body: String,
    pub submitted_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Comment {
    pub author: String,
    /// Markdown.
    pub body: String,
    pub created_ms: u64,
    /// A review comment's file, when it is attached to code.
    pub path: Option<String>,
    pub line: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PullRequestDetail {
    pub pr: PullRequest,
    /// Markdown.
    pub body: String,
    /// The head commit the detail was read at; merges are pinned to it.
    pub head_sha: String,
    pub mergeable: Mergeability,
    /// Methods the repository allows; empty when unknown (all are offered).
    pub merge_methods: Vec<MergeMethod>,
    /// Auto-merge is enabled, with this method.
    pub auto_merge: Option<MergeMethod>,
    pub checks: Vec<Check>,
    pub reviews: Vec<Review>,
    /// Conversation comments, oldest first.
    pub comments: Vec<Comment>,
    pub changed_files: u32,
}

/// A list as the engine last fetched it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PullRequestList {
    pub items: Vec<PullRequest>,
    pub fetched_ms: u64,
    /// Set when the list comes from the cache because a refresh failed
    /// (rate limit, offline); says why.
    pub stale: Option<String>,
}

// ── Issues ───────────────────────────────────────────────────────────────

/// Where issues come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TrackerKind {
    GitHub,
    GitLab,
    Linear,
    Jira,
}

impl TrackerKind {
    pub const ALL: [TrackerKind; 4] = [TrackerKind::GitHub, TrackerKind::GitLab, TrackerKind::Linear, TrackerKind::Jira];

    pub fn label(self) -> &'static str {
        match self {
            TrackerKind::GitHub => "GitHub",
            TrackerKind::GitLab => "GitLab",
            TrackerKind::Linear => "Linear",
            TrackerKind::Jira => "Jira",
        }
    }

    /// Trackers that belong to the project's host rather than an account.
    pub fn is_forge(self) -> bool {
        matches!(self, TrackerKind::GitHub | TrackerKind::GitLab)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum IssueState {
    #[default]
    Open,
    /// In progress (Linear "started", Jira "In Progress").
    Started,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Issue {
    pub tracker: TrackerKind,
    /// What people call it: `#42`, `ENG-123`, `PROJ-7`.
    pub key: String,
    pub title: String,
    pub state: IssueState,
    /// The tracker's own name for the state (`In Review`, `Backlog`).
    pub status: String,
    pub author: String,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub url: String,
    pub updated_ms: u64,
    pub comments: u32,
    /// The branch name the tracker suggests (Linear's `branchName`).
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct IssueDetail {
    pub issue: Issue,
    /// Markdown.
    pub body: String,
    /// Oldest first.
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct IssueList {
    pub items: Vec<Issue>,
    pub fetched_ms: u64,
    pub stale: Option<String>,
}

// ── Ship state ───────────────────────────────────────────────────────────

/// Where a thread's work is on the way from edit to merge. Each stage names
/// the next step, so one control can always offer it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ShipStage {
    /// Not in a git repository.
    #[default]
    NoRepo,
    /// Nothing changed yet.
    Clean,
    /// Files changed, not committed.
    Uncommitted,
    /// Commits not pushed.
    Committed,
    /// Pushed; no pull request yet.
    Pushed,
    /// A pull request is open and its checks run.
    ChecksRunning,
    ChecksFailed,
    /// Checks are done; reviews, conflicts or rules still block it.
    Blocked,
    Mergeable,
    Merged,
    Closed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ShipState {
    pub stage: ShipStage,
    pub branch: Option<String>,
    /// The branch is the project's default branch (pushing there skips
    /// review).
    pub on_default: bool,
    /// Uncommitted changes.
    pub files: u32,
    pub additions: u32,
    pub deletions: u32,
    /// Commits not on the upstream, and upstream commits not here.
    pub ahead: u32,
    pub behind: u32,
    pub forge: Option<ForgeKind>,
    pub pr: Option<PullRequest>,
    pub mergeable: Mergeability,
    pub auto_merge: bool,
    /// Why the forge part is missing (signed out, no remote); the stage then
    /// stops at `Pushed`.
    pub note: Option<String>,
}

// ── Commands and replies ─────────────────────────────────────────────────

/// Commands for hosts and trackers, carried by `Command::Forge`.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ForgeCommand {
    /// The project's host, repository and sign-in state.
    Info { project: ProjectId },
    /// `refresh` asks for fresh data (window focus, the Refresh button);
    /// the engine still serves a seconds-old result instead of refetching.
    PullRequests { project: ProjectId, filter: PrFilter, refresh: bool },
    PullRequest { project: ProjectId, number: u64, refresh: bool },
    /// Merges at `head_sha` (empty skips the pin), or enables auto-merge
    /// when `auto` is set.
    Merge { project: ProjectId, number: u64, method: MergeMethod, auto: bool, delete_branch: bool, head_sha: String },
    DisableAutoMerge { project: ProjectId, number: u64 },
    /// Opens the pull request's branch in a thread on its own worktree, or
    /// returns the thread that already has it. Replies `Reply::Thread`.
    Checkout { project: ProjectId, number: u64, config: ThreadConfig },
    /// Sends the failing checks' names and log excerpts to `thread` (or to
    /// the thread working on the branch, checking it out when none is).
    /// Replies `Reply::Thread`.
    FixChecks { project: ProjectId, number: u64, thread: Option<ThreadId>, config: ThreadConfig },
    /// Issues assigned to the user, or matching `query` when it is set.
    /// `project` picks the repository for GitHub and GitLab issues.
    Issues { project: Option<ProjectId>, tracker: TrackerKind, query: String, refresh: bool },
    Issue { project: Option<ProjectId>, tracker: TrackerKind, key: String, refresh: bool },
    /// Creates a thread (on its own branch and worktree when `worktree`) and
    /// sends it the issue as its first prompt. Replies `Reply::Thread`.
    StartFromIssue { project: ProjectId, tracker: TrackerKind, key: String, config: ThreadConfig, worktree: bool },
    /// Where a thread's work stands; see [`ShipStage`].
    Ship { thread: ThreadId, refresh: bool },
    /// The context `Command::GenerateText` needs to draft a commit message
    /// (`pull_request: false`: status, diff, recent subjects) or pull
    /// request text (commits, files, diff against the base, the
    /// repository's template). Replies `Reply::Text`.
    DraftInput { thread: ThreadId, pull_request: bool },
    /// The host and tracker settings. Replies [`ForgeReply::Integrations`].
    Integrations,
    /// Replaces them (sanitized). Replies `Reply::Done`.
    SetIntegrations { settings: IntegrationSettings },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ForgeReply {
    Info(Box<ForgeInfo>),
    PullRequests(PullRequestList),
    PullRequest(Box<PullRequestDetail>),
    Issues(IssueList),
    Issue(Box<IssueDetail>),
    Ship(Box<ShipState>),
    Integrations(IntegrationSettings),
}

// ── Integration settings ─────────────────────────────────────────────────

/// Host programs and tracker credentials.
///
/// Kept by the engine in their own file in the data folder, outside the
/// settings record: settings travel in logged, replayed events to every
/// client, and tokens should not. `Debug` redacts the tokens, so they never
/// reach a log either.
#[derive(Clone, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct IntegrationSettings {
    /// Explicit GitLab CLI binary; empty uses `glab` from PATH.
    pub glab_binary: String,
    /// A Linear personal API key (Linear › Settings › Security & access).
    pub linear_api_key: String,
    /// Jira site, e.g. `https://acme.atlassian.net`, or a Server / Data
    /// Center base URL.
    pub jira_url: String,
    /// The Atlassian account email for a Jira Cloud API token; empty sends
    /// the token as a bearer token (Server / Data Center personal tokens).
    pub jira_email: String,
    pub jira_api_token: String,
}

impl IntegrationSettings {
    /// Trimmed, with no trailing slash on the Jira URL. Idempotent.
    pub fn sanitized(mut self) -> Self {
        for field in [&mut self.glab_binary, &mut self.linear_api_key, &mut self.jira_url, &mut self.jira_email, &mut self.jira_api_token] {
            let trimmed = field.trim();
            if trimmed.len() != field.len() {
                *field = trimmed.to_owned();
            }
        }
        while self.jira_url.ends_with('/') {
            self.jira_url.pop();
        }
        self
    }

    pub fn linear_configured(&self) -> bool {
        !self.linear_api_key.is_empty()
    }

    pub fn jira_configured(&self) -> bool {
        !self.jira_url.is_empty() && !self.jira_api_token.is_empty()
    }

    pub fn configured(&self, tracker: TrackerKind) -> bool {
        match tracker {
            TrackerKind::Linear => self.linear_configured(),
            TrackerKind::Jira => self.jira_configured(),
            TrackerKind::GitHub | TrackerKind::GitLab => true,
        }
    }
}

impl std::fmt::Debug for IntegrationSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = |s: &str| if s.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("IntegrationSettings")
            .field("glab_binary", &self.glab_binary)
            .field("linear_api_key", &secret(&self.linear_api_key))
            .field("jira_url", &self.jira_url)
            .field("jira_email", &self.jira_email)
            .field("jira_api_token", &secret(&self.jira_api_token))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn checks_summary_reports_the_worst_state() {
        let mut s = ChecksSummary::default();
        assert_eq!(s.overall(), None);
        s.add(CheckState::Passed);
        assert_eq!(s.overall(), Some(CheckState::Passed));
        s.add(CheckState::Running);
        assert_eq!(s.overall(), Some(CheckState::Running));
        s.add(CheckState::Failed);
        assert_eq!(s.overall(), Some(CheckState::Failed));
        assert_eq!(s.total(), 3);
    }

    #[test]
    fn tokens_never_show_in_debug_output() {
        let s = IntegrationSettings {
            linear_api_key: "lin_api_SECRET".into(),
            jira_api_token: " ATATT-SECRET ".into(),
            jira_url: "https://acme.atlassian.net/ ".into(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.jira_api_token, "ATATT-SECRET");
        assert_eq!(s.jira_url, "https://acme.atlassian.net");
        assert!(s.jira_configured() && s.linear_configured());
        let shown = format!("{s:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("<redacted>"));
        let back: IntegrationSettings = decode(&encode(&s)).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn forge_replies_round_trip() {
        let detail = PullRequestDetail {
            pr: PullRequest { number: 7, title: "Fix it".into(), labels: vec!["bug".into()], ..Default::default() },
            checks: vec![Check { name: "test".into(), state: CheckState::Failed, ..Default::default() }],
            merge_methods: vec![MergeMethod::Squash],
            ..Default::default()
        };
        let reply = ForgeReply::PullRequest(Box::new(detail));
        let back: ForgeReply = decode(&encode(&reply)).unwrap();
        assert_eq!(back, reply);
    }
}
