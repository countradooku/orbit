//! Merging a thread's branch locally, without a pull request.
//!
//! The preview never touches a working tree, an index or a ref: uncommitted
//! work is folded into a throwaway commit object (`commit-tree` on a
//! snapshot) and `merge-tree --write-tree` computes the merged tree and its
//! conflicts in the object store. The merge itself then either fast-forwards
//! or writes the merge commit the preview computed, so what was shown is
//! exactly what lands. When the target branch is checked out somewhere, the
//! merge runs there with plain `git merge`, so that working tree follows.

use std::path::{Path, PathBuf};

use orbit_proto::FileChange;

use super::cmd::{Opts, first_line, nul_fields};
use super::{Git, GitError, Result, TreeId};

/// What merging the checked-out branch of a working tree into another
/// branch would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergePlan {
    /// The branch being merged.
    pub source: String,
    /// The commit merged: HEAD, or a throwaway commit of HEAD plus the
    /// uncommitted work when there is some.
    pub source_commit: String,
    pub target: String,
    pub target_commit: String,
    /// Commits on the source that the target does not have.
    pub commits: u32,
    /// The target is an ancestor of the source.
    pub fast_forward: bool,
    /// The merged tree (with conflict markers where `conflicts` says).
    pub tree: TreeId,
    pub conflicts: Vec<String>,
    /// Files the merge changes on the target.
    pub files: Vec<FileChange>,
    /// Files with uncommitted changes in the source working tree.
    pub uncommitted: u32,
    /// The working tree the target is checked out in, if any.
    pub target_worktree: Option<PathBuf>,
}

impl Git {
    /// Previews merging `repo`'s current branch (uncommitted work included)
    /// into the local branch `into`. Changes nothing.
    pub async fn merge_preview(&self, repo: &Path, into: &str) -> Result<MergePlan> {
        let uncommitted = self.status(repo).await?.files.len() as u32;
        self.plan(repo, into, uncommitted).await
    }

    /// Merges `repo`'s current branch into the local branch `into`. Any
    /// uncommitted work is committed first with `commit_message`. Fails
    /// without changing anything when the merge would conflict. Returns the
    /// plan that was carried out and the new tip of `into`.
    pub async fn merge_into(&self, repo: &Path, into: &str, commit_message: &str) -> Result<(MergePlan, String)> {
        // Check before committing anything, so a conflict leaves no trace.
        let preview = self.merge_preview(repo, into).await?;
        if preview.commits == 0 && preview.uncommitted == 0 {
            return Err(GitError::Invalid(format!("{} has nothing {into} doesn’t already have", preview.source)));
        }
        if !preview.conflicts.is_empty() {
            return Err(conflict_error(&preview));
        }
        if preview.uncommitted > 0 {
            self.commit(repo, commit_message, false).await?;
        }
        let mut plan = self.plan(repo, into, 0).await?;
        plan.uncommitted = preview.uncommitted;
        if !plan.conflicts.is_empty() {
            return Err(conflict_error(&plan));
        }
        let message = format!("Merge branch '{}' into {}", plan.source, plan.target);
        if let Some(worktree) = &plan.target_worktree {
            let out = self
                .git_output(worktree, ["merge", "--ff", "--no-edit", "-m", &message, &plan.source_commit], Opts::default())
                .await?;
            if !out.ok() {
                // Leave the user's checkout as it was.
                let _ = self.git_output(worktree, ["merge", "--abort"], Opts::default()).await;
                return Err(GitError::Invalid(format!(
                    "Couldn’t merge into {} where it is checked out ({}): {}",
                    plan.target,
                    worktree.display(),
                    if out.stderr_text().is_empty() { out.stdout_text() } else { out.stderr_text() }
                )));
            }
            let tip = self.git(worktree, ["rev-parse", "HEAD"]).await?;
            return Ok((plan, first_line(&tip)));
        }
        let info = self.repo(repo).await?;
        let tip = if plan.fast_forward {
            plan.source_commit.clone()
        } else {
            let out = self
                .git_with(
                    &info.root,
                    ["commit-tree", plan.tree.as_str(), "-p", &plan.target_commit, "-p", &plan.source_commit, "-F", "-"],
                    Opts { stdin: Some(message.as_bytes()), ..Opts::default() },
                )
                .await?;
            first_line(&out)
        };
        let reference = format!("refs/heads/{}", plan.target);
        // The old value guards against the branch moving since the preview.
        self.git(&info.root, ["update-ref", "-m", &message, &reference, &tip, &plan.target_commit]).await?;
        Ok((plan, tip))
    }

