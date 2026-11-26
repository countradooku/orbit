//! Checkpoints, restore and patch revert.
//!
//! A checkpoint is the tree of the whole working tree (tracked and untracked
//! files, ignored files excluded) written through a private index file,
//! `<git-dir>/orbit/index-<thread>`. The index persists between checkpoints
//! and is first seeded from the user's index, so `git add -A` finds unchanged
//! files by stat and rehashes only what changed: a checkpoint of a large repo
//! costs a directory walk, not a re-read of every file (T3 Code #21328,
//! #26227). Nothing touches the user's index, HEAD or branch, no temporary
//! object directories are created, and there is no timeout that could kill a
//! capture halfway and retry it forever.
//!
//! Trees are pinned by `refs/orbit/checkpoints/<thread>/<n>` so `git gc`
//! keeps them, and [`Git::prune`] / [`Git::delete_thread`] /
//! [`Git::prune_orphans`] drop them again (T3 Code never did, #38102).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use orbit_proto::ThreadId;

use super::cmd::{Opts, first_line};
use super::{Git, GitError, RepoInfo, Result, TreeId};

const CHECKPOINTS: &str = "refs/orbit/checkpoints";
const UNDO: &str = "refs/orbit/undo";

/// A recorded snapshot of a thread's working tree. Checkpoint 0 is the
/// baseline before the first turn; checkpoint `n` is taken after turn `n`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub thread: ThreadId,
    pub n: u32,
    pub tree: TreeId,
}

/// The outcome of [`Git::restore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// The working tree as it was just before the restore; pass it to
    /// [`Git::restore`] again (or call [`Git::undo_restore`]) to undo.
    pub undo: TreeId,
    pub restored: TreeId,
}

impl Git {
    /// Snapshots the working tree as checkpoint `n + 1` of `thread` (0 for
    /// the first). Never touches the user's index or HEAD.
    pub async fn checkpoint(&self, repo: &Path, thread: ThreadId) -> Result<Checkpoint> {
        let info = self.checkpoint_repo(repo).await?;
        let index = thread_index(&info, thread);
        let lock = self.index_lock(&index);
        let _guard = lock.lock().await;
        let known = self.load_checkpoints(&info, thread).await?;
        let tree = self.write_tree(&info, &index).await?;
        let n = known.last().map_or(0, |(n, _)| n + 1);
        self.git(&info.root, ["update-ref", &checkpoint_ref(thread, n), tree.as_str()]).await?;
        let key = (info.common_dir.clone(), thread);
        self.inner.checkpoints.lock().unwrap().entry(key).or_default().push((n, tree.clone()));
        Ok(Checkpoint { thread, n, tree })
    }

    /// The thread's checkpoints, oldest first.
    pub async fn checkpoints(&self, repo: &Path, thread: ThreadId) -> Result<Vec<Checkpoint>> {
        let info = self.checkpoint_repo(repo).await?;
        let list = self.load_checkpoints(&info, thread).await?;
        Ok(list.into_iter().map(|(n, tree)| Checkpoint { thread, n, tree }).collect())
    }

    /// The tree of checkpoint `n`.
    pub async fn checkpoint_tree(&self, repo: &Path, thread: ThreadId, n: u32) -> Result<TreeId> {
        let info = self.checkpoint_repo(repo).await?;
        let list = self.load_checkpoints(&info, thread).await?;
        list.into_iter()
            .find(|(m, _)| *m == n)
            .map(|(_, t)| t)
            .ok_or(GitError::NoCheckpoint { thread, n })
    }

    /// The tree of the working tree right now, without recording it. Uses
    /// its own persistent index, so it is as cheap as a checkpoint.
    pub async fn snapshot(&self, repo: &Path) -> Result<TreeId> {
        let info = self.checkpoint_repo(repo).await?;
        let index = info.orbit_dir().join("index-live");
        let lock = self.index_lock(&index);
        let _guard = lock.lock().await;
        self.write_tree(&info, &index).await
    }

