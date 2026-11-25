//! Hosts and trackers in the engine: [`Command::Forge`].
//!
//! Reads (lists, details, ship state) run as deferred futures against the
//! cached [`Forge`] service, so the engine loop never waits on a network
//! call. Commands that end in a thread (check out a pull request, fix its
//! checks, start from an issue) fetch what they need first, then create or
//! pick the thread on the engine thread through [`Internal::Run`].

use std::path::{Path, PathBuf};

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Deferred, Internal, Orchestrator, Outcome};
use crate::forge::{Forge, ForgeConfig, MergeRequest, prompt};
use crate::git::{Git, NewWorktree};

/// The integration settings' file in the data folder.
const INTEGRATIONS_FILE: &str = "integrations.bin";
const INTEGRATIONS_VERSION: u8 = 1;
/// Failing checks whose logs go into one fix prompt.
const FIX_LOGS: usize = 4;

/// The forge's engine-side state.
#[derive(Default)]
pub(crate) struct ForgeState {
    pub service: Forge,
    /// Loaded on first use.
    integrations: Option<IntegrationSettings>,
}

/// A worktree for a thread on a given branch, prepared by fetching first.
struct BranchPlan {
    branch: String,
    /// What a new branch starts from.
    base: Option<String>,
    /// `git` argument lists to run in the project first (fetches).
    prepare: Vec<Vec<String>>,
    /// Set the new branch's upstream to this (`origin/feature`).
    upstream: Option<String>,
    /// The branch is checked out in the project folder itself: the thread
    /// works there, with no worktree.
    in_project: bool,
}

impl Orchestrator {
    pub(crate) fn forge(&mut self, command: ForgeCommand) -> Outcome {
        match self.forge_inner(command) {
            Ok(outcome) => outcome,
            Err(message) => Outcome::Ready(Err(message)),
        }
    }

