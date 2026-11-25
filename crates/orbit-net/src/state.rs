//! Client-side state, independent of any UI framework.
//!
//! [`ClientState`] applies [`Update`]s and reports what changed as a
//! [`Changes`] value, so a UI can notify only the views that show it: the
//! sidebar when thread records change, one timeline when its items change,
//! and the composer's meter when live usage changes. Streaming deltas touch
//! one string in one item.

use std::collections::{HashMap, HashSet};

use orbit_proto::*;

use crate::{ConnectionStatus, Update};

/// The items of one open thread, in order, with an id index.
#[derive(Default, Debug)]
pub struct Timeline {
    items: Vec<Item>,
    index: HashMap<ItemId, usize>,
    /// Bumped on every change; views use it to skip work.
    revision: u64,
}

impl Timeline {
    pub fn new(items: Vec<Item>) -> Self {
        let index = items.iter().enumerate().map(|(ix, item)| (item.id, ix)).collect();
        Self { items, index, revision: 1 }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn get(&self, id: ItemId) -> Option<&Item> {
        self.index.get(&id).map(|&ix| &self.items[ix])
    }

    pub fn position(&self, id: ItemId) -> Option<usize> {
        self.index.get(&id).copied()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn upsert(&mut self, item: Item) -> ItemChange {
        self.revision += 1;
        match self.index.get(&item.id) {
            Some(&ix) => {
                self.items[ix] = item;
                ItemChange::Updated(ix)
            }
            None => {
                // Items normally arrive in id order; keep the vector sorted
                // even when one arrives late (replay after reconnect).
                let ix = self.items.partition_point(|i| i.id < item.id);
                self.items.insert(ix, item);
                if ix + 1 == self.items.len() {
                    self.index.insert(self.items[ix].id, ix);
                } else {
                    self.reindex(ix);
                }
                ItemChange::Inserted(ix)
            }
        }
    }

    fn remove(&mut self, id: ItemId) -> Option<usize> {
        let ix = self.index.remove(&id)?;
        self.items.remove(ix);
        self.reindex(ix);
        self.revision += 1;
        Some(ix)
    }

    fn reindex(&mut self, from: usize) {
        for (ix, item) in self.items.iter().enumerate().skip(from) {
            self.index.insert(item.id, ix);
        }
    }

    fn delta(&mut self, id: ItemId, delta: &Delta) -> Option<usize> {
        let ix = *self.index.get(&id)?;
        let item = &mut self.items[ix];
        match (&mut item.body, delta) {
            (ItemBody::Assistant { text } | ItemBody::Reasoning { text }, Delta::Text(d) | Delta::Reasoning(d)) => {
                text.push_str(d)
            }
            // The same tail rule as the engine, so both hold the same text.
            (ItemBody::Tool(call), Delta::ToolOutput(d)) => append_output_tail(&mut call.output, d),
            (ItemBody::Tool(call), Delta::ToolInput(d)) => {
                if call.input.len() < INLINE_OUTPUT {
                    call.input.push_str(d);
                }
            }
            _ => return None,
        }
        self.revision += 1;
        Some(ix)
    }
}

/// How one item changed, by its position in the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemChange {
    Inserted(usize),
    Updated(usize),
    Removed(usize),
}

/// What an applied batch changed.
#[derive(Default, Debug)]
pub struct Changes {
    /// Everything was replaced.
    pub reset: bool,
    pub projects: bool,
    pub threads: HashSet<ThreadId>,
    pub removed_threads: HashSet<ThreadId>,
    pub timelines: HashMap<ThreadId, Vec<ItemChange>>,
    pub usage: HashSet<ThreadId>,
    pub providers: bool,
    pub defaults: bool,
    pub settings: bool,
    pub conflicts: bool,
    pub connection: bool,
    pub workspace: HashSet<ThreadId>,
    pub terminals: Vec<(TermId, Vec<u8>)>,
    pub exited_terminals: Vec<(TermId, Option<i32>)>,
    /// Timelines that finished loading in this batch.
    pub loaded: Vec<ThreadId>,
    /// Composers whose saved draft changed.
    pub drafts: HashSet<DraftTarget>,
    /// Terminals whose agent started, changed or ended.
    pub terminal_agents: Vec<TermId>,
    /// An automation or one of its runs changed.
    pub automations: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        !self.reset
            && !self.projects
            && self.threads.is_empty()
            && self.removed_threads.is_empty()
            && self.timelines.is_empty()
            && self.usage.is_empty()
            && !self.providers
            && !self.defaults
            && !self.settings
            && !self.conflicts
            && !self.connection
            && self.workspace.is_empty()
            && self.terminals.is_empty()
            && self.exited_terminals.is_empty()
            && self.loaded.is_empty()
            && self.drafts.is_empty()
            && self.terminal_agents.is_empty()
            && !self.automations
    }

