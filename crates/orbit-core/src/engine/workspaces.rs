//! Workspaces: project configuration, run targets, live git status and
//! reading a workspace's files.
//!
//! * **Configuration.** A project's [`ProjectConfig`] is a logged record of
//!   its own ([`Event::ProjectConfigChanged`]). A project added with an
//!   `orbit.toml` takes its configuration from it; [`WorkspaceCommand::Import`]
//!   previews what the repository's files (Orbit's and other tools') say.
//! * **Run targets** run through the user's shell in a terminal the engine
//!   owns, in the thread's workspace, with its ports and the project's
//!   environment. They outlive the views that show them; their state goes
//!   out as [`Event::RunChanged`].
//! * **Git status** of every active project's checkout and every open
//!   thread's workspace comes from the review engine's watchers (one per
//!   folder, debounced, never polled) and goes out as [`Event::GitStatus`]
//!   for each root at that folder, only when it changed.
//! * **Files** are read on the blocking pool, bounded, and never outside
//!   the root (see [`fs`]).

pub(crate) mod fs;
mod import;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator, Outcome};
use crate::git::{Git, Status};
use crate::terminal::TermSize;

/// The workspaces engine's state on the orchestrator.
#[derive(Default)]
pub(crate) struct WorkspacesState {
    /// The last status sent for each root.
    git: HashMap<WorkspaceRoot, GitSummary>,
    /// The last status of each watched folder, for roots that join later.
    by_folder: HashMap<PathBuf, GitSummary>,
    runs: HashMap<(ThreadId, String), TargetRun>,
    run_terms: HashMap<TermId, (ThreadId, String)>,
}

impl WorkspacesState {
    pub(crate) fn git_snapshot(&self) -> Vec<(WorkspaceRoot, GitSummary)> {
        self.git.iter().map(|(root, summary)| (*root, summary.clone())).collect()
    }

    pub(crate) fn runs_snapshot(&self) -> Vec<TargetRun> {
        let mut runs: Vec<TargetRun> = self.runs.values().cloned().collect();
        runs.sort_by_key(|r| r.started_ms);
        runs
    }
}

/// `(base, ahead, behind)` of HEAD against `base`, when it resolves.
pub(crate) async fn ahead_behind(git: &Git, repo: &Path, base: &str) -> Option<(String, u32, u32)> {
    match git.ahead_behind(repo, base).await {
        Ok(Some((ahead, behind))) => Some((base.to_owned(), ahead, behind)),
        Ok(None) => None,
        Err(err) => {
            log::debug!("comparing {} with {base}: {err}", repo.display());
            None
        }
    }
}

/// The summary rows and headers show, from a full status.
fn summarize(status: Option<&Status>, compared: Option<(String, u32, u32)>) -> GitSummary {
    let Some(status) = status else { return GitSummary::default() };
    let (compared_to, ahead, behind) = match (&status.upstream, compared) {
        (Some(upstream), _) => (Some(upstream.clone()), status.ahead, status.behind),
        (None, Some((base, ahead, behind))) => (Some(base), ahead, behind),
        (None, None) => (None, 0, 0),
    };
    GitSummary {
        repo: true,
        branch: status.branch.clone(),
        head: status.head.as_ref().map(|h| h.chars().take(8).collect()),
        compared_to,
        ahead,
        behind,
        changed: status.files.len() as u32,
        additions: status.files.iter().map(|f| f.additions).sum(),
        deletions: status.files.iter().map(|f| f.deletions).sum(),
        conflicts: status.conflicted.len() as u32,
    }
}

impl Orchestrator {
    // ── Commands ─────────────────────────────────────────────────────────

    pub(crate) fn workspace_command(&mut self, command: WorkspaceCommand) -> Outcome {
        match command {
            WorkspaceCommand::ReadFile { root, path, max_bytes } => self.blocking(root, move |dir| {
                fs::read_file(&dir, &path, max_bytes).map(|file| WorkspaceReply::File(Box::new(file)))
            }),
            WorkspaceCommand::ListDir { root, path } => {
                self.blocking(root, move |dir| fs::list_dir(&dir, &path).map(WorkspaceReply::Dir))
            }
            WorkspaceCommand::MemoryFiles { root } => self.blocking(root, move |dir| {
                fs::memory_files(&dir, dirs::home_dir().as_deref()).map(WorkspaceReply::Memory)
            }),
            WorkspaceCommand::Status { root } => self.read_status(root),
            WorkspaceCommand::SetConfig { project, config } => self.set_project_config(project, *config).into(),
            WorkspaceCommand::Import { project } => self.import_project(project),
            WorkspaceCommand::Run { thread, target, cols, rows } => self.run_target(thread, target, cols, rows).into(),
            WorkspaceCommand::Stop { thread, target } => {
                if !self.stop_target(thread, &target) {
                    return Outcome::Ready(Err(format!("“{target}” is not running.")));
                }
                Outcome::Ready(Ok(Reply::Done))
            }
        }
    }

