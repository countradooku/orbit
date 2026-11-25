//! Agent teams and races on the engine thread.
//!
//! * **Teams.** An architect thread spawns workers (`TeamCommand::SpawnWorker`,
//!   usually through the Orbit MCP tools injected into its session). Each
//!   worker is a child thread in its own worktree off the architect's
//!   branch; it starts when its dependencies are done, a slot is free (per
//!   team and across Orbit) and no working teammate is editing its scope.
//!   Mail between members is stored and delivered at the recipient's next
//!   turn boundary as one structured message; urgent mail steers a working
//!   member at most once a minute. A finished worker's summary goes to its
//!   parent as mail. Failures and spend trip a circuit breaker that stops
//!   spawning until the user resumes it.
//! * **Races.** One prompt, several candidates, each in its own worktree off
//!   the same base. When a candidate finishes, its diff stats are taken and
//!   the project's test command runs in its worktree; a judge can name a
//!   winner; any two candidates can be compared and a hunk taken from one
//!   into the other; keeping the winner merges it and archives the rest.
//!
//! Everything here reacts to the engine's tick (500 ms) and to commands;
//! with no team and no running race the tick returns at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Deferred, Internal, Orchestrator, Outcome};
use crate::git::TreeId;
use crate::provider::McpServer;
use crate::teams::{plan, text};

/// Mail waits this long after the newest message, so a burst of results
/// reaches an idle recipient as one delivery.
const MAIL_QUIET_MS: u64 = 750;
/// Urgent mail steers a working member at most this often.
const STEER_EVERY: Duration = Duration::from_secs(60);
/// How often working members' changed files are compared.
const TOUCH_EVERY: Duration = Duration::from_secs(15);
const TEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Bytes of a failed test run's output kept.
const TEST_TAIL: usize = 4_000;
/// Run scripts that count as the project's test command, by name.
const TEST_SCRIPTS: &[&str] = &["test", "tests", "check"];

fn ready(result: CommandResult) -> Outcome {
    Outcome::Ready(result)
}

fn into_future(outcome: Outcome) -> Deferred {
    match outcome {
        Outcome::Ready(result) => Box::pin(async move { result }),
        Outcome::Deferred(future) => future,
    }
}

fn access_rank(a: Access) -> u8 {
    match a {
        Access::Readonly => 0,
        Access::Ask => 1,
        Access::AutoEdit => 2,
        Access::Full => 3,
    }
}

impl Orchestrator {
    pub(crate) fn team_command(&mut self, command: TeamCommand) -> Outcome {
        match command {
            TeamCommand::Start { start, limits, worker_config } => ready(self.team_start(start, limits, worker_config)),
            TeamCommand::SpawnWorker { actor, title, brief, depends_on, scope, config } => {
                ready(self.team_spawn(actor, title, brief, depends_on, scope, config))
            }
            TeamCommand::Message { actor, team, to, kind, text, urgent } => {
                ready(self.team_message(actor, team, &to, kind, text, urgent))
            }
            TeamCommand::PostNote { actor, team, text } => ready(self.team_note(actor, team, text)),
            TeamCommand::ReadNotes { actor, team, limit } => ready(self.team_find(actor, team).map(|id| {
                let limit = if limit == 0 { 20 } else { limit as usize };
                let (notes, more) = self.teams.teams[&id].latest_notes(limit);
                Reply::Team(TeamReply::Notes { notes: notes.into_iter().cloned().collect(), more: more as u32 })
            })),
            TeamCommand::Status { actor, team } => {
                ready(self.team_find(actor, team).map(|id| Reply::Team(TeamReply::Team(Box::new(self.teams.teams[&id].clone())))))
            }
            TeamCommand::MergeWorker { actor, team, worker, dry_run } => self.team_merge(actor, team, worker, dry_run),
            TeamCommand::StopWorker { actor, team, worker } => ready(self.team_stop(actor, team, &worker)),
            TeamCommand::ResetBreaker { team, limits } => ready(self.team_reset(team, limits)),
            TeamCommand::StartRace { project, prompt, configs, test_command, judge, base } => {
                ready(self.race_start(project, prompt, configs, test_command, judge, base))
            }
            TeamCommand::CompareCandidates { a, b } => self.race_compare(a, b),
            TeamCommand::ApplyPatch { thread, patch } => match self.repo_for(thread) {
                Err(e) => ready(Err(e)),
                Ok(repo) => {
                    let git = self.git.clone();
                    Outcome::Deferred(Box::pin(async move {
                        git.apply_patch(&repo, &patch).await.map(|_| Reply::Done).map_err(|e| e.to_string())
                    }))
                }
            },
            TeamCommand::RunTests { race } => ready(self.race_rerun_tests(race)),
            TeamCommand::Judge { race } => ready(self.race_judge(race)),
            TeamCommand::KeepWinner { race, winner } => self.race_keep(race, winner),
            TeamCommand::CancelRace { race } => ready(self.race_cancel(race)),
        }
    }

    // ── Bookkeeping ──────────────────────────────────────────────────────

    /// Saves a team and tells clients.
    fn team_changed(&mut self, id: TeamId) {
        let Some(team) = self.teams.teams.get(&id).cloned() else { return };
        if let Err(err) = self.teams.files.save_team(&team) {
            log::warn!("saving {id}: {err}");
        }
        self.teams.index_team(&team);
        self.emit(Event::Team(TeamEvent::Team(Box::new(team))));
    }

    fn race_changed(&mut self, id: RaceId) {
        let Some(race) = self.teams.races.get(&id).cloned() else { return };
        if let Err(err) = self.teams.files.save_race(&race) {
            log::warn!("saving {id}: {err}");
        }
        self.emit(Event::Team(TeamEvent::Race(Box::new(race))));
    }