    fn forge_inner(&mut self, command: ForgeCommand) -> Result<Outcome, String> {
        let config = self.forge_config();
        let (service, git) = (self.forge.service.clone(), self.git.clone());
        let deferred = |future: Deferred| Ok(Outcome::Deferred(future));
        match command {
            ForgeCommand::Info { project } => {
                let path = self.project_path(project)?;
                deferred(Box::pin(async move {
                    Ok(Reply::Forge(ForgeReply::Info(Box::new(service.info(&config, &git, &path).await))))
                }))
            }
            ForgeCommand::PullRequests { project, filter, refresh } => {
                let path = self.project_path(project)?;
                deferred(Box::pin(async move {
                    let list = service.pull_requests(&config, &git, &path, filter, refresh).await.map_err(|e| e.to_string())?;
                    Ok(Reply::Forge(ForgeReply::PullRequests(list)))
                }))
            }
            ForgeCommand::PullRequest { project, number, refresh } => {
                let path = self.project_path(project)?;
                deferred(Box::pin(async move {
                    let fresh = service.pull_request(&config, &git, &path, number, refresh).await.map_err(|e| e.to_string())?;
                    Ok(Reply::Forge(ForgeReply::PullRequest(Box::new(fresh.value))))
                }))
            }
            ForgeCommand::Merge { project, number, method, auto, delete_branch, head_sha } => {
                let path = self.project_path(project)?;
                deferred(Box::pin(async move {
                    // The cached detail names the branch to delete.
                    let detail = service.pull_request(&config, &git, &path, number, false).await.ok().map(|f| f.value);
                    let request = MergeRequest {
                        number,
                        method,
                        auto,
                        delete_branch,
                        head_sha,
                        head: detail.as_ref().map(|d| d.pr.head.clone()).unwrap_or_default(),
                        cross_repo: detail.as_ref().is_none_or(|d| d.pr.cross_repo),
                    };
                    service.merge(&config, &git, &path, request).await.map_err(|e| e.to_string())?;
                    Ok(Reply::Done)
                }))
            }
            ForgeCommand::DisableAutoMerge { project, number } => {
                let path = self.project_path(project)?;
                deferred(Box::pin(async move {
                    service.disable_auto_merge(&config, &git, &path, number).await.map_err(|e| e.to_string())?;
                    Ok(Reply::Done)
                }))
            }
            ForgeCommand::Checkout { project, number, config: thread_config } => {
                let path = self.project_path(project)?;
                let tx = self.internal_tx.clone();
                deferred(Box::pin(async move {
                    let fresh = service.pull_request(&config, &git, &path, number, false).await.map_err(|e| e.to_string())?;
                    let pr = fresh.value.pr;
                    let plan = checkout_plan(&service, &config, &git, &path, &pr).await?;
                    let title = format!("{}{} {}", sigil(&service, &git, &path).await, pr.number, pr.title);
                    on_engine(&tx, move |o| o.thread_on_branch(project, thread_config, plan, title, None)).await
                }))
            }
            ForgeCommand::FixChecks { project, number, thread, config: thread_config } => {
                let path = self.project_path(project)?;
                let tx = self.internal_tx.clone();
                deferred(Box::pin(async move {
                    let detail = service.pull_request(&config, &git, &path, number, true).await.map_err(|e| e.to_string())?.value;
                    if !detail.checks.iter().any(|c| c.state == CheckState::Failed) {
                        return Err(format!("No checks are failing on {}{number}.", sigil(&service, &git, &path).await));
                    }
                    let logs = service.failing_logs(&config, &git, &path, &detail.checks, FIX_LOGS).await.map_err(|e| e.to_string())?;
                    let forge = service.locate(&git, &path).await.map(|l| l.repo.kind).unwrap_or(ForgeKind::GitHub);
                    let text = prompt::fix_checks_prompt(forge, &detail.pr, &logs);
                    let pr = detail.pr;
                    let plan = checkout_plan(&service, &config, &git, &path, &pr).await?;
                    let title = format!("{}{} {}", forge.review_sigil(), pr.number, pr.title);
                    on_engine(&tx, move |o| {
                        let target = thread.filter(|t| o.store.thread(*t).is_some_and(|t| !t.archived));
                        match target {
                            Some(id) => o.send(id, text, Vec::new(), false).map(|_| Reply::Thread(id)),
                            None => o.thread_on_branch(project, thread_config, plan, title, Some(text)),
                        }
                    })
                    .await
                }))
            }
            ForgeCommand::Issues { project, tracker, query, refresh } => {
                let path = project.map(|p| self.project_path(p)).transpose()?;
                deferred(Box::pin(async move {
                    let list = service
                        .issues(&config, &git, path.as_deref(), tracker, &query, refresh)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Reply::Forge(ForgeReply::Issues(list)))
                }))
            }
            ForgeCommand::Issue { project, tracker, key, refresh } => {
                let path = project.map(|p| self.project_path(p)).transpose()?;
                deferred(Box::pin(async move {
                    let detail =
                        service.issue(&config, &git, path.as_deref(), tracker, &key, refresh).await.map_err(|e| e.to_string())?;
                    Ok(Reply::Forge(ForgeReply::Issue(Box::new(detail))))
                }))
            }
            ForgeCommand::StartFromIssue { project, tracker, key, config: thread_config, worktree } => {
                let path = self.project_path(project)?;
                let base = self.store.project(project).and_then(|p| p.base_branch.clone());
                let tx = self.internal_tx.clone();
                deferred(Box::pin(async move {
                    let detail = service.issue(&config, &git, Some(&path), tracker, &key, false).await.map_err(|e| e.to_string())?;
                    let text = prompt::issue_prompt(&detail);
                    let title = format!("{} {}", detail.issue.key, detail.issue.title);
                    let plan = worktree.then(|| BranchPlan {
                        branch: prompt::issue_branch(&detail.issue),
                        base,
                        prepare: Vec::new(),
                        upstream: None,
                        in_project: false,
                    });
                    on_engine(&tx, move |o| match plan {
                        Some(plan) => o.thread_on_branch(project, thread_config, Some(plan), title, Some(text)),
                        None => o.new_forge_thread(project, thread_config, None, title, Some(text)),
                    })
                    .await
                }))
            }
            ForgeCommand::Ship { thread, refresh } => {
                let t = self.store.thread(thread).ok_or("That thread no longer exists.")?;
                let Some(repo) = self.repo_of(t) else {
                    return Ok(Outcome::Ready(Ok(Reply::Forge(ForgeReply::Ship(Box::default())))));
                };
                let default = t.project.and_then(|p| self.store.project(p)).and_then(|p| p.base_branch.clone());
                deferred(Box::pin(async move {
                    let state = ship_state(&service, &config, &git, &repo, default.as_deref(), refresh).await;
                    Ok(Reply::Forge(ForgeReply::Ship(Box::new(state))))
                }))
            }
            ForgeCommand::DraftInput { thread, pull_request } => {
                let repo = self.repo_for(thread)?;
                let t = self.store.thread(thread).ok_or("That thread no longer exists.")?;
                let default = t.project.and_then(|p| self.store.project(p)).and_then(|p| p.base_branch.clone());
                deferred(Box::pin(async move {
                    let input = if pull_request {
                        pr_input(&git, &repo, default.as_deref()).await
                    } else {
                        commit_input(&git, &repo).await
                    };
                    input.map(Reply::Text)
                }))
            }
            ForgeCommand::Integrations => Ok(Outcome::Ready(Ok(Reply::Forge(ForgeReply::Integrations(self.integrations().clone()))))),
            ForgeCommand::SetIntegrations { settings } => Ok(Outcome::Ready(self.set_integrations(settings))),
        }
    }

    /// `Commit` with push, `Push` and `CreatePullRequest`, with the repository's
    /// cached host data dropped once the push lands.
    pub(crate) fn forge_after_push(&mut self, command: Command) -> Outcome {
        let thread = match &command {
            Command::Commit { thread, .. } | Command::Push { thread } | Command::CreatePullRequest { thread, .. } => *thread,
            _ => return self.git_command(command),
        };
        let repo = self.repo_for(thread).ok();
        let service = self.forge.service.clone();
        match self.git_command(command) {
            Outcome::Deferred(future) => Outcome::Deferred(Box::pin(async move {
                let result = future.await;
                if let Some(repo) = repo {
                    service.invalidate_path(&repo);
                }
                result
            })),
            ready => ready,
        }
    }

    // ── Threads ──────────────────────────────────────────────────────────

    /// The thread already on `plan.branch` in the project, or a new one on
    /// it (in the project folder when that is where the branch is checked
    /// out, else on its own worktree). Sends `prompt` to it when given.
    fn thread_on_branch(
        &mut self,
        project: ProjectId,
        config: ThreadConfig,
        plan: Option<BranchPlan>,
        title: String,
        prompt: Option<String>,
    ) -> CommandResult {
        let existing = plan.as_ref().and_then(|plan| {
            self.store
                .threads()
                .filter(|t| t.project == Some(project) && !t.archived && t.workspace.branch.as_deref() == Some(plan.branch.as_str()))
                .max_by_key(|t| t.updated_ms)
                .map(|t| t.id)
        });
        match existing {
            Some(id) => {
                if let Some(text) = prompt {
                    self.send(id, text, Vec::new(), false)?;
                }
                Ok(Reply::Thread(id))
            }
            None => self.new_forge_thread(project, config, plan, title, prompt),
        }
    }

    fn new_forge_thread(
        &mut self,
        project: ProjectId,
        config: ThreadConfig,
        plan: Option<BranchPlan>,
        title: String,
        prompt: Option<String>,
    ) -> CommandResult {
        let record = self.store.project(project).cloned().ok_or("That project no longer exists.")?;
        let Reply::Thread(id) = self.create_thread(Some(project), config, false)? else {
            return Err("Couldn’t create the thread.".into());
        };
        let checked_out_here = plan.as_ref().is_some_and(|p| p.in_project);
        self.update_thread(id, |t| {
            t.title = title;
            if plan.is_some() && !checked_out_here {
                // Messages queue until the worktree is ready.
                t.status = ThreadStatus::Starting;
            }
            if checked_out_here {
                t.workspace.branch = plan.as_ref().map(|p| p.branch.clone());
            }
        });
        if let Some(plan) = plan.filter(|_| !checked_out_here) {
            self.spawn_branch_worktree(id, record, plan);
        }
        if let Some(text) = prompt {
            self.send(id, text, Vec::new(), false)?;
        }
        Ok(Reply::Thread(id))
    }

    fn spawn_branch_worktree(&mut self, thread: ThreadId, project: Project, plan: BranchPlan) {
        let (git, tx) = (self.git.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let root = PathBuf::from(&project.path);
            for args in &plan.prepare {
                if let Err(e) = git.git(&root, args).await {
                    let message = format!("Couldn’t fetch the branch: {e}");
                    let _ = tx.send(Internal::WorktreeReady { thread, result: Err(message), setup_log: None });
                    return;
                }
            }
            let mut log = String::new();
            let mut on_output = |_: crate::git::OutputStream, line: &str| {
                if log.len() < 16 * 1024 {
                    log.push_str(line);
                    log.push('\n');
                }
            };
            let spec = NewWorktree {
                project_root: root.clone(),
                thread,
                branch: plan.branch.clone(),
                base: plan.base.clone(),
                path: None,
                setup: project.scripts.setup.clone(),
            };
            let (result, setup_log) = match git.create_worktree(spec, &mut on_output).await {
                Ok(info) => {
                    if let Some(upstream) = &plan.upstream {
                        let _ = git.git(&info.record.path, ["branch", "--set-upstream-to", upstream.as_str()]).await;
                    }
                    let failed = info.setup.as_ref().is_some_and(|s| s.code != Some(0));
                    let setup_log = info.setup.as_ref().map(|s| s.tail.clone()).filter(|_| failed);
                    let workspace = Workspace {
                        branch: Some(info.record.branch.clone()),
                        worktree: Some(info.record.path.display().to_string()),
                        port_base: Some(info.record.port_base),
                    };
                    (Ok(workspace), setup_log)
                }
                Err(err) => (Err(format!("Couldn’t create the worktree: {err}")), None),
            };
            let _ = tx.send(Internal::WorktreeReady { thread, result, setup_log });
        });
    }

    // ── Settings ─────────────────────────────────────────────────────────

    fn integrations(&mut self) -> &IntegrationSettings {
        if self.forge.integrations.is_none() {
            self.forge.integrations = Some(load_integrations(&self.store.dir().join(INTEGRATIONS_FILE)));
        }
        self.forge.integrations.as_ref().expect("loaded above")
    }

    fn set_integrations(&mut self, settings: IntegrationSettings) -> CommandResult {
        let settings = settings.sanitized();
        if *self.integrations() == settings {
            return Ok(Reply::Done);
        }
        save_integrations(&self.store.dir().join(INTEGRATIONS_FILE), &settings)
            .map_err(|e| format!("Couldn’t save the integration settings: {e}"))?;
        self.forge.integrations = Some(settings);
        self.forge.service.forget_trackers();
        Ok(Reply::Done)
    }

    pub(crate) fn forge_config(&mut self) -> ForgeConfig {
        let integrations = self.integrations().clone();
        ForgeConfig::new(self.store.settings(), &integrations)
    }

    fn project_path(&self, project: ProjectId) -> Result<PathBuf, String> {
        self.store.project(project).map(|p| PathBuf::from(&p.path)).ok_or_else(|| "That project no longer exists.".into())
    }
}

