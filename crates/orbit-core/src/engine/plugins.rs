//! Plugins in the engine: [`Command::Plugin`], agent manifests as provider
//! instances, hooks around prompts and tool calls, and what plugins ask
//! the engine to do.
//!
//! The plugins themselves (files, consent, audit log, storage, the
//! WebAssembly sandbox) are `orbit_plugin::host`; this module wires them
//! in:
//!
//! * installs, previews and catalog reads run in tasks (fetching, git,
//!   unpacking and hashing never block the engine thread), then apply here;
//! * agent CLIs from active plugins become provider presets
//!   ([`presets::set_plugin_agents`]), probed like any other instance, with
//!   the manifest's models and modes when the agent reports none;
//! * `agent.prompt.before` gates [`Command::Send`]; `agent.tool.before` is
//!   the sessions' [`ToolPolicy`] (adapters ask it before a call runs) and
//!   also reviews requests from runtimes that ask the engine; observers get
//!   `agent.tool.after` and `thread.finished`;
//! * dev plugins are watched and reloaded when their files change;
//! * the signed kill list and catalogs are refreshed every few hours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossfire::BlockingTxTrait as _;
use orbit_plugin::host::{AgentDef, EngineEffect, HookContext, HostOptions, PluginHost, Resolved};
use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator, Outcome};
use crate::provider::{AgentEvent, ToolPolicy, TurnOutcome, presets};

/// Where the signed kill list is published (next to the update manifests),
/// read only by builds that carry the release key.
pub const KILL_LIST_URL: &str = "https://updates.orbit.invalid/plugins/kill-list.toml";
const REFRESH: Duration = Duration::from_secs(6 * 3600);
const RETRY: Duration = Duration::from_secs(15 * 60);
/// Dev folders settle this long after a change before reloading.
const DEV_SETTLE: Duration = Duration::from_millis(300);
/// Threads one plugin may start in [`THREADS_WINDOW`]: a hook that starts a
/// thread when one finishes can't start a chain reaction.
const THREADS_PER_WINDOW: usize = 10;
const THREADS_WINDOW: Duration = Duration::from_secs(10 * 60);

pub(crate) struct PluginRuntime {
    pub(crate) host: PluginHost,
    /// The agent definitions last applied to the presets.
    agents: Vec<AgentDef>,
    /// Terminal agents' command names last given to the status engine.
    terminal_agents: Vec<String>,
    watches: HashMap<String, DevWatch>,
    threads_seq: u64,
    next_refresh: Instant,
    /// When each plugin last started threads.
    started: HashMap<String, std::collections::VecDeque<Instant>>,
}

struct DevWatch {
    _watcher: notify::RecommendedWatcher,
    folder: PathBuf,
}

impl PluginRuntime {
    pub(crate) fn open(data_dir: &Path) -> Self {
        Self {
            host: PluginHost::open(HostOptions::new(data_dir)),
            agents: Vec::new(),
            terminal_agents: Vec::new(),
            watches: HashMap::new(),
            threads_seq: u64::MAX,
            next_refresh: Instant::now() + Duration::from_secs(20),
            started: HashMap::new(),
        }
    }
}

fn reply(reply: PluginReply) -> CommandResult {
    Ok(Reply::Plugin(Box::new(reply)))
}

impl Orchestrator {
    /// Applies the installed plugins once the engine is up.
    pub(crate) fn plugins_start(&mut self) {
        self.sync_plugins();
    }

    /// The plugin state changed: tell clients, and bring agents, watches
    /// and probes in line.
    pub(crate) fn plugins_changed(&mut self) {
        self.emit(Event::Plugins(Box::new(self.plugins.host.state())));
        self.sync_plugins();
    }

