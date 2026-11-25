//! Git hosts and issue trackers: pull requests, checks, merges and issues.
//!
//! * **Hosts** are reached through the user's own CLI ([`github`] with `gh`,
//!   [`gitlab`] with `glab`), so Orbit never holds a hosting token. The host
//!   is detected from the project's remote ([`remote`]); Bitbucket, Azure
//!   DevOps and Gitea are detected too and plug in behind [`ForgeHost`].
//! * **Trackers** ([`IssueTracker`]) are the hosts' issues plus Linear
//!   (GraphQL) and Jira (REST) over a small HTTPS client ([`http`]).
//! * **No polling, no storms.** Results are cached per repository and query;
//!   a list is refetched only when it is older than its time to live or the
//!   client asks (window focus, Refresh) and it is more than a few seconds
//!   old. Concurrent requests for the same data share one process. A push or
//!   merge drops the repository's entries. When a host reports a rate limit,
//!   Orbit stops asking it until the limit resets and serves the last result
//!   with a note saying so (T3 Code re-fetched every PR on focus and burned
//!   its GraphQL quota, #13496).
//! * Every process is windowless and bounded by one semaphore.

mod cli;
mod github;
mod gitlab;
/// The shared HTTPS client (also used by the chat bots).
pub mod http;
mod jira;
mod json;
mod linear;
pub mod prompt;
pub mod remote;
mod time;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orbit_proto::*;
use tokio::sync::Semaphore;

use crate::git::Git;

pub use time::parse_ms as parse_time;

