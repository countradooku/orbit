//! Review and checkpoints: live workspace refresh, conflict warnings across
//! live worktrees, undoing a restore and merging locally.
//!
//! * **Live refresh without polling.** Each repository a client has a thread
//!   open in gets one file watcher ([`crate::git::watch`]), shared by every
//!   open thread working there. A debounced burst of changes costs one
//!   `git status` (two processes), whose result goes out as
//!   [`Event::WorkspaceChanged`] to each of those threads. Nothing runs for
//!   threads nobody is looking at, and closing the last one stops the
//!   watcher.
//! * **Conflicts across worktrees.** When a watched worktree changes, the
//!   files every live worktree changed since its base are compared (cached
//!   by tree id, so an unchanged worktree costs one snapshot) and
//!   [`Event::Conflicts`] is sent when the overlap changes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{Internal, Orchestrator, Outcome};
use crate::git::LiveWorktree;

/// How long conflict detection waits after a change, so a burst of edits
/// across several worktrees is compared once.
const CONFLICT_DELAY: Duration = Duration::from_millis(800);

/// Background results for the review engine.
pub enum ReviewInternal {
    /// A watched repository's working tree changed; its status now.
    Workspace { repo: PathBuf, files: Vec<FileChange> },
    /// Files changed by more than one live worktree.
    Conflicts(Vec<Conflict>),
}

/// The review engine's state on the orchestrator.
#[derive(Default)]
pub struct ReviewState {
    watchers: HashMap<PathBuf, Watch>,
    conflicts_pending: bool,
    conflicts: Vec<Conflict>,
}

struct Watch {
    threads: HashSet<ThreadId>,
    /// The last status, for threads that start watching later.
    last: Option<Vec<FileChange>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        // Dropping the task's watcher stops the OS watch and its thread.
        self.task.abort();
    }
}

impl Orchestrator {
    /// Starts reporting `thread`'s working-tree changes (it was opened).
    pub(crate) fn watch_workspace(&mut self, thread: ThreadId) {
        let Some(repo) = self.store.thread(thread).and_then(|t| self.repo_of(t)) else { return };
        // A thread whose workspace moved (into a new worktree) leaves its old one.
        self.unwatch_other(thread, &repo);
        if let Some(watch) = self.review.watchers.get_mut(&repo) {
            watch.threads.insert(thread);
            if let Some(files) = watch.last.clone() {
                self.emit(Event::WorkspaceChanged { thread, files });
            }
            return;
        }
        let (git, tx, path) = (self.git.clone(), self.internal_tx.clone(), repo.clone());
        let task = tokio::spawn(async move {
            let watcher = match git.watch(&path).await {
                Ok(watcher) => watcher,
                Err(err) => {
                    log::debug!("not watching {}: {err}", path.display());
                    return;
                }
            };
            loop {
                match git.status(&path).await {
                    Ok(status) => {
                        let msg = ReviewInternal::Workspace { repo: path.clone(), files: status.files };
                        if tx.send(Internal::Review(msg)).is_err() {
                            return;
                        }
                    }
                    Err(err) => log::debug!("status of {}: {err}", path.display()),
                }
                let Some(changes) = watcher.recv().await else { return };
                if changes.overflow || changes.paths.iter().any(|p| p.ends_with(".gitignore")) {
                    let _ = git.refresh_watch_ignores(&watcher).await;
                }
            }
        });
        let watch = Watch { threads: HashSet::from([thread]), last: None, task };
        self.review.watchers.insert(repo, watch);
    }

    /// Stops reporting `thread`'s changes (closed or deleted).
    pub(crate) fn unwatch_workspace(&mut self, thread: ThreadId) {
        self.review.watchers.retain(|_, w| {
            w.threads.remove(&thread);
            !w.threads.is_empty()
        });
    }

    fn unwatch_other(&mut self, thread: ThreadId, keep: &PathBuf) {
        self.review.watchers.retain(|repo, w| {
            if repo != keep {
                w.threads.remove(&thread);
            }
            !w.threads.is_empty()
        });
    }