/// Commands after which a repository's host data is stale.
pub(crate) fn pushes(command: &Command) -> bool {
    matches!(command, Command::Commit { push: true, .. } | Command::Push { .. } | Command::CreatePullRequest { .. })
}

/// Runs `work` on the engine thread and waits for its result.
async fn on_engine(
    tx: &crossfire::MTx<crossfire::mpsc::List<Internal>>,
    work: impl FnOnce(&mut Orchestrator) -> CommandResult + Send + 'static,
) -> CommandResult {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let run = Internal::Run(Box::new(move |o: &mut Orchestrator| {
        let _ = reply.send(work(o));
    }));
    tx.send(run).map_err(|_| "The engine stopped.".to_owned())?;
    rx.await.unwrap_or_else(|_| Err("The engine stopped.".into()))
}

async fn sigil(service: &Forge, git: &Git, path: &Path) -> &'static str {
    service.locate(git, path).await.map(|l| l.repo.kind.review_sigil()).unwrap_or("#")
}

/// How to get a pull request's branch into a worktree: the branch itself
/// (tracking the remote) when it lives in this repository, else its head
/// ref on a local `pr/<n>` branch.
async fn checkout_plan(service: &Forge, config: &ForgeConfig, git: &Git, path: &Path, pr: &PullRequest) -> Result<Option<BranchPlan>, String> {
    let at = service.locate(git, path).await.map_err(|e| e.to_string())?;
    let remote = at.repo.remote.clone();
    if !pr.cross_repo && prompt::valid_branch(&pr.head) {
        if git.current_branch(path).await.ok().flatten().as_deref() == Some(pr.head.as_str()) {
            return Ok(Some(BranchPlan { branch: pr.head.clone(), base: None, prepare: Vec::new(), upstream: None, in_project: true }));
        }
        let tracking = format!("refs/remotes/{remote}/{}", pr.head);
        let fetch = vec![
            "fetch".into(),
            "--no-tags".into(),
            remote.clone(),
            format!("+refs/heads/{}:{tracking}", pr.head),
        ];
        let upstream = format!("{remote}/{}", pr.head);
        return Ok(Some(BranchPlan { branch: pr.head.clone(), base: Some(upstream.clone()), prepare: vec![fetch], upstream: Some(upstream), in_project: false }));
    }
    let pr_ref = service.pr_ref(config, &at, pr.number).map_err(|e| e.to_string())?;
    let local = format!("refs/orbit/pr/{}", pr.number);
    let fetch = vec!["fetch".into(), "--no-tags".into(), remote, format!("+{pr_ref}:{local}")];
    let prefix = if at.repo.kind == ForgeKind::GitLab { "mr" } else { "pr" };
    Ok(Some(BranchPlan { branch: format!("{prefix}/{}", pr.number), base: Some(local), prepare: vec![fetch], upstream: None, in_project: false }))
}

