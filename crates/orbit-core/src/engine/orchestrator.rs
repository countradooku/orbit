//! The orchestrator: single owner of engine state.
//!
//! Commands and agent events are applied one at a time on the engine thread,
//! so there are no locks and no races between, say, a turn finishing and the
//! user steering it. Anything slow (provider probes, git, worktrees) runs in a
//! spawned task and reports back through [`Internal`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, MTx, mpsc};
use orbit_proto::*;

use super::bus::{Bus, EventRx};
use crate::provider::{
    self, AgentEvent, BlockKind, EventSender, PromptAttachment, SessionHandle, SessionInput, SessionSpec,
    TurnOutcome,
};
use crate::git::{Git, GitConfig, TreeId};
use crate::store::Store;
use crate::terminal::{ExitSender, OutputSender, TermSize, Terminals};

/// Results of background work, applied on the engine thread.
pub enum Internal {
    Probed(ProviderInfo),
    /// The baseline snapshot before a thread's first turn is taken; send the
    /// held prompt.
    BaselineReady { thread: ThreadId, tree: Option<String> },
    /// A finished turn's snapshot is taken.
    Checkpointed { thread: ThreadId, item: ItemId, tree: Option<String>, files: Option<Vec<FileChange>> },
    /// The project's current branch was read.
    ProjectBranch { project: ProjectId, branch: Option<String> },
    /// The provider's quota window has reset; resume the thread's queue.
    QuotaReset(ThreadId),
}

/// Starts provider sessions. Tests replace it with scripted fakes.
pub type Launcher = Box<dyn FnMut(ProviderKind, SessionSpec, EventSender) -> SessionHandle + Send>;

/// Bytes of tool output kept inline on an item; the rest goes to a blob.
const INLINE_OUTPUT: usize = 64 * 1024;
/// Thread titles taken from the first message are cut to this many chars.
const TITLE_CHARS: usize = 72;
/// Streaming items are persisted at most this often.
const STREAM_PERSIST: Duration = Duration::from_secs(2);

struct ActiveTurn {
    number: u32,
    started: Instant,
    /// Provider block id → timeline item.
    blocks: HashMap<String, ItemId>,
    /// Provider tool call id → timeline item.
    tools: HashMap<String, ItemId>,
    plan: Option<ItemId>,
    /// Provider request id → (request id, timeline item).
    requests: HashMap<String, (RequestId, ItemId)>,
    usage: Usage,
    cursor: Option<String>,
    files: Vec<FileChange>,
    interrupted: bool,
    /// The tree the turn started from.
    base: Option<String>,
}

impl ActiveTurn {
    fn new(number: u32) -> Self {
        Self {
            base: None,
            number,
            started: Instant::now(),
            blocks: HashMap::new(),
            tools: HashMap::new(),
            plan: None,
            requests: HashMap::new(),
            usage: Usage::default(),
            cursor: None,
            files: Vec::new(),
            interrupted: false,
        }
    }
}

#[derive(Default)]
struct Runtime {
    session: Option<SessionHandle>,
    turn: Option<ActiveTurn>,
    /// Queued user messages, in send order.
    queue: VecDeque<ItemId>,
    /// The next session must resume at this provider cursor (rollback).
    resume_at: Option<String>,
    /// A prompt held until the baseline snapshot exists.
    held_prompt: Option<SessionInput>,
    /// The last turn's snapshot is being taken; queued turns wait for it so
    /// one turn's files never land in another's diff.
    checkpointing: bool,
}

pub struct Orchestrator {
    store: Store,
    bus: Bus,
    providers: Vec<ProviderInfo>,
    runtimes: HashMap<ThreadId, Runtime>,
    agent_tx: EventSender,
    internal_tx: MTx<mpsc::List<Internal>>,
    launcher: Launcher,
    /// Streaming items changed since they were last persisted.
    dirty: HashSet<(ThreadId, ItemId)>,
    last_persist: Instant,
    /// Threads a client has open; they stay loaded.
    open: HashSet<ThreadId>,
    git: Git,
    terminals: Terminals,
}

pub type CommandResult = Result<Reply, String>;

/// Work a command hands off so the engine thread stays responsive.
pub type Deferred = std::pin::Pin<Box<dyn std::future::Future<Output = CommandResult> + Send>>;

/// A command's result: now, or later from a task.
pub enum Outcome {
    Ready(CommandResult),
    Deferred(Deferred),
}

impl From<CommandResult> for Outcome {
    fn from(result: CommandResult) -> Self {
        Outcome::Ready(result)
    }
}

impl Orchestrator {
    pub fn new(
        store: Store,
        agent_tx: EventSender,
        internal_tx: MTx<mpsc::List<Internal>>,
        launcher: Launcher,
        terminal_output: OutputSender,
        terminal_exits: ExitSender,
    ) -> Self {
        let git = Git::new(GitConfig::new(store.dir()));
        let providers = ProviderKind::ALL
            .iter()
            .map(|&kind| ProviderInfo { kind, message: Some("Checking…".into()), ..Default::default() })
            .collect();
        Self {
            store,
            bus: Bus::new(),
            providers,
            runtimes: HashMap::new(),
            agent_tx,
            internal_tx,
            launcher,
            dirty: HashSet::new(),
            last_persist: Instant::now(),
            open: HashSet::new(),
            git,
            terminals: Terminals::new(terminal_output, terminal_exits),
        }
    }

    /// The git repository a thread works in, if any.
    fn repo_of(&self, thread: &Thread) -> Option<PathBuf> {
        if let Some(worktree) = &thread.workspace.worktree {
            return Some(PathBuf::from(worktree));
        }
        thread.project.and_then(|p| self.store.project(p)).map(|p| PathBuf::from(&p.path))
    }

    fn repo_for(&self, thread: ThreadId) -> Result<PathBuf, String> {
        let t = self.store.thread(thread).ok_or("That thread no longer exists.")?;
        self.repo_of(t).ok_or_else(|| "This thread isn’t in a project, so it has no changes to show.".into())
    }

    /// The (from, to) trees of a turn; `to` is `None` while it runs (the
    /// working tree).
    fn turn_range(&mut self, thread: ThreadId, turn: u32) -> Option<(String, Option<String>)> {
        if let Some(active) = self.runtimes.get(&thread).and_then(|r| r.turn.as_ref())
            && active.number == turn
        {
            return active.base.clone().map(|b| (b, None));
        }
        let timeline = self.store.load_timeline(thread).ok()?;
        timeline.items().iter().rev().find_map(|i| match &i.body {
            ItemBody::TurnEnd(s) if i.turn == turn => s.base_checkpoint.clone().map(|b| (b, s.checkpoint.clone())),
            _ => None,
        })
    }

    /// The tree the thread's first turn started from.
    fn thread_base(&mut self, thread: ThreadId) -> Option<String> {
        let timeline = self.store.load_timeline(thread).ok()?;
        let from_items = timeline.items().iter().find_map(|i| match &i.body {
            ItemBody::TurnEnd(s) => s.base_checkpoint.clone(),
            _ => None,
        });
        from_items.or_else(|| self.runtimes.get(&thread).and_then(|r| r.turn.as_ref()?.base.clone()))
    }

