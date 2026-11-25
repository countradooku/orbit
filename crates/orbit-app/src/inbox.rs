//! The Inbox, as a Needs-you deck: one decision card per thread that waits
//! on the user, answered from the keyboard without opening the thread.
//!
//! * **Order is triage order.** Approvals, then questions, then failures,
//!   quota waits and finished results; newest first within each lane.
//! * **Cards never move under the pointer.** While the pointer is over the
//!   deck, cards that leave are removed but nothing reorders and new cards
//!   wait behind an "N new" pill; they slide in when the pointer leaves or
//!   the pill is clicked.
//! * **One key per decision.** `j`/`k` move, `1`–`9` pick an option, `a`
//!   takes the outlined (recommended) option, `A` approves every visible
//!   request of the same kind, `d` denies, `r` replies inline, `s` snoozes
//!   for an hour, `e` settles, Enter opens the thread. Snooze and settle can
//!   be undone from their toast.
//!
//! Selection follows the card, not its position, so a card arriving above
//! the selected one never changes what the next key acts on.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::state::ClientState;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::{self, Attention, Glyph, RequestClass, StatusGlyph};
use crate::{format, thread_menu};

/// What a card asks of the user, in triage order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    Approval,
    Question,
    Failed,
    Quota,
    Unread,
}

impl Lane {
    fn of(thread: &Thread) -> Option<Lane> {
        if thread.archived || thread.snoozed_until.is_some() {
            return None;
        }
        match Attention::of(thread) {
            Attention::Waiting => Some(match thread.pending.first().map(|p| RequestClass::of(&p.request.kind)) {
                Some(RequestClass::Question) | None => Lane::Question,
                Some(_) => Lane::Approval,
            }),
            Attention::Failed => Some(Lane::Failed),
            Attention::RateLimited => Some(Lane::Quota),
            Attention::Unread => Some(Lane::Unread),
            Attention::Working | Attention::Idle => None,
        }
    }
}

/// The deck's cards in triage order, and the settled threads that would
/// have a card (shown only as a count).
pub(crate) fn triage(state: &ClientState) -> (Vec<ThreadId>, Vec<ThreadId>) {
    let mut cards: Vec<(Lane, u64, ThreadId)> = Vec::new();
    let mut settled = Vec::new();
    for thread in state.threads.values() {
        let Some(lane) = Lane::of(thread) else { continue };
        if thread.settled_ms.is_some() {
            settled.push(thread.id);
            continue;
        }
        let since = thread.pending.first().map(|p| p.created_ms).unwrap_or(thread.updated_ms);
        cards.push((lane, since, thread.id));
    }
    cards.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.0.cmp(&b.2.0)));
    (cards.into_iter().map(|(_, _, id)| id).collect(), settled)
}

/// Merges a fresh triage into what is on screen. Unfrozen, the fresh order
/// wins. Frozen (pointer over the deck), cards keep their places, cards that
/// left are dropped, and new cards are held back.
pub(crate) fn merge_order(shown: &[ThreadId], fresh: &[ThreadId], frozen: bool) -> (Vec<ThreadId>, Vec<ThreadId>) {
    if !frozen {
        return (fresh.to_vec(), Vec::new());
    }
    let fresh_set: HashSet<ThreadId> = fresh.iter().copied().collect();
    let kept: Vec<ThreadId> = shown.iter().copied().filter(|id| fresh_set.contains(id)).collect();
    let kept_set: HashSet<ThreadId> = kept.iter().copied().collect();
    let held = fresh.iter().copied().filter(|id| !kept_set.contains(id)).collect();
    (kept, held)
}

/// The card to select after `gone` left: the one that followed it, else the
/// one before.
pub(crate) fn next_after(before: &[ThreadId], gone: ThreadId, now: &[ThreadId]) -> Option<ThreadId> {
    let at = before.iter().position(|id| *id == gone)?;
    before[at + 1..].iter().chain(before[..at].iter().rev()).find(|id| now.contains(id)).copied()
}

/// What picking an option does.
#[derive(Clone, Debug, PartialEq)]
enum Choice {
    Decide(Decision),
    /// Toggle one answer of a multi-select question.
    Toggle(usize),
    /// Send the multi-select answers picked so far.
    SendPicked,
    Reply,
    Open,
    MarkRead,
    Settle,
    SnoozeUntil(u64),
}

#[derive(Clone, Debug)]
struct Opt {
    label: SharedString,
    choice: Choice,
    recommended: bool,
}

fn opt(label: impl Into<SharedString>, choice: Choice, recommended: bool) -> Opt {
    Opt { label: label.into(), choice, recommended }
}