    /// Deletes all but the baseline and the newest `keep_last` checkpoints
    /// of a thread. Returns how many were deleted.
    pub async fn prune(&self, repo: &Path, thread: ThreadId, keep_last: usize) -> Result<usize> {
        let info = self.checkpoint_repo(repo).await?;
        let index = thread_index(&info, thread);
        let lock = self.index_lock(&index);
        let _guard = lock.lock().await;
        let list = self.load_checkpoints(&info, thread).await?;
        let cut = list.len().saturating_sub(keep_last);
        let doomed: Vec<u32> = list[..cut].iter().map(|(n, _)| *n).filter(|&n| n != 0).collect();
        if doomed.is_empty() {
            return Ok(0);
        }
        let refs: Vec<String> = doomed.iter().map(|&n| checkpoint_ref(thread, n)).collect();
        self.delete_refs(&info, &refs).await?;
        let key = (info.common_dir.clone(), thread);
        if let Some(list) = self.inner.checkpoints.lock().unwrap().get_mut(&key) {
            list.retain(|(n, _)| !doomed.contains(n));
        }
        Ok(doomed.len())
    }

    /// Removes every checkpoint, the undo point and the private index of a
    /// thread.
    pub async fn delete_thread(&self, repo: &Path, thread: ThreadId) -> Result<()> {
        let info = self.checkpoint_repo(repo).await?;
        let index = thread_index(&info, thread);
        let lock = self.index_lock(&index);
        let _guard = lock.lock().await;
        let mut refs: Vec<String> = self.list_refs(&info, &format!("{CHECKPOINTS}/{thread}/")).await?;
        refs.extend(self.list_refs(&info, &format!("{UNDO}/{thread}")).await?);
        self.delete_refs(&info, &refs).await?;
        self.inner.checkpoints.lock().unwrap().remove(&(info.common_dir.clone(), thread));
        remove_index_files(&index);
        Ok(())
    }

