//! Debounced file-system change notifications. This replaces polling: the
//! engine recomputes status and diffs when, and only when, this says
//! something changed.
//!
//! * Events are coalesced: a burst (a build, a branch switch, an agent
//!   writing 300 files) becomes one notification once the tree has been
//!   quiet for [`WatchOptions::debounce`] (~150 ms), or at most every
//!   [`WatchOptions::max_delay`] while writes continue.
//! * `.git/` internals are ignored except `HEAD`, `refs/` and `packed-refs`
//!   (branch switches, commits, fetches), and Orbit's own `refs/orbit/`
//!   writes never wake anyone, so a checkpoint cannot trigger a refresh loop.
//! * Gitignored directories (`target/`, `node_modules/`) can be ignored too
//!   ([`Git::watch`] fills them in), so a build does not cause refreshes.
//! * A slow consumer never blocks the watcher: pending changes keep merging
//!   until the channel has room.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossfire::{MAsyncRx, MTx, mpmc};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::cmd::nul_fields;
use super::{Git, GitError, Result};

#[derive(Clone, Debug)]
pub struct WatchOptions {
    /// Quiet time that ends a burst.
    pub debounce: Duration,
    /// Longest a continuous burst is held back.
    pub max_delay: Duration,
    /// Paths reported per notification before it is marked `overflow`.
    pub max_paths: usize,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions { debounce: Duration::from_millis(150), max_delay: Duration::from_secs(1), max_paths: 2048 }
    }
}

/// One coalesced notification.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Changed paths relative to the watched root, deduplicated and sorted.
    pub paths: Vec<PathBuf>,
    /// More paths changed than `max_paths`; treat everything as changed.
    pub overflow: bool,
    /// HEAD or a ref moved (commit, checkout, fetch, rebase).
    pub git_refs: bool,
}

impl Changes {
    fn merge(&mut self, other: Changes, max: usize) {
        self.git_refs |= other.git_refs;
        self.overflow |= other.overflow;
        let mut set: HashSet<PathBuf> = self.paths.drain(..).collect();
        set.extend(other.paths);
        if set.len() > max {
            self.overflow = true;
        }
        self.paths = set.into_iter().take(max).collect();
        self.paths.sort();
    }
}

/// A live watch. Dropping it stops the watcher and its debounce thread.
pub struct RepoWatcher {
    rx: MAsyncRx<mpmc::Array<Changes>>,
    root: PathBuf,
    ignored: Arc<Mutex<Vec<PathBuf>>>,
    _watcher: RecommendedWatcher,
}

impl RepoWatcher {
    /// The next notification; `None` once the watcher has stopped.
    pub async fn recv(&self) -> Option<Changes> {
        self.rx.recv().await.ok()
    }

    pub fn try_recv(&self) -> Option<Changes> {
        self.rx.try_recv().ok()
    }

    /// The underlying crossfire receiver, for `select!`-style use. It is
    /// `Sync`, so a task can await it and still be sent across threads.
    pub fn receiver(&self) -> &MAsyncRx<mpmc::Array<Changes>> {
        &self.rx
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Replaces the ignored path prefixes (relative to the root).
    pub fn set_ignored(&self, prefixes: Vec<PathBuf>) {
        *self.ignored.lock().unwrap() = prefixes;
    }
}

/// Watches `path` recursively with default options.
pub fn watch(path: &Path) -> Result<RepoWatcher> {
    watch_with(path, WatchOptions::default())
}

pub fn watch_with(path: &Path, opts: WatchOptions) -> Result<RepoWatcher> {
    let root = std::path::absolute(path)?;
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(raw_tx).map_err(notify_err)?;
    watcher.watch(&root, RecursiveMode::Recursive).map_err(notify_err)?;

    // A linked worktree keeps HEAD in the main repository's git dir.
    let mut extra_git_dir = None;
    if let Ok(dotgit) = std::fs::read_to_string(root.join(".git"))
        && let Some(dir) = dotgit.trim().strip_prefix("gitdir:") {
            let dir = root.join(dir.trim());
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                extra_git_dir = Some(dir);
            }
        }

    let (tx, rx) = mpmc::bounded_blocking_async::<Changes>(4);
    let ignored = Arc::new(Mutex::new(Vec::new()));
    let filter = Filter { root: root.clone(), extra_git_dir, ignored: ignored.clone() };
    std::thread::Builder::new()
        .name("orbit-watch".into())
        .spawn(move || debounce_loop(raw_rx, tx, filter, opts))?;
    Ok(RepoWatcher { rx, root, ignored, _watcher: watcher })
}

fn notify_err(e: notify::Error) -> GitError {
    GitError::Invalid(format!("file watcher: {e}"))
}

struct Filter {
    root: PathBuf,
    extra_git_dir: Option<PathBuf>,
    ignored: Arc<Mutex<Vec<PathBuf>>>,
}

enum Verdict {
    Ignore,
    Path(PathBuf),
    GitRefs,
}

