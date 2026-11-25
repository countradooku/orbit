//! Worktrees per thread, from the existing checkout (never a clone).
//!
//! * Location is configurable; the default is
//!   `<data>/worktrees/<project>/<branch>`.
//! * `.worktreeinclude` (gitignore syntax, e.g. `.env*`) names untracked
//!   files copied from the main checkout, so the agent does not start without
//!   its `.env`.
//! * Every worktree gets its own persisted block of ports, exported to its
//!   scripts (and, through [`WorktreeRecord::env`], to the agent) as `PORT`
//!   and `ORBIT_PORT_BASE`, so parallel dev servers never collide.
//! * The project's `setup` script runs on creation and `teardown` on removal,
//!   with output streamed line by line.
//! * Removal prunes stale git metadata, deletes the branch the worktree has
//!   *now* (not the one it was created with, T3 Code #17813) and frees its
//!   ports.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use orbit_proto::ThreadId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::cmd::{Opts, nul_fields};
use super::{Git, GitError, Result};

const REGISTRY: &str = "worktrees.json";
const OUTPUT_TAIL: usize = 8 * 1024;

/// What to create.
#[derive(Clone, Debug)]
pub struct NewWorktree {
    /// The project's main checkout.
    pub project_root: PathBuf,
    pub thread: ThreadId,
    /// The branch to create (or check out, if it exists and is free).
    pub branch: String,
    /// What a new branch starts from; `None` is the main checkout's HEAD.
    pub base: Option<String>,
    /// Explicit path; `None` uses the configured worktree root.
    pub path: Option<PathBuf>,
    /// Run after creation, inside the worktree.
    pub setup: Option<String>,
}

/// What a project adds to a new worktree beyond [`NewWorktree`]: its own
/// environment, more files to copy and how many ports it reserves.
#[derive(Clone, Debug, Default)]
pub struct WorktreeExtras {
    /// Exported to the setup script, after the worktree's own variables.
    pub env: Vec<(String, String)>,
    /// More `.worktreeinclude` patterns (gitignore syntax).
    pub include: Vec<String>,
    /// Ports to reserve; `None` uses the configured block size.
    pub ports: Option<u16>,
}

/// A worktree Orbit created, as persisted in `<data>/worktrees.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub thread: ThreadId,
    pub project_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub port_base: u16,
    pub port_count: u16,
}

/// The on-disk form of a [`WorktreeRecord`].
#[derive(Serialize, Deserialize)]
struct StoredRecord {
    thread: u64,
    project_root: PathBuf,
    path: PathBuf,
    branch: String,
    base: String,
    port_base: u16,
    port_count: u16,
}

impl From<&WorktreeRecord> for StoredRecord {
    fn from(r: &WorktreeRecord) -> Self {
        StoredRecord {
            thread: r.thread.0,
            project_root: r.project_root.clone(),
            path: r.path.clone(),
            branch: r.branch.clone(),
            base: r.base.clone(),
            port_base: r.port_base,
            port_count: r.port_count,
        }
    }
}

impl From<StoredRecord> for WorktreeRecord {
    fn from(r: StoredRecord) -> Self {
        WorktreeRecord {
            thread: ThreadId(r.thread),
            project_root: r.project_root,
            path: r.path,
            branch: r.branch,
            base: r.base,
            port_base: r.port_base,
            port_count: r.port_count,
        }
    }
}