    /// Deletes checkpoints and indexes of threads that no longer exist (for
    /// example after a crash between deleting a thread and its refs).
    /// Returns how many refs were deleted.
    pub async fn prune_orphans(&self, repo: &Path, live: &[ThreadId]) -> Result<usize> {
        let info = self.checkpoint_repo(repo).await?;
        let live: HashSet<ThreadId> = live.iter().copied().collect();
        let refs = self.list_refs(&info, "refs/orbit/").await?;
        let dead: Vec<String> = refs
            .into_iter()
            .filter(|r| ref_thread(r).is_some_and(|t| !live.contains(&t)))
            .collect();
        self.delete_refs(&info, &dead).await?;
        let mut cache = self.inner.checkpoints.lock().unwrap();
        cache.retain(|(dir, t), _| dir != &info.common_dir || live.contains(t));
        drop(cache);
        if let Ok(entries) = std::fs::read_dir(info.orbit_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(id) = name.strip_prefix("index-").map(|s| s.trim_end_matches(".lock")) else { continue };
                if let Ok(t) = id.parse::<ThreadId>()
                    && id.starts_with("thr_") && !live.contains(&t) {
                        let _ = std::fs::remove_file(entry.path());
                    }
            }
        }
        Ok(dead.len())
    }

    /// Makes the working tree match `tree`: tracked content is restored and
    /// files added since `tree` are deleted. Ignored files and files in
    /// neither snapshot are never touched, nor are the user's index and HEAD.
    /// The current state is checkpointed first (as the thread's undo point),
    /// so the restore can itself be undone.
    pub async fn restore(&self, repo: &Path, thread: ThreadId, tree: &TreeId) -> Result<Restored> {
        let info = self.checkpoint_repo(repo).await?;
        let index = thread_index(&info, thread);
        let lock = self.index_lock(&index);
        let _guard = lock.lock().await;
        // Refreshes the private index to exactly the current state, which is
        // what `read-tree --reset -u` diffs against to decide what to write
        // and what to delete.
        let current = self.write_tree(&info, &index).await?;
        self.git(&info.root, ["update-ref", &format!("{UNDO}/{thread}"), current.as_str()]).await?;
        if current != *tree {
            let env = [("GIT_INDEX_FILE", index.as_os_str())];
            self.git_with(
                &info.root,
                ["-c", "core.safecrlf=false", "read-tree", "--reset", "-u", tree.as_str()],
                Opts { env: &env, ..Opts::default() },
            )
            .await?;
        }
        Ok(Restored { undo: current, restored: tree.clone() })
    }

    /// Undoes the thread's last [`Git::restore`].
    pub async fn undo_restore(&self, repo: &Path, thread: ThreadId) -> Result<Restored> {
        let info = self.checkpoint_repo(repo).await?;
        let out = self
            .git_output(&info.root, ["rev-parse", "--verify", "-q", &format!("{UNDO}/{thread}")], Opts::default())
            .await?;
        if !out.ok() {
            return Err(GitError::Invalid(format!("{thread} has nothing to undo")));
        }
        let tree = TreeId::new(first_line(&out.stdout));
        self.restore(repo, thread, &tree).await
    }

    /// Reverse-applies a patch (from [`orbit_proto::diff::hunk_patch`] or
    /// [`orbit_proto::diff::file_patch`]) to the working tree. The patch is
    /// checked first and nothing changes unless all of it applies.
    pub async fn revert_patch(&self, repo: &Path, patch: &str) -> Result<()> {
        let info = self.checkpoint_repo(repo).await?;
        let opts = Opts { stdin: Some(patch.as_bytes()), ..Opts::default() };
        let check = self
            .git_output(&info.root, ["apply", "-R", "--check", "--whitespace=nowarn", "-"], opts)
            .await?;
        if !check.ok() {
            return Err(GitError::PatchDoesNotApply(check.stderr_text()));
        }
        self.git_with(&info.root, ["-c", "core.safecrlf=false", "apply", "-R", "--whitespace=nowarn", "-"], opts)
            .await?;
        Ok(())
    }

    /// Applies a patch to the working tree (taking another worktree's
    /// version of a hunk). Checked first: nothing changes unless all of it
    /// applies.
    pub async fn apply_patch(&self, repo: &Path, patch: &str) -> Result<()> {
        let info = self.checkpoint_repo(repo).await?;
        let opts = Opts { stdin: Some(patch.as_bytes()), ..Opts::default() };
        let check = self.git_output(&info.root, ["apply", "--check", "--whitespace=nowarn", "-"], opts).await?;
        if !check.ok() {
            return Err(GitError::PatchDoesNotApply(check.stderr_text()));
        }
        self.git_with(&info.root, ["-c", "core.safecrlf=false", "apply", "--whitespace=nowarn", "-"], opts).await?;
        Ok(())
    }

    /// Writes the working tree into `index` and returns its tree. The caller
    /// holds the index's lock.
    async fn write_tree(&self, info: &RepoInfo, index: &Path) -> Result<TreeId> {
        prepare_index(info, index)?;
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        let opts = Opts { env: &env, ..Opts::default() };

        // Leave out new untracked files over the size cap.
        let cap = self.inner.cfg.max_untracked_file;
        let mut pathspecs = None;
        if cap > 0 {
            let out = self.git_with(&info.root, ["ls-files", "-z", "-o", "--exclude-standard"], opts).await?;
            let root = info.root.clone();
            let big = tokio::task::spawn_blocking(move || oversized(&root, &out, cap))
                .await
                .map_err(|e| GitError::Invalid(e.to_string()))?;
            if !big.is_empty() {
                log::info!("checkpoint skips {} untracked file(s) over {cap} bytes", big.len());
                let mut spec = b":/\0".to_vec();
                for path in big {
                    spec.extend_from_slice(b":(top,literal,exclude)");
                    spec.extend_from_slice(path.as_bytes());
                    spec.push(0);
                }
                pathspecs = Some(spec);
            }
        }

        let mut argv: Vec<&str> = vec![
            "-c",
            "core.safecrlf=false",
            "-c",
            "advice.addEmbeddedRepo=false",
            "add",
            "-A",
            "--ignore-errors",
        ];
        if pathspecs.is_some() {
            argv.extend(["--pathspec-from-file=-", "--pathspec-file-nul"]);
        }
        let out = self.git_output(&info.root, &argv, Opts { env: &env, stdin: pathspecs.as_deref() }).await?;
        match out.code {
            Some(0) => {}
            // --ignore-errors: unreadable files (locked on Windows) are
            // skipped and the rest is still staged.
            Some(1) => log::warn!("checkpoint skipped unreadable files: {}", out.stderr_text()),
            _ => return Err(super::cmd::failed("git", &super::cmd::args(&argv), &out)),
        }
        let tree = self.git_with(&info.root, ["write-tree"], opts).await?;
        Ok(TreeId::new(first_line(&tree)))
    }

    async fn load_checkpoints(&self, info: &RepoInfo, thread: ThreadId) -> Result<Vec<(u32, TreeId)>> {
        let key = (info.common_dir.clone(), thread);
        if let Some(list) = self.inner.checkpoints.lock().unwrap().get(&key) {
            return Ok(list.clone());
        }
        let prefix = format!("{CHECKPOINTS}/{thread}/");
        let out = self
            .git(&info.root, ["for-each-ref", "--format=%(refname) %(objectname)", prefix.as_str()])
            .await?;
        let mut list: Vec<(u32, TreeId)> = String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let (name, oid) = line.split_once(' ')?;
                let n = name.strip_prefix(&prefix)?.parse().ok()?;
                Some((n, TreeId::new(oid.trim())))
            })
            .collect();
        list.sort_by_key(|(n, _)| *n);
        self.inner.checkpoints.lock().unwrap().insert(key, list.clone());
        Ok(list)
    }

    async fn list_refs(&self, info: &RepoInfo, prefix: &str) -> Result<Vec<String>> {
        let out = self.git(&info.root, ["for-each-ref", "--format=%(refname)", prefix]).await?;
        Ok(String::from_utf8_lossy(&out).lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect())
    }

    /// Deletes refs in one `update-ref --stdin` transaction.
    async fn delete_refs(&self, info: &RepoInfo, refs: &[String]) -> Result<()> {
        if refs.is_empty() {
            return Ok(());
        }
        let mut script = String::new();
        for r in refs {
            script.push_str("delete ");
            script.push_str(r);
            script.push('\n');
        }
        self.git_with(&info.root, ["update-ref", "--stdin"], Opts { stdin: Some(script.as_bytes()), ..Opts::default() })
            .await?;
        Ok(())
    }
}