    pub(crate) fn handle_review(&mut self, msg: ReviewInternal) {
        match msg {
            ReviewInternal::Workspace { repo, files } => {
                let Some(watch) = self.review.watchers.get_mut(&repo) else { return };
                watch.last = Some(files.clone());
                let threads: Vec<ThreadId> = watch.threads.iter().copied().collect();
                let mut in_worktree = false;
                for thread in threads {
                    in_worktree |= self.store.thread(thread).is_some_and(|t| t.workspace.worktree.is_some());
                    self.emit(Event::WorkspaceChanged { thread, files: files.clone() });
                }
                if in_worktree {
                    self.schedule_conflicts();
                }
            }
            ReviewInternal::Conflicts(conflicts) => {
                self.review.conflicts_pending = false;
                if conflicts != self.review.conflicts {
                    self.review.conflicts = conflicts.clone();
                    self.emit(Event::Conflicts(conflicts));
                }
            }
        }
    }

    /// Compares every live worktree's changes shortly, once per burst.
    pub(crate) fn schedule_conflicts(&mut self) {
        if self.review.conflicts_pending {
            return;
        }
        let live = self.live_worktrees();
        let tx = self.internal_tx.clone();
        if live.len() < 2 {
            let _ = tx.send(Internal::Review(ReviewInternal::Conflicts(Vec::new())));
            return;
        }
        self.review.conflicts_pending = true;
        let git = self.git.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CONFLICT_DELAY).await;
            let conflicts = match git.conflicts(&live).await {
                Ok(list) => list.into_iter().map(|c| Conflict { path: c.path, threads: c.threads }).collect(),
                Err(err) => {
                    log::debug!("conflict check failed: {err}");
                    Vec::new()
                }
            };
            let _ = tx.send(Internal::Review(ReviewInternal::Conflicts(conflicts)));
        });
    }

    /// Worktrees of threads still in play: not archived or settled, with a
    /// base branch to compare against.
    fn live_worktrees(&self) -> Vec<LiveWorktree> {
        self.store
            .threads()
            .filter(|t| !t.archived && t.settled_ms.is_none())
            .filter_map(|t| {
                let path = t.workspace.worktree.as_ref()?;
                let base = t.project.and_then(|p| self.store.project(p))?.base_branch.clone()?;
                Some(LiveWorktree { thread: t.id, path: PathBuf::from(path), base })
            })
            .collect()
    }

    /// The `(from, to)` trees of [`DiffScope::SinceTurn`]: from before the
    /// turn to the working tree.
    pub(crate) fn since_turn_range(&mut self, thread: ThreadId, turn: u32) -> Option<(String, Option<String>)> {
        self.turn_range(thread, turn).map(|(from, _)| (from, None))
    }

    /// Commands of the review engine.
    pub(crate) fn review_command(&mut self, command: Command) -> Outcome {
        let git = self.git.clone();
        match command {
            Command::UndoRestore { thread } => match self.repo_for(thread) {
                Err(e) => Outcome::Ready(Err(e)),
                Ok(repo) => Outcome::Deferred(Box::pin(async move {
                    git.undo_restore(&repo, thread).await.map(|_| Reply::Done).map_err(|e| match e {
                        crate::git::GitError::Invalid(_) => "There is no restore to undo.".to_owned(),
                        e => e.to_string(),
                    })
                })),
            },
            Command::MergeLocal { thread, into, message, dry_run } => {
                let repo = match self.repo_for(thread) {
                    Ok(repo) => repo,
                    Err(e) => return Outcome::Ready(Err(e)),
                };
                let into = if into.trim().is_empty() { self.base_branch_of(thread) } else { Some(into) };
                let Some(into) = into else {
                    return Outcome::Ready(Err("Pick the branch to merge into.".into()));
                };
                Outcome::Deferred(Box::pin(async move {
                    let (plan, merged) = if dry_run {
                        (git.merge_preview(&repo, &into).await.map_err(|e| e.to_string())?, None)
                    } else {
                        let message = if message.trim().is_empty() { "Work from Orbit".to_owned() } else { message };
                        let (plan, tip) = git.merge_into(&repo, &into, &message).await.map_err(|e| e.to_string())?;
                        (plan, Some(tip))
                    };
                    Ok(Reply::Merge(MergeInfo {
                        source: plan.source,
                        target: plan.target,
                        commits: plan.commits,
                        uncommitted: plan.uncommitted,
                        files: plan.files,
                        conflicts: plan.conflicts,
                        fast_forward: plan.fast_forward,
                        merged,
                    }))
                }))
            }
            _ => Outcome::Ready(Err("not a review command".into())),
        }
    }

    fn base_branch_of(&self, thread: ThreadId) -> Option<String> {
        let thread = self.store.thread(thread)?;
        self.store.project(thread.project?)?.base_branch.clone()
    }
}