impl WorktreeRecord {
    /// Environment for anything running in the worktree: its scripts, its
    /// agent and its terminals.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("PORT".into(), self.port_base.to_string()),
            ("ORBIT_PORT_BASE".into(), self.port_base.to_string()),
            ("ORBIT_PORT_COUNT".into(), self.port_count.to_string()),
            ("ORBIT_WORKTREE".into(), self.path.to_string_lossy().into_owned()),
            ("ORBIT_ROOT_PATH".into(), self.project_root.to_string_lossy().into_owned()),
            ("ORBIT_BRANCH".into(), self.branch.clone()),
            ("ORBIT_THREAD".into(), self.thread.to_string()),
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub record: WorktreeRecord,
    /// Files copied because they match `.worktreeinclude`.
    pub copied: Vec<String>,
    /// The setup script's result; `None` when the project has none.
    pub setup: Option<ScriptResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptResult {
    pub code: Option<i32>,
    /// The last few KB of combined output.
    pub tail: String,
}

impl ScriptResult {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BranchCleanup {
    #[default]
    Keep,
    /// `git branch -d`: only if merged.
    DeleteIfMerged,
    /// `git branch -D`.
    Delete,
}

#[derive(Clone, Debug, Default)]
pub struct RemoveWorktree {
    /// Run inside the worktree before it is removed.
    pub teardown: Option<String>,
    pub branch: BranchCleanup,
    /// Remove even with uncommitted or untracked files (checkpoint first).
    pub force: bool,
    /// Exported to the teardown script, after the worktree's own variables.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedWorktree {
    pub teardown: Option<ScriptResult>,
    /// The branch that was deleted, if any.
    pub deleted_branch: Option<String>,
    /// Why the branch was kept despite a cleanup request (e.g. unmerged).
    pub branch_error: Option<String>,
}

impl Git {
    /// Creates a worktree for a thread from the project's existing checkout,
    /// copies `.worktreeinclude` files, allocates its ports and runs setup.
    /// A failing setup script does not undo the worktree; inspect
    /// [`WorktreeInfo::setup`].
    pub async fn create_worktree(
        &self,
        spec: NewWorktree,
        on_output: &mut (dyn FnMut(OutputStream, &str) + Send),
    ) -> Result<WorktreeInfo> {
        self.create_worktree_with(spec, WorktreeExtras::default(), on_output).await
    }

    /// [`Self::create_worktree`] with a project's own environment, shared
    /// files and port count.
    pub async fn create_worktree_with(
        &self,
        spec: NewWorktree,
        extras: WorktreeExtras,
        on_output: &mut (dyn FnMut(OutputStream, &str) + Send),
    ) -> Result<WorktreeInfo> {
        let info = self.repo(&spec.project_root).await?;
        let root = info.root.clone();
        if spec.branch.trim().is_empty() || spec.branch.starts_with('-') {
            return Err(GitError::Invalid(format!("invalid branch name {:?}", spec.branch)));
        }
        let base = spec.base.clone().unwrap_or_else(|| "HEAD".into());
        let path = match spec.path.clone() {
            Some(p) => std::path::absolute(p)?,
            None => {
                let project = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
                unused_dir(&self.inner.cfg.worktree_root().join(sanitize(&project)).join(sanitize(&spec.branch)))
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Forget worktrees whose directories were deleted behind git's back,
        // so their branches can be checked out again (T3 Code #19124).
        self.git(&root, ["worktree", "prune"]).await?;
        let exists = self
            .git_output(&root, ["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", spec.branch)], Opts::default())
            .await?
            .ok();
        let path_arg = path.as_os_str().to_owned();
        let added = if exists {
            self.git(&root, [std::ffi::OsStr::new("worktree"), "add".as_ref(), &path_arg, spec.branch.as_ref()]).await
        } else {
            self.git(
                &root,
                [
                    std::ffi::OsStr::new("worktree"),
                    "add".as_ref(),
                    "-b".as_ref(),
                    spec.branch.as_ref(),
                    &path_arg,
                    base.as_ref(),
                ],
            )
            .await
        };
        if let Err(e) = added {
            // Leave nothing behind: a half-made worktree or a branch created
            // before the checkout failed (T3 Code #11735).
            if path.exists() {
                let _ = std::fs::remove_dir_all(&path);
            }
            let _ = self.git(&root, ["worktree", "prune"]).await;
            if !exists {
                let _ = self.git(&root, ["branch", "-D", "--", spec.branch.as_str()]).await;
            }
            return Err(e);
        }

        let record = {
            let _guard = self.inner.registry.lock().await;
            let mut records = self.load_registry()?;
            records.retain(|r| r.path != path);
            let (port_base, port_count) = allocate_ports(&self.inner.cfg, extras.ports, &records)?;
            let record = WorktreeRecord {
                thread: spec.thread,
                project_root: root.clone(),
                path: path.clone(),
                branch: spec.branch.clone(),
                base,
                port_base,
                port_count,
            };
            records.push(record.clone());
            self.save_registry(&records)?;
            record
        };
        // Registered first, so a failed copy still leaves a worktree that
        // remove_worktree knows how to clean up.
        let copied = self.copy_includes(&root, &path, &extras.include).await?;

        let setup = match spec.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(script) => {
                let mut env = record.env();
                env.extend(extras.env.iter().cloned());
                Some(run_script(script, &path, &env, on_output).await?)
            }
            None => None,
        };
        Ok(WorktreeInfo { record, copied, setup })
    }