    /// The last finished turn's snapshot, the next turn's starting point.
    fn last_checkpoint(&mut self, thread: ThreadId) -> Option<String> {
        let timeline = self.store.load_timeline(thread).ok()?;
        timeline.items().iter().rev().find_map(|i| match &i.body {
            ItemBody::TurnEnd(s) => s.checkpoint.clone(),
            _ => None,
        })
    }

    fn diff(&mut self, thread: ThreadId, scope: DiffScope) -> Outcome {
        let repo = match self.repo_for(thread) {
            Ok(repo) => repo,
            Err(e) => return Outcome::Ready(Err(e)),
        };
        let git = self.git.clone();
        let range = match &scope {
            DiffScope::Turn(n) => match self.turn_range(thread, *n) {
                Some(range) => Some(range),
                None => return Outcome::Ready(Err("This turn has no snapshot to compare.".into())),
            },
            DiffScope::Thread => match self.thread_base(thread) {
                Some(base) => Some((base, None)),
                None => return Outcome::Ready(Err("Nothing has run in this thread yet.".into())),
            },
            _ => None,
        };
        Outcome::Deferred(Box::pin(async move {
            let result = match range {
                Some((from, to)) => {
                    let to = to.map(TreeId::new);
                    git.diff(&repo, &TreeId::new(from), to.as_ref()).await
                }
                None => git.diff_scope(&repo, thread, &scope).await,
            };
            result
                .map(|diff| Reply::Diff(diff.files.iter().map(|f| f.raw.as_str()).collect::<String>()))
                .map_err(|e| e.to_string())
        }))
    }

    fn git_command(&mut self, command: Command) -> Outcome {
        let git = self.git.clone();
        match command {
            Command::Diff { thread, scope } => self.diff(thread, scope),
            Command::RevertPatch { thread, patch } => match self.repo_for(thread) {
                Err(e) => Outcome::Ready(Err(e)),
                Ok(repo) => Outcome::Deferred(Box::pin(async move {
                    git.revert_patch(&repo, &patch).await.map(|_| Reply::Done).map_err(|e| e.to_string())
                })),
            },
            Command::RestoreCheckpoint { thread, turn } => {
                let repo = match self.repo_for(thread) {
                    Ok(repo) => repo,
                    Err(e) => return Outcome::Ready(Err(e)),
                };
                let Some((_, Some(tree))) = self.turn_range(thread, turn) else {
                    return Outcome::Ready(Err("That turn has no snapshot to restore.".into()));
                };
                Outcome::Deferred(Box::pin(async move {
                    git.restore(&repo, thread, &TreeId::new(tree)).await.map(|_| Reply::Done).map_err(|e| e.to_string())
                }))
            }
            Command::Commit { thread, message, push } => match self.repo_for(thread) {
                Err(e) => Outcome::Ready(Err(e)),
                Ok(repo) => Outcome::Deferred(Box::pin(async move {
                    git.commit(&repo, &message, push).await.map(|_| Reply::Done).map_err(|e| e.to_string())
                })),
            },
            Command::CreatePullRequest { thread, title, body } => match self.repo_for(thread) {
                Err(e) => Outcome::Ready(Err(e)),
                Ok(repo) => Outcome::Deferred(Box::pin(async move {
                    git.create_pull_request(&repo, &title, &body, None)
                        .await
                        .map(|url| Reply::PullRequest { url })
                        .map_err(|e| e.to_string())
                })),
            },
            Command::Branches { project } => {
                let Some(path) = self.store.project(project).map(|p| PathBuf::from(&p.path)) else {
                    return Outcome::Ready(Err("That project no longer exists.".into()));
                };
                Outcome::Deferred(Box::pin(async move {
                    let branches = git.branches(&path).await.map_err(|e| e.to_string())?;
                    let current = branches.iter().find(|b| b.current).map(|b| b.name.clone());
                    Ok(Reply::Branches { current, all: branches.into_iter().map(|b| b.name).collect() })
                }))
            }
            other => Outcome::Ready(self.handle_command(other)),
        }
    }

    fn terminal_command(&mut self, command: Command) -> CommandResult {
        match command {
            Command::OpenTerminal { cwd, cols, rows } => self
                .terminals
                .open(Path::new(&cwd), TermSize::new(cols.max(2), rows.max(1)), None)
                .map(Reply::Terminal)
                .map_err(|e| format!("Couldn’t open a terminal in {cwd}: {e}")),
            Command::TerminalInput { term, data } => {
                self.terminals.input(term, &data);
                Ok(Reply::Done)
            }
            Command::ResizeTerminal { term, cols, rows } => {
                let _ = self.terminals.resize(term, TermSize::new(cols.max(2), rows.max(1)));
                Ok(Reply::Done)
            }
            Command::CloseTerminal { term } => {
                self.terminals.close(term);
                Ok(Reply::Done)
            }
            _ => Err("not a terminal command".into()),
        }
    }

    pub fn terminal_output(&mut self, term: TermId, data: Vec<u8>) {
        self.emit(Event::TerminalOutput { term, data });
    }