    fn sync_plugins(&mut self) {
        let agents = self.plugins.host.agents();
        if agents != self.plugins.agents {
            presets::set_plugin_agents(agents.iter().map(|a| a.instance.clone()).collect());
            let added: Vec<ProviderInstance> = agents
                .iter()
                .filter(|a| !self.plugins.agents.iter().any(|b| b.instance == a.instance))
                .map(|a| a.instance.clone())
                .collect();
            let gone = self.plugins.agents.iter().any(|b| !agents.iter().any(|a| a.instance.id == b.instance.id));
            self.plugins.agents = agents;
            for instance in added {
                self.probe_instance(&instance);
            }
            if gone {
                let before = self.providers.len();
                let effective = presets::effective(&self.store.settings().agents.instances);
                self.providers.retain(|p| effective.iter().any(|(i, _)| i.id == p.instance_id()));
                if self.providers.len() != before {
                    self.emit(Event::Providers(self.providers.clone()));
                }
            }
        }
        let state = self.plugins.host.state();
        let terminal: Vec<String> = state
            .active()
            .flat_map(|p| p.ui.agents.iter())
            .filter(|a| a.protocol == AgentProtocol::Terminal)
            .filter_map(|a| {
                let base = a.command.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
                Some([".exe", ".cmd", ".bat", ".ps1"].iter().find_map(|e| base.strip_suffix(e)).unwrap_or(&base).to_owned())
            })
            .collect();
        if terminal != self.plugins.terminal_agents {
            crate::status::term::set_plugin_agents(&terminal);
            self.plugins.terminal_agents = terminal;
        }
        self.sync_dev_watches();
    }