/// The options a card offers, numbered 1–9 in this order.
fn options(thread: &Thread, picked: Option<&BTreeSet<usize>>) -> Vec<Opt> {
    if let Some(pending) = thread.pending.first() {
        return match &pending.request.kind {
            RequestKind::Command { .. } | RequestKind::FileChange { .. } | RequestKind::Tool { .. } => vec![
                opt("Allow", Choice::Decide(Decision::Allow), true),
                opt("Allow for this session", Choice::Decide(Decision::AllowForSession), false),
                opt("Deny", Choice::Decide(Decision::Deny { feedback: String::new() }), false),
            ],
            RequestKind::Plan { .. } => vec![
                opt("Approve plan", Choice::Decide(Decision::Allow), true),
                opt("Keep planning", Choice::Decide(Decision::Deny { feedback: String::new() }), false),
            ],
            RequestKind::Question { options, multi_select, .. } => {
                let recommended = options.iter().position(|o| o.to_lowercase().contains("recommended")).unwrap_or(0);
                let mut out: Vec<Opt> = options
                    .iter()
                    .take(8)
                    .enumerate()
                    .map(|(ix, label)| {
                        let choice = if *multi_select {
                            Choice::Toggle(ix)
                        } else {
                            Choice::Decide(Decision::Answer { text: label.clone() })
                        };
                        opt(label.clone(), choice, !*multi_select && ix == recommended)
                    })
                    .collect();
                if *multi_select {
                    let count = picked.map_or(0, BTreeSet::len);
                    out.push(opt(
                        if count == 0 { "Send".to_owned() } else { format!("Send {count} picked") },
                        Choice::SendPicked,
                        count > 0,
                    ));
                } else if out.is_empty() {
                    out.push(opt("Answer…", Choice::Reply, true));
                }
                out
            }
        };
    }
    match &thread.status {
        ThreadStatus::Failed { .. } => {
            vec![opt("Open thread", Choice::Open, true), opt("Settle", Choice::Settle, false)]
        }
        ThreadStatus::RateLimited { resets_ms } if *resets_ms > now_ms() => vec![
            opt("Open thread", Choice::Open, true),
            opt("Snooze until it resets", Choice::SnoozeUntil(*resets_ms), false),
        ],
        ThreadStatus::RateLimited { .. } => vec![opt("Open thread", Choice::Open, true)],
        _ => vec![
            opt("Open thread", Choice::Open, true),
            opt("Mark read", Choice::MarkRead, false),
            opt("Settle", Choice::Settle, false),
        ],
    }
}

const ENTER: Duration = Duration::from_millis(160);
const SNOOZE: Duration = Duration::from_secs(3600);

/// Marks the deck's undo toasts, one per thread.
struct UndoNote;

pub struct InboxView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// Cards in the order shown.
    order: Vec<ThreadId>,
    /// New cards waiting for the pointer to leave.
    held: Vec<ThreadId>,
    settled: Vec<ThreadId>,
    show_settled: bool,
    selected: Option<ThreadId>,
    hovered: bool,
    /// Cards that just arrived, for their entrance.
    arrived: HashMap<ThreadId, (u32, Instant)>,
    arrivals: u32,
    reply: Entity<InputState>,
    replying: Option<ThreadId>,
    /// Answers picked so far on multi-select questions.
    picked: HashMap<RequestId, BTreeSet<usize>>,
    _subscriptions: Vec<Subscription>,
}