/// Host CLI processes running at once, across all repositories.
const MAX_PROCESSES: usize = 4;
/// A refresh request is ignored for data younger than this, so a burst of
/// focus events costs one request.
const MIN_REFRESH: Duration = Duration::from_secs(5);
const LIST_TTL: Duration = Duration::from_secs(90);
/// Pull requests whose checks still run are refreshed sooner.
const PENDING_TTL: Duration = Duration::from_secs(20);
const DETAIL_TTL: Duration = Duration::from_secs(120);
const AUTH_TTL: Duration = Duration::from_secs(300);
const LOCATE_TTL: Duration = Duration::from_secs(300);
/// Cached results kept, oldest dropped first.
const CACHE_ENTRIES: usize = 256;
/// Back-off after a rate limit with no reset time: 1, 2, 4… up to 15 min.
const LIMIT_BACKOFF: Duration = Duration::from_secs(60);
const LIMIT_MAX: Duration = Duration::from_secs(15 * 60);

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ForgeError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    NotInstalled { program: String, fix: String },
    SignedOut { program: String, fix: String },
    /// A tracker rejected its credentials.
    TrackerAuth { tracker: TrackerKind, fix: String },
    /// Not configured (no token, no remote on a known host).
    NotConfigured(String),
    Unsupported(ForgeKind),
    /// `reset_ms` is when it lifts, 0 when the host did not say.
    RateLimited { host: String, reset_ms: u64 },
    NotFound(String),
    Offline(String),
    Parse(String),
    Failed(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::NotInstalled { program, fix } => write!(f, "{program} isn’t installed. {fix}"),
            ForgeError::SignedOut { program, fix } => {
                write!(f, "{program} isn’t signed in to this host. Run `{fix}` in a terminal, then try again.")
            }
            ForgeError::TrackerAuth { fix, .. } => f.write_str(fix),
            ForgeError::NotConfigured(why) => f.write_str(why),
            ForgeError::Unsupported(kind) => {
                write!(f, "Orbit can’t read {} {}s yet.", kind.label(), kind.review_noun())
            }
            ForgeError::RateLimited { host, reset_ms } => {
                write!(f, "{host} rate limit reached")?;
                match wait_text(*reset_ms) {
                    Some(wait) => write!(f, "; it resets {wait}."),
                    None => f.write_str("; try again in a minute."),
                }
            }
            ForgeError::NotFound(what) => f.write_str(what),
            ForgeError::Offline(why) => f.write_str(why),
            ForgeError::Parse(why) => write!(f, "Couldn’t read the answer: {why}"),
            ForgeError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ForgeError {}

/// `in 12 min`, from a reset time.
fn wait_text(reset_ms: u64) -> Option<String> {
    let now = now_ms();
    (reset_ms > now).then(|| {
        let minutes = (reset_ms - now).div_ceil(60_000);
        if minutes <= 1 { "in a minute".into() } else { format!("in {minutes} min") }
    })
}

/// A merge to perform.
#[derive(Clone, Debug, Default)]
pub struct MergeRequest {
    pub number: u64,
    pub method: MergeMethod,
    pub auto: bool,
    pub delete_branch: bool,
    /// Merge only if the head is still this commit; empty skips the check.
    pub head_sha: String,
    /// The source branch, for deleting it after the merge.
    pub head: String,
    pub cross_repo: bool,
}

/// A git host: pull requests, checks and merges for one repository.
pub trait ForgeHost: Send + Sync {
    fn kind(&self) -> ForgeKind;
    fn auth(&self) -> BoxFuture<'_, ForgeAuth>;
    fn pull_requests(&self, filter: PrFilter) -> BoxFuture<'_, Vec<PullRequest>>;
    /// The pull request whose source is `branch` in this repository: an
    /// open one if any, else the most recent.
    fn pull_request_for_branch(&self, branch: &str) -> BoxFuture<'_, Option<PullRequest>>;
    fn pull_request(&self, number: u64) -> BoxFuture<'_, PullRequestDetail>;
    fn merge(&self, request: MergeRequest) -> BoxFuture<'_, ()>;
    fn disable_auto_merge(&self, number: u64) -> BoxFuture<'_, ()>;
    /// The failing part of a check's log, cleaned for reading; empty when
    /// the check has no log Orbit can read.
    fn failed_log(&self, check: &Check) -> BoxFuture<'_, String>;
    /// The ref that holds a pull request's head commit (fetchable from the
    /// base repository, forks included).
    fn pr_ref(&self, number: u64) -> String;
}

/// Somewhere issues live.
pub trait IssueTracker: Send + Sync {
    fn kind(&self) -> TrackerKind;
    /// Assigned to the user (trackers) or open (repositories) when `query`
    /// is empty; else matching it.
    fn issues(&self, query: &str) -> BoxFuture<'_, Vec<Issue>>;
    fn issue(&self, key: &str) -> BoxFuture<'_, IssueDetail>;
}

/// Programs and credentials, read from the settings for each request.
#[derive(Clone, Default)]
pub struct ForgeConfig {
    pub gh: Option<PathBuf>,
    pub glab: Option<PathBuf>,
    pub linear_api_key: String,
    /// Tests point Linear at a local server.
    pub linear_endpoint: Option<String>,
    pub jira_url: String,
    pub jira_email: String,
    pub jira_api_token: String,
}

impl fmt::Debug for ForgeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgeConfig").field("gh", &self.gh).field("glab", &self.glab).finish_non_exhaustive()
    }
}

impl ForgeConfig {
    pub fn new(settings: &Settings, i: &IntegrationSettings) -> Self {
        let path = |s: &str| (!s.is_empty()).then(|| PathBuf::from(s));
        Self {
            gh: path(&settings.advanced.gh_binary),
            glab: path(&i.glab_binary),
            linear_api_key: i.linear_api_key.clone(),
            linear_endpoint: None,
            jira_url: i.jira_url.clone(),
            jira_email: i.jira_email.clone(),
            jira_api_token: i.jira_api_token.clone(),
        }
    }

    fn program(&self, kind: ForgeKind) -> Result<PathBuf, ForgeError> {
        let (name, explicit) = match kind {
            ForgeKind::GitHub => ("gh", self.gh.as_deref()),
            ForgeKind::GitLab => ("glab", self.glab.as_deref()),
            other => return Err(ForgeError::Unsupported(other)),
        };
        crate::provider::process::resolve_binary(name, explicit, &cli_dirs(name)).ok_or_else(|| ForgeError::NotInstalled {
            program: name.into(),
            fix: match explicit {
                Some(path) => format!("Nothing runs at {}; check the path in Settings.", path.display()),
                None => cli::install_hint(name),
            },
        })
    }
}

