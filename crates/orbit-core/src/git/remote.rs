//! Commit, push and pull requests (through the user's own `gh`).

use std::path::{Path, PathBuf};

use super::cmd::{Opts, args, failed, first_line};
use super::{Git, GitError, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitResult {
    pub commit: String,
    pub pushed: Option<PushResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushResult {
    pub remote: String,
    pub branch: String,
    /// The branch had no upstream and `-u` set one.
    pub set_upstream: bool,
}

impl Git {
    /// Stages everything (`git add -A`) and commits it with `message`,
    /// optionally pushing. Fails with [`GitError::NothingToCommit`] when the
    /// working tree is clean.
    pub async fn commit(&self, repo: &Path, message: &str, push: bool) -> Result<CommitResult> {
        let info = self.repo(repo).await?;
        let root = &info.root;
        if message.trim().is_empty() {
            return Err(GitError::Invalid("commit message is empty".into()));
        }
        self.git(root, ["-c", "core.safecrlf=false", "add", "-A"]).await?;
        let argv = ["commit", "-q", "--no-edit", "-F", "-"];
        let out = self.git_output(root, argv, Opts { stdin: Some(message.as_bytes()), ..Opts::default() }).await?;
        if !out.ok() {
            let text = format!("{}{}", out.stdout_text(), out.stderr_text());
            if text.contains("nothing to commit") || text.contains("nothing added to commit") {
                return Err(GitError::NothingToCommit);
            }
            return Err(failed("git", &args(argv), &out));
        }
        let commit = first_line(&self.git(root, ["rev-parse", "HEAD"]).await?);
        let pushed = if push { Some(self.push(repo).await?) } else { None };
        Ok(CommitResult { commit, pushed })
    }

    /// Pushes the current branch, setting its upstream on first push.
    pub async fn push(&self, repo: &Path) -> Result<PushResult> {
        let info = self.repo(repo).await?;
        let root = &info.root;
        let branch = self.current_branch(repo).await?.ok_or(GitError::DetachedHead)?;
        let upstream = self
            .git_output(root, ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"], Opts::default())
            .await?;
        if upstream.ok() {
            let up = first_line(&upstream.stdout);
            let remote = up.split('/').next().unwrap_or("origin").to_owned();
            self.git(root, ["push", "--porcelain"]).await?;
            return Ok(PushResult { remote, branch, set_upstream: false });
        }
        let remotes = String::from_utf8_lossy(&self.git(root, ["remote"]).await?).into_owned();
        let remotes: Vec<&str> = remotes.lines().map(str::trim).filter(|r| !r.is_empty()).collect();
        let remote = if remotes.contains(&"origin") {
            "origin".to_owned()
        } else {
            remotes.first().ok_or(GitError::NoRemote)?.to_string()
        };
        self.git(root, ["push", "--porcelain", "-u", &remote, &branch]).await?;
        Ok(PushResult { remote, branch, set_upstream: true })
    }

    /// Pushes the branch and opens a pull request with the GitHub CLI.
    /// Returns the PR's URL.
    pub async fn create_pull_request(&self, repo: &Path, title: &str, body: &str, base: Option<&str>) -> Result<String> {
        let gh = self.gh()?;
        let info = self.repo(repo).await?;
        let pushed = self.push(repo).await?;
        let mut argv = vec!["pr", "create", "--title", title, "--body-file", "-", "--head", pushed.branch.as_str()];
        if let Some(base) = base {
            argv.extend(["--base", base]);
        }
        let argv = args(argv);
        let out = self.exec("gh", &gh, &info.root, argv.clone(), Opts { stdin: Some(body.as_bytes()), ..Opts::default() }).await?;
        if !out.ok() {
            return Err(failed("gh", &argv, &out));
        }
        parse_pr_url(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| GitError::Invalid(format!("gh did not print a pull request URL: {}", out.stdout_text())))
    }

    fn gh(&self) -> Result<PathBuf> {
        crate::provider::process::resolve_binary("gh", self.inner.cfg.gh.as_deref(), &[]).ok_or(GitError::NotInstalled("gh"))
    }
}

/// The last URL `gh pr create` printed (it may print warnings first).
pub fn parse_pr_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("https://") || l.starts_with("http://"))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_url_is_the_last_url_line() {
        let out = "Warning: 1 uncommitted change\nhttps://github.com/o/r/pull/12\n";
        assert_eq!(parse_pr_url(out).as_deref(), Some("https://github.com/o/r/pull/12"));
        assert_eq!(parse_pr_url("no url"), None);
    }
}