impl InboxView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let reply = cx.new(|cx| InputState::new(window, cx).placeholder("Answer…"));
        let subscriptions = vec![
            cx.observe(&app, |this, _, cx| {
                this.sync(cx);
                cx.notify();
            }),
            cx.subscribe_in(&reply, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.send_reply(window, cx);
                }
            }),
        ];
        let mut view = Self {
            app,
            workspace,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            order: Vec::new(),
            held: Vec::new(),
            settled: Vec::new(),
            show_settled: false,
            selected: None,
            hovered: false,
            arrived: HashMap::new(),
            arrivals: 0,
            reply,
            replying: None,
            picked: HashMap::new(),
            _subscriptions: subscriptions,
        };
        view.sync(cx);
        view
    }

    /// Focuses the deck so its keys work (the Inbox route calls this).
    pub fn focus(view: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let handle = view.read(cx).focus.clone();
        handle.focus(window, cx);
    }

    /// How many threads wait on the user: requests only, not results.
    pub fn waiting(state: &ClientState) -> usize {
        state
            .threads
            .values()
            .filter(|t| !t.archived && t.snoozed_until.is_none() && Attention::of(t) == Attention::Waiting)
            .count()
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let (fresh, settled) = triage(state);
        self.settled = settled;
        let before = self.order.clone();
        let (order, held) = merge_order(&self.order, &fresh, self.hovered);
        let now = Instant::now();
        if !before.is_empty() {
            let known: HashSet<ThreadId> = before.iter().copied().collect();
            for id in order.iter().filter(|id| !known.contains(id)) {
                self.arrivals += 1;
                self.arrived.insert(*id, (self.arrivals, now));
            }
        }
        self.arrived.retain(|_, (_, at)| now.duration_since(*at) < ENTER * 3);
        // Selection follows its card; if the card left, take its neighbour.
        if let Some(selected) = self.selected
            && !order.contains(&selected)
        {
            self.selected = next_after(&before, selected, &order).or_else(|| order.first().copied());
        }
        if self.replying.is_some_and(|id| !order.contains(&id)) {
            self.replying = None;
        }
        let live: HashSet<RequestId> =
            state.threads.values().flat_map(|t| t.pending.iter().map(|p| p.request.id)).collect();
        self.picked.retain(|id, _| live.contains(id));
        self.order = order;
        self.held = held;
    }

    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered == hovered {
            return;
        }
        self.hovered = hovered;
        if !hovered {
            self.sync(cx);
            cx.notify();
        }
    }

    fn show_held(&mut self, cx: &mut Context<Self>) {
        let hovered = std::mem::replace(&mut self.hovered, false);
        self.sync(cx);
        self.hovered = hovered;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn selected_ix(&self) -> Option<usize> {
        self.selected.and_then(|id| self.order.iter().position(|t| *t == id))
    }

    fn select_ix(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(id) = self.order.get(ix).copied() {
            self.selected = Some(id);
            // The header sits above the cards.
            self.scroll.scroll_to_item(ix + 1);
            cx.notify();
        }
    }

    fn thread(&self, id: ThreadId, cx: &App) -> Option<Thread> {
        self.app.read(cx).state().threads.get(&id).cloned()
    }

    /// The card the keys act on: the selected one, else the first.
    fn current(&self, cx: &App) -> Option<Thread> {
        self.selected
            .filter(|id| self.order.contains(id))
            .or_else(|| self.order.first().copied())
            .and_then(|id| self.thread(id, cx))
    }

    fn open(&self, id: ThreadId, window: &mut Window, cx: &mut App) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
    }

    fn patch(&self, id: ThreadId, patch: ThreadPatch, window: &mut Window, cx: &mut App) {
        self.app.update(cx, |app, cx| app.run(Command::UpdateThread { id, patch }, window, cx));
    }

    fn resolve(&self, thread: ThreadId, request: RequestId, decision: Decision, window: &mut Window, cx: &mut App) {
        self.app.update(cx, |app, cx| app.run(Command::Resolve { thread, request, decision }, window, cx));
    }

    /// Runs option `index` of the card for `id`.
    fn pick(&mut self, id: ThreadId, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(id, cx) else { return };
        let request = thread.pending.first().map(|p| p.request.id);
        let picked = request.and_then(|r| self.picked.get(&r));
        let Some(option) = options(&thread, picked).into_iter().nth(index) else { return };
        self.selected = Some(id);
        self.run(&thread, option.choice, window, cx);
    }

    fn run(&mut self, thread: &Thread, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let id = thread.id;
        let request = thread.pending.first().map(|p| (p.request.id, &p.request.kind));
        match choice {
            Choice::Decide(decision) => {
                if let Some((request, _)) = request {
                    self.resolve(id, request, decision, window, cx);
                }
            }
            Choice::Toggle(ix) => {
                if let Some((request, _)) = request {
                    let set = self.picked.entry(request).or_default();
                    if !set.remove(&ix) {
                        set.insert(ix);
                    }
                    cx.notify();
                }
            }
            Choice::SendPicked => {
                if let Some((request, RequestKind::Question { options, .. })) = request {
                    let picked = self.picked.remove(&request).unwrap_or_default();
                    if picked.is_empty() {
                        self.start_reply(id, window, cx);
                        return;
                    }
                    let text = picked.iter().filter_map(|ix| options.get(*ix).cloned()).collect::<Vec<_>>().join("\n");
                    self.resolve(id, request, Decision::Answer { text }, window, cx);
                }
            }
            Choice::Reply => self.start_reply(id, window, cx),
            Choice::Open => self.open(id, window, cx),
            Choice::MarkRead => self.patch(id, ThreadPatch { unread: Some(false), ..Default::default() }, window, cx),
            Choice::Settle => self.settle(thread, window, cx),
            Choice::SnoozeUntil(until) => self.snooze(thread, until, window, cx),
        }
    }

    fn settle(&mut self, thread: &Thread, window: &mut Window, cx: &mut Context<Self>) {
        let id = thread.id;
        self.patch(id, ThreadPatch { settled: Some(true), ..Default::default() }, window, cx);
        let message = format!("Settled “{}”", display_title(thread));
        self.undo_toast(id, message, ThreadPatch { settled: Some(false), ..Default::default() }, window, cx);
    }

    fn snooze(&mut self, thread: &Thread, until: u64, window: &mut Window, cx: &mut Context<Self>) {
        let id = thread.id;
        self.patch(id, ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() }, window, cx);
        let message = format!("Snoozed “{}” {}", display_title(thread), until_label(until));
        self.undo_toast(id, message, ThreadPatch { snooze_until: Some(None), ..Default::default() }, window, cx);
    }

    /// A short toast with Undo, which applies `undo` to the thread.
    fn undo_toast(&self, id: ThreadId, message: String, undo: ThreadPatch, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let note = Notification::new()
            .message(message)
            .id1::<UndoNote>(("deck-undo", id.0))
            .action(move |_, _, cx| {
                let (app, undo) = (app.clone(), undo.clone());
                Button::new(("undo", id.0)).small().label("Undo").on_click(cx.listener(move |note, _, window, cx| {
                    app.update(cx, |app, cx| app.run(Command::UpdateThread { id, patch: undo.clone() }, window, cx));
                    note.dismiss(window, cx);
                }))
            })
            .autohide(true);
        window.push_notification(note, cx);
    }

    fn start_reply(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(id, cx) else { return };
        let placeholder = match thread.pending.first().map(|p| RequestClass::of(&p.request.kind)) {
            Some(RequestClass::Question) => "Answer in your own words…",
            Some(_) => "Tell the agent what to do instead…",
            None => "Reply to the agent…",
        };
        self.selected = Some(id);
        self.replying = Some(id);
        self.reply.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(placeholder, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn cancel_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.replying.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    fn send_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.replying else { return };
        let text = self.reply.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(thread) = self.thread(id, cx) else { return };
        match thread.pending.first() {
            Some(pending) => {
                let decision = match &pending.request.kind {
                    RequestKind::Question { .. } => Decision::Answer { text },
                    _ => Decision::Deny { feedback: text },
                };
                self.resolve(id, pending.request.id, decision, window, cx);
            }
            None => self.app.update(cx, |app, cx| {
                app.run(Command::Send { thread: id, text, attachments: Vec::new(), steer: false }, window, cx)
            }),
        }
        self.reply.update(cx, |input, cx| input.set_value("", window, cx));
        self.cancel_reply(window, cx);
    }

    /// The requests "allow all of this kind" would resolve for the current
    /// card: the same kind, on cards the user can see.
    fn same_kind(&self, cx: &App) -> Option<(RequestClass, Vec<(ThreadId, RequestId)>)> {
        let thread = self.current(cx)?;
        let class = RequestClass::of(&thread.pending.first()?.request.kind);
        if !class.is_approval() {
            return None;
        }
        let state = self.app.read(cx).state();
        let all = self
            .order
            .iter()
            .filter_map(|id| state.threads.get(id))
            .flat_map(|t| t.pending.iter().map(move |p| (t.id, p)))
            .filter(|(_, p)| RequestClass::of(&p.request.kind) == class)
            .map(|(t, p)| (t, p.request.id))
            .collect();
        Some((class, all))
    }

    // ── Actions ──────────────────────────────────────────────────────────

    fn next(&mut self, _: &DeckNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_ix().map_or(0, |ix| (ix + 1).min(self.order.len().saturating_sub(1)));
        self.select_ix(ix, cx);
    }

    fn previous(&mut self, _: &DeckPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_ix().map_or(0, |ix| ix.saturating_sub(1));
        self.select_ix(ix, cx);
    }

    fn pick_action(&mut self, action: &DeckPick, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current(cx) {
            self.pick(thread.id, action.index, window, cx);
        }
    }

    fn approve(&mut self, _: &DeckApprove, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.current(cx) else { return };
        let picked = thread.pending.first().and_then(|p| self.picked.get(&p.request.id));
        if let Some(ix) = options(&thread, picked).iter().position(|o| o.recommended) {
            self.pick(thread.id, ix, window, cx);
        }
    }

    fn approve_all(&mut self, _: &DeckApproveAllOfKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some((class, requests)) = self.same_kind(cx) else { return };
        for (thread, request) in &requests {
            self.resolve(*thread, *request, Decision::Allow, window, cx);
        }
        let n = requests.len();
        let message = if n == 1 { format!("Allowed 1 of the {}", class.plural()) } else { format!("Allowed {n} {}", class.plural()) };
        window.push_notification(Notification::success(message), cx);
    }

    fn deny(&mut self, _: &DeckDeny, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.current(cx) else { return };
        if let Some(pending) = thread.pending.first() {
            self.resolve(thread.id, pending.request.id, Decision::Deny { feedback: String::new() }, window, cx);
        }
    }

    fn reply_action(&mut self, _: &DeckReply, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current(cx) {
            self.start_reply(thread.id, window, cx);
        }
    }

    fn snooze_action(&mut self, _: &DeckSnooze, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current(cx) {
            let until = now_ms() + SNOOZE.as_millis() as u64;
            self.snooze(&thread, until, window, cx);
        }
    }

    fn settle_action(&mut self, _: &DeckSettle, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current(cx) {
            self.settle(&thread, window, cx);
        }
    }

    fn open_action(&mut self, _: &DeckOpen, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.current(cx) {
            self.open(thread.id, window, cx);
        }
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_card(&self, thread: &Thread, selected: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let id = thread.id;
        let focused = self.focus.contains_focused(window, cx);
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
        let pending = thread.pending.first();
        let picked = pending.and_then(|p| self.picked.get(&p.request.id));
        let opts = options(thread, picked);
        let since = pending.map(|p| p.created_ms).unwrap_or(thread.updated_ms);

        let header = h_flex()
            .gap_2()
            .child(StatusGlyph::new(("deck-glyph", id.0), thread))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(display_title(thread).to_owned()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when_some(project, |el, p| el.child(ui::Monogram::new(p.name.clone(), p.id.0)).child(p.name.clone()))
                    .when_some(thread.workspace.branch.clone(), |el, branch| {
                        el.child("·").child(div().font_family(theme.mono_font_family.clone()).child(branch))
                    })
                    .child("·")
                    .child(format::relative_time(since, now_ms())),
            );

        let body = self.render_body(thread, &theme);
        let more = thread.pending.len().saturating_sub(1);

        let buttons = h_flex().flex_1().flex_wrap().gap_1p5().children(opts.iter().enumerate().map(|(n, option)| {
            let choice = option.choice.clone();
            let checked = matches!(option.choice, Choice::Toggle(ix) if picked.is_some_and(|p| p.contains(&ix)));
            let strong = option.recommended && selected;
            let button = Button::new(("deck-opt", id.0 ^ ((n as u64 + 1) << 56)))
                .small()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            div()
                                .text_xs()
                                .text_color(if strong { theme.primary_foreground.opacity(0.75) } else { theme.muted_foreground })
                                .child((n + 1).to_string()),
                        )
                        .when(checked, |el| el.child(Icon::new(IconName::Check).xsmall()))
                        .child(option.label.clone()),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let Some(thread) = this.thread(id, cx) else { return };
                    this.selected = Some(id);
                    this.run(&thread, choice.clone(), window, cx);
                }));
            match (option.recommended, selected) {
                (true, true) => button.primary(),
                (true, false) => button.outline(),
                _ => button.selected(checked),
            }
            .into_any_element()
        }));

        let replying = self.replying == Some(id);
        let arrival = self.arrived.get(&id).copied();
        let app = self.app.clone();
        let menu_thread = thread.clone();
        let menu_path = project.map(|p| p.path.clone());

        let card = v_flex()
            .id(("deck-card", id.0))
            .w_full()
            .p_3()
            .gap_2p5()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if selected && focused { theme.ring } else { theme.border })
            .bg(if selected { theme.list_active } else { theme.group_box })
            .cursor_default()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = Some(id);
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .context_menu(move |menu, _, _| thread_menu::build(menu, &app, &menu_thread, menu_path.clone()))
            .child(header)
            .children(body)
            .child(h_flex().gap_2().child(buttons).when(more > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{more} more after this")),
                )
            }))
            .when(replying, |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.reply).small()))
                        .child(
                            Button::new(("deck-send", id.0))
                                .small()
                                .primary()
                                .icon(Icon::new(Lucide::Send))
                                .label("Send")
                                .on_click(cx.listener(|this, _, window, cx| this.send_reply(window, cx))),
                        )
                        .child(
                            Button::new(("deck-cancel", id.0))
                                .small()
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| this.cancel_reply(window, cx))),
                        ),
                )
            });
        match arrival {
            Some((generation, _)) => card
                .with_animation(
                    ElementId::NamedInteger("deck-enter".into(), id.0 ^ ((generation as u64) << 48)),
                    Animation::new(ENTER).with_easing(ease_out_quint()),
                    |el, t| el.opacity(t).mt(rems(-0.5 * (1.0 - t))),
                )
                .into_any_element(),
            None => card.into_any_element(),
        }
    }

    /// The question or request, with what the user needs to decide.
    fn render_body(&self, thread: &Thread, theme: &gpui_kit::component::Theme) -> Option<AnyElement> {
        let code = |text: String, lines: usize| {
            div()
                .w_full()
                .px_2p5()
                .py_1p5()
                .rounded(theme.radius)
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .line_clamp(lines)
                .child(text)
        };
        let caption = |text: String| div().text_sm().line_clamp(3).child(text);
        let muted = |text: String| div().text_xs().text_color(theme.muted_foreground).truncate().child(text);
        if let Some(pending) = thread.pending.first() {
            let el = match &pending.request.kind {
                RequestKind::Command { command, cwd, reason } => v_flex()
                    .gap_1p5()
                    .child(caption(if reason.is_empty() { "Wants to run a command".into() } else { clip(reason, 140) }))
                    .child(code(command.clone(), 4))
                    .when(!cwd.is_empty(), |el| el.child(muted(format!("in {cwd}")))),
                RequestKind::FileChange { path, diff } => v_flex()
                    .gap_1p5()
                    .child(caption(format!("Wants to change {path}")))
                    .when(!diff.trim().is_empty(), |el| el.child(code(diff_excerpt(diff, 6), 6))),
                RequestKind::Tool { name, input } => v_flex()
                    .gap_1p5()
                    .child(caption(format!("Wants to use {name}")))
                    .when(!input.trim().is_empty() && input.trim() != "{}", |el| el.child(code(clip(input, 400), 3))),
                RequestKind::Question { question, .. } => {
                    v_flex().child(div().text_sm().line_clamp(4).child(clip(question, 280)))
                }
                RequestKind::Plan { plan } => v_flex()
                    .gap_1p5()
                    .child(caption("Has a plan for you to approve".into()))
                    .child(div().text_xs().text_color(theme.muted_foreground).line_clamp(5).child(clip(plan, 600))),
            };
            return Some(el.into_any_element());
        }
        match &thread.status {
            ThreadStatus::Failed { message } => {
                Some(div().text_sm().text_color(theme.danger).line_clamp(3).child(clip(message, 280)).into_any_element())
            }
            ThreadStatus::RateLimited { resets_ms } => Some(
                div().text_sm().text_color(theme.muted_foreground).child(format::resets_at(*resets_ms, now_ms())).into_any_element(),
            ),
            _ => None,
        }
    }

    fn render_legend(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let thread = self.current(cx);
        let kind = self.same_kind(cx);
        let approval = thread
            .as_ref()
            .and_then(|t| t.pending.first())
            .is_some_and(|p| RequestClass::of(&p.request.kind).is_approval());
        let item = |keys: AnyElement, label: SharedString| {
            h_flex().gap_1().child(keys).child(div().text_xs().text_color(theme.muted_foreground).child(label))
        };
        let caps = |action: &dyn Action, fallback: &'static str, cx: &App| ui::keycaps(action, cx).unwrap_or_else(|| ui::keycap(fallback));
        let range = h_flex().gap_0p5().text_xs().child(ui::keycap("1")).child("–").child(ui::keycap("9")).into_any_element();
        let mut legend = h_flex()
            .flex_wrap()
            .gap_x_3()
            .gap_y_1()
            .child(item(h_flex().gap_0p5().child(caps(&DeckNext, "j", cx)).child(caps(&DeckPrevious, "k", cx)).into_any_element(), "Move".into()))
            .child(item(range, "Pick".into()));
        if approval {
            legend = legend.child(item(caps(&DeckApprove, "a", cx), "Allow".into()));
            if let Some((class, all)) = kind.filter(|(_, all)| all.len() > 1) {
                legend = legend.child(item(
                    caps(&DeckApproveAllOfKind, "shift-a", cx),
                    format!("Allow all {} {}", all.len(), class.plural()).into(),
                ));
            }
            legend = legend.child(item(caps(&DeckDeny, "d", cx), "Deny".into()));
        } else {
            legend = legend.child(item(caps(&DeckApprove, "a", cx), "Recommended".into()));
        }
        legend
            .child(item(caps(&DeckReply, "r", cx), "Reply".into()))
            .child(item(caps(&DeckSnooze, "s", cx), "Snooze 1 h".into()))
            .child(item(caps(&DeckSettle, "e", cx), "Settle".into()))
            .child(item(caps(&DeckOpen, "enter", cx), "Open".into()))
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let row = |keys: AnyElement, text: &'static str| {
            h_flex()
                .gap_3()
                .child(h_flex().w(rems(6.)).justify_end().child(keys))
                .child(div().text_sm().text_color(theme.muted_foreground).child(text))
        };
        let caps = |action: &dyn Action, fallback: &'static str, cx: &App| ui::keycaps(action, cx).unwrap_or_else(|| ui::keycap(fallback));
        let lesson = v_flex()
            .mt_4()
            .gap_1p5()
            .child(row(
                h_flex().gap_0p5().child(caps(&DeckNext, "j", cx)).child(caps(&DeckPrevious, "k", cx)).into_any_element(),
                "Move between cards",
            ))
            .child(row(
                h_flex().gap_0p5().text_xs().child(ui::keycap("1")).child("–").child(ui::keycap("9")).into_any_element(),
                "Pick an answer",
            ))
            .child(row(caps(&DeckApprove, "a", cx), "Allow, or take the recommended answer"))
            .child(row(caps(&DeckApproveAllOfKind, "shift-a", cx), "Allow every request of the same kind"))
            .child(row(caps(&DeckSnooze, "s", cx), "Snooze for an hour"))
            .child(row(caps(&DeckSettle, "e", cx), "Settle: done for now"))
            .child(row(caps(&GoInbox, "mod-i", cx), "Come back here from anywhere"));
        ui::empty_state(
            Icon::new(Lucide::CheckCheck),
            "Nothing needs you",
            "When an agent asks for approval, has a question, fails or finishes, it lands here as a card you can answer without opening the thread.",
            Some(lesson.into_any_element()),
            cx,
        )
        .into_any_element()
    }
}

