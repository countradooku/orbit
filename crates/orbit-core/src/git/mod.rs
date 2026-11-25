//! Git without process storms.
//!
//! T3 Code spawned ~1,650 git processes to show 272 files and 15–17 per
//! thread on every 30 s poll. This layer is built so that cannot happen:
//!
//! * **One process per question.** A diff of any number of files is one
//!   `git diff`, parsed in Rust ([`orbit_proto::diff`]); additions and
//!   deletions come from the parsed hunks (identical to `--numstat`, which is
//!   tested), so no second pass is needed. Status is one `status
//!   --porcelain=v2` plus one `--numstat`.
//! * **Bounded concurrency.** Every git and gh process goes through one
//!   semaphore ([`GitConfig::max_processes`]), so no burst of UI requests can
//!   fork more than a handful at once.
//! * **Content-addressed caching.** Diffs are cached by `(from, to)` tree id,
//!   and concurrent requests for the same pair share one process. A tree id
//!   never changes meaning, so the cache is never stale.
//! * **No polling.** [`watch`] delivers debounced file-system changes; the
//!   engine refreshes on those and nothing else.
//! * **No console windows.** Every process is spawned with
//!   `CREATE_NO_WINDOW` on Windows.
//!
//! Checkpoints snapshot the working tree (untracked files included, ignored
//! files excluded) through a persistent per-thread index under
//! `<git-dir>/orbit/`, so a snapshot rehashes only what changed and never
//! touches the user's index or HEAD. They are recorded under
//! `refs/orbit/checkpoints/<thread>/<n>` (GC-safe) and are pruned per thread.

mod checkpoint;
mod cmd;
mod conflicts;
mod diff;
mod remote;
mod status;
mod watch;
mod worktree;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use orbit_proto::ThreadId;
use tokio::sync::{OnceCell, Semaphore};

pub use checkpoint::{Checkpoint, Restored};
pub use conflicts::{Conflict, LiveWorktree};
pub use diff::DiffResult;
pub use remote::{CommitResult, PushResult, parse_pr_url};
pub use status::{Branch, Status};
pub use watch::{Changes, RepoWatcher, WatchOptions, watch, watch_with};
pub use worktree::{
    BranchCleanup, NewWorktree, OutputStream, RemoveWorktree, RemovedWorktree, ScriptResult, WorktreeInfo,
    WorktreeRecord, disk_usage,
};

/// A git object id naming a tree (or any tree-ish: checkpoint trees, commit
/// ids, merge bases).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreeId(String);