/// Install locations of the host CLIs that are often missing from PATH
/// (WinGet links, Homebrew, scoop), T3 Code #10942.
fn cli_dirs(name: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
        }
        if let Some(pf) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
            dirs.push(pf.join(if name == "gh" { "GitHub CLI" } else { "glab" }));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join("scoop").join("shims"));
        }
    } else {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
    }
    dirs
}

/// A project's repository on its host.
#[derive(Clone, Debug)]
pub struct Located {
    /// The working tree's top.
    pub root: PathBuf,
    pub repo: ForgeRepo,
}

/// A cached value and how fresh it is.
#[derive(Clone, Debug)]
pub struct Fresh<T> {
    pub value: T,
    pub fetched_ms: u64,
    /// Why this is older than asked for (a rate limit, offline).
    pub stale: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Prs(PathBuf, PrFilter),
    Pr(PathBuf, u64),
    BranchPr(PathBuf, String),
    Auth(PathBuf),
    /// Tracker, scope (repository root, or empty for account trackers), query.
    Issues(TrackerKind, String, String),
    Issue(TrackerKind, String, String),
}

impl Key {
    fn path(&self) -> Option<&Path> {
        match self {
            Key::Prs(p, _) | Key::Pr(p, _) | Key::BranchPr(p, _) | Key::Auth(p) => Some(p),
            Key::Issues(_, scope, _) | Key::Issue(_, scope, _) if !scope.is_empty() => Some(Path::new(scope)),
            _ => None,
        }
    }
}

#[derive(Clone)]
enum Cached {
    Prs(Vec<PullRequest>),
    Pr(Box<PullRequestDetail>),
    BranchPr(Option<PullRequest>),
    Auth(ForgeAuth),
    Issues(Vec<Issue>),
    Issue(Box<IssueDetail>),
}

struct Entry {
    value: Cached,
    at: Instant,
    fetched_ms: u64,
}

struct Limit {
    until_ms: u64,
    strikes: u32,
}

#[derive(Default)]
struct State {
    cache: HashMap<Key, Entry>,
    gates: HashMap<Key, Arc<tokio::sync::Mutex<()>>>,
    limits: HashMap<String, Limit>,
    located: HashMap<PathBuf, (Result<Located, ForgeError>, Instant)>,
}

/// The forge service: caches, request coalescing and rate-limit state.
/// Cheap to clone; clones share everything.
#[derive(Clone)]
pub struct Forge {
    permits: Arc<Semaphore>,
    state: Arc<Mutex<State>>,
}

impl Default for Forge {
    fn default() -> Self {
        Self { permits: Arc::new(Semaphore::new(MAX_PROCESSES)), state: Arc::default() }
    }
}

impl Forge {
    // ── Location and adapters ────────────────────────────────────────────

    /// The hosted repository behind `path`'s remote.
    pub async fn locate(&self, git: &Git, path: &Path) -> Result<Located, ForgeError> {
        let key = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
        if let Some((found, at)) = self.state.lock().unwrap().located.get(&key)
            && at.elapsed() < LOCATE_TTL
        {
            return found.clone();
        }
        let found = locate_uncached(git, &key).await;
        self.state.lock().unwrap().located.insert(key, (found.clone(), Instant::now()));
        found
    }

    fn host(&self, config: &ForgeConfig, at: &Located) -> Result<Arc<dyn ForgeHost>, ForgeError> {
        let program = config.program(at.repo.kind)?;
        let (repo, root, permits) = (at.repo.clone(), at.root.clone(), self.permits.clone());
        Ok(match at.repo.kind {
            ForgeKind::GitHub => Arc::new(github::GitHub { gh: program, repo, root, permits }),
            ForgeKind::GitLab => Arc::new(gitlab::GitLab { glab: program, repo, root, permits }),
            // Bitbucket, Azure DevOps and Gitea plug in here.
            other => return Err(ForgeError::Unsupported(other)),
        })
    }