/// When a snooze ends, as its toast says it: "until 14:30", "until Tue 09:00".
fn until_label(until_ms: u64) -> String {
    use chrono::TimeZone as _;
    let Some(at) = chrono::Local.timestamp_millis_opt(until_ms as i64).single() else { return String::new() };
    if at.date_naive() == chrono::Local::now().date_naive() {
        format!("until {}", at.format("%H:%M"))
    } else {
        format!("until {}", at.format("%a %H:%M"))
    }
}

/// At most `max` characters, cut at a character boundary with an ellipsis.
pub(crate) fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

/// The changed lines of a diff, without its file headers.
fn diff_excerpt(diff: &str, lines: usize) -> String {
    diff.lines()
        .filter(|l| !(l.starts_with("+++") || l.starts_with("---")))
        .filter(|l| l.starts_with('+') || l.starts_with('-') || l.starts_with("@@"))
        .take(lines)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Focusable for InboxView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for InboxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let root = v_flex()
            .size_full()
            .key_context("Deck")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::pick_action))
            .on_action(cx.listener(Self::approve))
            .on_action(cx.listener(Self::approve_all))
            .on_action(cx.listener(Self::deny))
            .on_action(cx.listener(Self::reply_action))
            .on_action(cx.listener(Self::snooze_action))
            .on_action(cx.listener(Self::settle_action))
            .on_action(cx.listener(Self::open_action))
            .on_action(cx.listener(|this, _: &gpui_kit::component::input::Escape, window, cx| this.cancel_reply(window, cx)));
        if self.order.is_empty() && self.held.is_empty() {
            let settled = self.settled.len();
            return root.child(self.render_empty(cx)).when(settled > 0, |el| {
                el.child(
                    div()
                        .pb_6()
                        .flex()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{settled} settled thread{} still waiting", if settled == 1 { "" } else { "s" })),
                )
            });
        }

        let state = self.app.read(cx).state();
        let threads: Vec<Thread> = self.order.iter().filter_map(|id| state.threads.get(id).cloned()).collect();
        let lane_count = |lane: Lane| threads.iter().filter(|t| Lane::of(t) == Some(lane)).count();
        let counts = {
            let needs = lane_count(Lane::Approval) + lane_count(Lane::Question);
            let mut parts = Vec::new();
            if needs > 0 {
                parts.push(format!("{needs} need{} you", if needs == 1 { "s" } else { "" }));
            }
            for (lane, word) in [(Lane::Failed, "failed"), (Lane::Quota, "waiting for quota"), (Lane::Unread, "new")] {
                let n = lane_count(lane);
                if n > 0 {
                    parts.push(format!("{n} {word}"));
                }
            }
            parts.join(" · ")
        };
        let held = self.held.len();
        let column = |el: AnyElement, cx: &App| div().w_full().max_w(crate::settings::chat_column(cx)).px_4().child(el);

        let header = v_flex()
            .w_full()
            .gap_2()
            .pt_6()
            .pb_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Inbox"))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(counts)),
            )
            .when(held > 0, |el| {
                el.child(
                    h_flex().justify_center().child(
                        Button::new("deck-held")
                            .small()
                            .icon(Icon::new(IconName::ArrowUp))
                            .label(format!("{held} new"))
                            .on_click(cx.listener(|this, _, _, cx| this.show_held(cx))),
                    ),
                )
            });

        let selected = self.current(cx).map(|t| t.id);
        let mut list = v_flex()
            .id("deck-list")
            .flex_1()
            .min_h_0()
            .w_full()
            .items_center()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.set_hovered(*hovered, cx)))
            .child(column(header.into_any_element(), cx));
        for thread in &threads {
            let card = self.render_card(thread, selected == Some(thread.id), window, cx);
            list = list.child(column(div().pb_2().child(card).into_any_element(), cx));
        }
        if !self.settled.is_empty() {
            let n = self.settled.len();
            let open = self.show_settled;
            list = list.child(column(
                h_flex()
                    .py_1()
                    .child(
                        Button::new("deck-settled")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }))
                            .label(format!("{n} settled, still waiting"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_settled = !this.show_settled;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
                cx,
            ));
            if open {
                for id in self.settled.clone() {
                    let Some(thread) = self.thread(id, cx) else { continue };
                    let thread = &thread;
                    let row = h_flex()
                        .id(("deck-settled-row", id.0))
                        .h_8()
                        .px_2()
                        .gap_2()
                        .rounded(theme.radius)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .cursor_default()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _, window, cx| this.open(id, window, cx)))
                        .child(StatusGlyph::glyph(("deck-settled-glyph", id.0), Glyph::of(thread)))
                        .child(div().flex_1().min_w_0().truncate().child(display_title(thread).to_owned()));
                    list = list.child(column(row.into_any_element(), cx));
                }
            }
        }
        list = list.child(div().h_6());

        root.child(list).child(
            h_flex()
                .flex_none()
                .w_full()
                .justify_center()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .child(column(self.render_legend(cx).into_any_element(), cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{BTreeSet, Choice, clip, diff_excerpt, merge_order, next_after, options, triage};
    use orbit_net::state::ClientState;
    use orbit_proto::*;

    fn id() -> ThreadId {
        ThreadId::new()
    }

    #[test]
    fn frozen_decks_keep_places_and_hold_new_cards() {
        let (a, b, c, d) = (id(), id(), id(), id());
        // Unfrozen: the fresh order wins.
        assert_eq!(merge_order(&[a, b, c], &[c, a, d], false), (vec![c, a, d], vec![]));
        // Frozen: b left, a and c keep their places, d waits.
        assert_eq!(merge_order(&[a, b, c], &[c, a, d], true), (vec![a, c], vec![d]));
    }

    #[test]
    fn selection_moves_to_the_neighbour_of_a_card_that_left() {
        let (a, b, c) = (id(), id(), id());
        assert_eq!(next_after(&[a, b, c], b, &[a, c]), Some(c));
        assert_eq!(next_after(&[a, b, c], c, &[a, b]), Some(b));
        assert_eq!(next_after(&[a], a, &[]), None);
    }

    fn pending(kind: RequestKind, created_ms: u64) -> PendingRequest {
        PendingRequest {
            item: ItemId::new(),
            request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
            created_ms,
        }
    }

    #[test]
    fn triage_puts_approvals_first_and_skips_what_is_hidden() {
        let mut state = ClientState::default();
        let mut add = |f: &dyn Fn(&mut Thread)| {
            let mut t = Thread { id: ThreadId::new(), ..Default::default() };
            f(&mut t);
            let id = t.id;
            state.threads.insert(id, t);
            id
        };
        let unread = add(&|t| t.unread = true);
        let question = add(&|t| {
            t.status = ThreadStatus::Waiting;
            t.pending = vec![pending(RequestKind::Question { question: "Which?".into(), options: vec![], multi_select: false }, 50)];
        });
        let old_cmd = add(&|t| {
            t.status = ThreadStatus::Waiting;
            t.pending = vec![pending(RequestKind::Command { command: "ls".into(), cwd: String::new(), reason: String::new() }, 10)];
        });
        let new_cmd = add(&|t| {
            t.status = ThreadStatus::Waiting;
            t.pending = vec![pending(RequestKind::FileChange { path: "a.rs".into(), diff: String::new() }, 90)];
        });
        let failed = add(&|t| t.status = ThreadStatus::Failed { message: "x".into() });
        let _snoozed = add(&|t| {
            t.unread = true;
            t.snoozed_until = Some(u64::MAX);
        });
        let settled = add(&|t| {
            t.status = ThreadStatus::Waiting;
            t.settled_ms = Some(1);
        });
        let _idle = add(&|_| {});
        let (cards, settled_cards) = triage(&state);
        assert_eq!(cards, vec![new_cmd, old_cmd, question, failed, unread]);
        assert_eq!(settled_cards, vec![settled]);
    }

    #[test]
    fn options_number_the_decisions() {
        let mut t = Thread { status: ThreadStatus::Waiting, ..Default::default() };
        t.pending = vec![pending(
            RequestKind::Question {
                question: "Which database?".into(),
                options: vec!["Postgres".into(), "SQLite (Recommended)".into()],
                multi_select: false,
            },
            0,
        )];
        let opts = options(&t, None);
        assert_eq!(opts.len(), 2);
        assert!(opts[1].recommended, "the agent's recommendation is outlined");
        assert_eq!(opts[0].choice, Choice::Decide(Decision::Answer { text: "Postgres".into() }));

        t.pending = vec![pending(
            RequestKind::Question { question: "Pick".into(), options: vec!["a".into(), "b".into()], multi_select: true },
            0,
        )];
        let picked: BTreeSet<usize> = [1].into_iter().collect();
        let opts = options(&t, Some(&picked));
        assert_eq!(opts.last().map(|o| o.label.to_string()), Some("Send 1 picked".into()));

        let failed = Thread { status: ThreadStatus::Failed { message: "x".into() }, ..Default::default() };
        assert_eq!(options(&failed, None)[0].choice, Choice::Open);
    }

    #[test]
    fn clipping_respects_characters() {
        assert_eq!(clip("héllo world", 5), "héllo…");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(diff_excerpt("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx", 3), "@@ -1 +1 @@\n-old\n+new");
    }
}