    /// The folder a root names.
    pub(crate) fn root_dir(&self, root: WorkspaceRoot) -> Result<PathBuf, String> {
        match root {
            WorkspaceRoot::Project(id) => {
                let project = self.store.project(id).ok_or("That project no longer exists.")?;
                Ok(PathBuf::from(&project.path))
            }
            WorkspaceRoot::Thread(id) => {
                let thread = self.store.thread(id).ok_or("That thread no longer exists.")?;
                Ok(self.workspace_dir(thread))
            }
        }
    }

    /// Runs `work` on the root's folder on the blocking pool.
    fn blocking(
        &self,
        root: WorkspaceRoot,
        work: impl FnOnce(PathBuf) -> Result<WorkspaceReply, String> + Send + 'static,
    ) -> Outcome {
        let dir = match self.root_dir(root) {
            Ok(dir) => dir,
            Err(e) => return Outcome::Ready(Err(e)),
        };
        Outcome::Deferred(Box::pin(async move {
            tokio::task::spawn_blocking(move || work(dir))
                .await
                .map_err(|e| format!("Couldn’t read the workspace: {e}"))?
                .map(Reply::Workspace)
        }))
    }

    fn read_status(&self, root: WorkspaceRoot) -> Outcome {
        let dir = match self.root_dir(root) {
            Ok(dir) => dir,
            Err(e) => return Outcome::Ready(Err(e)),
        };
        if let Some(summary) = self.workspaces.git.get(&root).or_else(|| self.workspaces.by_folder.get(&dir)) {
            return Outcome::Ready(Ok(Reply::Workspace(WorkspaceReply::Status(Box::new(summary.clone())))));
        }
        let base = self.base_of(root);
        let git = self.git.clone();
        Outcome::Deferred(Box::pin(async move {
            let summary = match git.status(&dir).await {
                Ok(status) => {
                    let compared = match (&status.upstream, &base) {
                        (None, Some(base)) if status.branch.as_deref() != Some(base.as_str()) => {
                            ahead_behind(&git, &dir, base).await
                        }
                        _ => None,
                    };
                    summarize(Some(&status), compared)
                }
                Err(_) => GitSummary::default(),
            };
            Ok(Reply::Workspace(WorkspaceReply::Status(Box::new(summary))))
        }))
    }

    fn base_of(&self, root: WorkspaceRoot) -> Option<String> {
        let project = match root {
            WorkspaceRoot::Project(id) => id,
            WorkspaceRoot::Thread(id) => self.store.thread(id)?.project?,
        };
        self.store.project(project)?.base_branch.clone()
    }

    // ── Configuration ────────────────────────────────────────────────────

    /// The project's configuration, or the defaults.
    pub(crate) fn project_config(&self, project: ProjectId) -> ProjectConfig {
        self.store.project_config(project).cloned().unwrap_or_default()
    }

    fn set_project_config(&mut self, project: ProjectId, config: ProjectConfig) -> CommandResult {
        if self.store.project(project).is_none() {
            return Err("That project no longer exists.".into());
        }
        let config = config.sanitized();
        let archived = config.archived;
        self.emit(Event::ProjectConfigChanged { project, config: Box::new(config) });
        if archived {
            self.unwatch_project(project);
        } else {
            self.watch_project(project);
        }
        Ok(Reply::Done)
    }