impl TreeId {
    pub fn new(id: impl Into<String>) -> Self {
        TreeId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum GitError {
    /// A required binary (`git` or `gh`) is not installed.
    NotInstalled(&'static str),
    Io(std::io::Error),
    /// A command exited unsuccessfully.
    Failed { command: String, code: Option<i32>, stderr: String },
    NotARepo(PathBuf),
    NothingToCommit,
    /// `git apply --check` rejected a patch; nothing was changed.
    PatchDoesNotApply(String),
    NoCheckpoint { thread: ThreadId, n: u32 },
    DetachedHead,
    NoRemote,
    Invalid(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotInstalled("gh") => {
                f.write_str("GitHub CLI (gh) is not installed; install it from https://cli.github.com and run `gh auth login`")
            }
            GitError::NotInstalled(name) => write!(f, "{name} is not installed or not on PATH"),
            GitError::Io(e) => write!(f, "{e}"),
            GitError::Failed { command, code, stderr } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            GitError::NotARepo(p) => write!(f, "{} is not inside a git repository", p.display()),
            GitError::NothingToCommit => f.write_str("nothing to commit"),
            GitError::PatchDoesNotApply(why) => write!(f, "patch does not apply: {why}"),
            GitError::NoCheckpoint { thread, n } => write!(f, "checkpoint {n} of {thread} does not exist"),
            GitError::DetachedHead => f.write_str("HEAD is detached; check out a branch first"),
            GitError::NoRemote => f.write_str("the repository has no remote to push to"),
            GitError::Invalid(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(e: std::io::Error) -> Self {
        GitError::Io(e)
    }
}

pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Settings for the git layer.
#[derive(Clone, Debug)]
pub struct GitConfig {
    /// Explicit git binary; `None` resolves `git` from PATH.
    pub git: Option<PathBuf>,
    /// Explicit GitHub CLI binary; `None` resolves `gh` from PATH.
    pub gh: Option<PathBuf>,
    /// Orbit's data directory: holds the worktree registry and, unless
    /// `worktree_root` is set, the worktrees themselves.
    pub data_dir: PathBuf,
    /// Where new worktrees go: `<root>/<project>/<branch>`. Defaults to
    /// `<data_dir>/worktrees`.
    pub worktree_root: Option<PathBuf>,
    /// Upper bound on concurrently running git/gh processes.
    pub max_processes: usize,
    /// New untracked files larger than this are left out of checkpoints, so
    /// a stray dataset or build artifact cannot fill the disk turn after turn
    /// (T3 Code #38102). 0 disables the check.
    pub max_untracked_file: u64,
    /// Ports handed to worktrees: blocks of `port_block` between `port_first`
    /// and `port_last`.
    pub port_first: u16,
    pub port_last: u16,
    pub port_block: u16,
    /// Diff results kept in memory.
    pub diff_cache: usize,
}

impl GitConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        GitConfig {
            git: None,
            gh: None,
            data_dir: data_dir.into(),
            worktree_root: None,
            max_processes: 4,
            max_untracked_file: 50 * 1024 * 1024,
            port_first: 41_000,
            port_last: 60_999,
            port_block: 10,
            diff_cache: 64,
        }
    }

    pub fn worktree_root(&self) -> PathBuf {
        self.worktree_root.clone().unwrap_or_else(|| self.data_dir.join("worktrees"))
    }
}

/// Where a repository's git data lives.
#[derive(Clone, Debug)]
pub(crate) struct RepoInfo {
    /// Top of the working tree.
    pub root: PathBuf,
    /// This working tree's git directory (`.git`, or `.git/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// The shared git directory (objects and refs).
    #[allow(dead_code)]
    pub common_dir: PathBuf,
}

impl RepoInfo {
    /// Directory for Orbit's private files (checkpoint indexes).
    pub fn orbit_dir(&self) -> PathBuf {
        self.git_dir.join("orbit")
    }
}

/// The git layer. Cheap to clone; clones share the process limit and caches.
#[derive(Clone)]
pub struct Git {
    inner: Arc<Inner>,
}

/// Checkpoint numbers and trees, oldest first.
type CheckpointList = Vec<(u32, TreeId)>;

struct Inner {
    cfg: GitConfig,
    git: Option<PathBuf>,
    permits: Semaphore,
    spawned: AtomicU64,
    repos: Mutex<HashMap<PathBuf, Arc<RepoInfo>>>,
    index_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// Known checkpoints per (git dir, thread), sorted by number.
    checkpoints: Mutex<HashMap<(PathBuf, ThreadId), CheckpointList>>,
    diffs: Memo<(TreeId, TreeId), Arc<DiffResult>>,
    names: Memo<(TreeId, TreeId), Arc<Vec<String>>>,
    registry: tokio::sync::Mutex<()>,
}

impl Git {
    pub fn new(cfg: GitConfig) -> Self {
        let git = crate::provider::process::resolve_binary("git", cfg.git.as_deref(), &[]);
        let permits = Semaphore::new(cfg.max_processes.max(1));
        let cache = cfg.diff_cache.max(1);
        Git {
            inner: Arc::new(Inner {
                git,
                permits,
                spawned: AtomicU64::new(0),
                repos: Mutex::default(),
                index_locks: Mutex::default(),
                checkpoints: Mutex::default(),
                diffs: Memo::new(cache),
                names: Memo::new(cache * 4),
                registry: tokio::sync::Mutex::new(()),
                cfg,
            }),
        }
    }

    pub fn config(&self) -> &GitConfig {
        &self.inner.cfg
    }

    /// Total git/gh processes spawned so far (for diagnostics and tests).
    pub fn processes_spawned(&self) -> u64 {
        self.inner.spawned.load(Ordering::Relaxed)
    }

    /// Resolves (and caches) where `path`'s repository keeps its data.
    pub(crate) async fn repo(&self, path: &Path) -> Result<Arc<RepoInfo>> {
        let key = std::path::absolute(path)?;
        if let Some(info) = self.inner.repos.lock().unwrap().get(&key) {
            return Ok(info.clone());
        }
        let out = self
            .git_output(
                &key,
                ["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-dir", "--git-common-dir"],
                cmd::Opts::default(),
            )
            .await?;
        if !out.ok() {
            return Err(GitError::NotARepo(key));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines().map(|l| PathBuf::from(l.trim()));
        let (Some(root), Some(git_dir), Some(common_dir)) = (lines.next(), lines.next(), lines.next()) else {
            return Err(GitError::NotARepo(key));
        };
        let info = Arc::new(RepoInfo { root, git_dir, common_dir });
        self.inner.repos.lock().unwrap().insert(key, info.clone());
        Ok(info)
    }

    /// The async mutex serializing all writers of one index file.
    pub(crate) fn index_lock(&self, index: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.index_locks.lock().unwrap().entry(index.to_owned()).or_default().clone()
    }
}

/// Cells by key, plus keys from least to most recently used.
type MemoState<K, V> = (HashMap<K, Arc<OnceCell<V>>>, VecDeque<K>);

/// A small LRU map whose values are computed once even under concurrent
/// requests: callers asking for the same key share one computation (one git
/// process), and a failed computation is retried by the next caller.
pub(crate) struct Memo<K, V> {
    cap: usize,
    state: Mutex<MemoState<K, V>>,
}

impl<K: Eq + Hash + Clone, V: Clone> Memo<K, V> {
    fn new(cap: usize) -> Self {
        Memo { cap, state: Mutex::new((HashMap::new(), VecDeque::new())) }
    }

    pub(crate) async fn get_or_try_init<F, Fut>(&self, key: K, init: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V>>,
    {
        let cell = {
            let mut guard = self.state.lock().unwrap();
            let (map, order) = &mut *guard;
            let cell = map.entry(key.clone()).or_default().clone();
            if let Some(i) = order.iter().position(|k| k == &key) {
                order.remove(i);
            }
            order.push_back(key);
            while order.len() > self.cap {
                if let Some(old) = order.pop_front() {
                    map.remove(&old);
                }
            }
            cell
        };
        cell.get_or_try_init(init).await.cloned()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state.lock().unwrap().0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memo_shares_one_computation_and_evicts_old_keys() {
        let memo: Arc<Memo<u32, u32>> = Arc::new(Memo::new(2));
        let calls = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (memo, calls) = (memo.clone(), calls.clone());
            tasks.push(tokio::spawn(async move {
                memo.get_or_try_init(1, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Ok(10)
                })
                .await
                .unwrap()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), 10);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let failed = memo.get_or_try_init(2, || async { Err(GitError::Invalid("x".into())) }).await;
        assert!(failed.is_err());
        assert_eq!(memo.get_or_try_init(2, || async { Ok(20) }).await.unwrap(), 20, "failures are retried");
        memo.get_or_try_init(3, || async { Ok(30) }).await.unwrap();
        assert_eq!(memo.len(), 2);
    }
}