    pub fn terminal_exited(&mut self, term: TermId, code: Option<i32>) {
        self.terminals.remove_exited(term);
        self.emit(Event::TerminalExited { term, code });
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn epoch(&self) -> u64 {
        self.store.epoch()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.store.snapshot(self.providers.clone())
    }

    pub fn subscribe(&mut self, epoch: u64, since: u64) -> (Option<Snapshot>, Vec<Envelope>, EventRx) {
        let same_epoch = epoch == self.store.epoch() && since > 0;
        let (rx, replay) = self.bus.subscribe(same_epoch.then_some(since));
        match replay {
            Some(replay) => (None, replay, rx),
            None => (Some(self.snapshot()), Vec::new(), rx),
        }
    }

    // ── Emission helpers ─────────────────────────────────────────────────

    fn emit(&mut self, event: Event) {
        if event.is_durable() {
            match self.store.commit(event.clone()) {
                Ok(env) => self.bus.publish(env),
                Err(err) => {
                    log::error!("could not persist event: {err}");
                    self.bus.publish(Envelope { seq: 0, event });
                }
            }
        } else {
            self.bus.publish(Envelope { seq: 0, event });
        }
    }

    fn update_thread(&mut self, id: ThreadId, f: impl FnOnce(&mut Thread)) {
        let Some(mut thread) = self.store.thread(id).cloned() else { return };
        let before = thread.clone();
        f(&mut thread);
        if thread != before {
            thread.updated_ms = now_ms();
            self.emit(Event::ThreadUpserted(thread));
        }
    }

    fn put_item(&mut self, thread: ThreadId, item: Item) {
        self.dirty.remove(&(thread, item.id));
        self.emit(Event::ItemUpserted { thread, item });
    }

    fn item(&self, thread: ThreadId, id: ItemId) -> Option<Item> {
        self.store.timeline(thread)?.get(id).cloned()
    }

    /// Changes a streaming item in memory and broadcasts the delta; the item
    /// is persisted on the next [`Orchestrator::tick`].
    fn stream_into(&mut self, thread: ThreadId, id: ItemId, delta: Delta) {
        let Some(timeline) = self.store.timeline_mut(thread) else { return };
        let Some(item) = timeline.get_mut(id) else { return };
        match (&mut item.body, &delta) {
            (ItemBody::Assistant { text } | ItemBody::Reasoning { text }, Delta::Text(d) | Delta::Reasoning(d)) => {
                text.push_str(d)
            }
            (ItemBody::Tool(call), Delta::ToolOutput(d)) => {
                if call.output.len() < INLINE_OUTPUT {
                    call.output.push_str(d);
                }
            }
            (ItemBody::Tool(call), Delta::ToolInput(d)) => {
                if call.input.len() < INLINE_OUTPUT {
                    call.input.push_str(d);
                }
            }
            _ => return,
        }
        self.dirty.insert((thread, id));
        self.emit(Event::ItemDelta { thread, item: id, delta });
    }

    /// Periodic work: persist streaming items, flush logs.
    pub fn tick(&mut self) {
        if self.last_persist.elapsed() >= STREAM_PERSIST && !self.dirty.is_empty() {
            for (thread, id) in std::mem::take(&mut self.dirty) {
                if let Some(item) = self.item(thread, id) {
                    self.emit(Event::ItemUpserted { thread, item });
                }
            }
            if let Err(err) = self.store.sync() {
                log::warn!("sync failed: {err}");
            }
            self.last_persist = Instant::now();
        }
    }

    pub fn shutdown(&mut self) {
        for (thread, id) in std::mem::take(&mut self.dirty) {
            if let Some(item) = self.item(thread, id) {
                self.emit(Event::ItemUpserted { thread, item });
            }
        }
        for runtime in self.runtimes.values_mut() {
            if let Some(session) = runtime.session.take() {
                session.send(SessionInput::Shutdown);
            }
        }
        let _ = self.store.sync();
    }

    // ── Providers ────────────────────────────────────────────────────────

    pub fn refresh_providers(&self) {
        for kind in ProviderKind::ALL {
            let tx = self.internal_tx.clone();
            tokio::spawn(async move {
                let info = provider::probe(kind, None).await;
                let _ = tx.send(Internal::Probed(info));
            });
        }
    }

    fn set_provider(&mut self, mut info: ProviderInfo) {
        if let Some(slot) = self.providers.iter_mut().find(|p| p.kind == info.kind) {
            if info.quota.is_empty() {
                info.quota = std::mem::take(&mut slot.quota);
            }
            *slot = info;
        } else {
            self.providers.push(info);
        }
        self.emit(Event::Providers(self.providers.clone()));
    }

    pub fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Probed(info) => self.set_provider(info),
            Internal::BaselineReady { thread, tree } => {
                let Some(runtime) = self.runtimes.get_mut(&thread) else { return };
                if let Some(turn) = runtime.turn.as_mut() {
                    turn.base = tree;
                }
                if let Some(prompt) = runtime.held_prompt.take() {
                    let delivered = runtime.session.as_ref().is_some_and(|s| s.send(prompt));
                    if !delivered {
                        self.finish_turn(
                            thread,
                            TurnOutcome::Failed { message: "The agent stopped before it received the message.".into() },
                            Usage::default(),
                            0,
                        );
                    }
                }
            }
            Internal::Checkpointed { thread, item, tree, files } => {
                if let Some(runtime) = self.runtimes.get_mut(&thread) {
                    runtime.checkpointing = false;
                }
                if self.store.thread(thread).is_some()
                    && self.store.load_timeline(thread).is_ok()
                    && let Some(mut end) = self.item(thread, item)
                    && let ItemBody::TurnEnd(summary) = &mut end.body
                {
                    summary.checkpoint = tree;
                    if let Some(files) = files {
                        summary.files = files;
                    }
                    self.put_item(thread, end);
                }
                self.start_next_queued(thread);
                if !self.open.contains(&thread) && self.runtimes.get(&thread).is_none_or(|r| r.turn.is_none()) {
                    self.store.unload_timeline(thread);
                }
            }
            Internal::ProjectBranch { project, branch } => {
                if let Some(mut p) = self.store.project(project).cloned()
                    && p.base_branch != branch
                {
                    p.base_branch = branch;
                    self.emit(Event::ProjectUpserted(p));
                }
            }
            Internal::QuotaReset(thread) => {
                let limited = matches!(
                    self.store.thread(thread).map(|t| &t.status),
                    Some(ThreadStatus::RateLimited { .. })
                );
                if limited {
                    self.update_thread(thread, |t| t.status = ThreadStatus::Idle);
                    self.notice(thread, NoticeLevel::Info, "Your quota has reset.".into());
                    self.start_next_queued(thread);
                }
            }
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// Applies a command. Slow commands return [`Outcome::Deferred`]; the
    /// engine loop runs the future in a task and replies when it finishes.
    pub fn handle(&mut self, command: Command) -> Outcome {
        match command {
            Command::Search { query, limit } => Outcome::Deferred(self.search(query, limit as usize)),
            command @ (Command::Diff { .. }
            | Command::RevertPatch { .. }
            | Command::RestoreCheckpoint { .. }
            | Command::Commit { .. }
            | Command::CreatePullRequest { .. }
            | Command::Branches { .. }) => self.git_command(command),
            command => Outcome::Ready(self.handle_command(command)),
        }
    }

    pub fn handle_command(&mut self, command: Command) -> CommandResult {
        match command {
            Command::AddProject { path } => self.add_project(&path),
            Command::UpdateProject { project } => {
                if self.store.project(project.id).is_none() {
                    return Err("That project no longer exists.".into());
                }
                self.emit(Event::ProjectUpserted(project));
                Ok(Reply::Done)
            }
            Command::RemoveProject { id } => {
                let threads: Vec<ThreadId> =
                    self.store.threads().filter(|t| t.project == Some(id)).map(|t| t.id).collect();
                for thread in threads {
                    self.delete_thread(thread)?;
                }
                self.emit(Event::ProjectRemoved(id));
                Ok(Reply::Done)
            }
            Command::CreateThread { project, config, worktree } => self.create_thread(project, config, worktree),
            Command::UpdateThread { id, patch } => self.patch_thread(id, patch),
            Command::DeleteThread { id } => self.delete_thread(id),
            Command::ForkThread { id, at } => self.fork_thread(id, at),
            Command::OpenThread { id } => self.open_thread(id),
            Command::CloseThread { id } => {
                self.open.remove(&id);
                let busy = self.runtimes.get(&id).is_some_and(|r| r.turn.is_some());
                if !busy {
                    self.flush_thread(id);
                    self.store.unload_timeline(id);
                }
                Ok(Reply::Done)
            }
            Command::Send { thread, text, attachments, steer } => self.send(thread, text, attachments, steer),
            Command::EditQueued { thread, item, text } => self.edit_queued(thread, item, text),
            Command::EditAndResend { thread, item, text } => self.edit_and_resend(thread, item, text),
            Command::Interrupt { thread } => self.interrupt(thread),
            Command::Resolve { thread, request, decision } => self.resolve(thread, request, decision),
            Command::Search { query, limit } => {
                // Synchronous callers (tests) get a blocking scan.
                let future = self.search(query, limit as usize);
                futures_block_on(future)
            }
            Command::PutBlob { name, mime, data } => self
                .store
                .put_blob(name, mime, &data)
                .map(Reply::Blob)
                .map_err(|e| format!("Couldn’t store the attachment: {e}")),
            Command::RefreshProviders => {
                self.refresh_providers();
                Ok(Reply::Done)
            }
            Command::SetDefaults { config } => {
                self.emit(Event::DefaultsChanged(config));
                Ok(Reply::Done)
            }
            Command::Diff { .. }
            | Command::RevertPatch { .. }
            | Command::RestoreCheckpoint { .. }
            | Command::Commit { .. }
            | Command::CreatePullRequest { .. }
            | Command::Branches { .. } => futures_block_on(match self.git_command(command) {
                Outcome::Ready(result) => return result,
                Outcome::Deferred(future) => future,
            }),
            command @ (Command::OpenTerminal { .. }
            | Command::TerminalInput { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal { .. }) => self.terminal_command(command),
        }
    }

    fn add_project(&mut self, path: &str) -> CommandResult {
        let path = Path::new(path);
        let canonical = dunce_canonical(path).map_err(|e| format!("Couldn’t open “{}”: {e}", path.display()))?;
        if !canonical.is_dir() {
            return Err(format!("“{}” is not a folder.", canonical.display()));
        }
        let path_str = canonical.display().to_string();
        if let Some(existing) = self.store.projects().iter().find(|p| p.path == path_str) {
            return Ok(Reply::Project(existing.id));
        }
        let name = canonical.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path_str.clone());
        let order = self.store.projects().iter().map(|p| p.order).max().unwrap_or(0) + 1;
        let project = Project {
            id: ProjectId::new(),
            name,
            path: path_str,
            order,
            scripts: ProjectScripts::default(),
            base_branch: None,
        };
        let id = project.id;
        let path = PathBuf::from(&project.path);
        self.emit(Event::ProjectUpserted(project));
        let (git, tx) = (self.git.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let branch = git.current_branch(&path).await.ok().flatten();
            let _ = tx.send(Internal::ProjectBranch { project: id, branch });
        });
        Ok(Reply::Project(id))
    }