    /// The shell (sidebar, inbox, fleet) needs a render.
    pub fn affects_shell(&self) -> bool {
        self.reset
            || self.projects
            || !self.threads.is_empty()
            || !self.removed_threads.is_empty()
            || self.providers
            || self.defaults
            || self.settings
            || self.conflicts
            || self.connection
    }
}

/// Runs kept per automation on the client.
const KEEP_AUTOMATION_RUNS: usize = 50;

#[derive(Default)]
pub struct ClientState {
    pub connection: Option<ConnectionStatus>,
    pub projects: Vec<Project>,
    pub threads: HashMap<ThreadId, Thread>,
    pub providers: Vec<ProviderInfo>,
    pub defaults: ThreadConfig,
    /// The engine's settings, as last broadcast.
    pub settings: Settings,
    pub conflicts: Vec<Conflict>,
    /// Live usage of running turns, newer than the thread record's total.
    pub live_usage: HashMap<ThreadId, Usage>,
    /// Working-tree changes of threads with a watched workspace.
    pub workspace: HashMap<ThreadId, Vec<FileChange>>,
    /// Unsent messages saved on the host, per composer.
    pub drafts: HashMap<DraftTarget, Draft>,
    /// Live status evidence per thread (quiet, unverified, terminal
    /// agents); absent means nothing to qualify. See [`LiveStatus::of`].
    pub live: HashMap<ThreadId, ThreadLive>,
    /// Agents running in terminals, bound to a thread or not.
    pub terminal_agents: HashMap<TermId, TerminalAgent>,
    /// Automations, oldest first.
    pub automations: Vec<Automation>,
    /// Each automation's newest runs, newest first.
    pub automation_runs: HashMap<AutomationId, Vec<AutomationRun>>,
    timelines: HashMap<ThreadId, Timeline>,
    /// Timelines being fetched: the reply and the stream are separate
    /// channels, so events are held until both the reply and its mark have
    /// arrived, then the ones after the mark are applied.
    fetching: HashMap<ThreadId, Fetch>,
}

#[derive(Default)]
struct Fetch {
    reply: Option<(Vec<Item>, u64)>,
    /// Events for the thread (and marks) in stream order.
    buffer: Vec<Event>,
}

impl ClientState {
    pub fn timeline(&self, thread: ThreadId) -> Option<&Timeline> {
        self.timelines.get(&thread)
    }

    /// Installs a timeline directly (tests, tools). UIs use
    /// [`begin_fetch`](Self::begin_fetch) / [`finish_fetch`](Self::finish_fetch).
    pub fn set_timeline(&mut self, thread: ThreadId, items: Vec<Item>) {
        self.timelines.insert(thread, Timeline::new(items));
    }

    /// Starts holding `thread`'s events: call before sending `OpenThread`.
    pub fn begin_fetch(&mut self, thread: ThreadId) {
        self.fetching.insert(thread, Fetch::default());
    }

    /// The `OpenThread` reply arrived. Returns true when the timeline is now
    /// installed (its mark had already arrived on the stream).
    pub fn finish_fetch(&mut self, thread: ThreadId, items: Vec<Item>, mark: u64) -> bool {
        let Some(fetch) = self.fetching.get_mut(&thread) else { return false };
        fetch.reply = Some((items, mark));
        self.try_install(thread)
    }

