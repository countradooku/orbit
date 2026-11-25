//! Working tree status, branches and merge bases.

use std::collections::HashMap;
use std::path::Path;

use orbit_proto::{ChangeKind, FileChange};

use super::cmd::{Opts, first_line, nul_fields};
use super::{Git, Result};

/// Untracked files up to this size have their lines counted as additions.
const COUNT_UNTRACKED_MAX: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// The checked-out branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// HEAD's commit; `None` on an unborn branch.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Changes against HEAD: staged, unstaged and untracked together.
    pub files: Vec<FileChange>,
    /// Paths with unresolved merge conflicts (also listed in `files`).
    pub conflicted: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// Short name: `main`, or `origin/main` for remote branches.
    pub name: String,
    pub remote: bool,
    /// Checked out in this working tree.
    pub current: bool,
    pub commit: String,
    pub upstream: Option<String>,
    /// The working tree it is checked out in, if any (here or another worktree).
    pub worktree: Option<String>,
}

impl Git {
    /// `git status --porcelain=v2` plus one `--numstat`, run concurrently.
    pub async fn status(&self, repo: &Path) -> Result<Status> {
        let info = self.repo(repo).await?;
        let root = &info.root;
        let status = self.git(root, ["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all", "--no-renames"]);
        let numstat = self.git_output(root, ["diff", "HEAD", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-relative"], Opts::default());
        let (status, numstat) = tokio::join!(status, numstat);
        let status = status?;
        let numstat = numstat?;
        let counts = if numstat.ok() { parse_numstat(&numstat.stdout) } else { HashMap::new() };

        let mut st = Status::default();
        let mut untracked = Vec::new();
        let mut fields = nul_fields(&status);
        while let Some(rec) = fields.next() {
            if let Some(header) = rec.strip_prefix("# ") {
                let (key, value) = header.split_once(' ').unwrap_or((header, ""));
                match key {
                    "branch.oid" if value != "(initial)" => st.head = Some(value.to_owned()),
                    "branch.head" if value != "(detached)" => st.branch = Some(value.to_owned()),
                    "branch.upstream" => st.upstream = Some(value.to_owned()),
                    "branch.ab" => {
                        for part in value.split(' ') {
                            if let Some(a) = part.strip_prefix('+') {
                                st.ahead = a.parse().unwrap_or(0);
                            } else if let Some(b) = part.strip_prefix('-') {
                                st.behind = b.parse().unwrap_or(0);
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let (tag, rest) = rec.split_at(1);
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match tag {
                "1" | "2" | "u" => {
                    // Fields before the path: 1 → 7, 2 → 8, u → 10.
                    let skip = match tag {
                        "1" => 7,
                        "2" => 8,
                        _ => 10,
                    };
                    let mut parts = rest.splitn(skip + 1, ' ');
                    let xy = parts.next().unwrap_or("..");
                    let Some(path) = parts.nth(skip - 1) else { continue };
                    if tag == "2" {
                        fields.next(); // original path
                    }
                    let kind = if tag == "u" {
                        st.conflicted.push(path.to_owned());
                        ChangeKind::Modified
                    } else {
                        xy_kind(xy)
                    };
                    let (additions, deletions) = counts.get(path).copied().unwrap_or((0, 0));
                    st.files.push(FileChange { path: path.to_owned(), kind, additions, deletions });
                }
                "?" => untracked.push(rest.to_owned()),
                _ => {}
            }
        }

        // Untracked files are not in any diff; count their lines directly.
        let root = root.clone();
        let counted = tokio::task::spawn_blocking(move || {
            untracked
                .into_iter()
                .take(10_000)
                .map(|path| {
                    let lines = count_lines(&root.join(&path));
                    FileChange { path, kind: ChangeKind::Added, additions: lines, deletions: 0 }
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        st.files.extend(counted);
        st.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(st)
    }

    /// Local and remote-tracking branches in one `for-each-ref`.
    pub async fn branches(&self, repo: &Path) -> Result<Vec<Branch>> {
        let info = self.repo(repo).await?;
        let format = "--format=%(refname)%00%(objectname)%00%(HEAD)%00%(upstream:short)%00%(worktreepath)%00";
        let out = self.git(&info.root, ["for-each-ref", format, "refs/heads", "refs/remotes"]).await?;
        let mut fields = out.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).trim().to_owned());
        let mut branches = Vec::new();
        while let Some(refname) = fields.next() {
            if refname.is_empty() {
                continue;
            }
            let commit = fields.next().unwrap_or_default();
            let head = fields.next().unwrap_or_default();
            let upstream = fields.next().unwrap_or_default();
            let worktree = fields.next().unwrap_or_default();
            let (name, remote) = if let Some(n) = refname.strip_prefix("refs/heads/") {
                (n.to_owned(), false)
            } else if let Some(n) = refname.strip_prefix("refs/remotes/") {
                if n.ends_with("/HEAD") {
                    continue;
                }
                (n.to_owned(), true)
            } else {
                continue;
            };
            branches.push(Branch {
                name,
                remote,
                current: head == "*",
                commit,
                upstream: (!upstream.is_empty()).then_some(upstream),
                worktree: (!worktree.is_empty()).then_some(worktree),
            });
        }
        Ok(branches)
    }

    /// The checked-out branch, read straight from HEAD (no process).
    /// `None` when HEAD is detached.
    pub async fn current_branch(&self, repo: &Path) -> Result<Option<String>> {
        let info = self.repo(repo).await?;
        let head = tokio::fs::read_to_string(info.git_dir.join("HEAD")).await?;
        Ok(head.trim().strip_prefix("ref: refs/heads/").map(str::to_owned))
    }

    /// Commits HEAD has that `base` does not, and the other way round
    /// (`(ahead, behind)`); `None` when `base` does not resolve.
    pub async fn ahead_behind(&self, repo: &Path, base: &str) -> Result<Option<(u32, u32)>> {
        let info = self.repo(repo).await?;
        let range = format!("{base}...HEAD");
        let out = self.git_output(&info.root, ["rev-list", "--left-right", "--count", range.as_str(), "--"], Opts::default()).await?;
        if !out.ok() {
            return Ok(None);
        }
        let line = first_line(&out.stdout);
        let mut parts = line.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
        let behind = parts.next().unwrap_or(0);
        let ahead = parts.next().unwrap_or(0);
        Ok(Some((ahead, behind)))
    }

    /// The best common ancestor of two revisions, if they have one.
    pub async fn merge_base(&self, repo: &Path, a: &str, b: &str) -> Result<Option<String>> {
        let info = self.repo(repo).await?;
        let out = self.git_output(&info.root, ["merge-base", a, b], Opts::default()).await?;
        match out.code {
            Some(0) => Ok(Some(first_line(&out.stdout))),
            Some(1) => Ok(None),
            _ => Err(super::cmd::failed("git", &super::cmd::args(["merge-base", a, b]), &out)),
        }
    }
}

fn xy_kind(xy: &str) -> ChangeKind {
    let b = xy.as_bytes();
    let (x, y) = (b.first().copied().unwrap_or(b'.'), b.get(1).copied().unwrap_or(b'.'));
    if x == b'D' || y == b'D' {
        ChangeKind::Deleted
    } else if x == b'A' {
        ChangeKind::Added
    } else if x == b'R' || x == b'C' {
        ChangeKind::Renamed
    } else {
        ChangeKind::Modified
    }
}

/// `--numstat -z --no-renames`: `added\tdeleted\tpath\0`; binary is `-\t-`.
fn parse_numstat(bytes: &[u8]) -> HashMap<String, (u32, u32)> {
    nul_fields(bytes)
        .filter_map(|rec| {
            let mut parts = rec.splitn(3, '\t');
            let add = parts.next()?.parse().unwrap_or(0);
            let del = parts.next()?.parse().unwrap_or(0);
            let path = parts.next()?.trim_start_matches(['\n', '\r']);
            Some((path.to_owned(), (add, del)))
        })
        .collect()
}

fn count_lines(path: &Path) -> u32 {
    let Ok(meta) = std::fs::metadata(path) else { return 0 };
    if !meta.is_file() || meta.len() > COUNT_UNTRACKED_MAX {
        return 0;
    }
    let Ok(bytes) = std::fs::read(path) else { return 0 };
    if bytes.is_empty() || memchr::memchr(0, &bytes).is_some() {
        return 0;
    }
    let newlines = memchr::memchr_iter(b'\n', &bytes).count() as u32;
    newlines + u32::from(!bytes.ends_with(b"\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_and_xy_parsing() {
        let m = parse_numstat(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0");
        assert_eq!(m["src/a.rs"], (3, 1));
        assert_eq!(m["img.png"], (0, 0));
        assert_eq!(xy_kind(".M"), ChangeKind::Modified);
        assert_eq!(xy_kind("A."), ChangeKind::Added);
        assert_eq!(xy_kind(".D"), ChangeKind::Deleted);
        assert_eq!(xy_kind("R."), ChangeKind::Renamed);
    }
}
