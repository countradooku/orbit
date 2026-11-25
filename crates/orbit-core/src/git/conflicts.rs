//! Early warning when parallel threads edit the same files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orbit_proto::ThreadId;
use tokio::task::JoinSet;

use super::{Git, GitError, Result, TreeId};

/// A live worktree to compare.
#[derive(Clone, Debug)]
pub struct LiveWorktree {
    pub thread: ThreadId,
    pub path: PathBuf,
    /// What the worktree's work will merge into (e.g. `main`).
    pub base: String,
}

/// A file changed by more than one live worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub threads: Vec<ThreadId>,
}

impl Git {
    /// Files changed in more than one of `worktrees`, each relative to its
    /// merge-base with its base branch, counting committed, uncommitted and
    /// untracked work. Sorted by path. Worktrees run concurrently under the
    /// process limit; per-pair results are cached by tree id.
    pub async fn conflicts(&self, worktrees: &[LiveWorktree]) -> Result<Vec<Conflict>> {
        let mut tasks = JoinSet::new();
        for wt in worktrees.iter().cloned() {
            let git = self.clone();
            tasks.spawn(async move {
                let paths = git.changed_since_base(&wt).await?;
                Ok::<_, GitError>((wt.thread, paths))
            });
        }
        let mut by_path: BTreeMap<String, Vec<ThreadId>> = BTreeMap::new();
        while let Some(joined) = tasks.join_next().await {
            let (thread, paths) = joined.map_err(|e| GitError::Invalid(e.to_string()))??;
            for p in paths.iter() {
                let threads = by_path.entry(p.clone()).or_default();
                if !threads.contains(&thread) {
                    threads.push(thread);
                }
            }
        }
        Ok(by_path
            .into_iter()
            .filter(|(_, t)| t.len() > 1)
            .map(|(path, mut threads)| {
                threads.sort();
                Conflict { path, threads }
            })
            .collect())
    }

    async fn changed_since_base(&self, wt: &LiveWorktree) -> Result<std::sync::Arc<Vec<String>>> {
        let base = self
            .merge_base(&wt.path, &wt.base, "HEAD")
            .await?
            .ok_or_else(|| GitError::Invalid(format!("{} has no common ancestor with {}", wt.path.display(), wt.base)))?;
        let now = self.snapshot(&wt.path).await?;
        self.changed_paths(&wt.path, &TreeId::new(base), &now).await
    }
}