    /// Installs a fetched timeline once both its reply and its mark are in:
    /// events up to the mark are already in the reply and are dropped; the
    /// rest are applied in order.
    fn try_install(&mut self, thread: ThreadId) -> bool {
        let Some(fetch) = self.fetching.get(&thread) else { return false };
        let Some((_, mark)) = &fetch.reply else { return false };
        let mark = *mark;
        let Some(pos) = fetch
            .buffer
            .iter()
            .position(|e| matches!(e, Event::TimelineMark { mark: m, .. } if *m == mark))
        else {
            return false;
        };
        let mut fetch = self.fetching.remove(&thread).unwrap();
        let (items, _) = fetch.reply.take().unwrap();
        self.timelines.insert(thread, Timeline::new(items));
        let mut ignored = Changes::default();
        for event in fetch.buffer.drain(pos + 1..) {
            self.apply_event(event, &mut ignored);
        }
        true
    }

    /// Stops a fetch (the view went away first).
    pub fn cancel_fetch(&mut self, thread: ThreadId) {
        self.fetching.remove(&thread);
    }

    /// Forgets a timeline the UI no longer shows.
    pub fn drop_timeline(&mut self, thread: ThreadId) {
        self.timelines.remove(&thread);
    }

    pub fn open_timelines(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.timelines.keys().copied()
    }

