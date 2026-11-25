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
            (ItemBody::Tool(call), Delta::ToolOutput(d)) => call.output.push_str(d),
            (ItemBody::Tool(call), Delta::ToolInput(d)) => call.input.push_str(d),
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
    pub conflicts: bool,
    pub connection: bool,
    pub workspace: HashSet<ThreadId>,
    pub terminals: Vec<(TermId, Vec<u8>)>,
    pub exited_terminals: Vec<(TermId, Option<i32>)>,
    /// Timelines that finished loading in this batch.
    pub loaded: Vec<ThreadId>,
    /// Composers whose saved draft changed.
    pub drafts: HashSet<DraftTarget>,
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
            && !self.conflicts
            && !self.connection
            && self.workspace.is_empty()
            && self.terminals.is_empty()
            && self.exited_terminals.is_empty()
            && self.loaded.is_empty()
            && self.drafts.is_empty()
    }

    /// The shell (sidebar, inbox, fleet) needs a render.
    pub fn affects_shell(&self) -> bool {
        self.reset
            || self.projects
            || !self.threads.is_empty()
            || !self.removed_threads.is_empty()
            || self.providers
            || self.defaults
            || self.conflicts
            || self.connection
    }
}

#[derive(Default)]
pub struct ClientState {
    pub connection: Option<ConnectionStatus>,
    pub projects: Vec<Project>,
    pub threads: HashMap<ThreadId, Thread>,
    pub providers: Vec<ProviderInfo>,
    pub defaults: ThreadConfig,
    pub conflicts: Vec<Conflict>,
    /// Live usage of running turns, newer than the thread record's total.
    pub live_usage: HashMap<ThreadId, Usage>,
    /// Working-tree changes of threads with a watched workspace.
    pub workspace: HashMap<ThreadId, Vec<FileChange>>,
    /// Unsent messages saved on the host, per composer.
    pub drafts: HashMap<DraftTarget, Draft>,
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

    pub fn provider(&self, kind: ProviderKind) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.kind == kind)
    }

    /// The best-known usage of a thread: live while a turn runs.
    pub fn usage(&self, thread: ThreadId) -> Usage {
        self.live_usage
            .get(&thread)
            .copied()
            .or_else(|| self.threads.get(&thread).map(|t| t.usage))
            .unwrap_or_default()
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
            Event::DraftSaved { target, draft } => {
                if draft.is_empty() {
                    self.drafts.remove(&target);
                } else {
                    self.drafts.insert(target, draft);
                }
                changes.drafts.insert(target);
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