    fn tracker(&self, config: &ForgeConfig, kind: TrackerKind, at: Option<&Located>) -> Result<Arc<dyn IssueTracker>, ForgeError> {
        let need_repo = |want: ForgeKind| -> Result<&Located, ForgeError> {
            match at {
                Some(at) if at.repo.kind == want => Ok(at),
                Some(at) => Err(ForgeError::NotConfigured(format!(
                    "This project is on {}, not {}.",
                    at.repo.kind.label(),
                    want.label()
                ))),
                None => Err(ForgeError::NotConfigured(format!("Pick a project on {} to see its issues.", want.label()))),
            }
        };
        match kind {
            TrackerKind::GitHub => {
                let at = need_repo(ForgeKind::GitHub)?;
                let gh = config.program(ForgeKind::GitHub)?;
                Ok(Arc::new(github::GitHub { gh, repo: at.repo.clone(), root: at.root.clone(), permits: self.permits.clone() }))
            }
            TrackerKind::GitLab => {
                let at = need_repo(ForgeKind::GitLab)?;
                let glab = config.program(ForgeKind::GitLab)?;
                Ok(Arc::new(gitlab::GitLab { glab, repo: at.repo.clone(), root: at.root.clone(), permits: self.permits.clone() }))
            }
            TrackerKind::Linear => {
                if config.linear_api_key.is_empty() {
                    return Err(ForgeError::NotConfigured(
                        "Add a Linear API key in Settings › Integrations to see your Linear issues.".into(),
                    ));
                }
                let mut linear = linear::Linear::new(config.linear_api_key.clone());
                if let Some(endpoint) = &config.linear_endpoint {
                    linear.endpoint = endpoint.clone();
                }
                Ok(Arc::new(linear))
            }
            TrackerKind::Jira => {
                if config.jira_url.is_empty() || config.jira_api_token.is_empty() {
                    return Err(ForgeError::NotConfigured(
                        "Add your Jira site and API token in Settings › Integrations to see your Jira issues.".into(),
                    ));
                }
                Ok(Arc::new(jira::Jira {
                    site: config.jira_url.trim_end_matches('/').to_owned(),
                    email: config.jira_email.clone(),
                    token: config.jira_api_token.clone(),
                }))
            }
        }
    }

    // ── Reads ────────────────────────────────────────────────────────────

    /// The host, repository and sign-in state of a project.
    pub async fn info(&self, config: &ForgeConfig, git: &Git, path: &Path) -> ForgeInfo {
        let branch = git.current_branch(path).await.ok().flatten();
        let at = match self.locate(git, path).await {
            Ok(at) => at,
            Err(_) => return ForgeInfo { repo: None, auth: ForgeAuth::Unsupported, branch },
        };
        let auth = match self.host(config, &at) {
            Err(ForgeError::NotInstalled { program, fix }) => ForgeAuth::NotInstalled { program, fix },
            Err(_) => ForgeAuth::Unsupported,
            Ok(host) => {
                let key = Key::Auth(at.root.clone());
                let fetch = async { host.auth().await.map(Cached::Auth) };
                match self.cached(key, AUTH_TTL, false, "", fetch).await {
                    Ok(Fresh { value: Cached::Auth(auth), .. }) => auth,
                    Ok(_) => ForgeAuth::Unsupported,
                    Err(ForgeError::NotInstalled { program, fix }) => ForgeAuth::NotInstalled { program, fix },
                    Err(ForgeError::SignedOut { program, fix }) => ForgeAuth::SignedOut { program, fix },
                    Err(_) => ForgeAuth::Ready { account: String::new() },
                }
            }
        };
        ForgeInfo { repo: Some(at.repo), auth, branch }
    }