    /// The built-in instance of a runtime.
    pub fn provider(&self, kind: ProviderKind) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.kind == kind && p.instance_id() == kind.id())
    }

    /// The instance a thread config runs on.
    pub fn provider_for(&self, config: &ThreadConfig) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.serves(config))
    }

    /// The best-known usage of a thread: live while a turn runs.
    pub fn usage(&self, thread: ThreadId) -> Usage {
        self.live_usage
            .get(&thread)
            .copied()
            .or_else(|| self.threads.get(&thread).map(|t| t.usage))
            .unwrap_or_default()
    }

    pub fn automation(&self, id: AutomationId) -> Option<&Automation> {
        self.automations.iter().find(|a| a.id == id)
    }

    /// An automation's newest runs, newest first.
    pub fn runs(&self, id: AutomationId) -> &[AutomationRun] {
        self.automation_runs.get(&id).map(Vec::as_slice).unwrap_or_default()
    }

    /// The status glyph of a thread, with its live evidence: what every
    /// surface draws (see [`LiveStatus`]).
    pub fn live_status(&self, thread: &Thread) -> LiveStatus {
        LiveStatus::of(thread, self.live.get(&thread.id))
    }

    /// Threads in sidebar order: pinned first, then most recently updated.
    pub fn sorted_threads(&self, project: Option<ProjectId>) -> Vec<&Thread> {
        let mut threads: Vec<&Thread> =
            self.threads.values().filter(|t| t.project == project && !t.archived).collect();
        threads.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.updated_ms.cmp(&a.updated_ms)));
        threads
    }

    pub fn apply(&mut self, updates: Vec<Update>) -> Changes {
        let mut changes = Changes::default();
        for update in updates {
            match update {
                Update::Reset(snapshot) => {
                    self.projects = snapshot.projects;
                    self.threads = snapshot.threads.into_iter().map(|t| (t.id, t)).collect();
                    self.providers = snapshot.providers;
                    self.defaults = snapshot.defaults;
                    self.drafts = snapshot.drafts.into_iter().collect();
                    self.settings = *snapshot.settings;
                    self.live = snapshot.live.into_iter().collect();
                    self.terminal_agents = self
                        .live
                        .values()
                        .flat_map(|l| l.terminals.iter().map(|a| (a.term, a.clone())))
                        .collect();
                    self.automations = snapshot.automations;
                    self.automation_runs.clear();
                    for run in snapshot.automation_runs {
                        self.automation_runs.entry(run.automation).or_default().push(run);
                    }
                    self.live_usage.clear();
                    // Open timelines may be stale; the UI refetches them.
                    self.timelines.clear();
                    changes = Changes { reset: true, ..Changes::default() };
                }
                Update::Status(status) => {
                    self.connection = Some(status);
                    changes.connection = true;
                }
                Update::Events(envs) => {
                    for env in envs {
                        self.apply_event(env.event, &mut changes);
                    }
                }
            }
        }
        changes
    }

    fn apply_event(&mut self, event: Event, changes: &mut Changes) {
        // Hold a fetching thread's timeline events (and marks) until the
        // fetch settles; see `try_install`.
        if let Some(thread) = timeline_thread(&event)
            && let Some(fetch) = self.fetching.get_mut(&thread)
        {
            fetch.buffer.push(event);
            if self.try_install(thread) {
                changes.loaded.push(thread);
            }
            return;
        }
        match event {
            Event::ProjectUpserted(project) => {
                match self.projects.iter_mut().find(|p| p.id == project.id) {
                    Some(slot) => *slot = project,
                    None => self.projects.push(project),
                }
                self.projects.sort_by_key(|p| p.order);
                changes.projects = true;
            }
            Event::ProjectRemoved(id) => {
                self.projects.retain(|p| p.id != id);
                changes.projects = true;
            }
            Event::ThreadUpserted(thread) => {
                if !thread.status.is_busy() {
                    self.live_usage.remove(&thread.id);
                }
                changes.threads.insert(thread.id);
                self.threads.insert(thread.id, thread);
            }
            Event::ThreadRemoved(id) => {
                self.threads.remove(&id);
                self.timelines.remove(&id);
                self.live_usage.remove(&id);
                self.live.remove(&id);
                self.drafts.remove(&DraftTarget::Thread(id));
                changes.removed_threads.insert(id);
            }
            Event::ItemUpserted { thread, item } => {
                if let Some(timeline) = self.timelines.get_mut(&thread) {
                    let change = timeline.upsert(item);
                    changes.timelines.entry(thread).or_default().push(change);
                }
            }
            Event::ItemDelta { thread, item, delta } => {
                if let Some(ix) = self.timelines.get_mut(&thread).and_then(|t| t.delta(item, &delta)) {
                    changes.timelines.entry(thread).or_default().push(ItemChange::Updated(ix));
                }
            }
            Event::ItemRemoved { thread, item } => {
                if let Some(ix) = self.timelines.get_mut(&thread).and_then(|t| t.remove(item)) {
                    changes.timelines.entry(thread).or_default().push(ItemChange::Removed(ix));
                }
            }
            Event::Providers(providers) => {
                self.providers = providers;
                changes.providers = true;
            }
            Event::DefaultsChanged(config) => {
                self.defaults = config;
                changes.defaults = true;
            }
            Event::SettingsChanged(settings) => {
                self.settings = settings;
                changes.settings = true;
            }
            Event::UsageChanged { thread, usage } => {
                self.live_usage.insert(thread, usage);
                changes.usage.insert(thread);
            }
            Event::Conflicts(conflicts) => {
                self.conflicts = conflicts;
                changes.conflicts = true;
            }
            Event::WorkspaceChanged { thread, files } => {
                self.workspace.insert(thread, files);
                changes.workspace.insert(thread);
            }
            Event::TerminalOutput { term, data } => changes.terminals.push((term, data)),
            Event::TimelineMark { .. } => {}
            Event::TerminalExited { term, code } => changes.exited_terminals.push((term, code)),
            Event::ThreadLive { thread, live } => {
                if live.is_default() {
                    self.live.remove(&thread);
                } else {
                    self.live.insert(thread, live);
                }
                // The shell redraws the thread's row and glyph.
                changes.threads.insert(thread);
            }
            Event::TerminalAgent { term, agent } => {
                match agent {
                    Some(agent) => self.terminal_agents.insert(term, agent),
                    None => self.terminal_agents.remove(&term),
                };
                changes.terminal_agents.push(term);
            }
            Event::DraftSaved { target, draft } => {
                if draft.is_empty() {
                    self.drafts.remove(&target);
                } else {
                    self.drafts.insert(target, draft);
                }
                changes.drafts.insert(target);
            }
            Event::AutomationUpserted(automation) => {
                match self.automations.iter_mut().find(|a| a.id == automation.id) {
                    Some(slot) => *slot = *automation,
                    None => {
                        self.automations.push(*automation);
                        self.automations.sort_by_key(|a| a.id);
                    }
                }
                changes.automations = true;
            }
            Event::AutomationRemoved(id) => {
                self.automations.retain(|a| a.id != id);
                self.automation_runs.remove(&id);
                changes.automations = true;
            }
            Event::AutomationRun(run) => {
                let runs = self.automation_runs.entry(run.automation).or_default();
                match runs.iter_mut().find(|r| r.id == run.id) {
                    Some(slot) => *slot = *run,
                    None => {
                        runs.insert(0, *run);
                        runs.truncate(KEEP_AUTOMATION_RUNS);
                    }
                }
                changes.automations = true;
            }
        }
    }
}