// ── Ship state ───────────────────────────────────────────────────────────

async fn ship_state(service: &Forge, config: &ForgeConfig, git: &Git, repo: &Path, default: Option<&str>, refresh: bool) -> ShipState {
    let Ok(status) = git.status(repo).await else {
        return ShipState { stage: ShipStage::NoRepo, ..Default::default() };
    };
    let mut state = ShipState {
        branch: status.branch.clone(),
        files: status.files.len() as u32,
        additions: status.files.iter().map(|f| f.additions).sum(),
        deletions: status.files.iter().map(|f| f.deletions).sum(),
        ahead: status.ahead,
        behind: status.behind,
        ..Default::default()
    };
    state.on_default = matches!((status.branch.as_deref(), default), (Some(b), Some(d)) if b == d);
    if status.upstream.is_none()
        && let (Some(branch), Some(default)) = (status.branch.as_deref(), default)
        && branch != default
    {
        // Never pushed: everything since the default branch is unpushed.
        let range = format!("{default}..HEAD");
        if let Ok(out) = git.git(repo, ["rev-list", "--count", range.as_str()]).await {
            state.ahead = String::from_utf8_lossy(&out).trim().parse().unwrap_or(0);
        }
    }
    match service.locate(git, repo).await {
        Ok(at) => state.forge = Some(at.repo.kind),
        Err(e) => state.note = Some(e.to_string()),
    }
    if let (Some(branch), Some(_), false) = (status.branch.as_deref(), state.forge, state.on_default) {
        match service.branch_pull_request(config, git, repo, branch, refresh).await {
            Ok(pr) => state.pr = pr,
            Err(e) => state.note = Some(e.to_string()),
        }
    }
    if let Some(pr) = state.pr.as_ref().filter(|p| p.state.is_open())
        && let Ok(fresh) = service.pull_request(config, git, repo, pr.number, refresh).await
    {
        state.mergeable = fresh.value.mergeable;
        state.auto_merge = fresh.value.auto_merge.is_some();
        state.pr = Some(fresh.value.pr);
    }
    state.stage = ship_stage(&state, status.upstream.is_some());
    state
}