    /// Runs teardown, removes the worktree, optionally deletes its branch and
    /// frees its ports.
    pub async fn remove_worktree(
        &self,
        path: &Path,
        opts: RemoveWorktree,
        on_output: &mut (dyn FnMut(OutputStream, &str) + Send),
    ) -> Result<RemovedWorktree> {
        let path = std::path::absolute(path)?;
        let record = self.worktrees()?.into_iter().find(|r| same_path(&r.path, &path));
        let project_root = match &record {
            Some(r) => r.project_root.clone(),
            None => {
                let info = self.repo(&path).await?;
                info.common_dir.parent().map(Path::to_path_buf).ok_or_else(|| GitError::NotARepo(path.clone()))?
            }
        };

        let teardown = match (opts.teardown.as_deref().filter(|s| !s.trim().is_empty()), path.is_dir()) {
            (Some(script), true) => {
                let mut env = record.as_ref().map(WorktreeRecord::env).unwrap_or_default();
                env.extend(opts.env.iter().cloned());
                Some(run_script(script, &path, &env, on_output).await?)
            }
            _ => None,
        };

        // The branch it has now, which may differ from the one it was
        // created with.
        let branch = read_worktree_branch(&path).or_else(|| record.as_ref().map(|r| r.branch.clone()));

        let mut argv = vec![std::ffi::OsString::from("worktree"), "remove".into()];
        if opts.force {
            argv.push("--force".into());
        }
        argv.push(path.as_os_str().to_owned());
        let out = self.git_output(&project_root, &argv, Opts::default()).await?;
        if !out.ok() {
            if !opts.force {
                return Err(super::cmd::failed("git", &argv, &out));
            }
            // A file held open on Windows can make git give up halfway;
            // finish the job and let git forget the worktree.
            if path.exists() {
                let p = path.clone();
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(p))
                    .await
                    .map_err(|e| GitError::Invalid(e.to_string()))??;
            }
            self.git(&project_root, ["worktree", "prune"]).await?;
        }
        self.repos_forget(&path);

        let (mut deleted_branch, mut branch_error) = (None, None);
        if let Some(branch) = branch.filter(|_| opts.branch != BranchCleanup::Keep) {
            let flag = if opts.branch == BranchCleanup::Delete { "-D" } else { "-d" };
            let out = self.git_output(&project_root, ["branch", flag, "--", branch.as_str()], Opts::default()).await?;
            if out.ok() {
                deleted_branch = Some(branch);
            } else {
                branch_error = Some(out.stderr_text());
            }
        }

        let _guard = self.inner.registry.lock().await;
        let mut records = self.load_registry()?;
        let before = records.len();
        records.retain(|r| !same_path(&r.path, &path));
        if records.len() != before {
            self.save_registry(&records)?;
        }
        Ok(RemovedWorktree { teardown, deleted_branch, branch_error })
    }

    /// Every worktree Orbit created and has not removed.
    pub fn worktrees(&self) -> Result<Vec<WorktreeRecord>> {
        self.load_registry()
    }

    /// The worktree registered for a thread.
    pub fn worktree_for(&self, thread: ThreadId) -> Result<Option<WorktreeRecord>> {
        Ok(self.load_registry()?.into_iter().find(|r| r.thread == thread))
    }