impl Filter {
    fn classify(&self, path: &Path) -> Verdict {
        if let Some(dir) = &self.extra_git_dir
            && let Ok(rel) = path.strip_prefix(dir) {
                // A commit in a linked worktree moves a ref in the main
                // repository's git dir, which is not watched; it does
                // rewrite this worktree's index, so that counts too.
                return if rel == Path::new("HEAD") || rel == Path::new("index") { Verdict::GitRefs } else { Verdict::Ignore };
            }
        let Ok(rel) = path.strip_prefix(&self.root) else { return Verdict::Ignore };
        let mut comps = rel.components();
        match comps.next() {
            None => return Verdict::Ignore,
            Some(c) if c.as_os_str() == ".git" => {
                let rest = comps.as_path();
                let is_ref = rest == Path::new("HEAD")
                    || rest == Path::new("packed-refs")
                    || (rest.starts_with("refs") && !rest.starts_with("refs/orbit"));
                let lock = rest.extension().is_some_and(|e| e == "lock");
                return if is_ref && !lock { Verdict::GitRefs } else { Verdict::Ignore };
            }
            _ => {}
        }
        let ignored = self.ignored.lock().unwrap();
        if ignored.iter().any(|p| rel.starts_with(p)) {
            return Verdict::Ignore;
        }
        Verdict::Path(rel.to_owned())
    }
}

fn debounce_loop(
    raw: mpsc::Receiver<notify::Result<notify::Event>>,
    tx: MTx<mpmc::Array<Changes>>,
    filter: Filter,
    opts: WatchOptions,
) {
    let mut pending: Option<(Changes, HashSet<PathBuf>)> = None;
    let mut burst_start = Instant::now();
    let mut last_event = Instant::now();
    // Set while a ready notification waits for room in the channel.
    let mut blocked: Option<Changes> = None;
    loop {
        let wait = if pending.is_some() || blocked.is_some() {
            let due = (last_event + opts.debounce).min(burst_start + opts.max_delay);
            Some(due.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)))
        } else {
            None
        };
        let event = match wait {
            None => raw.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(d) => raw.recv_timeout(d),
        };
        let disconnected = matches!(event, Err(RecvTimeoutError::Disconnected));
        if let Ok(Err(e)) = &event {
            // The backend lost events (e.g. its buffer overflowed): ask the
            // consumer for a full rescan.
            log::debug!("watcher error: {e}");
            let (changes, _) = pending.get_or_insert_with(|| {
                burst_start = Instant::now();
                (Changes::default(), HashSet::new())
            });
            changes.overflow = true;
            last_event = Instant::now();
        }
        if let Ok(Ok(event)) = event
            && !matches!(event.kind, EventKind::Access(_)) {
                // A directory's own "modified" event only echoes changes to
                // its entries, which arrive as events of their own (Windows
                // also sends these lazily, long after the burst).
                let content_modify = matches!(
                    event.kind,
                    EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Other)
                );
                for path in &event.paths {
                    let verdict = filter.classify(path);
                    let dir_echo = content_modify && matches!(verdict, Verdict::Path(_)) && path.is_dir();
                    if matches!(verdict, Verdict::Ignore) || dir_echo {
                        continue;
                    }
                    let (changes, set) = pending.get_or_insert_with(|| {
                        burst_start = Instant::now();
                        (Changes::default(), HashSet::new())
                    });
                    match verdict {
                        Verdict::GitRefs => changes.git_refs = true,
                        Verdict::Path(p) => {
                            if set.len() < opts.max_paths {
                                set.insert(p);
                            } else if !set.contains(&p) {
                                changes.overflow = true;
                            }
                        }
                        Verdict::Ignore => {}
                    }
                    last_event = Instant::now();
                }
            }

        let now = Instant::now();
        let quiet = now >= last_event + opts.debounce;
        let overdue = now >= burst_start + opts.max_delay;
        if pending.is_some() && (quiet || overdue || disconnected) {
            let (mut changes, set) = pending.take().expect("checked");
            changes.paths = set.into_iter().collect();
            changes.paths.sort();
            match blocked.take() {
                Some(mut older) => {
                    older.merge(changes, opts.max_paths);
                    blocked = Some(older);
                }
                None => blocked = Some(changes),
            }
        }
        if let Some(changes) = blocked.take() {
            match tx.try_send(changes) {
                Ok(()) => {}
                Err(crossfire::TrySendError::Full(changes)) => {
                    blocked = Some(changes);
                    burst_start = Instant::now();
                    last_event = Instant::now();
                }
                Err(crossfire::TrySendError::Disconnected(_)) => return,
            }
        }
        if disconnected {
            return;
        }
    }
}

impl Git {
    /// [`watch`] that also ignores the repository's gitignored directories
    /// (one `ls-files` up front). Call [`Git::refresh_watch_ignores`] when a
    /// `.gitignore` changes.
    pub async fn watch(&self, repo: &Path) -> Result<RepoWatcher> {
        let info = self.repo(repo).await?;
        let watcher = watch(&info.root)?;
        self.refresh_watch_ignores(&watcher).await?;
        Ok(watcher)
    }

    pub async fn refresh_watch_ignores(&self, watcher: &RepoWatcher) -> Result<()> {
        let out = self
            .git(watcher.root(), ["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"])
            .await?;
        let prefixes: Vec<PathBuf> =
            nul_fields(&out).take(10_000).map(|p| PathBuf::from(p.trim_end_matches('/'))).collect();
        watcher.set_ignored(prefixes);
        Ok(())
    }
}