    pub async fn pull_requests(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: &Path,
        filter: PrFilter,
        refresh: bool,
    ) -> Result<PullRequestList, ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let key = Key::Prs(at.root.clone(), filter);
        // A list with running checks goes stale sooner.
        let ttl = match self.peek(&key) {
            Some(Cached::Prs(prs)) if prs.iter().any(|p| p.checks.pending > 0) => PENDING_TTL,
            _ => LIST_TTL,
        };
        let fetch = async { host.pull_requests(filter).await.map(Cached::Prs) };
        let fresh = self.cached(key, ttl, refresh, &at.repo.host, fetch).await?;
        let Cached::Prs(items) = fresh.value else { unreachable!("key and value kinds match") };
        Ok(PullRequestList { items, fetched_ms: fresh.fetched_ms, stale: fresh.stale })
    }

    pub async fn pull_request(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: &Path,
        number: u64,
        refresh: bool,
    ) -> Result<Fresh<PullRequestDetail>, ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let key = Key::Pr(at.root.clone(), number);
        let ttl = match self.peek(&key) {
            Some(Cached::Pr(d)) if d.pr.state.is_open() && (d.pr.checks.pending > 0 || d.mergeable == Mergeability::Unknown) => {
                PENDING_TTL
            }
            _ => DETAIL_TTL,
        };
        let fetch = async { host.pull_request(number).await.map(|d| Cached::Pr(Box::new(d))) };
        let fresh = self.cached(key, ttl, refresh, &at.repo.host, fetch).await?;
        let Cached::Pr(detail) = fresh.value else { unreachable!("key and value kinds match") };
        Ok(Fresh { value: *detail, fetched_ms: fresh.fetched_ms, stale: fresh.stale })
    }

    /// The pull request for a branch, for the ship state.
    pub async fn branch_pull_request(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: &Path,
        branch: &str,
        refresh: bool,
    ) -> Result<Option<PullRequest>, ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let key = Key::BranchPr(at.root.clone(), branch.to_owned());
        let fetch = async { host.pull_request_for_branch(branch).await.map(Cached::BranchPr) };
        let fresh = self.cached(key, LIST_TTL, refresh, &at.repo.host, fetch).await?;
        let Cached::BranchPr(pr) = fresh.value else { unreachable!("key and value kinds match") };
        Ok(pr)
    }

    pub async fn issues(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: Option<&Path>,
        kind: TrackerKind,
        query: &str,
        refresh: bool,
    ) -> Result<IssueList, ForgeError> {
        let (tracker, scope, host) = self.tracker_for(config, git, path, kind).await?;
        let key = Key::Issues(kind, scope, query.trim().to_owned());
        let fetch = async { tracker.issues(query).await.map(Cached::Issues) };
        let fresh = self.cached(key, LIST_TTL, refresh, &host, fetch).await?;
        let Cached::Issues(items) = fresh.value else { unreachable!("key and value kinds match") };
        Ok(IssueList { items, fetched_ms: fresh.fetched_ms, stale: fresh.stale })
    }

    pub async fn issue(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: Option<&Path>,
        kind: TrackerKind,
        key: &str,
        refresh: bool,
    ) -> Result<IssueDetail, ForgeError> {
        let (tracker, scope, host) = self.tracker_for(config, git, path, kind).await?;
        let cache_key = Key::Issue(kind, scope, key.trim().to_owned());
        let fetch = async { tracker.issue(key).await.map(|d| Cached::Issue(Box::new(d))) };
        let fresh = self.cached(cache_key, DETAIL_TTL, refresh, &host, fetch).await?;
        let Cached::Issue(detail) = fresh.value else { unreachable!("key and value kinds match") };
        Ok(*detail)
    }

    async fn tracker_for(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: Option<&Path>,
        kind: TrackerKind,
    ) -> Result<(Arc<dyn IssueTracker>, String, String), ForgeError> {
        let at = match path {
            Some(path) if kind.is_forge() => Some(self.locate(git, path).await?),
            _ => None,
        };
        let tracker = self.tracker(config, kind, at.as_ref())?;
        let scope = at.as_ref().map(|a| a.root.display().to_string()).unwrap_or_default();
        let host = match kind {
            TrackerKind::Linear => "api.linear.app".to_owned(),
            TrackerKind::Jira => config.jira_url.clone(),
            _ => at.map(|a| a.repo.host).unwrap_or_default(),
        };
        Ok((tracker, scope, host))
    }

    /// The failing checks' cleaned log tails, at most `max` of them.
    pub async fn failing_logs(
        &self,
        config: &ForgeConfig,
        git: &Git,
        path: &Path,
        checks: &[Check],
        max: usize,
    ) -> Result<Vec<(Check, String)>, ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let failing: Vec<Check> = checks.iter().filter(|c| c.state == CheckState::Failed).take(max).cloned().collect();
        let logs = failing.iter().map(|c| host.failed_log(c));
        let logs: Vec<Result<String, ForgeError>> = join_all(logs).await;
        Ok(failing.into_iter().zip(logs).map(|(c, log)| (c, log.unwrap_or_default())).collect())
    }

    /// The fetchable ref of a pull request's head on its host.
    pub fn pr_ref(&self, config: &ForgeConfig, at: &Located, number: u64) -> Result<String, ForgeError> {
        Ok(self.host(config, at)?.pr_ref(number))
    }

    // ── Writes ───────────────────────────────────────────────────────────

    pub async fn merge(&self, config: &ForgeConfig, git: &Git, path: &Path, request: MergeRequest) -> Result<(), ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let result = host.merge(request).await;
        self.invalidate(&at.root);
        result
    }

    pub async fn disable_auto_merge(&self, config: &ForgeConfig, git: &Git, path: &Path, number: u64) -> Result<(), ForgeError> {
        let at = self.locate(git, path).await?;
        let host = self.host(config, &at)?;
        let result = host.disable_auto_merge(number).await;
        self.invalidate(&at.root);
        result
    }

    /// Drops everything cached for a repository (after a push or merge).
    pub fn invalidate(&self, root: &Path) {
        let mut state = self.state.lock().unwrap();
        state.cache.retain(|key, _| matches!(key, Key::Auth(_)) || key.path() != Some(root));
    }

    /// Drops Linear and Jira results (their credentials changed).
    pub fn forget_trackers(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.retain(|key, _| !matches!(key, Key::Issues(k, ..) | Key::Issue(k, ..) if !k.is_forge()));
        state.limits.retain(|host, _| host != "api.linear.app" && !host.starts_with("https://"));
    }

    /// Drops everything cached for the repository containing `path`.
    pub fn invalidate_path(&self, path: &Path) {
        let located: Vec<PathBuf> = {
            let state = self.state.lock().unwrap();
            state
                .located
                .iter()
                .filter(|(p, _)| path.starts_with(p) || p.starts_with(path))
                .filter_map(|(_, (found, _))| found.as_ref().ok().map(|l| l.root.clone()))
                .collect()
        };
        for root in located {
            self.invalidate(&root);
        }
        self.invalidate(path);
    }

    // ── Cache ────────────────────────────────────────────────────────────

    fn peek(&self, key: &Key) -> Option<Cached> {
        self.state.lock().unwrap().cache.get(key).map(|e| e.value.clone())
    }

    /// Serves `key` from the cache when fresh enough, else runs `fetch`
    /// (one at a time per key). While `host` is rate limited, nothing is
    /// fetched and the last value is served with a note.
    async fn cached(
        &self,
        key: Key,
        ttl: Duration,
        refresh: bool,
        host: &str,
        fetch: impl Future<Output = Result<Cached, ForgeError>>,
    ) -> Result<Fresh<Cached>, ForgeError> {
        let gate = self.state.lock().unwrap().gates.entry(key.clone()).or_default().clone();
        let _turn = gate.lock().await;
        let max_age = if refresh { MIN_REFRESH } else { ttl };
        let existing = {
            let state = self.state.lock().unwrap();
            state.cache.get(&key).map(|e| (e.value.clone(), e.at, e.fetched_ms))
        };
        if let Some((value, at, fetched_ms)) = &existing
            && at.elapsed() < max_age
        {
            return Ok(Fresh { value: value.clone(), fetched_ms: *fetched_ms, stale: None });
        }
        if let Some(until) = self.limited_until(host) {
            let error = ForgeError::RateLimited { host: host.to_owned(), reset_ms: until };
            return match existing {
                Some((value, _, fetched_ms)) => Ok(Fresh { value, fetched_ms, stale: Some(stale_note(&error, fetched_ms)) }),
                None => Err(error),
            };
        }
        match fetch.await {
            Ok(value) => {
                let fetched_ms = now_ms();
                let mut state = self.state.lock().unwrap();
                if !host.is_empty() {
                    state.limits.remove(host);
                }
                state.cache.insert(key, Entry { value: value.clone(), at: Instant::now(), fetched_ms });
                if state.cache.len() > CACHE_ENTRIES {
                    let oldest = state.cache.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone());
                    if let Some(oldest) = oldest {
                        state.cache.remove(&oldest);
                        state.gates.remove(&oldest);
                    }
                }
                Ok(Fresh { value, fetched_ms, stale: None })
            }
            Err(error) => {
                let error = match error {
                    ForgeError::RateLimited { host: limited, reset_ms } => {
                        let until = self.note_limit(if host.is_empty() { &limited } else { host }, reset_ms);
                        ForgeError::RateLimited { host: limited, reset_ms: until }
                    }
                    other => other,
                };
                // Transient failures keep showing what was there.
                let transient = matches!(error, ForgeError::RateLimited { .. } | ForgeError::Offline(_));
                match existing {
                    Some((value, _, fetched_ms)) if transient => {
                        Ok(Fresh { value, fetched_ms, stale: Some(stale_note(&error, fetched_ms)) })
                    }
                    _ => Err(error),
                }
            }
        }
    }

    fn limited_until(&self, host: &str) -> Option<u64> {
        let state = self.state.lock().unwrap();
        state.limits.get(host).map(|l| l.until_ms).filter(|&until| until > now_ms())
    }

    /// Records a rate limit and returns when Orbit will ask again.
    fn note_limit(&self, host: &str, reset_ms: u64) -> u64 {
        let mut state = self.state.lock().unwrap();
        let limit = state.limits.entry(host.to_owned()).or_insert(Limit { until_ms: 0, strikes: 0 });
        limit.strikes += 1;
        let backoff = LIMIT_BACKOFF.saturating_mul(1 << (limit.strikes - 1).min(4)).min(LIMIT_MAX);
        limit.until_ms = if reset_ms > now_ms() { reset_ms } else { now_ms() + backoff.as_millis() as u64 };
        limit.until_ms
    }
}