/// The thread whose timeline an event belongs to, if any.
fn timeline_thread(event: &Event) -> Option<ThreadId> {
    match event {
        Event::ItemUpserted { thread, .. }
        | Event::ItemDelta { thread, .. }
        | Event::ItemRemoved { thread, .. }
        | Event::TimelineMark { thread, .. } => Some(*thread),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> Item {
        Item {
            id: ItemId::new(),
            turn: 1,
            status: ItemStatus::Streaming,
            body: ItemBody::Assistant { text: text.into() },
        }
    }

    fn events(events: Vec<Event>) -> Vec<Update> {
        vec![Update::Events(events.into_iter().map(|event| Envelope { seq: 1, event }).collect())]
    }

    #[test]
    fn deltas_append_and_report_only_their_item() {
        let mut state = ClientState::default();
        let thread = ThreadId::new();
        let first = item("Hello");
        state.set_timeline(thread, vec![first.clone()]);
        let changes = state.apply(events(vec![
            Event::ItemDelta { thread, item: first.id, delta: Delta::Text(", world".into()) },
            Event::ItemDelta { thread, item: first.id, delta: Delta::Text("!".into()) },
        ]));
        assert!(!changes.affects_shell(), "a delta must not re-render the sidebar");
        assert_eq!(changes.timelines[&thread], vec![ItemChange::Updated(0), ItemChange::Updated(0)]);
        assert_eq!(
            state.timeline(thread).unwrap().get(first.id).unwrap().body,
            ItemBody::Assistant { text: "Hello, world!".into() }
        );
    }

    #[test]
    fn late_items_are_inserted_in_order() {
        let mut state = ClientState::default();
        let thread = ThreadId::new();
        let (a, b, c) = (item("a"), item("b"), item("c"));
        state.set_timeline(thread, vec![a.clone(), c.clone()]);
        let changes = state.apply(events(vec![Event::ItemUpserted { thread, item: b.clone() }]));
        assert_eq!(changes.timelines[&thread], vec![ItemChange::Inserted(1)]);
        let ids: Vec<ItemId> = state.timeline(thread).unwrap().items().iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![a.id, b.id, c.id]);
        assert_eq!(state.timeline(thread).unwrap().position(c.id), Some(2));
    }

    #[test]
    fn events_for_closed_timelines_cost_nothing() {
        let mut state = ClientState::default();
        let thread = ThreadId::new();
        let changes = state.apply(events(vec![Event::ItemUpserted { thread, item: item("x") }]));
        assert!(changes.is_empty());
        assert!(state.timeline(thread).is_none());
    }

    #[test]
    fn live_status_reaches_the_glyph_and_clears() {
        let mut state = ClientState::default();
        let thread = Thread { id: ThreadId::new(), status: ThreadStatus::Running, ..Default::default() };
        let id = thread.id;
        let quiet = ThreadLive { liveness: Liveness::Quiet { since_ms: 1, tool: None }, last_signal_ms: 1, terminals: vec![] };
        let changes = state.apply(vec![Update::Events(vec![
            Envelope { seq: 1, event: Event::ThreadUpserted(thread.clone()) },
            Envelope { seq: 0, event: Event::ThreadLive { thread: id, live: quiet } },
        ])]);
        assert!(changes.affects_shell());
        assert_eq!(state.live_status(&thread), LiveStatus::Quiet);
        state.apply(vec![Update::Events(vec![Envelope { seq: 0, event: Event::ThreadLive { thread: id, live: ThreadLive::default() } }])]);
        assert_eq!(state.live_status(&thread), LiveStatus::Working);
        assert!(state.live.is_empty());
        let agent = TerminalAgent {
            term: TermId(7),
            agent: "claude".into(),
            state: AgentState::Working,
            evidence: Evidence::Hook,
            since_ms: 0,
            last_signal_ms: 0,
            detail: String::new(),
        };
        let changes = state.apply(vec![Update::Events(vec![Envelope { seq: 0, event: Event::TerminalAgent { term: TermId(7), agent: Some(agent) } }])]);
        assert_eq!(changes.terminal_agents, vec![TermId(7)]);
        assert!(state.terminal_agents.contains_key(&TermId(7)));
        state.apply(vec![Update::Events(vec![Envelope { seq: 0, event: Event::TerminalAgent { term: TermId(7), agent: None } }])]);
        assert!(state.terminal_agents.is_empty());
    }

    #[test]
    fn live_usage_wins_until_the_turn_settles() {
        let mut state = ClientState::default();
        let mut thread = Thread { id: ThreadId::new(), status: ThreadStatus::Running, ..Default::default() };
        state.apply(events(vec![Event::ThreadUpserted(thread.clone())]));
        let live = Usage { context_tokens: 500, context_window: 1000, ..Default::default() };
        state.apply(events(vec![Event::UsageChanged { thread: thread.id, usage: live }]));
        assert_eq!(state.usage(thread.id).context_fill(), Some(0.5));
        thread.status = ThreadStatus::Idle;
        thread.usage = Usage { context_tokens: 600, context_window: 1000, ..Default::default() };
        state.apply(events(vec![Event::ThreadUpserted(thread.clone())]));
        assert_eq!(state.usage(thread.id).context_fill(), Some(0.6));
    }

    #[test]
    fn settings_come_from_the_snapshot_and_follow_events() {
        let mut state = ClientState::default();
        let mut settings = Settings::default();
        settings.appearance.ui_scale = 110;
        state.apply(vec![Update::Reset(Snapshot { settings: Box::new(settings.clone()), ..Default::default() })]);
        assert_eq!(state.settings, settings);
        settings.composer.submit = SubmitKey::ModEnter;
        let changes = state.apply(events(vec![Event::SettingsChanged(settings.clone())]));
        assert!(changes.settings && changes.affects_shell());
        assert_eq!(state.settings.composer.submit, SubmitKey::ModEnter);
    }

    #[test]
    fn a_fetch_loses_nothing_and_doubles_nothing_in_either_order() {
        let thread = ThreadId::new();
        let first = item("Hel");
        let delta = |text: &str| Event::ItemDelta { thread, item: first.id, delta: Delta::Text(text.into()) };
        let snapshot_text = |state: &ClientState| match &state.timeline(thread).unwrap().items()[0].body {
            ItemBody::Assistant { text } => text.clone(),
            _ => panic!(),
        };

        // Stream first: "lo" is before the mark (already in the reply),
        // ", world" after it (must be applied once).
        let mut state = ClientState::default();
        state.begin_fetch(thread);
        state.apply(events(vec![delta("lo"), Event::TimelineMark { thread, mark: 7 }, delta(", world")]));
        assert!(state.timeline(thread).is_none());
        let reply = Item { body: ItemBody::Assistant { text: "Hello".into() }, ..first.clone() };
        assert!(state.finish_fetch(thread, vec![reply.clone()], 7));
        assert_eq!(snapshot_text(&state), "Hello, world");

        // Reply first: it waits for its mark; later events then apply live.
        let mut state = ClientState::default();
        state.begin_fetch(thread);
        assert!(!state.finish_fetch(thread, vec![reply], 7));
        let changes = state.apply(events(vec![delta("lo"), Event::TimelineMark { thread, mark: 7 }, delta(", world")]));
        assert_eq!(changes.loaded, vec![thread]);
        state.apply(events(vec![delta("!")]));
        assert_eq!(snapshot_text(&state), "Hello, world!");
    }
}