    /// Copies untracked files of the main checkout that match
    /// `.worktreeinclude` or `extra` patterns (one `ls-files`; gitignore
    /// rules, `!` negation).
    async fn copy_includes(&self, root: &Path, dest: &Path, extra: &[String]) -> Result<Vec<String>> {
        let include = root.join(".worktreeinclude");
        let has_file = include.is_file();
        if !has_file && extra.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv: Vec<std::ffi::OsString> = vec!["ls-files".into(), "-z".into(), "--others".into(), "--ignored".into()];
        if has_file {
            let mut flag = std::ffi::OsString::from("--exclude-from=");
            flag.push(include.as_os_str());
            argv.push(flag);
        }
        for pattern in extra.iter().map(|p| p.trim()).filter(|p| !p.is_empty() && !p.starts_with('#')) {
            argv.push(format!("--exclude={pattern}").into());
        }
        let out = self.git(root, argv).await?;
        let files: Vec<String> = nul_fields(&out).collect();
        let (root, dest) = (root.to_owned(), dest.to_owned());
        tokio::task::spawn_blocking(move || {
            let mut copied = Vec::new();
            for rel in files {
                let to = dest.join(&rel);
                if to.exists() {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(root.join(&rel), &to)?;
                copied.push(rel);
            }
            Ok::<_, std::io::Error>(copied)
        })
        .await
        .map_err(|e| GitError::Invalid(e.to_string()))?
        .map_err(GitError::Io)
    }

    fn registry_path(&self) -> PathBuf {
        self.inner.cfg.data_dir.join(REGISTRY)
    }

    fn load_registry(&self) -> Result<Vec<WorktreeRecord>> {
        match std::fs::read(self.registry_path()) {
            Ok(bytes) => sonic_rs::from_slice::<Vec<StoredRecord>>(&bytes)
                .map(|v| v.into_iter().map(WorktreeRecord::from).collect())
                .map_err(|e| GitError::Invalid(format!("corrupt worktree registry {}: {e}", self.registry_path().display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic replace: write a temp file, then rename over the old one.
    fn save_registry(&self, records: &[WorktreeRecord]) -> Result<()> {
        let path = self.registry_path();
        std::fs::create_dir_all(&self.inner.cfg.data_dir)?;
        let stored: Vec<StoredRecord> = records.iter().map(StoredRecord::from).collect();
        let bytes = sonic_rs::to_vec_pretty(&stored).map_err(|e| GitError::Invalid(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn repos_forget(&self, path: &Path) {
        self.inner.repos.lock().unwrap().retain(|k, info| !k.starts_with(path) && !info.root.starts_with(path));
    }
}

/// Bytes used by a directory tree (files only, links not followed).
pub async fn disk_usage(path: &Path) -> std::io::Result<u64> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut total = 0u64;
        let mut stack = vec![path];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e),
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(entry.path());
                } else if meta.is_file() {
                    total += meta.len();
                }
            }
        }
        Ok(total)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// The lowest free port block that is also bindable right now.
fn allocate_ports(cfg: &super::GitConfig, ports: Option<u16>, records: &[WorktreeRecord]) -> Result<(u16, u16)> {
    let block = ports.filter(|p| *p > 0).unwrap_or(cfg.port_block).max(1);
    let mut base = cfg.port_first;
    while u32::from(base) + u32::from(block) - 1 <= u32::from(cfg.port_last) {
        let (lo, hi) = (u32::from(base), u32::from(base) + u32::from(block));
        let taken = records
            .iter()
            .any(|r| lo < u32::from(r.port_base) + u32::from(r.port_count) && u32::from(r.port_base) < hi);
        if !taken && (base..base + block).all(port_free) {
            return Ok((base, block));
        }
        base += block;
    }
    Err(GitError::Invalid("no free port range left for a new worktree".into()))
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Turns a branch or project name into one safe path component.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let s = s.trim_matches(['.', '-']).to_owned();
    if s.is_empty() { "worktree".into() } else { s }
}

/// `path`, or `path-2`, `path-3`, … if it is taken.
fn unused_dir(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    for i in 2.. {
        let mut name = path.file_name().unwrap_or_default().to_owned();
        name.push(format!("-{i}"));
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

/// The branch checked out in a worktree, from its HEAD file.
fn read_worktree_branch(path: &Path) -> Option<String> {
    let dotgit = std::fs::read_to_string(path.join(".git")).ok()?;
    let gitdir = dotgit.trim().strip_prefix("gitdir:")?.trim();
    let head = std::fs::read_to_string(Path::new(gitdir).join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_owned)
}

/// Runs a project script through the platform shell inside `cwd`,
/// streaming each output line.
async fn run_script(
    script: &str,
    cwd: &Path,
    env: &[(String, String)],
    on_output: &mut (dyn FnMut(OutputStream, &str) + Send),
) -> Result<ScriptResult> {
    #[cfg(windows)]
    let mut cmd = {
        let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        let mut cmd = tokio::process::Command::new(comspec);
        // Passed verbatim: Rust's argument quoting is not cmd's.
        cmd.raw_arg(format!("/d /s /c \"{script}\""));
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let mut out = BufReader::new(child.stdout.take().expect("piped"));
    let mut err = BufReader::new(child.stderr.take().expect("piped"));
    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let (mut out_done, mut err_done) = (false, false);
    let mut tail = String::new();
    let mut emit = |stream: OutputStream, buf: &mut Vec<u8>, tail: &mut String| {
        let line = String::from_utf8_lossy(buf);
        let line = line.trim_end_matches(['\r', '\n']);
        on_output(stream, line);
        tail.push_str(line);
        tail.push('\n');
        if tail.len() > OUTPUT_TAIL * 2 {
            let mut cut = tail.len() - OUTPUT_TAIL;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
        buf.clear();
    };
    while !(out_done && err_done) {
        // read_until keeps partial lines in the buffer, so losing the race
        // in select! loses nothing.
        let (stream, n) = tokio::select! {
            n = out.read_until(b'\n', &mut out_buf), if !out_done => (OutputStream::Stdout, n),
            n = err.read_until(b'\n', &mut err_buf), if !err_done => (OutputStream::Stderr, n),
        };
        let (buf, done) = match stream {
            OutputStream::Stdout => (&mut out_buf, &mut out_done),
            OutputStream::Stderr => (&mut err_buf, &mut err_done),
        };
        match n {
            Ok(0) | Err(_) => *done = true,
            Ok(_) => emit(stream, buf, &mut tail),
        }
    }
    let status = child.wait().await?;
    if tail.len() > OUTPUT_TAIL {
        let mut cut = tail.len() - OUTPUT_TAIL;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail.drain(..cut);
    }
    Ok(ScriptResult { code: status.code(), tail })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_makes_one_component() {
        assert_eq!(sanitize("feature/login page"), "feature-login-page");
        assert_eq!(sanitize("../.."), "worktree");
        assert_eq!(sanitize("orbit/thr_1f"), "orbit-thr_1f");
    }

    #[test]
    fn ports_skip_taken_blocks() {
        let mut cfg = super::super::GitConfig::new(std::env::temp_dir());
        cfg.port_first = 52_000;
        cfg.port_last = 52_099;
        let rec = |base| WorktreeRecord {
            thread: ThreadId(1),
            project_root: PathBuf::new(),
            path: PathBuf::new(),
            branch: String::new(),
            base: String::new(),
            port_base: base,
            port_count: 10,
        };
        let (a, n) = allocate_ports(&cfg, None, &[]).unwrap();
        assert_eq!(n, 10);
        assert!(a >= 52_000);
        let (b, _) = allocate_ports(&cfg, None, &[rec(a)]).unwrap();
        assert!(b >= a + 10);
        let all: Vec<_> = (0..10).map(|i| rec(52_000 + i * 10)).collect();
        assert!(allocate_ports(&cfg, None, &all).is_err());
    }
}
