//! Usage, quota and accounts in the engine (see [`crate::usage`] for the
//! parsers and the math, and `orbit_proto::usage` for what clients see).
//!
//! # Reading limits without polling for nothing
//!
//! Every account's windows come from two sources: what running sessions
//! report with their turns (free, and the only source for most agents), and
//! the provider's usage endpoint (Claude's OAuth usage, Codex
//! `account/rateLimits/read`). The endpoint is read only while a client was
//! in front recently ([`UsageCommand::Seen`] opens a [`LEASE`]): every
//! [`BUSY_EVERY`] while the account is in use, every [`IDLE_EVERY`] when it
//! is not, soon after a turn ends, and at once when the user asks or comes
//! back to a stale reading. Failures back off exponentially (honoring
//! `Retry-After`) up to an hour; a state only the user can change (signed
//! out, an API key) is checked hourly. With every window in the background
//! nothing is read at all.
//!
//! # History
//!
//! Each reading is a sample in its window's history (`crate::usage::burn`),
//! which gives the burn rate and the projected run-out time. The history,
//! the last readings and the settings are kept in `<data>/usage.bin`, so the
//! rings show the last known state at once after a restart. Finished turns
//! go to the usage ledger (`crate::usage::ledger`).
//!
//! # Failover
//!
//! When a turn ends rate-limited, [`FailoverPolicy`] decides: `Auto` moves an
//! unpinned thread to the same agent's account with the most headroom,
//! `Ask` leaves the offer to the thread's limit banner, `Never` waits. A
//! move always writes a notice into the timeline, hands the conversation
//! off (`Handoff` item) and sends the limited turn's message again on the
//! new account.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator, Outcome};
use crate::provider::{Launch, presets};
use crate::usage::burn::{self, Sample};
use crate::usage::failover::{self, Decision};
use crate::usage::ledger::{self, Labels, Ledger};
use crate::usage::saved::{self, Saved, SavedAccount};
use crate::usage::{ReadError, Reading};

/// How long a client counts as in front after it said so.
const LEASE: Duration = Duration::from_secs(30 * 60);
/// A reading older than this is refreshed when a client comes to the front.
const STALE_MS: u64 = 5 * 60_000;
/// How often an account is read while it is in use, and while it is not.
const BUSY_EVERY: Duration = Duration::from_secs(5 * 60);
const IDLE_EVERY: Duration = Duration::from_secs(15 * 60);
/// An account counts as in use this long after its last turn.
const IN_USE: Duration = Duration::from_secs(15 * 60);
/// After a turn, its account is read again this soon…
const AFTER_TURN: Duration = Duration::from_secs(20);
/// …unless it was read this recently.
const MIN_GAP: Duration = Duration::from_secs(2 * 60);
/// How often a state only the user can change is checked again.
const NO_PLAN_EVERY: Duration = Duration::from_secs(60 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// History is written at most this often (and at shutdown).
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// Threads listed as an account's top consumers.
const TOP: usize = 3;

/// Reads one account's limits; tests replace it.
pub(crate) type Reader =
    Arc<dyn Fn(ProviderKind, Launch) -> Pin<Box<dyn Future<Output = Result<Reading, ReadError>> + Send>> + Send + Sync>;

fn provider_reader() -> Reader {
    Arc::new(|kind, launch| {
        Box::pin(async move {
            match kind {
                ProviderKind::Claude => crate::usage::claude::read(launch.env).await,
                ProviderKind::Codex => crate::provider::codex::read_limits(launch.binary, launch.env).await,
                _ => Err(ReadError::no_plan("This agent doesn’t report plan limits.")),
            }
        })
    })
}

/// Engine tests never reach a provider: they set `UsageState::reader`
/// themselves, and anything else reads as unavailable.
#[cfg(test)]
fn reader_for(_: &Path) -> Reader {
    Arc::new(|_, _| Box::pin(async { Err(ReadError::unavailable("Engine tests never reach a provider.")) }))
}

#[cfg(not(test))]
fn reader_for(_: &Path) -> Reader {
    provider_reader()
}

// ── State ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Account {
    plan: Option<String>,
    windows: Vec<QuotaWindow>,
    history: HashMap<String, VecDeque<Sample>>,
    status: QuotaStatus,
    updated_ms: u64,
    source: QuotaSource,
    reading: bool,
    /// When it is next due; `None` reads at the first chance.
    next_read: Option<Instant>,
    failures: u32,
    last_read: Option<Instant>,
    last_used: Option<Instant>,
}