    /// The team `actor` is on, or `team` (which `actor`, if given, must be on).
    fn team_find(&self, actor: Option<ThreadId>, team: Option<TeamId>) -> Result<TeamId, String> {
        match (team, actor) {
            (Some(id), actor) => {
                let t = self.teams.teams.get(&id).ok_or("That team no longer exists.")?;
                if let Some(actor) = actor
                    && !t.has_member(actor)
                {
                    return Err("That thread is not on this team.".into());
                }
                Ok(id)
            }
            (None, Some(actor)) => self
                .teams
                .member
                .get(&actor)
                .copied()
                .filter(|id| self.teams.teams.contains_key(id))
                .ok_or_else(|| "This thread is not on a team. Start it as a team's architect first.".into()),
            (None, None) => Err("Name the team, or act from one of its threads.".into()),
        }
    }

    fn member_label(&self, team: &Team, thread: Option<ThreadId>) -> String {
        match thread {
            None => "you".into(),
            Some(t) if t == team.architect => "architect".into(),
            Some(t) => team.worker_of(t).map(|w| w.key.clone()).unwrap_or_else(|| t.to_string()),
        }
    }

    /// The branch workers start from and merge into.
    fn architect_branch(&self, team: &Team) -> Option<String> {
        let arch = self.store.thread(team.architect)?;
        if let Some(branch) = &arch.workspace.branch {
            return Some(branch.clone());
        }
        if arch.status == ThreadStatus::Starting {
            // Its worktree is being created on this branch.
            return Some(super::orchestrator::worktree_branch(arch.id));
        }
        arch.project.and_then(|p| self.store.project(p)).and_then(|p| p.base_branch.clone())
    }

    /// Whether the agent's tools come over MCP (else the `orbit` command).
    fn team_tools(&mut self, config: &ThreadConfig) -> bool {
        config.provider != ProviderKind::Pi && self.teams.cli().is_some()
    }

    /// The Orbit MCP server for a team member's session; for OpenCode its
    /// inline configuration goes in the environment.
    pub(crate) fn team_session(&mut self, thread: &Thread, env: &mut Vec<(String, String)>) -> Vec<McpServer> {
        if !self.teams.member.get(&thread.id).is_some_and(|t| self.teams.teams.contains_key(t)) {
            return Vec::new();
        }
        let Some(cli) = self.teams.cli() else { return Vec::new() };
        let mut server = McpServer {
            name: "orbit".into(),
            command: cli,
            args: vec!["mcp".into()],
            env: vec![
                ("ORBIT_THREAD".into(), thread.id.to_string()),
                ("ORBIT_DATA".into(), self.store.dir().display().to_string()),
            ],
            config_file: None,
        };
        match thread.config.provider {
            ProviderKind::Claude => {
                let json = McpServer::claude_config(std::slice::from_ref(&server));
                match self.teams.files.write_mcp_config(thread.id, &json) {
                    Ok(path) => server.config_file = Some(path),
                    Err(err) => log::warn!("writing the MCP config of {}: {err}", thread.id),
                }
            }
            ProviderKind::OpenCode => {
                env.push(("OPENCODE_CONFIG_CONTENT".into(), McpServer::opencode_config(std::slice::from_ref(&server))));
            }
            _ => {}
        }
        vec![server]
    }

    // ── Teams ────────────────────────────────────────────────────────────

    fn team_start(&mut self, start: ThreadStart, limits: TeamLimits, worker_config: Option<ThreadConfig>) -> CommandResult {
        let prompt = start.prompt.clone().filter(|p| !p.trim().is_empty());
        let worktree = start.worktree;
        let config = start.config.clone();
        let Reply::Thread(architect) = self.start_thread(ThreadStart { prompt: None, ..start })? else {
            return Err("Couldn’t start the architect.".into());
        };
        let team = Team {
            id: TeamId::new(),
            architect,
            project: self.store.thread(architect).and_then(|t| t.project),
            limits,
            worker_config,
            created_ms: now_ms(),
            ..Default::default()
        };
        let id = team.id;
        self.teams.teams.insert(id, team);
        self.team_changed(id);
        let branch = if worktree {
            super::orchestrator::worktree_branch(architect)
        } else {
            self.architect_branch(&self.teams.teams[&id]).unwrap_or_else(|| "the current branch".into())
        };
        let tools = self.team_tools(&config);
        let context = text::architect_context(&self.teams.teams[&id], &branch, tools);
        self.runtimes.entry(architect).or_default().handoff = Some(context);
        if let Some(prompt) = prompt {
            self.send(architect, prompt, Vec::new(), false)?;
        }
        Ok(Reply::Thread(architect))
    }

    fn team_spawn(
        &mut self,
        actor: ThreadId,
        title: String,
        brief: String,
        depends_on: Vec<String>,
        scope: Vec<String>,
        config: Option<ThreadConfig>,
    ) -> CommandResult {
        let id = self.team_find(Some(actor), None)?;
        let team = &self.teams.teams[&id];
        if brief.trim().is_empty() {
            return Err("Write the worker's brief.".into());
        }
        let depth = plan::depth(team, actor);
        if depth >= team.limits.max_depth.max(1) {
            return Err(format!("Workers may be nested {} deep on this team; ask the architect to spawn it.", team.limits.max_depth.max(1)));
        }
        for key in &depends_on {
            if team.worker(key).is_none() {
                return Err(format!("There is no worker {key} on this team."));
            }
        }
        let actor_config = self.store.thread(actor).map(|t| t.config.clone()).unwrap_or_default();
        let mut config = config.or_else(|| team.worker_config.clone()).unwrap_or_else(|| actor_config.clone());
        // A worker never gets more access than whoever spawned it.
        if access_rank(config.access) > access_rank(actor_config.access) {
            config.access = actor_config.access;
        }
        let title = match title.trim() {
            "" => text::first_chars(brief.lines().find(|l| !l.trim().is_empty()).unwrap_or("Task").trim(), 60).to_owned(),
            t => t.to_owned(),
        };
        let key = plan::next_key(team);
        let worker = Worker {
            key: key.clone(),
            title,
            brief,
            parent: actor,
            config,
            depends_on,
            scope: scope.iter().map(|s| plan::normalize(s)).filter(|s| !s.is_empty()).collect(),
            created_ms: now_ms(),
            ..Default::default()
        };
        self.teams.teams.get_mut(&id).unwrap().workers.push(worker);
        self.team_schedule(id);
        self.team_changed(id);
        let w = self.teams.teams[&id].worker(&key).cloned().unwrap_or_default();
        Ok(Reply::Team(TeamReply::Worker { team: id, key, thread: w.thread, state: w.state }))
    }