/// The stage, from the most urgent next step backwards.
pub(crate) fn ship_stage(s: &ShipState, has_upstream: bool) -> ShipStage {
    if s.files > 0 {
        return ShipStage::Uncommitted;
    }
    if s.ahead > 0 {
        return ShipStage::Committed;
    }
    match &s.pr {
        Some(pr) if pr.state == PrState::Merged => ShipStage::Merged,
        Some(pr) if pr.state == PrState::Closed => ShipStage::Closed,
        Some(pr) => match pr.checks.overall() {
            Some(CheckState::Failed) => ShipStage::ChecksFailed,
            Some(CheckState::Running | CheckState::Queued) => ShipStage::ChecksRunning,
            _ if s.mergeable.can_merge() && pr.state != PrState::Draft => ShipStage::Mergeable,
            _ => ShipStage::Blocked,
        },
        None if has_upstream && !s.on_default => ShipStage::Pushed,
        None => ShipStage::Clean,
    }
}

// ── Draft inputs ─────────────────────────────────────────────────────────

async fn text(git: &Git, repo: &Path, args: &[&str]) -> String {
    git.git(repo, args).await.map(|out| String::from_utf8_lossy(&out).into_owned()).unwrap_or_default()
}

async fn commit_input(git: &Git, repo: &Path) -> Result<String, String> {
    let status = text(git, repo, &["status", "--short", "--untracked-files=all"]).await;
    if status.trim().is_empty() {
        return Err("There are no uncommitted changes to describe.".into());
    }
    let (diff, recent) =
        tokio::join!(text(git, repo, &["diff", "HEAD", "--no-ext-diff", "--no-color"]), text(git, repo, &["log", "-n", "8", "--format=%s"]));
    Ok(prompt::commit_input(&status, &diff, &recent))
}