impl Account {
    fn set_window(&mut self, window: QuotaWindow, at_ms: u64) {
        if let Some(used) = window.utilization {
            let used = if window.rejected { 1.0 } else { used };
            let history = self.history.entry(window.window.clone()).or_default();
            burn::record(history, Sample { at_ms, used, resets_ms: window.resets_ms });
        }
        match self.windows.iter_mut().find(|w| w.window == window.window) {
            // A rejection without a figure keeps the last known share.
            Some(slot) => {
                let utilization = window.utilization.or(slot.utilization);
                *slot = QuotaWindow { utilization, ..window };
            }
            None => self.windows.push(window),
        }
    }

    fn interval(&self, now: Instant) -> Duration {
        if self.last_used.is_some_and(|t| now.duration_since(t) < IN_USE) { BUSY_EVERY } else { IDLE_EVERY }
    }
}

#[derive(Default)]
pub(crate) struct UsageState {
    loaded: bool,
    settings: UsageSettings,
    accounts: BTreeMap<String, Account>,
    /// Loaded with the state; `None` only before.
    ledger: Option<Ledger>,
    /// Turns that ended before this were left to the backfill.
    backfill_from: u64,
    /// A client was in front until this.
    front_until: Option<Instant>,
    /// Failovers waiting for their thread's turn snapshot.
    pending: HashMap<ThreadId, (String, bool)>,
    /// History changed since this and is not saved yet.
    dirty_since: Option<Instant>,
    /// An automation's pinned account, for the thread its run creates.
    pin_override: Option<String>,
    pub(crate) reader: Option<Reader>,
}

fn display_title(thread: &Thread) -> String {
    if thread.title.trim().is_empty() { "New thread".into() } else { thread.title.clone() }
}

impl Orchestrator {
    /// The usage state, loaded on first use: settings, last readings and
    /// history from `usage.bin`, the ledger (backfilled in the background
    /// when it is new).
    pub(crate) fn usage_state(&mut self) -> &mut UsageState {
        if !self.usage.loaded {
            self.usage.loaded = true;
            let dir = self.store.dir().to_owned();
            let saved = saved::load(&dir);
            self.usage.settings = saved.settings.sanitized();
            for a in saved.accounts {
                let account = Account {
                    plan: a.plan,
                    windows: a.windows,
                    history: a.history.into_iter().map(|(w, s)| (w, s.into())).collect(),
                    updated_ms: a.updated_ms,
                    source: a.source,
                    status: QuotaStatus::Ok,
                    ..Default::default()
                };
                self.usage.accounts.insert(a.instance, account);
            }
            self.usage.reader = Some(reader_for(&dir));
            let (ledger, backfill) = Ledger::open(&dir);
            self.usage.ledger = Some(ledger);
            if backfill {
                self.usage.backfill_from = now_ms();
                let threads: Vec<(ThreadId, Option<ProjectId>, ThreadConfig)> =
                    self.store.threads().map(|t| (t.id, t.project, t.config.clone())).collect();
                let (tx, from) = (self.internal_tx.clone(), self.usage.backfill_from);
                tokio::task::spawn_blocking(move || {
                    let rows = ledger::backfill(&dir, &threads);
                    let _ = tx.send(Internal::Run(Box::new(move |o| o.usage_backfilled(rows, from))));
                });
            }
        }
        &mut self.usage
    }

    fn usage_backfilled(&mut self, rows: Vec<TurnUsage>, from: u64) {
        let count = rows.len();
        if let Some(ledger) = self.usage_state().ledger.as_mut()
            && let Err(e) = ledger.backfilled(rows, from)
        {
            log::warn!("couldn’t write the usage ledger: {e}");
        }
        log::debug!("usage ledger backfilled with {count} turns");
        self.usage_changed();
    }

    fn usage_save(&mut self) {
        let state = self.usage_state();
        state.dirty_since = None;
        let saved = Saved {
            settings: state.settings.clone(),
            accounts: state
                .accounts
                .iter()
                .filter(|(_, a)| a.updated_ms > 0)
                .map(|(id, a)| SavedAccount {
                    instance: id.clone(),
                    plan: a.plan.clone(),
                    windows: a.windows.clone(),
                    updated_ms: a.updated_ms,
                    source: a.source,
                    history: a.history.iter().map(|(w, s)| (w.clone(), s.iter().copied().collect())).collect(),
                })
                .collect(),
        };
        if let Err(e) = saved::save(self.store.dir(), &saved) {
            log::warn!("couldn’t save usage history: {e}");
        }
    }