    fn create_thread(&mut self, project: Option<ProjectId>, config: ThreadConfig, worktree: bool) -> CommandResult {
        if let Some(project) = project
            && self.store.project(project).is_none()
        {
            return Err("That project no longer exists.".into());
        }
        if worktree {
            return Err("Worktrees are not available in this build.".into());
        }
        let now = now_ms();
        let thread = Thread {
            id: ThreadId::new(),
            project,
            title: String::new(),
            config: config.clone(),
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        };
        let id = thread.id;
        self.emit(Event::ThreadUpserted(thread));
        if *self.store.defaults() != config {
            self.emit(Event::DefaultsChanged(config));
        }
        Ok(Reply::Thread(id))
    }

    fn patch_thread(&mut self, id: ThreadId, patch: ThreadPatch) -> CommandResult {
        if self.store.thread(id).is_none() {
            return Err("That thread no longer exists.".into());
        }
        let config = patch.config.clone();
        self.update_thread(id, |t| {
            if let Some(title) = patch.title {
                t.title = title;
            }
            if let Some(config) = patch.config {
                t.config = config;
            }
            if let Some(archived) = patch.archived {
                t.archived = archived;
            }
            if let Some(pinned) = patch.pinned {
                t.pinned = pinned;
            }
            if let Some(unread) = patch.unread {
                t.unread = unread;
            }
        });
        if let Some(config) = config {
            if let Some(session) = self.runtimes.get(&id).and_then(|r| r.session.as_ref()) {
                session.send(SessionInput::Configure(config.clone()));
            }
            self.emit(Event::DefaultsChanged(config));
        }
        Ok(Reply::Done)
    }

    fn delete_thread(&mut self, id: ThreadId) -> CommandResult {
        if let Some(repo) = self.store.thread(id).and_then(|t| self.repo_of(t)) {
            let git = self.git.clone();
            tokio::spawn(async move {
                if let Err(err) = git.delete_thread(&repo, id).await {
                    log::debug!("checkpoint cleanup for {id}: {err}");
                }
            });
        }
        if let Some(mut runtime) = self.runtimes.remove(&id)
            && let Some(session) = runtime.session.take()
        {
            session.send(SessionInput::Shutdown);
        }
        self.open.remove(&id);
        self.dirty.retain(|(t, _)| *t != id);
        self.emit(Event::ThreadRemoved(id));
        if let Err(err) = self.store.collect_garbage() {
            log::warn!("blob cleanup failed: {err}");
        }
        Ok(Reply::Done)
    }

    fn fork_thread(&mut self, id: ThreadId, at: ItemId) -> CommandResult {
        let source = self.store.thread(id).cloned().ok_or("That thread no longer exists.")?;
        let items: Vec<Item> = {
            let timeline = self.store.load_timeline(id).map_err(|e| e.to_string())?;
            let end = timeline.items().iter().position(|i| i.id == at).ok_or("That message no longer exists.")?;
            timeline.items()[..=end].to_vec()
        };
        let cursor = items.iter().rev().find_map(|i| match &i.body {
            ItemBody::TurnEnd(summary) => summary.provider_cursor.clone(),
            _ => None,
        });
        let now = now_ms();
        let fork = Thread {
            id: ThreadId::new(),
            project: source.project,
            title: format!("{} (fork)", display_title(&source)),
            config: source.config.clone(),
            created_ms: now,
            updated_ms: now,
            parent: Some(id),
            turns: items.iter().map(|i| i.turn).max().unwrap_or(0),
            ..Default::default()
        };
        let fork_id = fork.id;
        self.emit(Event::ThreadUpserted(fork));
        for item in items {
            let item = Item { id: ItemId::new(), ..item };
            self.put_item(fork_id, item);
        }
        // The fork's first turn resumes the source's provider history at the
        // fork point, in a new provider session.
        self.runtimes.entry(fork_id).or_default().resume_at = cursor;
        if let Some(session) = source.session {
            self.update_thread(fork_id, |t| t.session = Some(session));
        }
        Ok(Reply::Thread(fork_id))
    }

    fn open_thread(&mut self, id: ThreadId) -> CommandResult {
        if self.store.thread(id).is_none() {
            return Err("That thread no longer exists.".into());
        }
        self.open.insert(id);
        let items = self.store.load_timeline(id).map_err(|e| e.to_string())?.items().to_vec();
        self.update_thread(id, |t| t.unread = false);
        Ok(Reply::Timeline(items))
    }

    fn flush_thread(&mut self, id: ThreadId) {
        let dirty: Vec<ItemId> = self.dirty.iter().filter(|(t, _)| *t == id).map(|(_, i)| *i).collect();
        for item in dirty {
            if let Some(item) = self.item(id, item) {
                self.put_item(id, item);
            }
        }
    }

    // ── Turns ────────────────────────────────────────────────────────────

