//! Diffs between snapshots: one git process for all files, cached by tree id.

use std::path::Path;
use std::sync::Arc;

use orbit_proto::diff::{self, FileDiff};
use orbit_proto::{DiffScope, FileChange, ThreadId};

use super::cmd::{Opts, first_line, nul_fields};
use super::{Git, GitError, Result, TreeId};

/// Flags that make `git diff` output independent of user configuration
/// (external diff tools, textconv filters, colors, custom prefixes).
const DIFF_FLAGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "-M",
    "--full-index",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// A parsed diff between two trees.
#[derive(Debug, PartialEq, Eq)]
pub struct DiffResult {
    pub from: TreeId,
    pub to: TreeId,
    pub files: Vec<FileDiff>,
}

impl DiffResult {
    /// Per-file summary (path, kind, additions, deletions).
    pub fn changes(&self) -> Vec<FileChange> {
        self.files.iter().map(FileDiff::to_change).collect()
    }

    pub fn additions(&self) -> u32 {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn deletions(&self) -> u32 {
        self.files.iter().map(|f| f.deletions).sum()
    }

    /// The full patch text.
    pub fn patch(&self) -> String {
        self.files.iter().map(|f| f.raw.as_str()).collect()
    }

    pub fn file(&self, path: &str) -> Option<&FileDiff> {
        self.files.iter().find(|f| f.path() == path)
    }
}

impl Git {
    /// The diff from `from` to `to`, or to the current working tree
    /// (untracked files included) when `to` is `None`. One `git diff` for
    /// all files; repeated and concurrent requests for the same trees share
    /// one result.
    pub async fn diff(&self, repo: &Path, from: &TreeId, to: Option<&TreeId>) -> Result<Arc<DiffResult>> {
        let to = match to {
            Some(t) => t.clone(),
            None => self.snapshot(repo).await?,
        };
        let key = (from.clone(), to.clone());
        let repo = repo.to_owned();
        self.inner
            .diffs
            .get_or_try_init(key, || async move {
                let info = self.repo(&repo).await?;
                let mut argv: Vec<&str> = vec!["diff"];
                argv.extend_from_slice(DIFF_FLAGS);
                argv.extend([from.as_str(), to.as_str(), "--"]);
                let out = self.git(&info.root, &argv).await?;
                let files = tokio::task::spawn_blocking(move || diff::parse_bytes(&out))
                    .await
                    .map_err(|e| GitError::Invalid(e.to_string()))?;
                Ok(Arc::new(DiffResult { from: from.clone(), to, files }))
            })
            .await
    }

    /// The diff a review scope shows; see [`Git::scope_range`].
    pub async fn diff_scope(&self, repo: &Path, thread: ThreadId, scope: &DiffScope) -> Result<Arc<DiffResult>> {
        let (from, to) = self.scope_range(repo, thread, scope).await?;
        self.diff(repo, &from, to.as_ref()).await
    }

    /// The (from, to) trees of a review scope; `to == None` is the working
    /// tree.
    ///
    /// * `Turn(n)`: checkpoint `n-1` → checkpoint `n` (→ working tree while
    ///   turn `n` has no checkpoint yet, i.e. is still running).
    /// * `Thread`: the thread's first checkpoint → working tree.
    /// * `Base(rev)`: merge-base of `rev` and HEAD → working tree.
    /// * `Uncommitted`: HEAD → working tree.
    pub async fn scope_range(&self, repo: &Path, thread: ThreadId, scope: &DiffScope) -> Result<(TreeId, Option<TreeId>)> {
        match scope {
            DiffScope::Turn(n) => {
                let n = *n;
                if n == 0 {
                    return Err(GitError::Invalid("turns are numbered from 1".into()));
                }
                let list = self.checkpoints(repo, thread).await?;
                let find = |m: u32| list.iter().find(|c| c.n == m).map(|c| c.tree.clone());
                let from = find(n - 1).ok_or(GitError::NoCheckpoint { thread, n: n - 1 })?;
                Ok((from, find(n)))
            }
            DiffScope::Thread => {
                let list = self.checkpoints(repo, thread).await?;
                let first = list.first().ok_or(GitError::NoCheckpoint { thread, n: 0 })?;
                Ok((first.tree.clone(), None))
            }
            DiffScope::Base(rev) => {
                let base = self
                    .merge_base(repo, rev, "HEAD")
                    .await?
                    .ok_or_else(|| GitError::Invalid(format!("{rev} and HEAD have no common ancestor")))?;
                Ok((TreeId::new(base), None))
            }
            DiffScope::Uncommitted => Ok((self.head_tree(repo).await?, None)),
        }
    }

    /// HEAD's tree, or the empty tree on an unborn branch.
    pub async fn head_tree(&self, repo: &Path) -> Result<TreeId> {
        let info = self.repo(repo).await?;
        let out = self.git_output(&info.root, ["rev-parse", "--verify", "-q", "HEAD^{tree}"], Opts::default()).await?;
        if out.ok() {
            return Ok(TreeId::new(first_line(&out.stdout)));
        }
        let empty = self
            .git_with(&info.root, ["hash-object", "-t", "tree", "-w", "--stdin"], Opts { stdin: Some(b""), ..Opts::default() })
            .await?;
        Ok(TreeId::new(first_line(&empty)))
    }

    /// Paths changed between two tree-ish ids (no rename pairing), cached.
    pub async fn changed_paths(&self, repo: &Path, from: &TreeId, to: &TreeId) -> Result<Arc<Vec<String>>> {
        let key = (from.clone(), to.clone());
        let repo = repo.to_owned();
        self.inner
            .names
            .get_or_try_init(key, || async move {
                let info = self.repo(&repo).await?;
                let out = self
                    .git(
                        &info.root,
                        ["diff", "--no-ext-diff", "--no-relative", "--no-renames", "--name-only", "-z", from.as_str(), to.as_str(), "--"],
                    )
                    .await?;
                Ok(Arc::new(nul_fields(&out).collect()))
            })
            .await
    }
}