    /// A probe result for a plugin agent: the manifest's models and modes
    /// when the agent reports none, and how to install it when missing.
    pub(crate) fn plugin_overlay(&self, info: &mut ProviderInfo) {
        let Some(def) = self.plugins.agents.iter().find(|a| a.instance.id == info.instance_id()) else { return };
        // An ACP agent probed without a session reports only its
        // "Default" placeholder.
        if info.models.iter().all(|m| m.slug.is_empty()) && !def.models.is_empty() {
            info.models = def.models.clone();
        }
        if info.modes.is_empty() {
            info.modes = def.modes.clone();
        }
        if !info.installed && !def.install.is_empty() {
            info.message = Some(format!("Not installed. Install it with: {}", def.install));
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    pub(crate) fn plugin_command(&mut self, command: PluginCommand) -> Outcome {
        let host = self.plugins.host.clone();
        match command {
            PluginCommand::Preview(source) => {
                let resolved = match host.resolve(source) {
                    Ok(r) => r,
                    Err(e) => return Outcome::Ready(Err(e)),
                };
                Outcome::Deferred(Box::pin(async move {
                    let staged = stage(&host, resolved).await?;
                    let (plugin, replaces) = host.preview(&staged);
                    reply(PluginReply::Preview { plugin: Box::new(plugin), replaces })
                }))
            }
            PluginCommand::Install { source, grant } => {
                let resolved = match host.resolve(source) {
                    Ok(r) => r,
                    Err(e) => return Outcome::Ready(Err(e)),
                };
                let tx = self.internal_tx.clone();
                Outcome::Deferred(Box::pin(async move {
                    let staged = stage(&host, resolved).await?;
                    let info = host.commit(staged, &grant)?;
                    let _ = tx.send(Internal::Run(Box::new(|o: &mut Orchestrator| o.plugins_changed())));
                    reply(PluginReply::Installed(Box::new(info)))
                }))
            }
            PluginCommand::RefreshCatalogs => self.refresh_catalogs(true),
            PluginCommand::AddCatalog { name, location, public_key } => match host.add_catalog(&name, &location, &public_key) {
                Ok(()) => self.refresh_catalogs(true),
                Err(e) => Outcome::Ready(Err(e)),
            },
            command => Outcome::Ready(self.plugin_command_now(command)),
        }
    }

    fn plugin_command_now(&mut self, command: PluginCommand) -> CommandResult {
        let host = self.plugins.host.clone();
        let result = match command {
            PluginCommand::Enable { id } => host.set_enabled(&id, true).map(|_| PluginReply::Done),
            PluginCommand::Disable { id } => host.set_enabled(&id, false).map(|_| PluginReply::Done),
            PluginCommand::Grant { id, grant } => host.grant(&id, &grant).map(|_| PluginReply::Done),
            PluginCommand::Remove { id } => host.remove(&id).map(|_| PluginReply::Done),
            PluginCommand::Reload { id } => host.reload(id.as_deref()).map(|_| PluginReply::Done),
            PluginCommand::RemoveCatalog { name } => host.remove_catalog(&name).map(|_| PluginReply::Done),
            PluginCommand::Logs { id, limit } => return reply(PluginReply::Logs(host.logs(id.as_deref(), limit.clamp(1, 5000) as usize))),
            PluginCommand::Setting { id, key, value: None } => return host.setting(&id, &key, None).and_then(|v| reply(PluginReply::Setting(v))),
            PluginCommand::Setting { id, key, value: Some(value) } => {
                let saved = host.setting(&id, &key, Some(&value))?;
                self.plugins_changed();
                return reply(PluginReply::Setting(saved));
            }
            PluginCommand::RunCommand { id, command } => return self.run_plugin_command(&id, &command),
            PluginCommand::ApplyTemplate { id, template, project } => return self.apply_plugin_template(&id, &template, project),
            PluginCommand::PanelAction { id, panel, action, inputs } => {
                let outcome = host.panel_action(&id, &panel, &action, &inputs)?;
                let message = outcome.message.clone();
                self.apply_plugin_effects(outcome.effects);
                return reply(PluginReply::Message(message));
            }
            PluginCommand::Preview(_) | PluginCommand::Install { .. } | PluginCommand::RefreshCatalogs | PluginCommand::AddCatalog { .. } => {
                return Err("That plugin command runs asynchronously.".into());
            }
        };
        result?;
        self.plugins_changed();
        reply(PluginReply::Done)
    }

    fn run_plugin_command(&mut self, id: &str, command: &str) -> CommandResult {
        let state = self.plugins.host.state();
        let plugin = state.plugin(id).ok_or_else(|| format!("No plugin “{id}” is installed."))?;
        if !plugin.status.is_active() {
            return Err(format!("“{}” isn’t running ({}).", plugin.name, plugin.status.label().to_lowercase()));
        }
        let info = plugin.ui.commands.iter().find(|c| c.id == command).ok_or_else(|| format!("“{id}” has no command “{command}”."))?;
        match &info.kind {
            PluginCommandKind::Terminal(agent) => {
                let command = self.plugins.host.terminal_agent(id, agent)?;
                reply(PluginReply::Terminal { command })
            }
            PluginCommandKind::Wasm => {
                let outcome = self.plugins.host.run_command(id, command)?;
                let message = outcome.message.clone();
                self.apply_plugin_effects(outcome.effects);
                reply(PluginReply::Message(message))
            }
            PluginCommandKind::Prompt(_) | PluginCommandKind::Panel(_) => Err("That command runs in the app.".into()),
        }
    }

    /// Writes a plugin's project template as the project's `orbit.toml`.
    fn apply_plugin_template(&mut self, id: &str, template: &str, project: ProjectId) -> CommandResult {
        let state = self.plugins.host.state();
        let plugin = state.plugin(id).filter(|p| p.status.is_active()).ok_or_else(|| format!("“{id}” isn’t running."))?;
        let text = plugin.ui.templates.iter().find(|t| t.name == template).map(|t| t.orbit_toml.clone()).ok_or_else(|| format!("“{id}” has no template “{template}”."))?;
        let root = self.store.project(project).map(|p| PathBuf::from(&p.path)).ok_or("That project no longer exists.")?;
        let path = root.join("orbit.toml");
        if path.exists() {
            return Err("This project already has an orbit.toml; Orbit won’t replace it.".into());
        }
        std::fs::write(&path, text).map_err(|e| format!("Couldn’t write {}: {e}", path.display()))?;
        reply(PluginReply::Message(format!("Wrote {}. Import it in Settings › Projects.", path.display())))
    }

    /// Does what plugins asked (each already checked against its
    /// capability by the host).
    pub(crate) fn apply_plugin_effects(&mut self, effects: Vec<EngineEffect>) {
        for effect in effects {
            match effect {
                EngineEffect::Storage { plugin, json } => self.emit(Event::PluginStorage { plugin, storage: json }),
                EngineEffect::Notify { plugin, title, body } => self.emit(Event::PluginNotice { plugin, title, body }),
                EngineEffect::CreateThread { plugin, prompt } => {
                    let now = Instant::now();
                    let recent = self.plugins.started.entry(plugin.clone()).or_default();
                    while recent.front().is_some_and(|t| now.duration_since(*t) > THREADS_WINDOW) {
                        recent.pop_front();
                    }
                    if recent.len() >= THREADS_PER_WINDOW {
                        log::warn!("plugins: {plugin} started {THREADS_PER_WINDOW} threads in ten minutes; not starting another");
                        self.emit(Event::PluginNotice {
                            plugin,
                            title: String::new(),
                            body: format!("Stopped starting threads: {THREADS_PER_WINDOW} in ten minutes is the most a plugin may start."),
                        });
                        continue;
                    }
                    recent.push_back(now);
                    let start = ThreadStart {
                        project: None,
                        config: self.store.defaults().clone(),
                        worktree: false,
                        parent: None,
                        title: String::new(),
                        prompt: Some(prompt),
                    };
                    if let Err(e) = self.start_thread(start) {
                        log::warn!("plugins: {plugin} couldn’t start a thread: {e}");
                    }
                }
            }
        }
    }

    // ── Hooks ────────────────────────────────────────────────────────────

    fn hook_context(&self, thread: ThreadId) -> HookContext {
        let cwd = self.store.thread(thread).map(|t| self.workspace_dir(t).display().to_string()).unwrap_or_default();
        HookContext { thread: thread.to_string(), cwd }
    }

    /// `agent.prompt.before`: a message a plugin blocks is not sent.
    pub(crate) fn plugin_prompt_gate(&mut self, thread: ThreadId, text: &str) -> Result<(), String> {
        if !self.plugins.host.wants("agent.prompt.before") {
            return Ok(());
        }
        let ctx = self.hook_context(thread);
        let verdict = self.plugins.host.prompt_before(&ctx, text);
        let effects = self.plugins.host.take_effects();
        self.apply_plugin_effects(effects);
        match verdict {
            Some(denial) => Err(denial.message()),
            None => Ok(()),
        }
    }

    /// The tool policy a new session gets: asks the plugins' tool hooks
    /// whenever its adapter sees a call before it runs.
    pub(crate) fn plugin_tool_policy(&self, thread: ThreadId, cwd: &Path) -> ToolPolicy {
        let host = self.plugins.host.clone();
        let ctx = HookContext { thread: thread.to_string(), cwd: cwd.display().to_string() };
        let every_tool = host.wants("agent.tool.before");
        ToolPolicy::new(move |kind| host.tool_before(&ctx, kind).map(|d| d.message()), every_tool)
    }

    /// A request that reached the engine from a runtime whose adapter does
    /// not ask the policy itself: a plugin's veto answers it at once.
    pub(crate) fn plugin_review_request(&mut self, thread: ThreadId, request: RequestId, kind: &RequestKind) {
        let Some(t) = self.store.thread(thread) else { return };
        if matches!(t.config.provider, ProviderKind::Claude | ProviderKind::Acp) || !self.plugins.host.wants("agent.tool.before") {
            return;
        }
        let ctx = self.hook_context(thread);
        if let Some(denial) = self.plugins.host.tool_before(&ctx, kind) {
            let message = denial.message();
            self.notice(thread, NoticeLevel::Warning, message.clone());
            if let Err(e) = self.resolve(thread, request, Decision::Deny { feedback: message }) {
                log::warn!("plugins: couldn’t refuse a request: {e}");
            }
        }
    }

    /// Observer hooks on agent events. Called for every event: everything
    /// but finished tools and turns returns at once.
    pub(crate) fn plugins_on_agent(&mut self, thread: ThreadId, event: &AgentEvent) {
        match event {
            AgentEvent::ToolFinished { call_id, output, is_error, .. } => {
                if !self.plugins.host.wants("agent.tool.after") {
                    return;
                }
                let Some(item) = self.tool_item(thread, call_id).and_then(|id| self.item(thread, id)) else { return };
                if let ItemBody::Tool(call) = &item.body {
                    let ctx = self.hook_context(thread);
                    self.plugins.host.tool_after(&ctx, call, output, *is_error);
                }
            }
            AgentEvent::TurnFinished { outcome, .. } => {
                if !self.plugins.host.wants("thread.finished") {
                    return;
                }
                let turn = self.active_turn_number(thread).unwrap_or_default();
                let outcome = match outcome {
                    TurnOutcome::Completed => "completed",
                    TurnOutcome::Interrupted => "interrupted",
                    TurnOutcome::Failed { .. } => "failed",
                    TurnOutcome::RateLimited { .. } => "rate-limited",
                };
                let ctx = self.hook_context(thread);
                let effects = self.plugins.host.thread_finished(&ctx, turn, outcome);
                self.apply_plugin_effects(effects);
            }
            _ => {}
        }
    }

    // ── Upkeep ───────────────────────────────────────────────────────────

    pub(crate) fn plugins_tick(&mut self) {
        // What tool hooks asked for from inside sessions.
        let effects = self.plugins.host.take_effects();
        if !effects.is_empty() {
            self.apply_plugin_effects(effects);
        }
        let seq = self.store.seq();
        if seq != self.plugins.threads_seq && self.plugins.host.wants_threads() {
            self.plugins.threads_seq = seq;
            self.plugins.host.set_threads(self.threads_json());
        }
        if Instant::now() >= self.plugins.next_refresh {
            self.plugins.next_refresh = Instant::now() + REFRESH;
            if let Outcome::Deferred(work) = self.refresh_catalogs(false) {
                tokio::spawn(work);
            }
            self.refresh_kill_list();
        }
    }

    /// `threads.list`: the newest threads that are not archived.
    fn threads_json(&self) -> String {
        let mut threads: Vec<&Thread> = self.store.threads().filter(|t| !t.archived).collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let items: Vec<String> = threads
            .iter()
            .take(200)
            .map(|t| {
                let project = t.project.and_then(|p| self.store.project(p)).map(|p| p.name.clone()).unwrap_or_default();
                let status = match &t.status {
                    ThreadStatus::Idle => "idle",
                    ThreadStatus::Starting => "starting",
                    ThreadStatus::Running => "working",
                    ThreadStatus::Waiting => "needs-you",
                    ThreadStatus::RateLimited { .. } => "rate-limited",
                    ThreadStatus::Failed { .. } => "failed",
                };
                format!(
                    "{{\"id\":{},\"title\":{},\"project\":{},\"status\":\"{status}\",\"updated_ms\":{}}}",
                    json(&t.id.to_string()),
                    json(&t.title),
                    json(&project),
                    t.updated_ms
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    /// Reads every catalog (in a task); `reply` answers with `Done` once
    /// they are read.
    fn refresh_catalogs(&mut self, reply_when_done: bool) -> Outcome {
        let host = self.plugins.host.clone();
        let tx = self.internal_tx.clone();
        let locations = host.catalog_locations();
        if locations.is_empty() {
            return Outcome::Ready(if reply_when_done { reply(PluginReply::Done) } else { Ok(Reply::Done) });
        }
        Outcome::Deferred(Box::pin(async move {
            for (name, location) in locations {
                let read = read_catalog(&location).await;
                host.set_catalog(&name, read);
            }
            let _ = tx.send(Internal::Run(Box::new(|o: &mut Orchestrator| o.plugins_changed())));
            if reply_when_done { reply(PluginReply::Done) } else { Ok(Reply::Done) }
        }))
    }

    fn refresh_kill_list(&mut self) {
        if orbit_plugin::sign::release_key().is_none() {
            return;
        }
        let host = self.plugins.host.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let fetched = async {
                let list = crate::forge::http::send("GET", KILL_LIST_URL, &[], None).await.map_err(|e| e.to_string())?;
                let sig = crate::forge::http::send("GET", &format!("{KILL_LIST_URL}.minisig"), &[], None).await.map_err(|e| e.to_string())?;
                if list.status != 200 || sig.status != 200 {
                    return Err(format!("HTTP {} / {}", list.status, sig.status));
                }
                Ok::<_, String>((list.body.to_vec(), String::from_utf8_lossy(&sig.body).into_owned()))
            };
            match fetched.await.and_then(|(bytes, sig)| host.set_kill_list(&bytes, &sig)) {
                Ok(killed) if !killed.is_empty() => {
                    log::warn!("plugins: the kill list turned off {}", killed.join(", "));
                    let _ = tx.send(Internal::Run(Box::new(|o: &mut Orchestrator| o.plugins_changed())));
                }
                Ok(_) => {}
                Err(e) => {
                    log::info!("plugins: couldn’t refresh the kill list ({e}); trying again later");
                    let _ = tx.send(Internal::Run(Box::new(|o: &mut Orchestrator| o.plugins.next_refresh = Instant::now() + RETRY)));
                }
            }
        });
    }

    fn sync_dev_watches(&mut self) {
        let folders = self.plugins.host.dev_folders();
        self.plugins.watches.retain(|id, w| folders.iter().any(|(i, f)| i == id && *f == w.folder));
        for (id, folder) in folders {
            if self.plugins.watches.contains_key(&id) {
                continue;
            }
            let pending = Arc::new(AtomicBool::new(false));
            let tx = self.internal_tx.clone();
            let plugin = id.clone();
            let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if !(event.kind.is_create() || event.kind.is_modify() || event.kind.is_remove()) || pending.swap(true, Ordering::SeqCst) {
                    return;
                }
                let (plugin, pending) = (plugin.clone(), pending.clone());
                let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| o.dev_plugin_changed(plugin, pending))));
            });
            let watcher = watcher.and_then(|mut w| {
                use notify::Watcher as _;
                w.watch(&folder, notify::RecursiveMode::Recursive).map(|_| w)
            });
            match watcher {
                Ok(watcher) => {
                    self.plugins.watches.insert(id, DevWatch { _watcher: watcher, folder });
                }
                Err(e) => log::warn!("plugins: not watching {}: {e}", folder.display()),
            }
        }
    }

    /// A dev plugin's files changed: reload it once they settle.
    fn dev_plugin_changed(&mut self, plugin: String, pending: Arc<AtomicBool>) {
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DEV_SETTLE).await;
            pending.store(false, Ordering::SeqCst);
            let _ = tx.send(Internal::Run(Box::new(move |o: &mut Orchestrator| {
                if o.plugins.host.reload(Some(&plugin)).is_ok() {
                    o.plugins_changed();
                }
            })));
        });
    }
}