    fn send(&mut self, thread_id: ThreadId, text: String, attachments: Vec<Attachment>, steer: bool) -> CommandResult {
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        if text.trim().is_empty() && attachments.is_empty() {
            return Err("Write a message first.".into());
        }
        self.store.load_timeline(thread_id).map_err(|e| e.to_string())?;
        let in_turn = self.runtimes.get(&thread_id).is_some_and(|r| r.turn.is_some());
        // Between a turn's end and its snapshot nothing can be steered; new
        // messages queue behind the snapshot instead.
        let busy = in_turn || self.runtimes.get(&thread_id).is_some_and(|r| r.checkpointing);
        let rate_limited = matches!(thread.status, ThreadStatus::RateLimited { .. });
        let item_id = ItemId::new();
        let current_turn = thread.turns;
        let steering = in_turn && steer;
        let item = Item {
            id: item_id,
            turn: if steering { current_turn } else { current_turn + 1 },
            status: if (busy && !steer) || rate_limited { ItemStatus::Queued } else { ItemStatus::Done },
            body: ItemBody::User(UserMessage { text: text.clone(), attachments: attachments.clone(), steer: steering }),
        };
        self.put_item(thread_id, item);
        if thread.title.is_empty() {
            let title = title_from(&text);
            self.update_thread(thread_id, |t| t.title = title);
        }
        if steering {
            let prompt = self.prompt_input(thread_id, item_id, text, &attachments, true);
            if let Some(session) = self.runtimes.get(&thread_id).and_then(|r| r.session.as_ref()) {
                session.send(prompt);
            }
        } else if busy || rate_limited {
            self.runtimes.entry(thread_id).or_default().queue.push_back(item_id);
            self.update_thread(thread_id, |t| t.queued += 1);
        } else {
            self.start_turn(thread_id, item_id)?;
        }
        Ok(Reply::Done)
    }

    fn prompt_input(
        &self,
        _thread: ThreadId,
        item: ItemId,
        text: String,
        attachments: &[Attachment],
        steer: bool,
    ) -> SessionInput {
        let attachments = attachments
            .iter()
            .map(|a| PromptAttachment { attachment: a.clone(), path: self.store.blob_path(&a.hash) })
            .collect();
        SessionInput::Prompt { uuid: prompt_uuid(item), text, attachments, steer }
    }