    fn team_message(
        &mut self,
        actor: Option<ThreadId>,
        team: Option<TeamId>,
        to: &str,
        kind: MailKind,
        text: String,
        urgent: bool,
    ) -> CommandResult {
        let id = self.team_find(actor, team)?;
        if text.trim().is_empty() {
            return Err("Write the message.".into());
        }
        let t = &self.teams.teams[&id];
        let to = to.trim();
        let recipients: Vec<ThreadId> = match to {
            "architect" | "lead" => vec![t.architect],
            "parent" => {
                let actor = actor.ok_or("Only a worker has a parent.")?;
                vec![t.worker_of(actor).map(|w| w.parent).ok_or("The architect has no parent on the team.")?]
            }
            "@all" | "all" => t
                .workers
                .iter()
                .filter(|w| w.state == WorkerState::Working)
                .filter_map(|w| w.thread)
                .filter(|th| Some(*th) != actor)
                .collect(),
            key => match t.worker(key) {
                Some(w) => vec![w.thread.ok_or_else(|| {
                    format!("{key} hasn’t started yet. Post a note instead: its brief will include the newest notes.")
                })?],
                None => match key.parse::<ThreadId>() {
                    Ok(thread) if t.has_member(thread) => vec![thread],
                    _ => return Err(format!("There is no one called “{key}” on this team (use architect, a worker key or @all).")),
                },
            },
        };
        if recipients.is_empty() {
            return Err("No worker is working right now.".into());
        }
        let from_label = self.member_label(t, actor);
        let team = self.teams.teams.get_mut(&id).unwrap();
        let first = team.mail.iter().map(|m| m.id).max().unwrap_or(0) + 1;
        for (id, to) in (first..).zip(recipients) {
            team.mail.push(Mail {
                id,
                from: actor,
                from_label: from_label.clone(),
                to,
                kind,
                text: text.trim().to_owned(),
                urgent,
                sent_ms: now_ms(),
                delivered_ms: None,
            });
        }
        self.team_deliver(id);
        self.team_changed(id);
        Ok(Reply::Done)
    }

    fn team_note(&mut self, actor: Option<ThreadId>, team: Option<TeamId>, text: String) -> CommandResult {
        let id = self.team_find(actor, team)?;
        if text.trim().is_empty() {
            return Err("Write the note.".into());
        }
        let label = self.member_label(&self.teams.teams[&id], actor);
        let team = self.teams.teams.get_mut(&id).unwrap();
        let next = team.notes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        team.notes.push(Note {
            id: next,
            author: actor.unwrap_or_default(),
            author_label: label,
            text: text.trim().to_owned(),
            at_ms: now_ms(),
        });
        self.team_changed(id);
        Ok(Reply::Done)
    }

    fn team_stop(&mut self, actor: Option<ThreadId>, team: Option<TeamId>, key: &str) -> CommandResult {
        let id = self.team_find(actor, team)?;
        let worker = self.teams.teams[&id].worker(key).cloned().ok_or_else(|| format!("There is no worker {key} on this team."))?;
        if let Some(actor) = actor
            && actor != worker.parent
            && actor != self.teams.teams[&id].architect
        {
            return Err("Only the worker’s parent or the architect may stop it.".into());
        }
        if let Some(thread) = worker.thread {
            self.interrupt(thread)?;
        }
        if let Some(w) = self.teams.teams.get_mut(&id).unwrap().workers.iter_mut().find(|w| w.key == key) {
            w.state = WorkerState::Stopped;
            w.finished_ms = Some(now_ms());
        }
        self.team_schedule(id);
        self.team_changed(id);
        Ok(Reply::Done)
    }

    fn team_reset(&mut self, id: TeamId, limits: Option<TeamLimits>) -> CommandResult {
        let team = self.teams.teams.get_mut(&id).ok_or("That team no longer exists.")?;
        team.breaker = None;
        if let Some(limits) = limits {
            team.limits = limits;
        }
        let architect = team.architect;
        self.notice(architect, NoticeLevel::Info, "You resumed spawning for this team.".into());
        self.team_schedule(id);
        self.team_changed(id);
        Ok(Reply::Done)
    }