    async fn plan(&self, repo: &Path, into: &str, uncommitted: u32) -> Result<MergePlan> {
        let info = self.repo(repo).await?;
        let root = &info.root;
        let source = self.current_branch(repo).await?.ok_or(GitError::DetachedHead)?;
        if source == into {
            return Err(GitError::Invalid(format!("This work is already on {into}.")));
        }
        let target_ref = format!("refs/heads/{into}");
        let target = self.git_output(root, ["rev-parse", "--verify", "-q", &format!("{target_ref}^{{commit}}")], Opts::default()).await?;
        if !target.ok() {
            return Err(GitError::Invalid(format!("There is no local branch named {into}.")));
        }
        let target_commit = first_line(&target.stdout);
        let head = first_line(&self.git(root, ["rev-parse", "HEAD"]).await?);
        let source_commit = if uncommitted > 0 {
            let tree = self.snapshot(repo).await?;
            let out = self
                .git_with(
                    root,
                    ["commit-tree", tree.as_str(), "-p", &head, "-F", "-"],
                    Opts { stdin: Some(b"Uncommitted work (preview)"), ..Opts::default() },
                )
                .await?;
            first_line(&out)
        } else {
            head
        };
        // Real commits only: the preview's throwaway commit is not one.
        let range = format!("{target_commit}..HEAD");
        let count = self.git(root, ["rev-list", "--count", &range]).await?;
        let commits = first_line(&count).parse().unwrap_or(0);
        let fast_forward =
            self.git_output(root, ["merge-base", "--is-ancestor", &target_commit, &source_commit], Opts::default()).await?.ok();

        let out = self
            .git_output(
                root,
                ["merge-tree", "--write-tree", "--name-only", "--no-messages", "-z", &target_commit, &source_commit],
                Opts::default(),
            )
            .await?;
        // 0: clean, 1: conflicts; anything else is a real failure.
        if !matches!(out.code, Some(0 | 1)) {
            return Err(super::cmd::failed("git", &super::cmd::args(["merge-tree", "--write-tree"]), &out));
        }
        let mut fields = nul_fields(&out.stdout);
        let tree = TreeId::new(fields.next().unwrap_or_default().trim());
        let mut conflicts: Vec<String> = fields.map(|f| f.trim_matches('\n').to_owned()).filter(|f| !f.is_empty()).collect();
        conflicts.sort();
        conflicts.dedup();

        let files = self.diff(repo, &TreeId::new(target_commit.clone()), Some(&tree)).await?.changes();
        let target_worktree = self
            .branches(repo)
            .await?
            .into_iter()
            .find(|b| !b.remote && b.name == into)
            .and_then(|b| b.worktree)
            .map(PathBuf::from);
        Ok(MergePlan {
            source,
            source_commit,
            target: into.to_owned(),
            target_commit,
            commits,
            fast_forward,
            tree,
            conflicts,
            files,
            uncommitted,
            target_worktree,
        })
    }
}

fn conflict_error(plan: &MergePlan) -> GitError {
    let shown: Vec<&str> = plan.conflicts.iter().take(5).map(String::as_str).collect();
    let more = plan.conflicts.len().saturating_sub(shown.len());
    let mut text = format!("Merging into {} would conflict in {}", plan.target, shown.join(", "));
    if more > 0 {
        text.push_str(&format!(" and {more} more"));
    }
    text.push('.');
    GitError::Invalid(text)
}