async fn pr_input(git: &Git, repo: &Path, default: Option<&str>) -> Result<String, String> {
    let branch = git.current_branch(repo).await.ok().flatten().ok_or("Check out a branch first.")?;
    let base = default.unwrap_or("main");
    // Compare with the remote's copy of the base when there is one.
    let remote_base = format!("origin/{base}");
    let base_ref = if git.git(repo, ["rev-parse", "--verify", "--quiet", remote_base.as_str()]).await.is_ok() { remote_base } else { base.to_owned() };
    let merge_base = text(git, repo, &["merge-base", &base_ref, "HEAD"]).await.trim().to_owned();
    if merge_base.is_empty() {
        return Err(format!("This branch shares no history with {base}."));
    }
    let range = format!("{merge_base}..HEAD");
    let log = ["log", "-n", "50", "--format=- %s%n%b", &range];
    let stat = ["diff", "--stat", "--no-color", &range];
    let diff = ["diff", "--no-ext-diff", "--no-color", &range];
    let (commits, stat, diff) = tokio::join!(text(git, repo, &log), text(git, repo, &stat), text(git, repo, &diff));
    if commits.trim().is_empty() {
        return Err(format!("{branch} has no commits that aren’t on {base}."));
    }
    let template = pr_template(repo);
    Ok(prompt::pr_input(&branch, base, &commits, &stat, &diff, template.as_deref()))
}