fn stale_note(error: &ForgeError, fetched_ms: u64) -> String {
    let age = now_ms().saturating_sub(fetched_ms) / 60_000;
    let when = match age {
        0 => "just now".to_owned(),
        1 => "a minute ago".to_owned(),
        n => format!("{n} minutes ago"),
    };
    format!("Showing what was loaded {when}. {error}")
}

async fn locate_uncached(git: &Git, path: &Path) -> Result<Located, ForgeError> {
    let info = git.repo(path).await.map_err(|e| ForgeError::NotConfigured(e.to_string()))?;
    // Raw config values: `remote get-url` would expand `insteadOf` rewrites.
    let out = git
        .git(&info.root, ["config", "--get-regexp", r"^remote\..*\.url$"])
        .await
        .map_err(|_| ForgeError::NotConfigured("This repository has no remote, so it has no pull requests.".into()))?;
    let remotes = remote::parse_remote_config(&String::from_utf8_lossy(&out));
    let repo = remote::pick_remote(&remotes).ok_or_else(|| {
        ForgeError::NotConfigured(match remotes.first() {
            Some((_, url)) => format!("Orbit can’t tell which service hosts {url}."),
            None => "This repository has no remote, so it has no pull requests.".into(),
        })
    })?;
    Ok(Located { root: info.root.clone(), repo })
}

/// Runs futures concurrently on the current task and returns their results
/// in input order.
async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    use std::task::Poll;
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut results: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (future, result) in futures.iter_mut().zip(results.iter_mut()) {
            if result.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => *result = Some(value),
                    Poll::Pending => done = false,
                }
            }
        }
        if done { Poll::Ready(()) } else { Poll::Pending }
    })
    .await;
    results.into_iter().map(|r| r.expect("every future finished")).collect()
}

#[cfg(test)]
mod tests;