/// Stages a resolved source in a task: downloads an https archive first,
/// then unpacks and checks it off the engine thread.
async fn stage(host: &PluginHost, resolved: Resolved) -> Result<orbit_plugin::host::Staged, String> {
    let resolved = match resolved.fetch {
        orbit_plugin::host::Fetch::Url(url) => {
            let response = crate::forge::http::send("GET", &url, &[], None).await.map_err(|e| e.to_string())?;
            if response.status != 200 {
                return Err(format!("{url} answered HTTP {}.", response.status));
            }
            Resolved { fetch: orbit_plugin::host::Fetch::Archive(response.body.to_vec()), ..resolved }
        }
        _ => resolved,
    };
    let dir = host.dir();
    tokio::task::spawn_blocking(move || PluginHost::stage(&dir, resolved)).await.map_err(|e| format!("The install stopped: {e}"))?
}

/// A catalog's listing and signature, from a file on the host or HTTPS.
async fn read_catalog(location: &str) -> Result<(Vec<u8>, String), String> {
    let (listing, signature) = orbit_plugin::catalog::files(location);
    if listing.starts_with("https://") {
        let list = crate::forge::http::send("GET", &listing, &[], None).await.map_err(|e| e.to_string())?;
        let sig = crate::forge::http::send("GET", &signature, &[], None).await.map_err(|e| e.to_string())?;
        if list.status != 200 || sig.status != 200 {
            return Err(format!("The catalog answered HTTP {} (signature {}).", list.status, sig.status));
        }
        return Ok((list.body.to_vec(), String::from_utf8_lossy(&sig.body).into_owned()));
    }
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&listing).map_err(|e| format!("Couldn’t read {listing}: {e}"))?;
        let sig = std::fs::read_to_string(&signature).map_err(|e| format!("Couldn’t read {signature}: {e}"))?;
        Ok((bytes, sig))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn json(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".into())
}