    fn team_merge(&mut self, actor: Option<ThreadId>, team: Option<TeamId>, key: String, dry_run: bool) -> Outcome {
        let id = match self.team_find(actor, team) {
            Ok(id) => id,
            Err(e) => return ready(Err(e)),
        };
        let t = &self.teams.teams[&id];
        let Some(worker) = t.worker(&key).cloned() else {
            return ready(Err(format!("There is no worker {key} on this team.")));
        };
        if let Some(actor) = actor
            && actor != t.architect
            && actor != worker.parent
        {
            return ready(Err("Only the architect or the worker’s parent may merge it.".into()));
        }
        let Some(thread) = worker.thread else {
            return ready(Err(format!("{key} hasn’t started yet.")));
        };
        if worker.state == WorkerState::Working {
            return ready(Err(format!("{key} is still working; merge it when it is done.")));
        }
        let Some(into) = self.architect_branch(t) else {
            return ready(Err("The architect has no branch to merge into.".into()));
        };
        let message = format!("{key}: {}", worker.title);
        let preview = into_future(self.review_command(Command::MergeLocal {
            thread,
            into: into.clone(),
            message: message.clone(),
            dry_run: true,
        }));
        let merge = (!dry_run).then(|| into_future(self.review_command(Command::MergeLocal { thread, into, message, dry_run: false })));
        let tx = self.internal_tx.clone();
        Outcome::Deferred(Box::pin(async move {
            let Reply::Merge(preview) = preview.await? else { return Err("Unexpected reply.".into()) };
            let info = match merge {
                Some(merge) if preview.conflicts.is_empty() => match merge.await? {
                    Reply::Merge(info) => info,
                    _ => return Err("Unexpected reply.".into()),
                },
                _ => preview,
            };
            let record = info.clone();
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| o.team_merged(id, &key, record))));
            Ok(Reply::Merge(info))
        }))
    }

    fn team_merged(&mut self, id: TeamId, key: &str, info: MergeInfo) {
        let Some(team) = self.teams.teams.get_mut(&id) else { return };
        let Some(w) = team.workers.iter_mut().find(|w| w.key == key) else { return };
        if info.merged.is_some() {
            w.state = WorkerState::Merged;
        }
        let conflicts = info.conflicts.clone();
        let architect = team.architect;
        w.merge = Some(info);
        if !conflicts.is_empty() {
            let shown: Vec<&str> = conflicts.iter().take(5).map(String::as_str).collect();
            self.notice(
                architect,
                NoticeLevel::Warning,
                format!("Merging {key} would conflict in {}. Nothing was changed.", shown.join(", ")),
            );
        }
        self.team_changed(id);
    }

    /// Starts every waiting worker that can start, and explains the rest.
    fn team_schedule(&mut self, id: TeamId) {
        loop {
            let Some(team) = self.teams.teams.get(&id) else { return };
            let global: u32 = self
                .teams
                .teams
                .values()
                .map(|t| t.workers.iter().filter(|w| w.state == WorkerState::Working).count() as u32)
                .sum();
            let slots = plan::Slots {
                team: team.workers.iter().filter(|w| w.state == WorkerState::Working).count() as u32,
                global,
                global_limit: self.teams.global_limit,
            };
            let busy: Vec<(String, Vec<String>)> = team
                .workers
                .iter()
                .filter(|w| w.state == WorkerState::Working)
                .map(|w| {
                    let mut paths = w.scope.clone();
                    if let Some(touched) = w.thread.and_then(|t| self.teams.touched.get(&t)) {
                        paths.extend(touched.iter().cloned());
                    }
                    (w.key.clone(), paths)
                })
                .collect();
            let mut start = None;
            let mut waits = Vec::new();
            for (ix, w) in team.workers.iter().enumerate() {
                let WorkerState::Waiting(reason) = &w.state else { continue };
                match plan::decide(team, w, slots, &busy) {
                    plan::Verdict::Start => {
                        start = Some(ix);
                        break;
                    }
                    plan::Verdict::Wait(new) if new != *reason => waits.push((ix, new)),
                    plan::Verdict::Wait(_) => {}
                }
            }
            let team = self.teams.teams.get_mut(&id).unwrap();
            for (ix, reason) in waits {
                team.workers[ix].state = WorkerState::Waiting(reason);
            }
            let Some(ix) = start else { return };
            if let Err(message) = self.team_launch(id, ix) {
                let team = self.teams.teams.get_mut(&id).unwrap();
                team.workers[ix].state = WorkerState::Failed { message };
                team.workers[ix].finished_ms = Some(now_ms());
            }
        }
    }

    /// Creates a waiting worker's thread and worktree and sends its brief.
    fn team_launch(&mut self, id: TeamId, ix: usize) -> Result<(), String> {
        let team = self.teams.teams[&id].clone();
        let worker = team.workers[ix].clone();
        let project = team.project.and_then(|p| self.store.project(p).cloned()).ok_or("Workers need a project.")?;
        let branch = self.architect_branch(&team);
        let now = now_ms();
        let thread = Thread {
            id: ThreadId::new(),
            project: Some(project.id),
            title: worker.title.clone(),
            config: worker.config.clone(),
            created_ms: now,
            updated_ms: now,
            parent: Some(worker.parent),
            status: ThreadStatus::Starting,
            ..Default::default()
        };
        let thread_id = thread.id;
        self.emit(Event::ThreadUpserted(Box::new(thread)));
        self.create_worktree_from(thread_id, project, branch.clone());
        self.teams.member.insert(thread_id, id);
        {
            let w = &mut self.teams.teams.get_mut(&id).unwrap().workers[ix];
            w.thread = Some(thread_id);
            w.state = WorkerState::Working;
            w.started_ms = Some(now);
        }
        let tools = self.team_tools(&worker.config);
        let context = text::worker_context(&team, &worker, branch.as_deref().unwrap_or("the base branch"), tools);
        self.runtimes.entry(thread_id).or_default().handoff = Some(context);
        let brief = text::worker_brief(&self.teams.teams[&id], &worker);
        self.send(thread_id, brief, Vec::new(), false)?;
        Ok(())
    }

    /// Delivers mail to members at their turn boundaries, and urgent mail
    /// into a working member's turn (at most once a minute).
    fn team_deliver(&mut self, id: TeamId) {
        let Some(team) = self.teams.teams.get(&id) else { return };
        let mut by_to: HashMap<ThreadId, Vec<u64>> = HashMap::new();
        for m in team.mail.iter().filter(|m| m.delivered_ms.is_none()) {
            by_to.entry(m.to).or_default().push(m.id);
        }
        let now = now_ms();
        for (to, ids) in by_to {
            let Some(thread) = self.store.thread(to).cloned() else { continue };
            if thread.archived {
                continue;
            }
            let runtime = self.runtimes.get(&to);
            let in_turn = runtime.is_some_and(|r| r.turn.is_some());
            let idle = !in_turn
                && runtime.is_none_or(|r| !r.checkpointing && r.held_prompt.is_none() && r.queue.is_empty())
                && !thread.status.is_busy();
            let team = &self.teams.teams[&id];
            let mail: Vec<&Mail> = team.mail.iter().filter(|m| ids.contains(&m.id)).collect();
            let newest = mail.iter().map(|m| m.sent_ms).max().unwrap_or(0);
            let (send, steer): (Vec<u64>, bool) = if idle && now.saturating_sub(newest) >= MAIL_QUIET_MS {
                (ids.clone(), false)
            } else if in_turn
                && thread.status == ThreadStatus::Running
                && mail.iter().any(|m| m.urgent)
                && self.teams.last_steer.get(&to).is_none_or(|t| t.elapsed() >= STEER_EVERY)
            {
                (mail.iter().filter(|m| m.urgent).map(|m| m.id).collect(), true)
            } else {
                continue;
            };
            let chosen: Vec<&Mail> = mail.into_iter().filter(|m| send.contains(&m.id)).collect();
            let message = text::mail_message(id, &chosen);
            if steer {
                self.teams.last_steer.insert(to, Instant::now());
            }
            match self.send(to, message, Vec::new(), steer) {
                Ok(_) => {
                    let team = self.teams.teams.get_mut(&id).unwrap();
                    for m in team.mail.iter_mut().filter(|m| send.contains(&m.id)) {
                        m.delivered_ms = Some(now);
                    }
                }
                Err(err) => log::debug!("team mail to {to} waits: {err}"),
            }
        }
    }

    /// The text of a thread's last reply.
    fn last_reply(&mut self, thread: ThreadId) -> String {
        let was_loaded = self.store.timeline(thread).is_some();
        let reply = self
            .store
            .load_timeline(thread)
            .ok()
            .and_then(|t| {
                t.items().iter().rev().find_map(|i| match &i.body {
                    ItemBody::Assistant { text } if !text.trim().is_empty() => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        if !was_loaded && !self.open.contains(&thread) && self.runtimes.get(&thread).is_none_or(|r| r.turn.is_none()) {
            self.store.unload_timeline(thread);
        }
        reply
    }

    /// Whether a thread finished its work for now: not in a turn, nothing
    /// queued, its snapshot taken.
    fn settled_thread(&self, thread: &Thread) -> bool {
        let runtime = self.runtimes.get(&thread.id);
        !thread.status.is_busy()
            && thread.queued == 0
            && runtime.is_none_or(|r| r.turn.is_none() && !r.checkpointing && r.held_prompt.is_none() && r.queue.is_empty())
    }

    /// Moves workers along with their threads, trips the breaker, starts
    /// who can start and delivers mail.
    fn team_reconcile(&mut self, id: TeamId) {
        let Some(team) = self.teams.teams.get(&id).cloned() else { return };
        if self.store.thread(team.architect).is_none() {
            // The architect was deleted: the team goes with it.
            self.teams.teams.remove(&id);
            self.teams.files.remove_team(id);
            self.teams.member.retain(|_, t| *t != id);
            self.emit(Event::Team(TeamEvent::TeamRemoved(id)));
            return;
        }
        let mut changed = false;
        let mut finished: Vec<(String, Result<String, String>)> = Vec::new();
        let pending_mail: Vec<ThreadId> = team.mail.iter().filter(|m| m.delivered_ms.is_none()).map(|m| m.to).collect();
        for w in &team.workers {
            let Some(tid) = w.thread else { continue };
            let next = match self.store.thread(tid) {
                None if !w.state.is_finished() => Some(WorkerState::Stopped),
                None => None,
                Some(t) => {
                    // Failed before its first turn (its worktree): the brief
                    // stays queued and never runs.
                    let stuck = t.turns == 0 && matches!(t.status, ThreadStatus::Failed { .. });
                    let settled = stuck || (self.settled_thread(t) && !pending_mail.contains(&tid));
                    match (&w.state, &t.status) {
                        (WorkerState::Stopped, _) => None,
                        (old, ThreadStatus::Failed { message }) if settled => {
                            (!matches!(old, WorkerState::Failed { .. }) && *old != WorkerState::Stopped)
                                .then(|| WorkerState::Failed { message: message.clone() })
                        }
                        (WorkerState::Working, _) if settled && t.turns > 0 => Some(WorkerState::Done),
                        (old, _) if !settled && t.turns > 0 && *old != WorkerState::Working => Some(WorkerState::Working),
                        _ => None,
                    }
                }
            };
            let Some(next) = next else { continue };
            changed = true;
            match &next {
                WorkerState::Done => finished.push((w.key.clone(), Ok(self.last_reply(tid)))),
                WorkerState::Failed { message } => finished.push((w.key.clone(), Err(message.clone()))),
                _ => {}
            }
            let team = self.teams.teams.get_mut(&id).unwrap();
            if let Some(slot) = team.workers.iter_mut().find(|x| x.key == w.key) {
                if next.is_finished() {
                    slot.finished_ms = Some(now_ms());
                }
                slot.state = next;
            }
        }
        for (key, result) in finished {
            // Fresh files to compare now that a worker stopped changing them.
            self.teams.next_touch = None;
            let team = self.teams.teams.get_mut(&id).unwrap();
            let Some(w) = team.workers.iter_mut().find(|w| w.key == key) else { continue };
            let (kind, body) = match &result {
                Ok(summary) => {
                    w.summary = summary.clone();
                    (MailKind::Done, text::done_mail(w))
                }
                Err(message) => (MailKind::Status, text::failed_mail(w, message)),
            };
            let (from, to, label) = (w.thread, w.parent, w.key.clone());
            let next = team.mail.iter().map(|m| m.id).max().unwrap_or(0) + 1;
            team.mail.push(Mail {
                id: next,
                from,
                from_label: label,
                to,
                kind,
                text: body,
                urgent: false,
                sent_ms: now_ms(),
                delivered_ms: None,
            });
        }
        changed |= self.team_check_breaker(id);
        let before = self.teams.teams.get(&id).cloned();
        self.team_schedule(id);
        self.team_deliver(id);
        if changed || self.teams.teams.get(&id) != before.as_ref() {
            self.team_changed(id);
        }
    }

    /// Trips the circuit breaker when failures or spend reach the limits.
    fn team_check_breaker(&mut self, id: TeamId) -> bool {
        let team = &self.teams.teams[&id];
        if team.breaker.is_some() {
            return false;
        }
        let failures = team.workers.iter().filter(|w| matches!(w.state, WorkerState::Failed { .. })).count() as u32;
        let mut members = vec![team.architect];
        members.extend(team.workers.iter().filter_map(|w| w.thread));
        let cost: u64 = members.iter().filter_map(|t| self.store.thread(*t)).map(|t| t.usage.cost_micros).sum();
        let l = team.limits;
        let reason = if l.max_failures > 0 && failures >= l.max_failures {
            format!("{failures} workers failed")
        } else if l.max_cost_micros > 0 && cost >= l.max_cost_micros {
            format!("the team spent ${:.2} of its ${:.2} limit", cost as f64 / 1e6, l.max_cost_micros as f64 / 1e6)
        } else {
            return false;
        };
        let architect = team.architect;
        let team = self.teams.teams.get_mut(&id).unwrap();
        team.breaker = Some(Breaker { reason: reason.clone(), at_ms: now_ms() });
        let next = team.mail.iter().map(|m| m.id).max().unwrap_or(0) + 1;
        team.mail.push(Mail {
            id: next,
            from: None,
            from_label: "orbit".into(),
            to: architect,
            kind: MailKind::Status,
            text: format!("Orbit paused spawning because {reason}. Workers already working continue. The user decides when to resume."),
            urgent: false,
            sent_ms: now_ms(),
            delivered_ms: None,
        });
        self.notice(
            architect,
            NoticeLevel::Warning,
            format!("Spawning paused: {reason}. Resume it from the crew strip when you are ready."),
        );
        self.update_thread(architect, |t| t.unread = true);
        true
    }

    /// Engine tick: keeps teams and races moving. Free with none.
    pub(crate) fn teams_tick(&mut self) {
        if self.teams.is_idle() {
            return;
        }
        let teams: Vec<TeamId> = self.teams.teams.keys().copied().collect();
        for id in teams {
            self.team_reconcile(id);
        }
        let races: Vec<RaceId> =
            self.teams.races.values().filter(|r| r.state == RaceState::Running).map(|r| r.id).collect();
        for id in races {
            self.race_reconcile(id);
        }
        self.teams_touch();
    }

    /// Compares the files working members changed, against the
    /// architect's branch (the review engine's per-worktree comparison).
    fn teams_touch(&mut self) {
        if self.teams.touched_pending || self.teams.next_touch.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let mut live = Vec::new();
        for team in self.teams.teams.values() {
            let Some(base) = self.architect_branch(team) else { continue };
            for w in team.workers.iter().filter(|w| w.state == WorkerState::Working) {
                let Some(thread) = w.thread.and_then(|t| self.store.thread(t)) else { continue };
                if let Some(path) = &thread.workspace.worktree {
                    live.push((thread.id, PathBuf::from(path), base.clone()));
                }
            }
        }
        self.teams.next_touch = Some(Instant::now() + TOUCH_EVERY);
        if live.is_empty() {
            if !self.teams.touched.is_empty() {
                self.teams.touched.clear();
                self.teams_apply_touched();
            }
            return;
        }
        self.teams.touched_pending = true;
        let (git, tx) = (self.git.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let mut touched = HashMap::new();
            for (thread, path, base) in live {
                let Ok(Some(from)) = git.merge_base(&path, &base, "HEAD").await else { continue };
                let Ok(now) = git.snapshot(&path).await else { continue };
                if let Ok(paths) = git.changed_paths(&path, &TreeId::new(from), &now).await {
                    touched.insert(thread, paths.as_ref().clone());
                }
            }
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                o.teams.touched_pending = false;
                o.teams.touched = touched;
                o.teams_apply_touched();
            })));
        });
    }

    fn teams_apply_touched(&mut self) {
        let ids: Vec<TeamId> = self.teams.teams.keys().copied().collect();
        for id in ids {
            let team = &self.teams.teams[&id];
            let working: HashMap<ThreadId, Vec<String>> = team
                .workers
                .iter()
                .filter(|w| w.state == WorkerState::Working)
                .filter_map(|w| w.thread)
                .filter_map(|t| self.teams.touched.get(&t).map(|p| (t, p.clone())))
                .collect();
            let overlaps = plan::file_overlaps(&working);
            let before = team.clone();
            self.teams.teams.get_mut(&id).unwrap().overlaps = overlaps;
            self.team_schedule(id);
            if self.teams.teams.get(&id) != Some(&before) {
                self.team_changed(id);
            }
        }
    }

    // ── Races ────────────────────────────────────────────────────────────

    /// The project's test command: a run script named test, tests or check.
    fn project_test_command(&self, project: ProjectId) -> Option<String> {
        let p = self.store.project(project)?;
        p.scripts
            .run
            .iter()
            .find(|s| TEST_SCRIPTS.iter().any(|n| s.name.trim().eq_ignore_ascii_case(n)))
            .map(|s| s.command.clone())
    }

    fn race_start(
        &mut self,
        project: ProjectId,
        prompt: String,
        configs: Vec<ThreadConfig>,
        test_command: Option<String>,
        judge: bool,
        base: String,
    ) -> CommandResult {
        let record = self.store.project(project).cloned().ok_or("That project no longer exists.")?;
        if prompt.trim().is_empty() {
            return Err("Write the prompt to race.".into());
        }
        if configs.len() < 2 {
            return Err("A race needs at least two candidates.".into());
        }
        if configs.len() > 8 {
            return Err("A race can have up to eight candidates.".into());
        }
        let base = match base.trim() {
            "" => record.base_branch.clone().ok_or("The project has no branch to race from.")?,
            b => b.to_owned(),
        };
        let test_command = test_command.filter(|c| !c.trim().is_empty()).or_else(|| self.project_test_command(project));
        let id = RaceId::new();
        let short = text::first_chars(prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("Race").trim(), 56).to_owned();
        let mut candidates = Vec::new();
        for (ix, config) in configs.into_iter().enumerate() {
            let label = ((b'A' + ix as u8) as char).to_string();
            let now = now_ms();
            let thread = Thread {
                id: ThreadId::new(),
                project: Some(project),
                title: format!("{short} ({label})"),
                config: config.clone(),
                created_ms: now,
                updated_ms: now,
                status: ThreadStatus::Starting,
                ..Default::default()
            };
            let tid = thread.id;
            self.emit(Event::ThreadUpserted(Box::new(thread)));
            self.create_worktree_from(tid, record.clone(), Some(base.clone()));
            self.teams.candidate.insert(tid, id);
            candidates.push(Candidate {
                thread: tid,
                label,
                config,
                test: if test_command.is_some() { TestState::Pending } else { TestState::None },
                ..Default::default()
            });
        }
        let race = Race {
            id,
            project,
            prompt: prompt.clone(),
            base,
            test_command,
            candidates,
            judge: if judge { JudgeState::Waiting } else { JudgeState::Off },
            state: RaceState::Running,
            created_ms: now_ms(),
        };
        let threads: Vec<ThreadId> = race.candidates.iter().map(|c| c.thread).collect();
        self.teams.races.insert(id, race);
        self.race_changed(id);
        for thread in threads {
            if let Err(err) = self.send(thread, prompt.clone(), Vec::new(), false) {
                self.notice(thread, NoticeLevel::Error, err);
            }
        }
        Ok(Reply::Team(TeamReply::Race(id)))
    }

    fn race_reconcile(&mut self, id: RaceId) {
        let Some(race) = self.teams.races.get(&id).cloned() else { return };
        let mut changed = false;
        for c in &race.candidates {
            if c.finished.is_some() || self.teams.gathering.contains(&c.thread) {
                continue;
            }
            let Some(t) = self.store.thread(c.thread).cloned() else {
                let r = self.teams.races.get_mut(&id).unwrap();
                if let Some(slot) = r.candidates.iter_mut().find(|x| x.thread == c.thread) {
                    slot.finished = Some(CandidateEnd::Failed { message: "The thread was deleted.".into() });
                    slot.test = TestState::None;
                }
                changed = true;
                continue;
            };
            let failed = matches!(t.status, ThreadStatus::Failed { .. });
            let stuck = t.turns == 0 && failed;
            if !stuck && (!self.settled_thread(&t) || t.turns == 0) {
                continue;
            }
            let end = match &t.status {
                ThreadStatus::Failed { message } => CandidateEnd::Failed { message: message.clone() },
                _ => CandidateEnd::Completed,
            };
            self.race_gather(id, c.thread, end);
        }
        if changed {
            self.race_changed(id);
        }
        self.race_maybe_judge(id);
    }

    /// Takes a finished candidate's duration, usage and diff stats, then
    /// runs the tests in its worktree.
    fn race_gather(&mut self, id: RaceId, thread: ThreadId, end: CandidateEnd) {
        self.teams.gathering.insert(thread);
        let usage = self.store.thread(thread).map(|t| t.usage).unwrap_or_default();
        let duration = self.turn_durations(thread);
        let base = self.thread_base(thread);
        let repo = self.repo_for(thread).ok();
        let race = &self.teams.races[&id];
        let test = (matches!(end, CandidateEnd::Completed) && race.candidate(thread).is_some_and(|c| c.test != TestState::None))
            .then(|| race.test_command.clone())
            .flatten();
        {
            let r = self.teams.races.get_mut(&id).unwrap();
            if let Some(c) = r.candidates.iter_mut().find(|c| c.thread == thread) {
                c.finished = Some(end);
                c.usage = usage;
                c.duration_ms = duration;
                if test.is_some() {
                    c.test = TestState::Running;
                } else if c.test == TestState::Pending {
                    c.test = TestState::None;
                }
            }
        }
        self.race_changed(id);
        let (git, tx) = (self.git.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let mut files = Vec::new();
            if let (Some(repo), Some(base)) = (&repo, base) {
                match git.diff(repo, &TreeId::new(base), None).await {
                    Ok(diff) => files = diff.changes(),
                    Err(err) => log::debug!("race diff of {thread}: {err}"),
                }
            }
            let test = match (test, &repo) {
                (Some(command), Some(repo)) => Some(run_test(repo.clone(), command).await),
                _ => None,
            };
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                o.teams.gathering.remove(&thread);
                let Some(r) = o.teams.races.get_mut(&id) else { return };
                if let Some(c) = r.candidates.iter_mut().find(|c| c.thread == thread) {
                    c.files = files;
                    if let Some(test) = test {
                        c.test = test;
                    }
                }
                o.race_changed(id);
                o.race_maybe_judge(id);
            })));
        });
    }

    /// The sum of a thread's turn durations.
    fn turn_durations(&mut self, thread: ThreadId) -> u64 {
        let was_loaded = self.store.timeline(thread).is_some();
        let total = self
            .store
            .load_timeline(thread)
            .map(|t| {
                t.items()
                    .iter()
                    .map(|i| match &i.body {
                        ItemBody::TurnEnd(s) => s.duration_ms,
                        _ => 0,
                    })
                    .sum()
            })
            .unwrap_or(0);
        if !was_loaded && !self.open.contains(&thread) && self.runtimes.get(&thread).is_none_or(|r| r.turn.is_none()) {
            self.store.unload_timeline(thread);
        }
        total
    }

    fn race_rerun_tests(&mut self, id: RaceId) -> CommandResult {
        let race = self.teams.races.get(&id).ok_or("That race no longer exists.")?;
        let Some(command) = race.test_command.clone() else {
            return Err("This project has no test command. Add a run script named “test”.".into());
        };
        let targets: Vec<ThreadId> = race
            .candidates
            .iter()
            .filter(|c| c.finished.is_some() && !matches!(c.test, TestState::Running))
            .map(|c| c.thread)
            .collect();
        for thread in targets {
            let Ok(repo) = self.repo_for(thread) else { continue };
            if let Some(c) = self.teams.races.get_mut(&id).unwrap().candidates.iter_mut().find(|c| c.thread == thread) {
                c.test = TestState::Running;
            }
            let (tx, command) = (self.internal_tx.clone(), command.clone());
            tokio::spawn(async move {
                let test = run_test(repo, command).await;
                let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                    let Some(r) = o.teams.races.get_mut(&id) else { return };
                    if let Some(c) = r.candidates.iter_mut().find(|c| c.thread == thread) {
                        c.test = test;
                    }
                    o.race_changed(id);
                })));
            });
        }
        self.race_changed(id);
        Ok(Reply::Done)
    }

    fn race_maybe_judge(&mut self, id: RaceId) {
        if self.teams.races.get(&id).is_some_and(|r| r.judge == JudgeState::Waiting && r.settled()) {
            let _ = self.race_judge(id);
        }
    }

    /// Asks a judge (one-shot text generation) to pick a winner.
    fn race_judge(&mut self, id: RaceId) -> CommandResult {
        let race = self.teams.races.get(&id).cloned().ok_or("That race no longer exists.")?;
        if race.judge == JudgeState::Running {
            return Ok(Reply::Done);
        }
        let mut gather = Vec::new();
        for c in &race.candidates {
            let reply = self.last_reply(c.thread);
            let agent = self.providers.iter().find(|p| p.serves(&c.config)).map(|p| p.display_name().to_owned());
            let agent = match (agent, c.config.model.as_str()) {
                (Some(a), "") => a,
                (Some(a), m) => format!("{a}, {m}"),
                (None, m) => format!("{} {m}", c.config.provider.label()),
            };
            gather.push((c.clone(), agent, reply, self.repo_for(c.thread).ok(), self.thread_base(c.thread)));
        }
        self.teams.races.get_mut(&id).unwrap().judge = JudgeState::Running;
        self.race_changed(id);
        let (git, tx) = (self.git.clone(), self.internal_tx.clone());
        let prompt = race.prompt.clone();
        tokio::spawn(async move {
            let mut entries = Vec::new();
            for (candidate, agent, reply, repo, base) in gather {
                let diff = match (repo, base) {
                    (Some(repo), Some(base)) => git.diff(&repo, &TreeId::new(base), None).await.map(|d| d.patch()).unwrap_or_default(),
                    _ => String::new(),
                };
                entries.push(text::JudgeEntry { label: candidate.label.clone(), agent, candidate, reply, diff });
            }
            let input = text::judge_input(&prompt, &entries);
            let labels: Vec<(String, ThreadId)> = entries.iter().map(|e| (e.label.clone(), e.candidate.thread)).collect();
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                let task = TextTask::Custom { instructions: text::JUDGE_INSTRUCTIONS.into() };
                let future = o.generate_text(None, task, input);
                let tx = o.internal_tx.clone();
                tokio::spawn(async move {
                    let verdict = match future.await {
                        Ok(Reply::Text(answer)) => {
                            let names: Vec<&str> = labels.iter().map(|(l, _)| l.as_str()).collect();
                            match text::parse_verdict(&answer, &names) {
                                Some((label, reason)) => {
                                    let winner = labels.iter().find(|(l, _)| *l == label).map(|(_, t)| *t).unwrap();
                                    JudgeState::Verdict { winner, reason }
                                }
                                None => JudgeState::Failed { message: "The judge didn’t name a winner.".into() },
                            }
                        }
                        Ok(_) => JudgeState::Failed { message: "The judge didn’t answer.".into() },
                        Err(message) => JudgeState::Failed { message },
                    };
                    let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                        if let Some(r) = o.teams.races.get_mut(&id) {
                            r.judge = verdict;
                            o.race_changed(id);
                        }
                    })));
                });
            })));
        });
        Ok(Reply::Done)
    }

    /// `a`'s working tree against `b`'s, both as they are now.
    fn race_compare(&mut self, a: ThreadId, b: ThreadId) -> Outcome {
        let (repo_a, repo_b) = match (self.repo_for(a), self.repo_for(b)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return ready(Err(e)),
        };
        let git = self.git.clone();
        Outcome::Deferred(Box::pin(async move {
            let tree_a = git.snapshot(&repo_a).await.map_err(|e| e.to_string())?;
            let tree_b = git.snapshot(&repo_b).await.map_err(|e| e.to_string())?;
            let diff = git.diff(&repo_a, &tree_a, Some(&tree_b)).await.map_err(|e| e.to_string())?;
            Ok(Reply::Diff(diff.patch()))
        }))
    }

    fn race_keep(&mut self, id: RaceId, winner: ThreadId) -> Outcome {
        let Some(race) = self.teams.races.get(&id).cloned() else {
            return ready(Err("That race no longer exists.".into()));
        };
        let Some(candidate) = race.candidate(winner) else {
            return ready(Err("That thread is not in this race.".into()));
        };
        let message = format!("Race winner {}: {}", candidate.label, text::first_chars(race.prompt.trim(), 60));
        let merge = into_future(self.review_command(Command::MergeLocal { thread: winner, into: race.base.clone(), message, dry_run: false }));
        let tx = self.internal_tx.clone();
        Outcome::Deferred(Box::pin(async move {
            let reply = merge.await?;
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| o.race_decided(id, winner))));
            Ok(reply)
        }))
    }

    fn race_decided(&mut self, id: RaceId, winner: ThreadId) {
        let Some(race) = self.teams.races.get_mut(&id) else { return };
        race.state = RaceState::Decided { winner };
        let losers: Vec<ThreadId> = race.candidates.iter().map(|c| c.thread).filter(|t| *t != winner).collect();
        for thread in losers {
            // Archived, not deleted: the worktree stays and can be restored.
            let _ = self.patch_thread(thread, ThreadPatch { archived: Some(true), ..Default::default() });
        }
        self.race_changed(id);
    }

    fn race_cancel(&mut self, id: RaceId) -> CommandResult {
        let race = self.teams.races.get_mut(&id).ok_or("That race no longer exists.")?;
        race.state = RaceState::Cancelled;
        let threads: Vec<ThreadId> = race.candidates.iter().map(|c| c.thread).collect();
        for thread in threads {
            let _ = self.interrupt(thread);
        }
        self.race_changed(id);
        Ok(Reply::Done)
    }
}

/// Runs a test command in `cwd` through the user's shell, headless.
async fn run_test(cwd: PathBuf, command: String) -> TestState {
    let shell = super::cli::command_shell(&command);
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(shell.program());
    cmd.args(shell.arguments())
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = match tokio::time::timeout(TEST_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return TestState::Failed { code: None, ms: 0, tail: format!("Couldn’t run “{command}”: {err}") },
        Err(_) => {
            return TestState::Failed { code: None, ms: TEST_TIMEOUT.as_millis() as u64, tail: "Timed out after 20 minutes.".into() };
        }
    };
    let ms = started.elapsed().as_millis() as u64;
    if output.status.success() {
        return TestState::Passed { ms };
    }
    let mut all = String::from_utf8_lossy(&output.stdout).into_owned();
    all.push_str(&String::from_utf8_lossy(&output.stderr));
    let tail = output_tail(&all, TEST_TAIL).to_owned();
    TestState::Failed { code: output.status.code(), ms, tail }
}