    fn workspace_dir(&self, thread: &Thread) -> PathBuf {
        if let Some(worktree) = &thread.workspace.worktree {
            return PathBuf::from(worktree);
        }
        if let Some(project) = thread.project.and_then(|p| self.store.project(p)) {
            return PathBuf::from(&project.path);
        }
        let dir = self.store.dir().join("scratch").join(thread.id.to_string());
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn ensure_session(&mut self, thread_id: ThreadId) -> Result<(), String> {
        if self.runtimes.get(&thread_id).is_some_and(|r| r.session.is_some()) {
            return Ok(());
        }
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        let cwd = self.workspace_dir(&thread);
        let mut env = Vec::new();
        if let Some(port) = thread.workspace.port_base {
            env.push(("PORT".to_owned(), port.to_string()));
            env.push(("ORBIT_PORT_BASE".to_owned(), port.to_string()));
        }
        let runtime = self.runtimes.entry(thread_id).or_default();
        let spec = SessionSpec {
            thread: thread_id,
            cwd,
            config: thread.config.clone(),
            resume: thread.session.clone(),
            resume_at: runtime.resume_at.take(),
            binary: None,
            env,
            extra_dirs: vec![self.store.dir().join("blobs")],
        };
        let session = (self.launcher)(thread.config.provider, spec, self.agent_tx.clone());
        self.runtimes.get_mut(&thread_id).unwrap().session = Some(session);
        Ok(())
    }

    fn start_turn(&mut self, thread_id: ThreadId, user_item: ItemId) -> Result<(), String> {
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        let number = thread.turns + 1;
        let Some(mut item) = self.item(thread_id, user_item) else {
            return Err("That message no longer exists.".into());
        };
        let ItemBody::User(message) = &item.body else {
            return Err("Only messages can start a turn.".into());
        };
        let (text, attachments) = (message.text.clone(), message.attachments.clone());
        if item.status != ItemStatus::Done || item.turn != number {
            item.status = ItemStatus::Done;
            item.turn = number;
            self.put_item(thread_id, item);
        }
        self.ensure_session(thread_id)?;
        let prompt = self.prompt_input(thread_id, user_item, text, &attachments, false);
        let base = self.last_checkpoint(thread_id);
        let repo = self.repo_of(&thread);
        let runtime = self.runtimes.get_mut(&thread_id).unwrap();
        let mut turn = ActiveTurn::new(number);
        turn.base = base.clone();
        runtime.turn = Some(turn);
        let delivered = if base.is_none() && let Some(repo) = repo {
            // First turn in a repository: snapshot the starting point before
            // the agent can touch anything, then send.
            runtime.held_prompt = Some(prompt);
            let (git, tx) = (self.git.clone(), self.internal_tx.clone());
            tokio::spawn(async move {
                let tree = match git.snapshot(&repo).await {
                    Ok(tree) => Some(tree.as_str().to_owned()),
                    Err(err) => {
                        log::debug!("no baseline for {thread_id}: {err}");
                        None
                    }
                };
                let _ = tx.send(Internal::BaselineReady { thread: thread_id, tree });
            });
            true
        } else {
            runtime.session.as_ref().is_some_and(|s| s.send(prompt))
        };
        self.update_thread(thread_id, |t| {
            t.turns = number;
            t.status = ThreadStatus::Running;
        });
        if !delivered {
            self.finish_turn(
                thread_id,
                TurnOutcome::Failed { message: "The agent stopped before it received the message.".into() },
                Usage::default(),
                0,
            );
        }
        Ok(())
    }

    fn start_next_queued(&mut self, thread_id: ThreadId) {
        if self.runtimes.get(&thread_id).is_some_and(|r| r.checkpointing) {
            return; // resumed when the snapshot lands
        }
        let Some(next) = self.runtimes.get_mut(&thread_id).and_then(|r| r.queue.pop_front()) else { return };
        self.update_thread(thread_id, |t| t.queued = t.queued.saturating_sub(1));
        if let Err(err) = self.start_turn(thread_id, next) {
            self.notice(thread_id, NoticeLevel::Error, err);
        }
    }

    fn edit_queued(&mut self, thread: ThreadId, item_id: ItemId, text: String) -> CommandResult {
        let item = self.item(thread, item_id).ok_or("That message no longer exists.")?;
        if item.status != ItemStatus::Queued {
            return Err("That message was already sent.".into());
        }
        if text.trim().is_empty() {
            if let Some(runtime) = self.runtimes.get_mut(&thread) {
                runtime.queue.retain(|id| *id != item_id);
            }
            self.update_thread(thread, |t| t.queued = t.queued.saturating_sub(1));
            self.emit(Event::ItemRemoved { thread, item: item_id });
        } else if let ItemBody::User(mut message) = item.body {
            message.text = text;
            self.put_item(thread, Item { body: ItemBody::User(message), ..item });
        }
        Ok(Reply::Done)
    }

    fn edit_and_resend(&mut self, thread_id: ThreadId, item_id: ItemId, text: String) -> CommandResult {
        if self.runtimes.get(&thread_id).is_some_and(|r| r.turn.is_some()) {
            return Err("Stop the current turn before editing an earlier message.".into());
        }
        let (target_turn, removed, cursor) = {
            let timeline = self.store.load_timeline(thread_id).map_err(|e| e.to_string())?;
            let ix = timeline.items().iter().position(|i| i.id == item_id).ok_or("That message no longer exists.")?;
            let target = &timeline.items()[ix];
            if !matches!(target.body, ItemBody::User(_)) {
                return Err("Only your own messages can be edited.".into());
            }
            let turn = target.turn;
            let removed: Vec<ItemId> = timeline.items()[ix..].iter().map(|i| i.id).collect();
            let cursor = timeline.items()[..ix].iter().rev().find_map(|i| match &i.body {
                ItemBody::TurnEnd(summary) => summary.provider_cursor.clone(),
                _ => None,
            });
            (turn, removed, cursor)
        };
        for id in removed {
            self.emit(Event::ItemRemoved { thread: thread_id, item: id });
        }
        // Restart the provider session at the cursor before the edited turn so
        // its own history forgets the replaced turns too.
        let runtime = self.runtimes.entry(thread_id).or_default();
        if let Some(session) = runtime.session.take() {
            session.send(SessionInput::Shutdown);
        }
        runtime.resume_at = cursor.clone();
        let fresh = cursor.is_none();
        self.update_thread(thread_id, |t| {
            t.turns = target_turn.saturating_sub(1);
            if fresh {
                t.session = None;
            }
        });
        self.send(thread_id, text, Vec::new(), false)
    }

    fn interrupt(&mut self, thread: ThreadId) -> CommandResult {
        let Some(runtime) = self.runtimes.get_mut(&thread) else { return Ok(Reply::Done) };
        let Some(turn) = runtime.turn.as_mut() else { return Ok(Reply::Done) };
        turn.interrupted = true;
        if let Some(session) = runtime.session.as_ref()
            && session.send(SessionInput::Interrupt)
        {
            return Ok(Reply::Done);
        }
        self.finish_turn(thread, TurnOutcome::Interrupted, Usage::default(), 0);
        Ok(Reply::Done)
    }

    fn resolve(&mut self, thread: ThreadId, request: RequestId, decision: Decision) -> CommandResult {
        let pending = self
            .store
            .thread(thread)
            .and_then(|t| t.pending.iter().find(|p| p.request.id == request).cloned())
            .ok_or("That request was already answered.")?;
        let provider_ref = pending.request.provider_ref.clone();
        let delivered = self
            .runtimes
            .get(&thread)
            .and_then(|r| r.session.as_ref())
            .is_some_and(|s| s.send(SessionInput::Resolve { provider_ref, decision: decision.clone() }));
        if !delivered {
            return Err("The agent is no longer running, so it can’t receive your answer.".into());
        }
        self.settle_request(thread, request, decision);
        Ok(Reply::Done)
    }

    fn settle_request(&mut self, thread: ThreadId, request: RequestId, decision: Decision) {
        let Some(pending) =
            self.store.thread(thread).and_then(|t| t.pending.iter().find(|p| p.request.id == request).cloned())
        else {
            return;
        };
        if let Some(mut item) = self.item(thread, pending.item)
            && let ItemBody::Request(req) = &mut item.body
        {
            req.resolution = Some(decision);
            item.status = ItemStatus::Done;
            self.put_item(thread, item);
        }
        let busy = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
        self.update_thread(thread, |t| {
            t.pending.retain(|p| p.request.id != request);
            if t.pending.is_empty() && t.status == ThreadStatus::Waiting {
                t.status = if busy { ThreadStatus::Running } else { ThreadStatus::Idle };
            }
        });
    }

    fn notice(&mut self, thread: ThreadId, level: NoticeLevel, text: String) {
        if self.store.load_timeline(thread).is_err() {
            return;
        }
        let turn = self.store.thread(thread).map(|t| t.turns).unwrap_or(0);
        self.put_item(
            thread,
            Item { id: ItemId::new(), turn, status: ItemStatus::Done, body: ItemBody::Notice { level, text } },
        );
    }

    fn finish_turn(&mut self, thread_id: ThreadId, outcome: TurnOutcome, usage: Usage, duration_ms: u64) {
        let Some(runtime) = self.runtimes.get_mut(&thread_id) else { return };
        let Some(turn) = runtime.turn.take() else { return };
        // Settle everything still streaming in this turn.
        let open_items: Vec<ItemId> = turn.blocks.values().chain(turn.tools.values()).copied().collect();
        let interrupted = turn.interrupted || outcome == TurnOutcome::Interrupted;
        for id in open_items {
            if let Some(mut item) = self.item(thread_id, id)
                && item.status == ItemStatus::Streaming
            {
                item.status = if interrupted { ItemStatus::Cancelled } else { ItemStatus::Done };
                self.put_item(thread_id, item);
            }
        }
        let mut total = turn.usage;
        if usage != Usage::default() {
            total = usage;
        }
        let duration = if duration_ms > 0 { duration_ms } else { turn.started.elapsed().as_millis() as u64 };
        let summary = TurnSummary {
            usage: total,
            provider_cursor: turn.cursor.clone(),
            duration_ms: duration,
            checkpoint: None,
            base_checkpoint: turn.base.clone(),
            files: turn.files.clone(),
            interrupted,
        };
        let end_item = ItemId::new();
        self.put_item(
            thread_id,
            Item { id: end_item, turn: turn.number, status: ItemStatus::Done, body: ItemBody::TurnEnd(summary) },
        );
        // Snapshot what the turn left behind; its diff replaces the
        // tool-reported file list with what actually changed on disk.
        if let Some(repo) = self.store.thread(thread_id).and_then(|t| self.repo_of(t)) {
            if let Some(runtime) = self.runtimes.get_mut(&thread_id) {
                runtime.checkpointing = true;
            }
            let (git, tx, base) = (self.git.clone(), self.internal_tx.clone(), turn.base.clone());
            tokio::spawn(async move {
                let (tree, files) = match git.snapshot(&repo).await {
                    Ok(tree) => {
                        let files = match &base {
                            Some(base) => git.diff(&repo, &TreeId::new(base.clone()), Some(&tree)).await.ok().map(|d| d.changes()),
                            None => None,
                        };
                        (Some(tree.as_str().to_owned()), files)
                    }
                    Err(err) => {
                        log::debug!("no checkpoint for {thread_id}: {err}");
                        (None, None)
                    }
                };
                let _ = tx.send(Internal::Checkpointed { thread: thread_id, item: end_item, tree, files });
            });
        }
        let status = match &outcome {
            TurnOutcome::Completed | TurnOutcome::Interrupted => ThreadStatus::Idle,
            TurnOutcome::Failed { message } => ThreadStatus::Failed { message: message.clone() },
            TurnOutcome::RateLimited { resets_ms } => ThreadStatus::RateLimited { resets_ms: *resets_ms },
        };
        if let TurnOutcome::Failed { message } = &outcome {
            self.notice(thread_id, NoticeLevel::Error, message.clone());
        }
        let open = self.open.contains(&thread_id);
        self.update_thread(thread_id, |t| {
            t.usage.add_turn(&total);
            t.status = status;
            t.unread = !open;
            t.pending.clear();
        });
        match outcome {
            TurnOutcome::RateLimited { resets_ms } => self.schedule_quota_reset(thread_id, resets_ms),
            TurnOutcome::Completed | TurnOutcome::Interrupted => self.start_next_queued(thread_id),
            TurnOutcome::Failed { .. } => {}
        }
        if !self.open.contains(&thread_id) && self.runtimes.get(&thread_id).is_none_or(|r| r.turn.is_none()) {
            self.flush_thread(thread_id);
            self.store.unload_timeline(thread_id);
        }
    }

    fn schedule_quota_reset(&self, thread: ThreadId, resets_ms: u64) {
        if resets_ms == 0 {
            return;
        }
        let wait = Duration::from_millis(resets_ms.saturating_sub(now_ms()) + 5_000);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = tx.send(Internal::QuotaReset(thread));
        });
    }

    // ── Agent events ─────────────────────────────────────────────────────

    pub fn handle_agent(&mut self, thread_id: ThreadId, event: AgentEvent) {
        if self.store.thread(thread_id).is_none() {
            return; // deleted while the session was still reporting
        }
        if self.store.load_timeline(thread_id).is_err() {
            return;
        }
        match event {
            AgentEvent::Ready { session_id, .. } => {
                self.update_thread(thread_id, |t| t.session = Some(session_id));
            }
            AgentEvent::Catalog(info) => self.set_provider(info),
            AgentEvent::TurnStarted { prompt } => {
                let thread = self.store.thread(thread_id).cloned().unwrap();
                let runtime = self.runtimes.entry(thread_id).or_default();
                if runtime.turn.is_none() && prompt.is_none() {
                    // The agent started a turn on its own (background work
                    // finished): track it like any other turn.
                    runtime.turn = Some(ActiveTurn::new(thread.turns + 1));
                    self.update_thread(thread_id, |t| {
                        t.turns += 1;
                        t.status = ThreadStatus::Running;
                    });
                }
            }
            AgentEvent::PromptAccepted { .. } => {}
            AgentEvent::Delta { block, kind: BlockKind::ToolInput, text } => {
                // `block` is the tool call id; the call's card already exists.
                if let Some(id) = self.tool_item(thread_id, &block) {
                    self.stream_into(thread_id, id, Delta::ToolInput(text));
                }
            }
            AgentEvent::BlockDone { kind: BlockKind::ToolInput, .. } => {
                // The adapter re-sends ToolStarted with the complete input.
            }
            AgentEvent::Delta { block, kind, text } => {
                let id = self.ensure_block(thread_id, &block, kind);
                if let Some(id) = id {
                    let delta = match kind {
                        BlockKind::Reasoning => Delta::Reasoning(text),
                        _ => Delta::Text(text),
                    };
                    self.stream_into(thread_id, id, delta);
                }
            }
            AgentEvent::BlockDone { block, kind, text } => {
                if let Some(id) = self.ensure_block(thread_id, &block, kind)
                    && let Some(mut item) = self.item(thread_id, id)
                {
                    item.body = match kind {
                        BlockKind::Reasoning => ItemBody::Reasoning { text },
                        _ => ItemBody::Assistant { text },
                    };
                    item.status = ItemStatus::Done;
                    self.put_item(thread_id, item);
                }
            }
            AgentEvent::ToolStarted(call) => {
                let Some(turn) = self.active_turn_number(thread_id) else { return };
                let call_id = call.call_id.clone();
                let existing = self.runtimes.get(&thread_id).and_then(|r| r.turn.as_ref()?.tools.get(&call_id).copied());
                let id = match existing {
                    Some(id) => id,
                    None => ItemId::new(),
                };
                self.put_item(thread_id, Item { id, turn, status: ItemStatus::Streaming, body: ItemBody::Tool(call) });
                if let Some(t) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    t.tools.insert(call_id, id);
                }
            }
            AgentEvent::ToolOutput { call_id, text } => {
                if let Some(id) = self.tool_item(thread_id, &call_id) {
                    self.stream_into(thread_id, id, Delta::ToolOutput(text));
                }
            }
            AgentEvent::ToolFinished { call_id, output, is_error, exit_code, files } => {
                let Some(id) = self.tool_item(thread_id, &call_id) else { return };
                let Some(mut item) = self.item(thread_id, id) else { return };
                if let ItemBody::Tool(call) = &mut item.body {
                    if output.len() > INLINE_OUTPUT {
                        let mut cut = INLINE_OUTPUT;
                        while !output.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        call.output_blob = self
                            .store
                            .put_blob(format!("{call_id}.txt"), "text/plain".into(), output.as_bytes())
                            .ok()
                            .map(|a| a.hash);
                        call.output = output[..cut].to_owned();
                    } else if !output.is_empty() {
                        call.output = output;
                    }
                    call.exit_code = exit_code;
                    if !files.is_empty() {
                        call.files = files.clone();
                    }
                }
                item.status = if is_error { ItemStatus::Failed } else { ItemStatus::Done };
                self.put_item(thread_id, item);
                if let Some(turn) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    for file in files {
                        merge_file_change(&mut turn.files, file);
                    }
                }
            }
            AgentEvent::Plan(steps) => {
                let Some(turn) = self.active_turn_number(thread_id) else { return };
                let existing = self.runtimes.get(&thread_id).and_then(|r| r.turn.as_ref()?.plan);
                let id = match existing {
                    Some(id) => id,
                    None => ItemId::new(),
                };
                self.put_item(thread_id, Item { id, turn, status: ItemStatus::Done, body: ItemBody::Plan { steps } });
                if let Some(t) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    t.plan = Some(id);
                }
            }
            AgentEvent::Request { provider_ref, kind } => {
                let turn = self.active_turn_number(thread_id).unwrap_or_else(|| {
                    self.store.thread(thread_id).map(|t| t.turns).unwrap_or(0)
                });
                let request = AgentRequest { id: RequestId::new(), provider_ref: provider_ref.clone(), kind, resolution: None };
                let item = Item { id: ItemId::new(), turn, status: ItemStatus::Streaming, body: ItemBody::Request(request.clone()) };
                let pending = PendingRequest { item: item.id, request: request.clone(), created_ms: now_ms() };
                if let Some(t) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    t.requests.insert(provider_ref, (request.id, item.id));
                }
                self.put_item(thread_id, item);
                self.update_thread(thread_id, |t| {
                    t.pending.push(pending);
                    t.status = ThreadStatus::Waiting;
                    if !t.unread {
                        t.unread = true;
                    }
                });
            }
            AgentEvent::RequestWithdrawn { provider_ref } => {
                let request = self
                    .store
                    .thread(thread_id)
                    .and_then(|t| t.pending.iter().find(|p| p.request.provider_ref == provider_ref))
                    .map(|p| p.request.id);
                if let Some(request) = request {
                    self.settle_request(thread_id, request, Decision::Deny { feedback: "Withdrawn by the agent.".into() });
                }
            }
            AgentEvent::Usage(usage) => {
                if let Some(t) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    t.usage = usage;
                }
                let mut live = self.store.thread(thread_id).map(|t| t.usage).unwrap_or_default();
                live.add_turn(&usage);
                self.emit(Event::UsageChanged { thread: thread_id, usage: live });
            }
            AgentEvent::Quota { window, utilization, resets_ms, rejected } => {
                let kind = self.store.thread(thread_id).map(|t| t.config.provider).unwrap_or_default();
                if let Some(info) = self.providers.iter().find(|p| p.kind == kind).cloned() {
                    let mut info = info;
                    let entry = QuotaWindow { window: window.clone(), utilization, resets_ms, rejected };
                    match info.quota.iter_mut().find(|q| q.window == window) {
                        Some(slot) => *slot = entry,
                        None => info.quota.push(entry),
                    }
                    self.set_provider(info);
                }
            }
            AgentEvent::Notice { level, text } => self.notice(thread_id, level, text),
            AgentEvent::Checkpoint { provider_message } => {
                if let Some(t) = self.runtimes.get_mut(&thread_id).and_then(|r| r.turn.as_mut()) {
                    t.cursor = Some(provider_message);
                }
            }
            AgentEvent::TurnFinished { outcome, usage, duration_ms } => {
                self.finish_turn(thread_id, outcome, usage, duration_ms)
            }
            AgentEvent::TitleChanged(title) => self.update_thread(thread_id, |t| t.title = title),
            AgentEvent::Exited { error } => {
                if let Some(runtime) = self.runtimes.get_mut(&thread_id) {
                    runtime.session = None;
                }
                let busy = self.runtimes.get(&thread_id).is_some_and(|r| r.turn.is_some());
                if busy {
                    let message = error.unwrap_or_else(|| "The agent exited unexpectedly.".into());
                    self.finish_turn(thread_id, TurnOutcome::Failed { message }, Usage::default(), 0);
                } else if let Some(error) = error {
                    self.notice(thread_id, NoticeLevel::Error, error);
                }
            }
        }
    }

    fn active_turn_number(&self, thread: ThreadId) -> Option<u32> {
        self.runtimes.get(&thread)?.turn.as_ref().map(|t| t.number)
    }

    fn tool_item(&self, thread: ThreadId, call_id: &str) -> Option<ItemId> {
        self.runtimes.get(&thread)?.turn.as_ref()?.tools.get(call_id).copied()
    }

    /// The timeline item for a streamed block, created on first use.
    fn ensure_block(&mut self, thread: ThreadId, block: &str, kind: BlockKind) -> Option<ItemId> {
        let turn = self.runtimes.get_mut(&thread)?.turn.as_mut()?;
        if let Some(id) = turn.blocks.get(block) {
            return Some(*id);
        }
        let id = ItemId::new();
        turn.blocks.insert(block.to_owned(), id);
        let number = turn.number;
        let body = match kind {
            BlockKind::Reasoning => ItemBody::Reasoning { text: String::new() },
            BlockKind::Text | BlockKind::ToolInput => ItemBody::Assistant { text: String::new() },
        };
        self.put_item(thread, Item { id, turn: number, status: ItemStatus::Streaming, body });
        Some(id)
    }

    // ── Search ───────────────────────────────────────────────────────────

    /// Full-text search over every thread, newest first. Loaded timelines
    /// are searched in memory; the rest are scanned from their logs on the
    /// blocking pool, so a search never stalls streaming.
    fn search(&mut self, query: String, limit: usize) -> Deferred {
        let needle = query.trim().to_lowercase();
        let mut threads: Vec<(ThreadId, u64)> = self.store.threads().map(|t| (t.id, t.updated_ms)).collect();
        threads.sort_by_key(|(_, updated)| std::cmp::Reverse(*updated));
        let mut loaded: HashMap<ThreadId, Vec<(ItemId, String)>> = HashMap::new();
        for (id, _) in &threads {
            if let Some(timeline) = self.store.timeline(*id) {
                loaded.insert(*id, timeline.items().iter().filter_map(searchable).collect());
            }
        }
        let dir = self.store.dir().to_owned();
        Box::pin(async move {
            if needle.is_empty() {
                return Ok(Reply::SearchHits(Vec::new()));
            }
            tokio::task::spawn_blocking(move || {
                let mut hits = Vec::new();
                for (id, _) in threads {
                    let items = match loaded.remove(&id) {
                        Some(items) => items,
                        None => Store::scan_timeline(&dir, id).iter().filter_map(searchable).collect(),
                    };
                    for (item, text) in items {
                        if let Some(excerpt) = excerpt(&text, &needle) {
                            hits.push(SearchHit { thread: id, item, excerpt });
                            if hits.len() >= limit {
                                return Ok(Reply::SearchHits(hits));
                            }
                        }
                    }
                }
                Ok(Reply::SearchHits(hits))
            })
            .await
            .unwrap_or_else(|e| Err(format!("Search failed: {e}")))
        })
    }
}