fn checkpoint_ref(thread: ThreadId, n: u32) -> String {
    format!("{CHECKPOINTS}/{thread}/{n}")
}

fn thread_index(info: &RepoInfo, thread: ThreadId) -> PathBuf {
    info.orbit_dir().join(format!("index-{thread}"))
}

/// The thread a `refs/orbit/...` ref belongs to.
fn ref_thread(name: &str) -> Option<ThreadId> {
    let rest = name.strip_prefix("refs/orbit/")?;
    let (_, rest) = rest.split_once('/')?;
    let id = rest.split('/').next()?;
    id.starts_with("thr_").then(|| id.parse().ok()).flatten()
}

/// Creates the private index on first use, seeded from the user's index so
/// the first snapshot already has stat data for every tracked file. A lock
/// file left by a killed git is stale: writers of this index are serialized
/// by the caller.
fn prepare_index(info: &RepoInfo, index: &Path) -> Result<()> {
    std::fs::create_dir_all(info.orbit_dir())?;
    let mut lock = index.as_os_str().to_owned();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
    if !index.exists() {
        let user = info.git_dir.join("index");
        if user.is_file()
            && let Err(e) = seed_index(&user, index) {
                log::debug!("could not seed {}: {e}", index.display());
                let _ = std::fs::remove_file(index);
            }
    }
    Ok(())
}

/// Copies the user's index as a starting point, keeping its modification
/// time. Git trusts an entry's cached stat only when the entry is older
/// than the index file; a copy stamped "now" would make an entry written
/// in the same second as the original index look trustworthy, and a file
/// rewritten in that second with the same size would be snapshotted with
/// its old content ("racy git").
fn seed_index(user: &Path, index: &Path) -> std::io::Result<()> {
    std::fs::copy(user, index)?;
    let modified = std::fs::metadata(user)?.modified()?;
    std::fs::File::options().write(true).open(index)?.set_modified(modified)
}

fn remove_index_files(index: &Path) {
    let _ = std::fs::remove_file(index);
    let mut lock = index.as_os_str().to_owned();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

/// Paths (from NUL-separated `ls-files` output) of regular files over `cap`.
fn oversized(root: &Path, listing: &[u8], cap: u64) -> Vec<String> {
    listing
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let rel = String::from_utf8_lossy(p);
            let meta = std::fs::symlink_metadata(root.join(OsStr::new(rel.as_ref()))).ok()?;
            (meta.is_file() && meta.len() > cap).then(|| rel.into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_thread_parses_orbit_refs_only() {
        let t = ThreadId(0xabc);
        assert_eq!(ref_thread(&format!("refs/orbit/checkpoints/{t}/3")), Some(t));
        assert_eq!(ref_thread(&format!("refs/orbit/undo/{t}")), Some(t));
        assert_eq!(ref_thread("refs/orbit/checkpoints/other/3"), None);
        assert_eq!(ref_thread("refs/heads/main"), None);
    }
}