    /// Broadcasts the board and marks the history for saving.
    fn usage_changed(&mut self) {
        let state = self.usage_state();
        state.dirty_since.get_or_insert_with(Instant::now);
        let board = self.usage_board();
        self.emit(Event::Quota(Box::new(board)));
    }

    /// Accounts shown: instances of agents with readable plans that are
    /// installed, and any instance a session reported windows for.
    fn usage_instances(&self) -> Vec<ProviderInstance> {
        let mut instances: Vec<ProviderInstance> = presets::effective(&self.store.settings().agents.instances)
            .into_iter()
            .map(|(i, _)| i)
            .filter(|i| {
                let installed = self.providers.iter().any(|p| p.instance_id() == i.id && p.installed);
                let reported = self.usage.accounts.get(&i.id).is_some_and(|a| !a.windows.is_empty());
                (crate::usage::readable(i.kind) && installed) || reported
            })
            .collect();
        // One agent's accounts side by side, in the presets' order.
        instances.sort_by_key(|i| ProviderKind::ALL.iter().position(|k| *k == i.kind));
        instances
    }

    /// The board clients draw.
    pub(crate) fn usage_board(&self) -> QuotaBoard {
        let now = now_ms();
        let state = &self.usage;
        let rows = state.ledger.as_ref().map(Ledger::rows).unwrap_or_default();
        let labels = Labels {
            threads: self.store.threads().map(|t| (t.id, display_title(t))).collect(),
            ..Default::default()
        };
        let accounts = self
            .usage_instances()
            .into_iter()
            .map(|instance| {
                let name = self.display_name(&instance);
                let Some(a) = state.accounts.get(&instance.id) else {
                    return AccountQuota { instance: instance.id.clone(), kind: instance.kind, name, ..Default::default() };
                };
                let windows: Vec<QuotaWindowState> =
                    a.windows.iter().map(|w| burn::window_state(w, a.history.get(&w.window), now)).collect();
                // Top consumers over the shortest window (five hours by default).
                let span = windows.iter().map(|w| w.span_ms).filter(|s| *s > 0).min().unwrap_or(5 * 3_600_000);
                let status = if a.reading && a.updated_ms == 0 { QuotaStatus::Loading } else { a.status.clone() };
                AccountQuota {
                    instance: instance.id.clone(),
                    kind: instance.kind,
                    name,
                    plan: a.plan.clone(),
                    windows,
                    status,
                    updated_ms: a.updated_ms,
                    source: a.source,
                    top: ledger::consumers(rows, &instance.id, now.saturating_sub(span), &labels, TOP),
                }
            })
            .collect();
        QuotaBoard { accounts, settings: state.settings.clone() }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    pub(crate) fn usage_command(&mut self, command: UsageCommand) -> Outcome {
        self.usage_state();
        let result = match command {
            UsageCommand::Board => Ok(Reply::Usage(Box::new(UsageReply::Board(self.usage_board())))),
            UsageCommand::Refresh { instance } => {
                self.usage_front();
                let now = Instant::now();
                for i in self.usage_instances() {
                    if instance.as_deref().is_none_or(|want| want == i.id) {
                        self.usage.accounts.entry(i.id).or_default().next_read = Some(now);
                    }
                }
                self.usage_schedule();
                Ok(Reply::Done)
            }
            UsageCommand::Seen => {
                self.usage_front();
                let (now, now_ms) = (Instant::now(), now_ms());
                for i in self.usage_instances() {
                    let account = self.usage.accounts.entry(i.id).or_default();
                    if account.failures == 0 && now_ms.saturating_sub(account.updated_ms) > STALE_MS {
                        account.next_read = Some(now);
                    }
                }
                self.usage_schedule();
                Ok(Reply::Done)
            }
            UsageCommand::SetSettings(settings) => {
                self.usage.settings = settings.sanitized();
                self.usage_save();
                self.usage_changed();
                Ok(Reply::Usage(Box::new(UsageReply::Board(self.usage_board()))))
            }
            UsageCommand::Stats(query) => {
                let labels = self.usage_labels();
                let rows = self.usage.ledger.as_ref().map(Ledger::rows).unwrap_or_default();
                Ok(Reply::Usage(Box::new(UsageReply::Stats(ledger::stats(rows, &query, &labels, now_ms())))))
            }
            UsageCommand::Export(query) => {
                let labels = self.usage_labels();
                let rows = self.usage.ledger.as_ref().map(Ledger::rows).unwrap_or_default();
                Ok(Reply::Text(ledger::csv(rows, &query, &labels, now_ms())))
            }
            UsageCommand::Failover { thread, instance } => self.usage_failover(thread, &instance, false),
        };
        Outcome::Ready(result)
    }

    fn usage_labels(&self) -> Labels {
        let mut labels = Labels {
            threads: self.store.threads().map(|t| (t.id, display_title(t))).collect(),
            projects: self.store.projects().iter().map(|p| (p.id, p.name.clone())).collect(),
            instances: self.providers.iter().map(|p| (p.instance_id().to_owned(), p.display_name().to_owned())).collect(),
            models: HashMap::new(),
        };
        for p in &self.providers {
            for m in &p.models {
                labels.models.entry(m.slug.clone()).or_insert_with(|| m.name.clone());
            }
        }
        labels
    }

    fn usage_front(&mut self) {
        self.usage.front_until = Some(Instant::now() + LEASE);
    }

    // ── Reading limits ───────────────────────────────────────────────────

    /// Periodic work: reads what is due, saves history now and then.
    pub(crate) fn usage_tick(&mut self) {
        let state = self.usage_state();
        if state.dirty_since.is_some_and(|t| t.elapsed() >= SAVE_EVERY) {
            self.usage_save();
        }
        self.usage_schedule();
    }

    pub(crate) fn usage_shutdown(&mut self) {
        if self.usage.loaded && self.usage.dirty_since.is_some() {
            self.usage_save();
        }
    }

    /// Starts the reads that are due, while a client is in front.
    fn usage_schedule(&mut self) {
        let now = Instant::now();
        if !self.usage.settings.poll || !self.usage.front_until.is_some_and(|t| now < t) {
            return;
        }
        for instance in self.usage_instances() {
            if !crate::usage::readable(instance.kind) {
                continue;
            }
            let signed_out = self.providers.iter().any(|p| p.instance_id() == instance.id && p.signed_in == Some(false));
            let account = self.usage.accounts.entry(instance.id.clone()).or_default();
            if account.reading || signed_out || account.next_read.is_some_and(|t| t > now) {
                continue;
            }
            account.reading = true;
            let launch = self.launch_of(&instance);
            let reader = self.usage.reader.clone().unwrap_or_else(provider_reader);
            let tx = self.internal_tx.clone();
            let (id, kind) = (instance.id.clone(), instance.kind);
            tokio::spawn(async move {
                let result = reader(kind, launch).await;
                let _ = tx.send(Internal::Run(Box::new(move |o| o.usage_read(id, result))));
            });
        }
    }

    fn usage_read(&mut self, instance: String, result: Result<Reading, ReadError>) {
        let (now, at_ms) = (Instant::now(), now_ms());
        let account = self.usage_state().accounts.entry(instance).or_default();
        account.reading = false;
        account.last_read = Some(now);
        match result {
            Ok(reading) => {
                account.failures = 0;
                // The provider's answer is the whole truth: windows it no
                // longer lists are gone.
                account.windows.retain(|w| reading.windows.iter().any(|r| r.window == w.window));
                for window in reading.windows {
                    account.set_window(window, at_ms);
                }
                if reading.plan.is_some() {
                    account.plan = reading.plan;
                }
                account.status = QuotaStatus::Ok;
                account.updated_ms = at_ms;
                account.source = QuotaSource::Provider;
                account.next_read = Some(now + account.interval(now));
            }
            Err(error) => {
                account.failures += 1;
                let wait = if error.transient {
                    let backoff = Duration::from_secs(60) * 2u32.saturating_pow(account.failures.saturating_sub(1).min(6));
                    backoff.max(Duration::from_secs(error.retry_after.unwrap_or(0))).min(MAX_BACKOFF)
                } else {
                    NO_PLAN_EVERY
                };
                account.next_read = Some(now + wait);
                account.status = error.status;
            }
        }
        self.usage_changed();
    }

    /// A session reported a window with its turn.
    pub(crate) fn usage_on_quota(&mut self, thread: ThreadId, window: QuotaWindow) {
        let Some(config) = self.store.thread(thread).map(|t| t.config.clone()) else { return };
        let instance = self.instance_of(&config).id;
        let at_ms = now_ms();
        let account = self.usage_state().accounts.entry(instance).or_default();
        account.set_window(window, at_ms);
        account.updated_ms = at_ms;
        account.source = QuotaSource::Session;
        account.last_used = Some(Instant::now());
        if !matches!(account.status, QuotaStatus::NoPlan(_)) {
            account.status = QuotaStatus::Ok;
        }
        self.usage_changed();
    }

    // ── Turns ────────────────────────────────────────────────────────────

    /// The model a config resolves to, as the ledger records it.
    fn resolved_model(&self, config: &ThreadConfig) -> String {
        if !config.model.is_empty() && config.model != "default" {
            return config.model.clone();
        }
        let default = self.providers.iter().find(|p| p.serves(config)).and_then(|p| p.models.iter().find(|m| m.is_default));
        match default {
            // Claude's `default` entry names the model in its description.
            Some(m) if m.slug == "default" => m
                .description
                .as_deref()
                .and_then(|d| d.split(" · ").next())
                .filter(|n| !n.is_empty() && n.len() < 40)
                .map(str::to_owned)
                .unwrap_or_default(),
            Some(m) => m.slug.clone(),
            None => config.model.clone(),
        }
    }

    /// A turn ended: record what it used, and read its account again soon.
    pub(crate) fn usage_record_turn(&mut self, thread: ThreadId, usage: &Usage) {
        let Some(t) = self.store.thread(thread).cloned() else { return };
        let instance = self.instance_of(&t.config).id;
        let model = self.resolved_model(&t.config);
        let now = Instant::now();
        let state = self.usage_state();
        let in_front = state.front_until.is_some_and(|f| now < f);
        let account = state.accounts.entry(instance.clone()).or_default();
        account.last_used = Some(now);
        if in_front && account.last_read.is_none_or(|r| now.duration_since(r) >= MIN_GAP) {
            let soon = now + AFTER_TURN;
            account.next_read = Some(account.next_read.map_or(soon, |n| n.min(soon)));
        }
        if *usage == Usage::default() {
            return;
        }
        let row = TurnUsage { ended_ms: now_ms(), thread, project: t.project, kind: t.config.provider, instance, model, usage: *usage };
        if let Some(ledger) = self.usage.ledger.as_mut() {
            ledger.append(row);
        }
        self.usage_changed();
    }

    // ── Pins ─────────────────────────────────────────────────────────────

    /// The config a new thread of `project` starts with: the account its
    /// project (or the automation starting it) is pinned to, when that is an
    /// account of the same agent.
    pub(crate) fn usage_pin_config(&mut self, project: Option<ProjectId>, config: &ThreadConfig) -> ThreadConfig {
        let state = self.usage_state();
        let pinned = state
            .pin_override
            .take()
            .or_else(|| project.and_then(|p| state.settings.pin(PinTarget::Project(p)).map(str::to_owned)));
        let instance = pinned.and_then(|id| presets::find(&self.store.settings().agents.instances, &id));
        match instance {
            Some(instance) if instance.kind == config.provider => failover::moved_config(config, &instance),
            _ => config.clone(),
        }
    }

    /// Before an automation's run starts its thread: its pinned account,
    /// if any, is used for that thread (`None` clears it afterwards).
    pub(crate) fn usage_pin_automation(&mut self, automation: Option<AutomationId>) {
        let state = self.usage_state();
        state.pin_override =
            automation.and_then(|a| state.settings.pin(PinTarget::Automation(a)).map(str::to_owned));
    }

    /// Whether a thread stays on its account: its project, or the
    /// automation running it, is pinned.
    fn usage_pinned(&mut self, thread: &Thread) -> bool {
        let automation = self.automation_of_thread(thread.id);
        let settings = &self.usage_state().settings;
        thread.project.is_some_and(|p| settings.pin(PinTarget::Project(p)).is_some())
            || automation.is_some_and(|a| settings.pin(PinTarget::Automation(a)).is_some())
    }

    // ── Failover ─────────────────────────────────────────────────────────

    /// A turn ended at its account's limit: read the account again (for its
    /// reset time) and apply the failover policy.
    pub(crate) fn usage_on_limit(&mut self, thread_id: ThreadId) {
        let Some(thread) = self.store.thread(thread_id).cloned() else { return };
        let from = self.instance_of(&thread.config);
        let now = Instant::now();
        let state = self.usage_state();
        if state.front_until.is_some_and(|f| now < f) {
            state.accounts.entry(from.id.clone()).or_default().next_read = Some(now);
        }
        let policy = state.settings.failover;
        let pinned = self.usage_pinned(&thread);
        let instances: Vec<ProviderInstance> =
            presets::effective(&self.store.settings().agents.instances).into_iter().map(|(i, _)| i).collect();
        let board = self.usage_board();
        let targets = failover::same_agent(&from, &instances, &self.providers, &board, now_ms());
        if let Decision::Switch(target) = failover::decide(policy, pinned, &targets)
            && let Err(message) = self.usage_failover(thread_id, &target.instance, true)
        {
            log::warn!("failover of {thread_id} to {}: {message}", target.instance);
        }
    }

    /// A turn's snapshot landed: run a failover that waited for it.
    pub(crate) fn usage_after_checkpoint(&mut self, thread: ThreadId) {
        if let Some((instance, auto)) = self.usage.pending.remove(&thread)
            && let Err(message) = self.usage_failover(thread, &instance, auto)
        {
            self.notice(thread, NoticeLevel::Error, format!("Couldn’t move to another account: {message}"));
        }
    }

    /// Moves a rate-limited thread to `to` and sends its limited turn again
    /// there. `auto` says the policy did it, not the user.
    pub(crate) fn usage_failover(&mut self, thread_id: ThreadId, to: &str, auto: bool) -> CommandResult {
        let thread = self.store.thread(thread_id).cloned().ok_or("That thread no longer exists.")?;
        if thread.archived {
            return Err("This thread is archived. Unarchive it to continue.".into());
        }
        if self.runtimes.get(&thread_id).is_some_and(|r| r.turn.is_some() || r.held_prompt.is_some()) {
            return Err("The agent is still working on this thread.".into());
        }
        if !matches!(thread.status, ThreadStatus::RateLimited { .. }) {
            return Err("Only a thread that reached its usage limit moves to another account.".into());
        }
        let to = presets::find(&self.store.settings().agents.instances, to).ok_or("That account isn’t set up any more.")?;
        let from = self.instance_of(&thread.config);
        let (from_name, to_name) = (self.display_name(&from), self.display_name(&to));
        if from.id == to.id || from.shares_sessions_with(&to) {
            return Err(format!("{to_name} uses the same sign-in as {from_name}."));
        }
        if self.runtimes.get(&thread_id).is_some_and(|r| r.checkpointing) {
            // The turn's files are being snapshotted; move right after.
            self.usage_state().pending.insert(thread_id, (to.id, auto));
            return Ok(Reply::Done);
        }
        let text = match (auto, from.kind == to.kind) {
            (true, _) => format!(
                "{from_name} reached its usage limit, so Orbit moved this thread to {to_name} and sent your message again. \
                 Settings › Usage decides when this happens."
            ),
            (false, true) => format!("{from_name} reached its usage limit. Moved this thread to {to_name}."),
            (false, false) => format!(
                "{from_name} reached its usage limit. Moved this thread to {to_name}, a different agent: it continues \
                 from a summary of this conversation, with its own models and tools."
            ),
        };
        self.notice(thread_id, if auto { NoticeLevel::Warning } else { NoticeLevel::Info }, text);
        let old = thread.config.clone();
        let new = failover::moved_config(&old, &to);
        self.update_thread(thread_id, |t| t.config = new.clone());
        self.switch_instance(thread_id, &old, &new, Some(thread.turns));
        // Send the limited turn's message again, as Retry does.
        let turn = thread.turns;
        let message = self.store.load_timeline(thread_id).ok().and_then(|timeline| {
            timeline
                .items()
                .iter()
                .find(|i| i.turn == turn && i.status == ItemStatus::Done && matches!(&i.body, ItemBody::User(m) if !m.steer))
                .map(|i| i.id)
        });
        match message {
            Some(message) => {
                self.update_thread(thread_id, |t| {
                    t.turns = turn.saturating_sub(1);
                    t.status = ThreadStatus::Idle;
                });
                self.start_turn(thread_id, message)?;
            }
            None => {
                self.update_thread(thread_id, |t| t.status = ThreadStatus::Idle);
                self.start_next_queued(thread_id);
            }
        }
        Ok(Reply::Done)
    }
}