fn searchable(item: &Item) -> Option<(ItemId, String)> {
    let text = match &item.body {
        ItemBody::User(m) => m.text.clone(),
        ItemBody::Assistant { text } => text.clone(),
        ItemBody::Tool(call) => call.title.clone(),
        ItemBody::Plan { steps } => steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n"),
        _ => return None,
    };
    Some((item.id, text))
}

/// Runs a future to completion on a throwaway runtime (synchronous callers).
fn futures_block_on(future: Deferred) -> CommandResult {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(future)
}

/// A stable UUID-shaped id for a prompt, derived from its item id, so the
/// provider's acknowledgement maps straight back to the timeline item.
pub fn prompt_uuid(item: ItemId) -> String {
    let v = item.0;
    let mix = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (v >> 32) as u32,
        (v >> 16) as u16,
        (v & 0xfff) as u16,
        (mix >> 52) as u16 & 0xfff,
        mix & 0xffff_ffff_ffff
    )
}

fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New thread");
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

fn display_title(thread: &Thread) -> &str {
    if thread.title.is_empty() { "New thread" } else { &thread.title }
}

fn excerpt(text: &str, needle: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find(needle)?;
    // Map the byte offset in the lowercase copy back to a char position.
    let char_at = lower[..at].chars().count();
    let start = char_at.saturating_sub(40);
    let snippet: String = text.chars().skip(start).take(needle.chars().count() + 80).collect();
    Some(format!("{}{}", if start > 0 { "…" } else { "" }, snippet.trim()))
}

fn merge_file_change(files: &mut Vec<FileChange>, change: FileChange) {
    match files.iter_mut().find(|f| f.path == change.path) {
        Some(existing) => {
            existing.additions += change.additions;
            existing.deletions += change.deletions;
            if change.kind == ChangeKind::Deleted || existing.kind != ChangeKind::Added {
                existing.kind = change.kind;
            }
        }
        None => files.push(change),
    }
}

/// `canonicalize` without Windows' `\\?\` prefix, so paths stay readable and
/// work with tools that do not understand verbatim paths.
fn dunce_canonical(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let s = canonical.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(canonical)
}