/// The repository's pull (or merge) request template, if it has one.
fn pr_template(repo: &Path) -> Option<String> {
    const CANDIDATES: &[&str] = &[
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "docs/pull_request_template.md",
        "pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
        ".gitlab/merge_request_templates/Default.md",
    ];
    let root = std::iter::successors(Some(repo), |p| p.parent()).find(|p| p.join(".git").exists())?;
    CANDIDATES.iter().find_map(|c| std::fs::read_to_string(root.join(c)).ok()).filter(|t| t.len() < 20_000)
}

// ── Integration settings file ────────────────────────────────────────────

fn load_integrations(path: &Path) -> IntegrationSettings {
    let Ok(bytes) = std::fs::read(path) else { return IntegrationSettings::default() };
    match bytes.split_first() {
        Some((&INTEGRATIONS_VERSION, rest)) => bitcode::decode(rest).unwrap_or_else(|e| {
            log::warn!("{}: unreadable ({e}); starting empty", path.display());
            IntegrationSettings::default()
        }),
        _ => IntegrationSettings::default(),
    }
}

fn save_integrations(path: &Path, settings: &IntegrationSettings) -> std::io::Result<()> {
    let mut bytes = vec![INTEGRATIONS_VERSION];
    bytes.extend(bitcode::encode(settings));
    let tmp = path.with_extension("bin.tmp");
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Tokens: readable by the user only.
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: PrState, checks: ChecksSummary) -> PullRequest {
        PullRequest { number: 1, state, checks, ..Default::default() }
    }

    #[test]
    fn the_ship_stage_names_the_next_step() {
        let mut s = ShipState { files: 2, ..Default::default() };
        assert_eq!(ship_stage(&s, true), ShipStage::Uncommitted);
        s.files = 0;
        s.ahead = 1;
        assert_eq!(ship_stage(&s, true), ShipStage::Committed);
        s.ahead = 0;
        assert_eq!(ship_stage(&s, true), ShipStage::Pushed);
        assert_eq!(ship_stage(&s, false), ShipStage::Clean);
        s.pr = Some(pr(PrState::Open, ChecksSummary { pending: 1, passed: 2, ..Default::default() }));
        assert_eq!(ship_stage(&s, true), ShipStage::ChecksRunning);
        s.pr = Some(pr(PrState::Open, ChecksSummary { failed: 1, ..Default::default() }));
        assert_eq!(ship_stage(&s, true), ShipStage::ChecksFailed);
        s.pr = Some(pr(PrState::Open, ChecksSummary { passed: 3, ..Default::default() }));
        assert_eq!(ship_stage(&s, true), ShipStage::Blocked, "mergeability unknown");
        s.mergeable = Mergeability::Clean;
        assert_eq!(ship_stage(&s, true), ShipStage::Mergeable);
        s.pr = Some(pr(PrState::Draft, ChecksSummary::default()));
        assert_eq!(ship_stage(&s, true), ShipStage::Blocked, "drafts can't merge");
        s.pr = Some(pr(PrState::Merged, ChecksSummary::default()));
        assert_eq!(ship_stage(&s, true), ShipStage::Merged);
    }

    #[test]
    fn integration_settings_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INTEGRATIONS_FILE);
        assert_eq!(load_integrations(&path), IntegrationSettings::default());
        let settings = IntegrationSettings { linear_api_key: "lin_api_x".into(), jira_url: "https://a.atlassian.net".into(), ..Default::default() };
        save_integrations(&path, &settings).unwrap();
        assert_eq!(load_integrations(&path), settings);
        std::fs::write(&path, [9, 9, 9]).unwrap();
        assert_eq!(load_integrations(&path), IntegrationSettings::default(), "an unknown version starts empty");
    }
}