    fn import_project(&self, id: ProjectId) -> Outcome {
        let Some(project) = self.store.project(id).cloned() else {
            return Outcome::Ready(Err("That project no longer exists.".into()));
        };
        let config = self.project_config(id);
        Outcome::Deferred(Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let (project, config, sources) = import::import(Path::new(&project.path), &project, &config);
                Reply::Workspace(WorkspaceReply::Imported { project: Box::new(project), config: Box::new(config), sources })
            })
            .await
            .map_err(|e| format!("Couldn’t read the project’s files: {e}"))
        }))
    }

    /// A project was added: watch its checkout, and take its configuration
    /// from a checked-in `orbit.toml`.
    pub(crate) fn project_added(&mut self, id: ProjectId) {
        self.watch_project(id);
        let Some(project) = self.store.project(id).cloned() else { return };
        if self.store.project_config(id).is_some() {
            return;
        }
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let root = PathBuf::from(&project.path);
            if !root.join("orbit.toml").is_file() {
                return;
            }
            let (imported, config, _) = import::import(&root, &project, &ProjectConfig::default());
            let work = move |this: &mut Orchestrator| {
                let Some(current) = this.store.project(id).cloned() else { return };
                if this.store.project_config(id).is_some() {
                    return;
                }
                // The base branch detected meanwhile stays unless the file names one.
                let base_branch = if imported.base_branch != project.base_branch { imported.base_branch } else { current.base_branch.clone() };
                let updated = Project { name: imported.name, scripts: imported.scripts, base_branch, ..current.clone() };
                if updated != current {
                    this.emit(Event::ProjectUpserted(Box::new(updated)));
                }
                let _ = this.set_project_config(id, config);
            };
            let _ = crossfire::BlockingTxTrait::send(&tx, Internal::Run(Box::new(work)));
        });
    }

    /// Watches every active project's checkout (once, at startup).
    pub(crate) fn watch_projects(&mut self) {
        let ids: Vec<ProjectId> = self
            .store
            .projects()
            .iter()
            .filter(|p| !self.store.project_config(p.id).is_some_and(|c| c.archived))
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.watch_project(id);
        }
    }

    /// The environment of anything the project runs in a thread's
    /// workspace: the worktree's ports and paths, then the project's own.
    pub(crate) fn workspace_env(&self, thread: &Thread) -> Vec<(String, String)> {
        let mut env = Vec::new();
        let project = thread.project.and_then(|p| self.store.project(p));
        if let Some(port) = thread.workspace.port_base {
            let count = project.and_then(|p| self.store.project_config(p.id)).map_or(0, |c| c.ports);
            env.push(("PORT".into(), port.to_string()));
            env.push(("ORBIT_PORT_BASE".into(), port.to_string()));
            env.push(("ORBIT_PORT_COUNT".into(), if count == 0 { 10 } else { count }.to_string()));
        }
        if let Some(worktree) = &thread.workspace.worktree {
            env.push(("ORBIT_WORKTREE".into(), worktree.clone()));
        }
        if let Some(branch) = &thread.workspace.branch {
            env.push(("ORBIT_BRANCH".into(), branch.clone()));
        }
        if let Some(project) = project {
            env.push(("ORBIT_ROOT_PATH".into(), project.path.clone()));
            if let Some(config) = self.store.project_config(project.id) {
                env.extend(config.env.iter().map(|v| (v.name.clone(), v.value.clone())));
            }
        }
        env.push(("ORBIT_THREAD".into(), thread.id.to_string()));
        env
    }

    // ── Run targets ──────────────────────────────────────────────────────

    fn run_target(&mut self, thread_id: ThreadId, name: String, cols: u16, rows: u16) -> CommandResult {
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        let project = thread.project.ok_or("This thread has no project, so it has no run targets.")?;
        let config = self.project_config(project);
        let target = config.target(&name).cloned().ok_or_else(|| format!("The project has no target named “{name}”."))?;
        if thread.status == ThreadStatus::Starting && thread.workspace.worktree.is_none() {
            return Err("The thread’s worktree is still being set up.".into());
        }
        // Running again restarts.
        self.stop_target(thread_id, &name);
        let cwd = self.workspace_dir(&thread);
        let mut shell = super::cli::command_shell(&target.command);
        for (key, value) in self.workspace_env(&thread) {
            shell = shell.env(key, value);
        }
        let size = TermSize::new(if cols == 0 { 160 } else { cols.max(2) }, if rows == 0 { 40 } else { rows.max(1) });
        let url = expand_port(&target.url, thread.workspace.port_base);
        let now = now_ms();
        let state = match self.terminals.open(&cwd, size, Some(shell)) {
            Ok(term) => {
                // Kept like any command terminal: readable later, answered
                // when it asks for the cursor with no screen attached.
                self.cli.record(term, target.command.clone(), cwd.display().to_string());
                self.workspaces.run_terms.insert(term, (thread_id, name.clone()));
                TargetRun { thread: thread_id, target: name.clone(), term: Some(term), status: TargetStatus::Running, url, started_ms: now, ended_ms: None }
            }
            Err(err) => TargetRun {
                thread: thread_id,
                target: name.clone(),
                term: None,
                status: TargetStatus::Failed { message: format!("Couldn’t start “{}”: {err}", target.command) },
                url,
                started_ms: now,
                ended_ms: Some(now),
            },
        };
        self.workspaces.runs.insert((thread_id, name), state.clone());
        self.emit(Event::RunChanged(Box::new(state.clone())));
        Ok(Reply::Workspace(WorkspaceReply::Run(Box::new(state))))
    }

    /// Stops a running target; `false` when it was not running.
    fn stop_target(&mut self, thread: ThreadId, name: &str) -> bool {
        let key = (thread, name.to_owned());
        let Some(state) = self.workspaces.runs.get_mut(&key).filter(|s| s.status.is_running()) else { return false };
        state.status = TargetStatus::Stopped;
        state.ended_ms = Some(now_ms());
        let state = state.clone();
        if let Some(term) = state.term {
            self.workspaces.run_terms.remove(&term);
            self.terminals.close(term);
            self.status_terminal_closed(term);
        }
        self.emit(Event::RunChanged(Box::new(state)));
        true
    }

    /// Stops every target of a thread (it was deleted).
    pub(crate) fn stop_thread_runs(&mut self, thread: ThreadId) {
        let names: Vec<String> = self.workspaces.runs.keys().filter(|(t, _)| *t == thread).map(|(_, n)| n.clone()).collect();
        for name in names {
            self.stop_target(thread, &name);
            self.workspaces.runs.remove(&(thread, name));
        }
    }

    /// A terminal ended: if a target ran in it, it exited on its own.
    pub(crate) fn run_exited(&mut self, term: TermId, code: Option<i32>) {
        let Some(key) = self.workspaces.run_terms.remove(&term) else { return };
        let Some(state) = self.workspaces.runs.get_mut(&key) else { return };
        if !state.status.is_running() {
            return;
        }
        state.status = TargetStatus::Exited { code };
        state.ended_ms = Some(now_ms());
        let state = state.clone();
        self.emit(Event::RunChanged(Box::new(state)));
    }

    // ── Git status ───────────────────────────────────────────────────────

    /// Every root whose folder is `folder` among the watched ones.
    fn roots_at(&self, folder: &Path) -> Vec<WorkspaceRoot> {
        let Some(watch) = self.review.watchers.get(folder) else { return Vec::new() };
        let mut roots: Vec<WorkspaceRoot> = watch.projects.iter().map(|p| WorkspaceRoot::Project(*p)).collect();
        roots.extend(watch.threads.iter().map(|t| WorkspaceRoot::Thread(*t)));
        roots
    }

    /// A watched folder's status changed (or was read the first time).
    pub(crate) fn git_status_changed(&mut self, folder: &Path, status: Option<&Status>, compared: Option<(String, u32, u32)>) {
        let summary = summarize(status, compared);
        // New files show in the tree and in quick open on the next query.
        self.files.mark_stale(folder);
        self.workspaces.by_folder.insert(folder.to_owned(), summary);
        self.replay_git_status(folder);
    }

    /// Sends the folder's last status to each of its roots that has not
    /// seen it.
    pub(crate) fn replay_git_status(&mut self, folder: &Path) {
        let Some(summary) = self.workspaces.by_folder.get(folder).cloned() else { return };
        for root in self.roots_at(folder) {
            if self.workspaces.git.get(&root) != Some(&summary) {
                self.workspaces.git.insert(root, summary.clone());
                self.emit(Event::GitStatus { root, status: Box::new(summary.clone()) });
            }
        }
    }

    /// A project went away: forget what was known about it.
    pub(crate) fn project_removed(&mut self, id: ProjectId) {
        self.unwatch_project(id);
        self.workspaces.git.remove(&WorkspaceRoot::Project(id));
    }

    /// A thread went away.
    pub(crate) fn thread_removed(&mut self, id: ThreadId) {
        self.stop_thread_runs(id);
        self.workspaces.git.remove(&WorkspaceRoot::Thread(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_compare_against_the_upstream_else_the_base() {
        let status = Status {
            branch: Some("topic".into()),
            head: Some("0123456789abcdef".into()),
            upstream: None,
            files: vec![
                FileChange { path: "a".into(), kind: ChangeKind::Modified, additions: 3, deletions: 1 },
                FileChange { path: "b".into(), kind: ChangeKind::Added, additions: 2, deletions: 0 },
            ],
            conflicted: vec!["a".into()],
            ..Default::default()
        };
        let summary = summarize(Some(&status), Some(("main".into(), 4, 2)));
        assert_eq!(summary.compared_to.as_deref(), Some("main"));
        assert_eq!((summary.ahead, summary.behind), (4, 2));
        assert_eq!((summary.changed, summary.additions, summary.deletions, summary.conflicts), (2, 5, 1, 1));
        assert_eq!(summary.head.as_deref(), Some("01234567"));
        let tracked = Status { upstream: Some("origin/topic".into()), ahead: 1, ..status };
        let summary = summarize(Some(&tracked), Some(("main".into(), 4, 2)));
        assert_eq!((summary.compared_to.as_deref(), summary.ahead, summary.behind), (Some("origin/topic"), 1, 0));
        assert!(!summarize(None, None).repo);
    }
}
