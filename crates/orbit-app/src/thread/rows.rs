//! Timeline rows: how a thread is laid out, and how each row reads.
//!
//! Layout ([`build`]) is pure and tested:
//! * items are read turn by turn, so a message that waited in the queue
//!   appears after the turn it waited for, not in the middle of it;
//! * a finished turn folds its work (thinking, steps, notes in between)
//!   behind one line, and its answer is always visible: folding never hides
//!   the reply, only the way there;
//! * a retried turn keeps its earlier attempts, folded under "Attempt 1
//!   failed", with the message shown once;
//! * your own steering messages are never folded away;
//! * a thread that spans days gets a divider at each new day.
//!
//! Rendering:
//! * your messages sit in a quiet bubble at the trailing edge; long ones
//!   collapse;
//! * replies are full-width Markdown with copy, reply, fork and the time;
//! * consecutive tool calls form a work log; a subagent gets its own card
//!   with its live progress;
//! * a request is a card with the exact command or change and buttons that
//!   name the result.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::ThreadView;
use super::find::Part;
use crate::assets::Lucide;
use crate::format;

/// A message longer than this collapses until opened.
const LONG_MESSAGE_LINES: usize = 18;
const LONG_MESSAGE_CHARS: usize = 1_600;

/// Tool output shown inline: the end of it. The inspector has all of it.
const INLINE_LINES: usize = 60;
const INLINE_BYTES: usize = 6 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A day divider, when the thread spans more than one day. Holds a time
    /// in that day.
    Day(u64),
    User(ItemId),
    Assistant(ItemId),
    Reasoning(ItemId),
    /// Consecutive tool calls. Work inside an opened fold (`nested`) shows
    /// its steps without a header of its own: the fold is the header.
    Work { items: Vec<ItemId>, nested: bool },
    /// A subagent the agent started.
    Task(ItemId),
    Plan(ItemId),
    Request(ItemId),
    Notice(ItemId),
    TurnEnd(ItemId),
    Fold(Fold),
    /// The live "working" line at the end while a turn runs.
    Working,
}

/// A run of rows shown as one line until the reader opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    pub kind: FoldKind,
    /// The first item inside; the fold's identity.
    pub key: ItemId,
    pub open: bool,
    pub items: Vec<ItemId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldKind {
    /// A finished turn's work before its answer.
    Work,
    /// An attempt that a retry replaced (1-based).
    Attempt(u32),
}

impl Row {
    /// The items this row shows; updates to them remeasure the row.
    pub fn items(&self) -> Vec<ItemId> {
        match self {
            Row::Work { items, .. } => items.clone(),
            Row::Fold(fold) => fold.items.clone(),
            Row::Day(_) | Row::Working => Vec::new(),
            Row::User(id)
            | Row::Assistant(id)
            | Row::Reasoning(id)
            | Row::Task(id)
            | Row::Plan(id)
            | Row::Request(id)
            | Row::Notice(id)
            | Row::TurnEnd(id) => vec![*id],
        }
    }
}

/// The rows of a timeline and where folded items went.
#[derive(Default, Debug)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Items inside a closed fold → the fold's key.
    pub folded: HashMap<ItemId, ItemId>,
}

/// Whether an item gets a row at all.
fn shown(item: &Item) -> bool {
    if item.status == ItemStatus::Queued {
        return false; // shown above the composer instead
    }
    match &item.body {
        ItemBody::Assistant { text } => !text.is_empty() || item.status == ItemStatus::Streaming,
        ItemBody::Reasoning { text } => !text.trim().is_empty() || item.status == ItemStatus::Streaming,
        // Questions and plan approvals are shown as request cards.
        ItemBody::Tool(call) => !matches!(call.name.as_str(), "AskUserQuestion" | "ExitPlanMode"),
        _ => true,
    }
}

/// The items in reading order: turn by turn, each turn in arrival order.
pub fn display_order(items: &[Item]) -> Vec<&Item> {
    let mut ordered: Vec<&Item> = items.iter().filter(|i| shown(i)).collect();
    ordered.sort_by_key(|i| i.turn); // stable: arrival order within a turn
    ordered
}

fn is_message(item: &Item, steer: bool) -> bool {
    matches!(&item.body, ItemBody::User(m) if m.steer == steer)
}

/// Lays out the timeline. `open` holds the keys of folds the reader opened;
/// `day` gives the local calendar day of a Unix time.
pub fn build(items: &[Item], busy: bool, open: &HashSet<ItemId>, day: impl Fn(u64) -> i32) -> Layout {
    let ordered = display_order(items);
    let turns: Vec<&[&Item]> = ordered.chunk_by(|a, b| a.turn == b.turn).collect();
    let days: Vec<i32> = turns.iter().map(|t| day(t[0].id.created_ms())).collect();
    let several_days = days.windows(2).any(|w| w[0] != w[1]);
    let mut layout = Layout { rows: Vec::with_capacity(ordered.len() + 4), folded: HashMap::new() };
    for (ix, turn) in turns.iter().enumerate() {
        if several_days && (ix == 0 || days[ix] != days[ix - 1]) {
            layout.rows.push(Row::Day(turn[0].id.created_ms()));
        }
        let live = busy && ix + 1 == turns.len();
        layout_turn(turn, live, open, &mut layout);
    }
    if busy {
        layout.rows.push(Row::Working);
    }
    layout
}

fn layout_turn(items: &[&Item], live: bool, open: &HashSet<ItemId>, out: &mut Layout) {
    // The message that opened the turn leads it, once, above every attempt.
    let rest = match items.first() {
        Some(first) if is_message(first, false) => {
            out.rows.push(Row::User(first.id));
            &items[1..]
        }
        _ => items,
    };
    let attempts = split_attempts(rest);
    let last = attempts.len().saturating_sub(1);
    for (ix, attempt) in attempts.iter().enumerate() {
        if ix < last {
            let key = attempt[0].id;
            let is_open = open.contains(&key);
            out.rows.push(Row::Fold(Fold {
                kind: FoldKind::Attempt(ix as u32 + 1),
                key,
                open: is_open,
                items: attempt.iter().map(|i| i.id).collect(),
            }));
            if is_open {
                out.rows.extend(flat_rows(attempt, true));
            } else {
                out.folded.extend(attempt.iter().map(|i| (i.id, key)));
            }
        } else if live || !attempt.iter().any(|i| matches!(i.body, ItemBody::TurnEnd(_))) {
            out.rows.extend(flat_rows(attempt, false));
        } else {
            fold_work(attempt, open, out);
        }
    }
}

/// Splits a turn's items into attempts: each ends with its turn-end record
/// and the notices right after it (why it ended).
fn split_attempts<'a>(items: &'a [&'a Item]) -> Vec<&'a [&'a Item]> {
    let mut attempts = Vec::new();
    let mut start = 0;
    let mut ended = false;
    for (ix, item) in items.iter().enumerate() {
        match &item.body {
            ItemBody::TurnEnd(_) => ended = true,
            ItemBody::Notice { .. } => {}
            _ if ended => {
                attempts.push(&items[start..ix]);
                start = ix;
                ended = false;
            }
            _ => {}
        }
    }
    if start < items.len() {
        attempts.push(&items[start..]);
    }
    attempts
}

/// A finished attempt: the work before the answer folds, the answer and the
/// turn's footer stay. Your steering messages split the fold and stay.
fn fold_work(items: &[&Item], open: &HashSet<ItemId>, out: &mut Layout) {
    let end = items.iter().position(|i| matches!(i.body, ItemBody::TurnEnd(_))).unwrap_or(items.len());
    let answer = items[..end].iter().rposition(|i| matches!(i.body, ItemBody::Assistant { .. }));
    let work_end = answer.unwrap_or(end);
    let mut start = 0;
    for ix in 0..=work_end {
        if ix == work_end || is_message(items[ix], true) {
            fold_chunk(&items[start..ix], open, out);
            if ix < work_end {
                out.rows.push(Row::User(items[ix].id));
            }
            start = ix + 1;
        }
    }
    out.rows.extend(flat_rows(&items[work_end..], false));
}

fn fold_chunk(items: &[&Item], open: &HashSet<ItemId>, out: &mut Layout) {
    if items.is_empty() {
        return;
    }
    let rows = flat_rows(items, true);
    if rows.len() < 2 {
        // One row folds itself (a work log or a thought); a fold adds nothing.
        out.rows.extend(flat_rows(items, false));
        return;
    }
    let key = items[0].id;
    let is_open = open.contains(&key);
    out.rows.push(Row::Fold(Fold { kind: FoldKind::Work, key, open: is_open, items: items.iter().map(|i| i.id).collect() }));
    if is_open {
        out.rows.extend(rows);
    } else {
        out.folded.extend(items.iter().map(|i| (i.id, key)));
    }
}

fn flat_rows(items: &[&Item], nested: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::with_capacity(items.len());
    for item in items {
        let row = match &item.body {
            ItemBody::User(_) => Row::User(item.id),
            ItemBody::Assistant { .. } => Row::Assistant(item.id),
            ItemBody::Reasoning { .. } => Row::Reasoning(item.id),
            ItemBody::Tool(call) if call.kind == ToolKind::Task => Row::Task(item.id),
            ItemBody::Tool(_) => {
                if let Some(Row::Work { items, .. }) = rows.last_mut() {
                    items.push(item.id);
                    continue;
                }
                Row::Work { items: vec![item.id], nested }
            }
            ItemBody::Plan { .. } => Row::Plan(item.id),
            ItemBody::Request(_) => Row::Request(item.id),
            ItemBody::Notice { .. } | ItemBody::Handoff(_) => Row::Notice(item.id),
            ItemBody::TurnEnd(_) => Row::TurnEnd(item.id),
        };
        rows.push(row);
    }
    rows
}

/// The smallest change from `old` to `new` rows: the range of old rows to
/// replace and how many new rows take their place. Rows outside it are
/// kept, with their measured heights, so the reader's position holds.
pub fn splice(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

// ── Rendering ─────────────────────────────────────────────────────────────

/// A row centered in the reading column, `width` wide at most (Settings ›
/// Appearance › Chat width).
fn column(content: impl IntoElement, width: Length) -> Div {
    h_flex().w_full().justify_center().px_4().child(div().w_full().max_w(width).child(content))
}

/// `text` with find matches painted behind it; the current one stronger.
fn marked(text: SharedString, hits: &[(Range<usize>, bool)], theme: &gpui_kit::component::Theme) -> StyledText {
    let runs: Vec<(Range<usize>, HighlightStyle)> = hits
        .iter()
        .filter(|(r, _)| r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
        .map(|(r, current)| {
            let background = if *current { theme.warning.opacity(0.55) } else { theme.warning.opacity(0.22) };
            (r.clone(), HighlightStyle { background_color: Some(background), ..Default::default() })
        })
        .collect();
    StyledText::new(text).with_highlights(runs)
}

/// A muted time that shows the full date on hover.
fn time_label(id: impl Into<ElementId>, ms: u64, now: u64, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let full: SharedString = format::full_timestamp(ms).into();
    div()
        .id(id)
        .px_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(format::timestamp(ms, now))
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
}

fn chevron(open: bool) -> Icon {
    Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall()
}

pub fn render_row(view: &Entity<ThreadView>, row: &Row, _: &mut Window, cx: &mut App) -> AnyElement {
    let this = view.read(cx);
    let state = this.app.read(cx).state();
    let Some(timeline) = state.timeline(this.id) else { return div().into_any_element() };
    let thread = state.threads.get(&this.id);
    let busy = thread.is_some_and(|t| t.status.is_busy());
    let theme = cx.theme().clone();
    let item = |id: ItemId| timeline.get(id);
    let weak = view.downgrade();
    let now = now_ms();
    let empty = || div().into_any_element();

    let content: AnyElement = match row {
        Row::Day(ms) => h_flex()
            .pt_6()
            .pb_1()
            .gap_3()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.muted_foreground)
            .child(div().h_px().flex_1().bg(theme.border))
            .child(format::day_label(*ms, now))
            .child(div().h_px().flex_1().bg(theme.border))
            .into_any_element(),
        Row::User(id) => {
            let Some(item) = item(*id) else { return empty() };
            let ItemBody::User(message) = &item.body else { return empty() };
            render_user(this, &weak, item, message, busy, now, &theme).into_any_element()
        }
        Row::Assistant(id) => {
            let Some(item) = item(*id) else { return empty() };
            let Some(md) = this.markdown.get(id).cloned() else { return empty() };
            let ItemBody::Assistant { text } = &item.body else { return empty() };
            render_assistant(&weak, item, text, md, now, &theme).into_any_element()
        }
        Row::Reasoning(id) => {
            let Some(item) = item(*id) else { return empty() };
            let streaming = item.status == ItemStatus::Streaming;
            let open = this.expanded.contains(id);
            let key = *id;
            let header = h_flex()
                .id(("reasoning", id.0))
                .gap_1p5()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .cursor_default()
                .hover(|s| s.text_color(theme.foreground))
                .on_click({
                    let weak = weak.clone();
                    move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_expanded(key, cx));
                    }
                })
                .child(Icon::new(Lucide::Brain).xsmall())
                .child(if streaming {
                    ShimmerText::new("Thinking…").id(("thinking", id.0)).into_any_element()
                } else {
                    div().child("Thought").into_any_element()
                })
                .child(chevron(open));
            v_flex()
                .pt_2()
                .child(header)
                .when(open, |el| match this.markdown.get(id).cloned() {
                    Some(md) => el.child(
                        div()
                            .pl_4()
                            .ml_1p5()
                            .border_l_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(TextView::new(&md).selectable(true)),
                    ),
                    None => el,
                })
                .into_any_element()
        }
        Row::Work { items, nested } => {
            let calls: Vec<&Item> = items.iter().filter_map(|id| item(*id)).collect();
            if calls.is_empty() {
                return empty();
            }
            let live = calls.iter().any(|i| i.status == ItemStatus::Streaming);
            let key = items[0];
            // A live work log stays open so you can follow it; a finished one
            // folds away unless you open it (inside an opened fold it starts
            // open: you asked to see the work).
            // Inside an opened fold the fold is the header: the steps show
            // directly, without a second "Worked" line.
            let open = live || *nested || this.expanded.contains(&key);
            let failed = calls.iter().filter(|i| i.status == ItemStatus::Failed).count();
            let summary = format!(
                "{} · {} step{}",
                if live { "Working" } else { "Worked" },
                calls.len(),
                if calls.len() == 1 { "" } else { "s" }
            );
            let header = h_flex()
                .id(("work", key.0))
                .gap_1p5()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .cursor_default()
                .hover(|s| s.text_color(theme.foreground))
                .on_click({
                    let weak = weak.clone();
                    move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_expanded(key, cx));
                    }
                })
                .child(Icon::new(Lucide::Wrench).xsmall())
                .child(summary)
                .when(failed > 0, |el| el.child(div().text_color(theme.danger).child(format!("· {failed} failed"))))
                .when(!live, |el| el.child(chevron(open)));
            let mut list = v_flex().gap_0p5().pl_1();
            if open {
                for call in &calls {
                    list = list.child(render_tool(this, &weak, call, &theme));
                }
            }
            v_flex().pt_2().when(!*nested, |el| el.child(header)).when(open, |el| el.child(list)).into_any_element()
        }
        Row::Task(id) => {
            let Some(item) = item(*id) else { return empty() };
            render_task(&weak, item, &theme)
        }
        Row::Plan(id) => {
            let Some(item) = item(*id) else { return empty() };
            let ItemBody::Plan { steps } = &item.body else { return empty() };
            v_flex()
                .mt_3()
                .p_3()
                .gap_1p5()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(Lucide::ListChecks).xsmall())
                        .child(format!(
                            "Plan · {} of {} done",
                            steps.iter().filter(|s| s.state == StepState::Done).count(),
                            steps.len()
                        )),
                )
                .children(steps.iter().enumerate().map(|(ix, step)| {
                    let (icon, color) = match step.state {
                        StepState::Done => (Icon::new(IconName::CircleCheck), theme.success),
                        StepState::Active => (Icon::new(Lucide::CircleDot), theme.primary),
                        StepState::Pending => (Icon::new(Lucide::CircleDot), theme.muted_foreground),
                    };
                    let hits = this.hits(item.id, Part::Step(ix));
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .when(step.state == StepState::Done, |el| el.text_color(theme.muted_foreground).line_through())
                        .child(icon.xsmall().text_color(color))
                        .child(marked(step.text.clone().into(), &hits, &theme))
                }))
                .into_any_element()
        }
        Row::Request(id) => {
            let Some(item) = item(*id) else { return empty() };
            let ItemBody::Request(request) = &item.body else { return empty() };
            let hits = this.hits(item.id, Part::Body);
            render_request(&this.app, this.id, request, &hits, &theme)
        }
        Row::Notice(id) => {
            let Some(item) = item(*id) else { return empty() };
            let handoff;
            let (level, text) = match &item.body {
                ItemBody::Notice { level, text } => (level, text),
                // The full context is in the item; the row says what happened.
                ItemBody::Handoff(h) => {
                    handoff = format!(
                        "Context handed off from {} to {}: {} turn{} in full{}, about {} tokens.",
                        h.from,
                        h.to,
                        h.turns,
                        if h.turns == 1 { "" } else { "s" },
                        if h.condensed > 0 { format!(", {} condensed", h.condensed) } else { String::new() },
                        h.tokens
                    );
                    (&NoticeLevel::Info, &handoff)
                }
                _ => return empty(),
            };
            let (icon, color) = match level {
                NoticeLevel::Info => (Icon::new(IconName::Info), theme.muted_foreground),
                NoticeLevel::Warning => (Icon::new(IconName::TriangleAlert), theme.warning),
                NoticeLevel::Error => (Icon::new(IconName::CircleAlert), theme.danger),
            };
            let hits = this.hits(item.id, Part::Body);
            h_flex()
                .mt_2()
                .gap_2()
                .px_3()
                .py_2()
                .items_start()
                .rounded(theme.radius)
                .when(*level == NoticeLevel::Error, |el| el.bg(theme.danger.opacity(0.08)))
                .text_sm()
                .text_color(color)
                .child(div().pt_0p5().child(icon.small()))
                .child(div().flex_1().min_w_0().child(marked(text.clone().into(), &hits, &theme)))
                .into_any_element()
        }
        Row::Fold(fold) => render_fold(&weak, fold, timeline, &theme).into_any_element(),
        Row::Working => {
            let waiting = thread.is_some_and(|t| t.status == ThreadStatus::Waiting);
            let since = this.turn_started.unwrap_or_else(std::time::Instant::now);
            let elapsed = format::duration(since.elapsed().as_millis() as u64 / 1000 * 1000);
            // While the agent waits on its subagents, say so: the count is
            // what the reader wants to know.
            let subagents = timeline
                .items()
                .iter()
                .rev()
                .take(400)
                .filter(|i| {
                    i.status == ItemStatus::Streaming && matches!(&i.body, ItemBody::Tool(c) if c.kind == ToolKind::Task)
                })
                .count();
            let label: SharedString = match (waiting, subagents) {
                (true, _) => "Waiting for you".into(),
                (false, 0) => "Working".into(),
                (false, 1) => "Working · 1 subagent running".into(),
                (false, n) => format!("Working · {n} subagents running").into(),
            };
            h_flex()
                .pt_3()
                .gap_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(crate::ui::StatusGlyph::attention(
                    "working-glyph",
                    if waiting { crate::ui::Attention::Waiting } else { crate::ui::Attention::Working },
                ))
                .child(if waiting {
                    div().child(label).into_any_element()
                } else {
                    ShimmerText::new(label).id("working-shimmer").into_any_element()
                })
                .child(elapsed)
                .into_any_element()
        }
        Row::TurnEnd(id) => {
            let Some(item) = item(*id) else { return empty() };
            let ItemBody::TurnEnd(summary) = &item.body else { return empty() };
            render_turn_end(this, item, summary, busy, &theme).into_any_element()
        }
    };
    column(content, crate::settings::chat_column(cx)).into_any_element()
}

fn render_user(
    this: &ThreadView,
    weak: &WeakEntity<ThreadView>,
    item: &Item,
    message: &UserMessage,
    busy: bool,
    now: u64,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let (id, turn) = (item.id, item.turn);
    let text = message.text.clone();
    let long = text.len() > LONG_MESSAGE_CHARS || text.lines().count() > LONG_MESSAGE_LINES;
    let expanded = this.expanded.contains(&id);
    let shown: SharedString = if long && !expanded { collapsed(&text).into() } else { text.clone().into() };
    let hits = this.hits(id, Part::Body);
    let app = this.app.clone();
    let thread_id = this.id;
    v_flex()
        .items_end()
        .gap_1()
        .pt_5()
        .group("user-msg")
        .child(
            v_flex()
                .max_w(relative(0.85))
                .px_4()
                .py_2p5()
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .text_color(theme.secondary_foreground)
                .text_sm()
                .when(message.steer, |el| el.border_1().border_color(theme.ring))
                .child(marked(shown, &hits, theme))
                .when(long, |el| {
                    let weak = weak.clone();
                    el.child(
                        div().pt_1().child(
                            Button::new(("long-msg", id.0))
                                .ghost()
                                .xsmall()
                                .label(if expanded { "Show less" } else { "Show full message" })
                                .on_click(move |_, _, cx| {
                                    let _ = weak.update(cx, |this, cx| this.toggle_expanded(id, cx));
                                }),
                        ),
                    )
                }),
        )
        .when(!message.attachments.is_empty(), |el| {
            el.child(h_flex().gap_1().flex_wrap().justify_end().children(message.attachments.iter().map(|a| {
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::Paperclip).xsmall())
                    .child(a.name.clone())
                    .child(div().child(format::bytes(a.size)))
            })))
        })
        .child(
            h_flex()
                .gap_0p5()
                .opacity(0.)
                .group_hover("user-msg", |s| s.opacity(1.))
                .when(message.steer, |el| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).mr_1().child("Steered the turn"))
                })
                .child(time_label(("user-time", id.0), id.created_ms(), now, theme))
                .child(Clipboard::new(("copy-user", id.0)).value(text.clone()))
                .when(!busy, |el| {
                    el.child(
                        Button::new(("edit-user", id.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::Pencil)
                            .tooltip("Edit and resend from here")
                            .on_click(move |_, window, cx| {
                                crate::review::edit_message(app.clone(), thread_id, id, turn, text.clone(), window, cx)
                            }),
                    )
                }),
        )
}

/// The first lines of a long message.
fn collapsed(text: &str) -> String {
    let mut out: String = text.lines().take(LONG_MESSAGE_LINES - 6).collect::<Vec<_>>().join("\n");
    if out.len() > LONG_MESSAGE_CHARS * 2 / 3 {
        let mut cut = LONG_MESSAGE_CHARS * 2 / 3;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out.push('…');
    out
}

fn render_assistant(
    weak: &WeakEntity<ThreadView>,
    item: &Item,
    text: &str,
    md: Entity<gpui_kit::component::text::TextViewState>,
    now: u64,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let id = item.id;
    let streaming = item.status == ItemStatus::Streaming;
    let text: SharedString = text.to_owned().into();
    let body = div()
        .id(("agent-body", id.0))
        .child(
            TextView::new(&md)
                .selectable(true)
                .stream_fade(streaming)
                .code_block_actions(|block, _, cx| code_block_actions(block.lang(), block.code(), cx)),
        )
        .context_menu({
            let (weak, text) = (weak.clone(), text.clone());
            move |menu, _, _| {
                let (quote, copy, fork) = (weak.clone(), text.clone(), weak.clone());
                menu.item(PopupMenuItem::new("Copy as Markdown").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                }))
                .item(PopupMenuItem::new("Quote in reply").on_click(move |_, window, cx| {
                    let _ = quote.update(cx, |this, cx| this.quote_reply(id, window, cx));
                }))
                .separator()
                .item(PopupMenuItem::new("Fork from here").on_click(move |_, window, cx| {
                    let _ = fork.update(cx, |this, cx| this.fork_at(id, window, cx));
                }))
            }
        });
    v_flex()
        .pt_3()
        .gap_1()
        .group("agent-msg")
        .text_sm()
        .child(body)
        .when(!streaming, |el| {
            let (quote, fork) = (weak.clone(), weak.clone());
            el.child(
                h_flex()
                    .h_6()
                    .gap_0p5()
                    .opacity(0.)
                    .group_hover("agent-msg", |s| s.opacity(1.))
                    .child(Clipboard::new(("copy-agent", id.0)).value(text))
                    .child(
                        Button::new(("quote", id.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::Reply)
                            .tooltip("Reply: quotes your selection, or the whole reply")
                            .on_click(move |_, window, cx| {
                                let _ = quote.update(cx, |this, cx| this.quote_reply(id, window, cx));
                            }),
                    )
                    .child(
                        Button::new(("fork", id.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::GitFork)
                            .tooltip("Fork from here: a new thread with the history up to this reply")
                            .on_click(move |_, window, cx| {
                                let _ = fork.update(cx, |this, cx| this.fork_at(id, window, cx));
                            }),
                    )
                    .child(time_label(("agent-time", id.0), id.created_ms(), now, theme)),
            )
        })
}

/// The corner of a fenced code block: its language and a copy button.
fn code_block_actions(lang: Option<SharedString>, code: SharedString, cx: &App) -> AnyElement {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    h_flex()
        .gap_1()
        .items_center()
        .when_some(lang.filter(|l| !l.is_empty()), |el, lang| {
            el.child(div().text_xs().text_color(cx.theme().muted_foreground).child(lang))
        })
        .child(Clipboard::new(ElementId::Name(format!("code-{:x}", hasher.finish()).into())).value(code))
        .into_any_element()
}

fn tool_icon(kind: ToolKind) -> Icon {
    match kind {
        ToolKind::Command => Icon::new(Lucide::Terminal),
        ToolKind::Read => Icon::new(Lucide::FileSearch),
        ToolKind::Edit => Icon::new(Lucide::FilePen),
        ToolKind::Search => Icon::new(Lucide::SearchCode),
        ToolKind::Web => Icon::new(IconName::Globe),
        ToolKind::Task => Icon::new(IconName::Bot),
        ToolKind::Mcp => Icon::new(Lucide::Plug),
        ToolKind::Todo => Icon::new(Lucide::ListTodo),
        ToolKind::Other => Icon::new(Lucide::Wrench),
    }
}

fn spinner(id: impl Into<ElementId>, theme: &gpui_kit::component::Theme) -> AnyElement {
    Icon::new(IconName::LoaderCircle)
        .xsmall()
        .text_color(theme.muted_foreground)
        .with_animation(id, Animation::new(Duration::from_millis(900)).repeat(), |icon, t| {
            icon.transform(Transformation::rotate(percentage(t)))
        })
        .into_any_element()
}

/// The end of a tool's output, for the inline peek, and whether earlier
/// output was left out.
pub fn output_peek(call: &ToolCall) -> (&str, bool) {
    let tail = output_tail(&call.output, INLINE_BYTES);
    let lines: Vec<usize> = tail.match_indices('\n').map(|(at, _)| at).collect();
    let tail = if lines.len() > INLINE_LINES { &tail[lines[lines.len() - INLINE_LINES - 1] + 1..] } else { tail };
    let cut = tail.len() < call.output.len() || call.output_blob.is_some();
    (tail, cut)
}

fn render_tool(this: &ThreadView, weak: &WeakEntity<ThreadView>, item: &Item, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let ItemBody::Tool(call) = &item.body else { return v_flex() };
    let running = item.status == ItemStatus::Streaming;
    let failed = item.status == ItemStatus::Failed;
    let key = item.id;
    let open = this.expanded.contains(&key);
    let changes: (u32, u32) = call.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let status: AnyElement = if running {
        spinner(("tool-spin", key.0), theme)
    } else if failed {
        Icon::new(IconName::CircleX).xsmall().text_color(theme.danger).into_any_element()
    } else {
        div().size_3().into_any_element()
    };
    let title: SharedString = if call.title.is_empty() { call.name.clone().into() } else { call.title.clone().into() };
    let mono = matches!(call.kind, ToolKind::Command | ToolKind::Read | ToolKind::Edit | ToolKind::Search);
    let hits = this.hits(key, Part::Title);
    let inspect = weak.clone();
    v_flex()
        .child(
            h_flex()
                .id(("tool", key.0))
                .group("tool-row")
                .gap_2()
                .py_0p5()
                .px_1()
                .rounded(theme.radius)
                .text_xs()
                .cursor_default()
                .hover(|s| s.bg(theme.muted))
                .when(open, |el| el.bg(theme.muted))
                .on_click({
                    let weak = weak.clone();
                    move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_expanded(key, cx));
                    }
                })
                .child(tool_icon(call.kind).xsmall().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                        .text_color(if failed { theme.danger } else { theme.foreground })
                        .child(marked(title, &hits, theme)),
                )
                .when(changes != (0, 0), |el| {
                    el.child(div().text_color(theme.success).child(format!("+{}", changes.0)))
                        .child(div().text_color(theme.danger).child(format!("−{}", changes.1)))
                })
                .when_some(call.exit_code.filter(|c| *c != 0), |el, code| {
                    el.child(div().text_color(theme.danger).child(format!("exit {code}")))
                })
                .child(
                    div().opacity(0.).group_hover("tool-row", |s| s.opacity(1.)).child(
                        Button::new(("inspect", key.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ScanSearch)
                            .tooltip("Inspect: full input and output")
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                let _ = inspect.update(cx, |this, cx| this.inspect(key, false, window, cx));
                            }),
                    ),
                )
                .child(status),
        )
        .when(open, |el| el.child(render_tool_details(weak, item, call, theme)))
}

/// The inline peek under an opened step: the end of its output and what to
/// do with it.
fn render_tool_details(weak: &WeakEntity<ThreadView>, item: &Item, call: &ToolCall, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let key = item.id;
    let (tail, cut) = output_peek(call);
    let (inspect, diff) = (weak.clone(), weak.clone());
    let has_files = !call.files.is_empty();
    v_flex()
        .ml_6()
        .mt_0p5()
        .mb_1()
        .gap_1()
        .when(!tail.is_empty(), |el| {
            el.child(
                v_flex()
                    .id(("tool-output", key.0))
                    .p_2()
                    .max_h(rems(18.))
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(cut, |el| el.child(div().italic().pb_1().child("Earlier output not shown")))
                    .child(tail.to_owned()),
            )
        })
        .when(tail.is_empty() && !has_files && item.status != ItemStatus::Streaming, |el| {
            el.child(div().text_xs().text_color(theme.muted_foreground).child("No output"))
        })
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(("inspect-open", key.0))
                        .ghost()
                        .xsmall()
                        .icon(Lucide::ScanSearch)
                        .label("Inspect")
                        .on_click(move |_, window, cx| {
                            let _ = inspect.update(cx, |this, cx| this.inspect(key, false, window, cx));
                        }),
                )
                .when(has_files, |el| {
                    el.child(
                        Button::new(("open-diff", key.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FileDiff)
                            .label("Open diff")
                            .on_click(move |_, window, cx| {
                                let _ = diff.update(cx, |this, cx| this.inspect(key, true, window, cx));
                            }),
                    )
                })
                .when(!call.output.is_empty(), |el| {
                    el.child(Clipboard::new(("copy-output", key.0)).value(call.output.clone()))
                }),
        )
}

/// Steps a running subagent has reported (one per line while it works).
pub fn subagent_steps(call: &ToolCall) -> Vec<&str> {
    call.output.lines().filter(|l| !l.trim().is_empty()).collect()
}

fn render_task(weak: &WeakEntity<ThreadView>, item: &Item, theme: &gpui_kit::component::Theme) -> AnyElement {
    let ItemBody::Tool(call) = &item.body else { return div().into_any_element() };
    let key = item.id;
    let running = item.status == ItemStatus::Streaming;
    let failed = item.status == ItemStatus::Failed;
    let title = if call.title.is_empty() { "Subagent".to_owned() } else { call.title.clone() };
    let (status, color) = match item.status {
        ItemStatus::Streaming => ("Running", theme.muted_foreground),
        ItemStatus::Failed => ("Failed", theme.danger),
        ItemStatus::Cancelled => ("Stopped", theme.muted_foreground),
        _ => ("Done", theme.success),
    };
    let steps = subagent_steps(call);
    let detail: Vec<String> = if running {
        steps.iter().rev().take(3).rev().map(|s| s.to_string()).collect()
    } else {
        call.output.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).map(str::to_owned).collect()
    };
    let weak = weak.clone();
    v_flex()
        .id(("task", key.0))
        .mt_2()
        .p_3()
        .gap_1p5()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(if failed { theme.danger.opacity(0.5) } else { theme.border })
        .cursor_default()
        .hover(|s| s.bg(theme.muted.opacity(0.5)))
        .on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| this.inspect(key, false, window, cx));
        })
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .child(Icon::new(IconName::Bot).small().text_color(theme.muted_foreground))
                .child(div().text_xs().text_color(theme.muted_foreground).child("Subagent"))
                .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(title))
                .when(running, |el| el.child(spinner(("task-spin", key.0), theme)))
                .child(div().text_xs().text_color(color).child(status)),
        )
        .when(running && !steps.is_empty(), |el| {
            el.child(div().pl_6().text_xs().text_color(theme.muted_foreground).child(format!(
                "{} step{} so far",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            )))
        })
        .when(!detail.is_empty(), |el| {
            el.child(
                v_flex()
                    .pl_6()
                    .gap_0p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(running, |el| el.font_family(theme.mono_font_family.clone()))
                    .children(detail.into_iter().map(|line| div().truncate().child(line))),
            )
        })
        .into_any_element()
}

fn render_fold(
    weak: &WeakEntity<ThreadView>,
    fold: &Fold,
    timeline: &orbit_net::state::Timeline,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let items: Vec<&Item> = fold.items.iter().filter_map(|id| timeline.get(*id)).collect();
    let steps = items.iter().filter(|i| matches!(i.body, ItemBody::Tool(_))).count();
    let failed = items.iter().filter(|i| matches!(i.body, ItemBody::Tool(_)) && i.status == ItemStatus::Failed).count();
    let subagents = items.iter().filter(|i| matches!(&i.body, ItemBody::Tool(c) if c.kind == ToolKind::Task)).count();
    let mut parts = Vec::new();
    let (icon, lead) = match fold.kind {
        FoldKind::Work => (Icon::new(Lucide::Wrench), "Worked".to_owned()),
        FoldKind::Attempt(number) => {
            let error = items.iter().find_map(|i| match &i.body {
                ItemBody::Notice { level: NoticeLevel::Error, text } => Some(text.lines().next().unwrap_or("").to_owned()),
                _ => None,
            });
            let stopped = items.iter().any(|i| matches!(&i.body, ItemBody::TurnEnd(s) if s.interrupted));
            let outcome = match (error, stopped) {
                (Some(error), _) => format!("failed: {error}"),
                (None, true) => "stopped".to_owned(),
                (None, false) => "ended early".to_owned(),
            };
            (Icon::new(Lucide::RotateCcw), format!("Attempt {number} {outcome}"))
        }
    };
    if steps > 0 {
        parts.push(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
    }
    if fold.kind == FoldKind::Work {
        let thoughts = items.iter().filter(|i| matches!(i.body, ItemBody::Reasoning { .. })).count();
        let notes = items.iter().filter(|i| matches!(i.body, ItemBody::Assistant { .. })).count();
        if thoughts > 0 {
            parts.push(format!("{thoughts} thought{}", if thoughts == 1 { "" } else { "s" }));
        }
        if notes > 0 {
            parts.push(format!("{notes} note{}", if notes == 1 { "" } else { "s" }));
        }
    }
    if subagents > 0 {
        parts.push(format!("{subagents} subagent{}", if subagents == 1 { "" } else { "s" }));
    }
    let key = fold.key;
    let open = fold.open;
    let weak = weak.clone();
    h_flex()
        .id(("fold", key.0))
        .mt_2()
        .gap_1p5()
        .py_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .cursor_default()
        .hover(|s| s.text_color(theme.foreground))
        .on_click(move |_, _, cx| {
            let _ = weak.update(cx, |this, cx| this.toggle_fold(key, cx));
        })
        .child(icon.xsmall())
        .child(div().min_w_0().truncate().child(lead))
        .when(!parts.is_empty(), |el| el.child(format!("· {}", parts.join(" · "))))
        .when(failed > 0, |el| el.child(div().text_color(theme.danger).child(format!("· {failed} failed"))))
        .child(chevron(open))
}

fn render_turn_end(
    this: &ThreadView,
    item: &Item,
    summary: &TurnSummary,
    busy: bool,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let id = item.id;
    let mut parts = vec![format::duration(summary.duration_ms)];
    let tokens = summary.usage.input_tokens + summary.usage.output_tokens;
    if tokens > 0 {
        parts.push(format!("{} tokens", format::tokens(tokens)));
    }
    let cost = format::cost(summary.usage.cost_micros);
    if !cost.is_empty() {
        parts.push(cost);
    }
    if summary.interrupted {
        parts.insert(0, "Stopped".into());
    }
    let files = summary.files.len();
    let (adds, dels) = summary.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let restorable = summary.base_checkpoint.is_some() && files > 0 && !busy;
    let (app, thread_id, turn) = (this.app.clone(), this.id, item.turn);
    let review_app = app.clone();
    v_flex()
        .mt_1()
        .gap_1()
        .group("turn-end")
        .child(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(div().h_px().flex_1().bg(theme.border))
                .child(parts.join(" · "))
                .when(files > 0, |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .child(format!("{files} file{}", if files == 1 { "" } else { "s" }))
                            .child(div().text_color(theme.success).child(format!("+{adds}")))
                            .child(div().text_color(theme.danger).child(format!("−{dels}"))),
                    )
                })
                .child(div().h_px().flex_1().bg(theme.border)),
        )
        .when(files > 0, |el| {
            el.child(
                h_flex()
                    .justify_center()
                    .gap_1()
                    .opacity(0.)
                    .group_hover("turn-end", |s| s.opacity(1.))
                    .child(
                        Button::new(("turn-review", id.0))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FileDiff)
                            .label("Review changes")
                            .tooltip("This turn’s changes, file by file")
                            .on_click(move |_, window, cx| {
                                crate::review::open_at(review_app.clone(), thread_id, Some(DiffScope::Turn(turn)), window, cx)
                            }),
                    )
                    .when(restorable, |el| {
                        el.child(
                            Button::new(("turn-undo", id.0))
                                .ghost()
                                .xsmall()
                                .icon(Lucide::RotateCcw)
                                .label("Undo turn…")
                                .tooltip("Put the files back the way they were before this turn. You can undo this too.")
                                .on_click(move |_, window, cx| crate::review::confirm_restore(app.clone(), thread_id, turn, window, cx)),
                        )
                    }),
            )
        })
}

fn render_request(
    app: &Entity<crate::app_state::AppState>,
    thread: ThreadId,
    request: &AgentRequest,
    hits: &[(Range<usize>, bool)],
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    // Questions are answered in the panel above the composer; the timeline
    // keeps a one-line record.
    if let RequestKind::Question { question, .. } = &request.kind {
        let answer = match &request.resolution {
            Some(Decision::Answer { text }) => Some(format!("Answered: {}", text.replace('\n', ", "))),
            Some(_) => Some("Not answered".to_owned()),
            None => None,
        };
        let pending = answer.is_none();
        return v_flex()
            .mt_2()
            .gap_0p5()
            .text_xs()
            .child(
                h_flex()
                    .gap_1p5()
                    .text_color(if pending { theme.warning } else { theme.muted_foreground })
                    .child(Icon::new(Lucide::MessageSquare).xsmall())
                    .child(div().min_w_0().child(marked(question.clone().into(), hits, theme))),
            )
            .child(div().pl_5().text_color(theme.muted_foreground).child(match answer {
                Some(answer) => answer,
                None => "Waiting for your answer below".to_owned(),
            }))
            .into_any_element();
    }
    let (title, body): (String, Option<String>) = match &request.kind {
        RequestKind::Command { command, cwd, reason } => (
            if reason.is_empty() { "Run this command?".into() } else { reason.clone() },
            Some(if cwd.is_empty() { command.clone() } else { format!("{command}\n  in {cwd}") }),
        ),
        RequestKind::FileChange { path, diff } => (
            format!("Change {path}?"),
            (!diff.is_empty()).then(|| diff.lines().take(24).collect::<Vec<_>>().join("\n")),
        ),
        RequestKind::Tool { name, input } => (format!("Use {name}?"), Some(format::pretty_json(input))),
        RequestKind::Plan { plan } => ("Carry out this plan?".into(), Some(plan.clone())),
        RequestKind::Question { .. } => unreachable!("handled above"),
    };
    if let Some(decision) = &request.resolution {
        let (icon, text) = match decision {
            Decision::Allow => (IconName::Check, "Allowed"),
            Decision::AllowForSession => (IconName::Check, "Allowed for this session"),
            Decision::Deny { .. } => (IconName::Close, "Denied"),
            Decision::Answer { .. } => (IconName::Check, "Answered"),
        };
        return h_flex()
            .mt_1()
            .gap_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(Icon::new(icon).xsmall())
            .child(text)
            .child("·")
            .child(div().truncate().child(title))
            .into_any_element();
    }
    let resolve = |decision: Decision| {
        let app = app.clone();
        let request = request.id;
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let decision = decision.clone();
            app.update(cx, |app, cx| app.run(Command::Resolve { thread, request, decision }, window, cx));
        }
    };
    let mut actions = h_flex().gap_2().justify_end().pt_1();
    match &request.kind {
        RequestKind::Plan { .. } => {
            actions = actions
                .child(
                    Button::new(("deny", request.id.0))
                        .outline()
                        .small()
                        .label("Keep planning")
                        .on_click(resolve(Decision::Deny { feedback: String::new() })),
                )
                .child(Button::new(("allow", request.id.0)).primary().small().label("Start building").on_click(resolve(Decision::Allow)));
        }
        _ => {
            actions = actions
                .child(
                    Button::new(("deny", request.id.0))
                        .outline()
                        .small()
                        .label("Deny")
                        .on_click(resolve(Decision::Deny { feedback: String::new() })),
                )
                .child(
                    Button::new(("session", request.id.0))
                        .small()
                        .label("Allow for this session")
                        .on_click(resolve(Decision::AllowForSession)),
                )
                .child(Button::new(("allow", request.id.0)).primary().small().label("Allow").on_click(resolve(Decision::Allow)));
        }
    }
    v_flex()
        .mt_3()
        .p_3()
        .gap_2()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.warning.opacity(0.55))
        .bg(theme.warning.opacity(0.06))
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(Icon::new(Lucide::Hand).small().text_color(theme.warning))
                .child(title),
        )
        .when_some(body, |el, body| {
            el.child(
                div()
                    .id(("request-body", request.id.0))
                    .p_2()
                    .max_h(rems(16.))
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(body),
            )
        })
        .child(actions)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Fold, FoldKind, Layout, Row, build, output_peek, splice};
    use orbit_proto::*;

    const EPOCH: u64 = 1_704_067_200_000;

    /// Items one minute apart; `at` jumps to a minute (1,440 a day).
    struct Builder {
        items: Vec<Item>,
        minute: u64,
    }

    impl Builder {
        fn new() -> Self {
            Self { items: Vec::new(), minute: 0 }
        }

        fn at(mut self, minute: u64) -> Self {
            self.minute = minute;
            self
        }

        fn push(mut self, turn: u32, body: ItemBody) -> Self {
            self.minute += 1;
            let id = ItemId(((self.minute * 60_000) << 20) + self.items.len() as u64);
            self.items.push(Item { id, turn, status: ItemStatus::Done, body });
            self
        }

        fn user(self, turn: u32, text: &str) -> Self {
            self.push(turn, ItemBody::User(UserMessage { text: text.into(), ..Default::default() }))
        }

        fn steer(self, turn: u32, text: &str) -> Self {
            self.push(turn, ItemBody::User(UserMessage { text: text.into(), steer: true, ..Default::default() }))
        }

        fn reply(self, turn: u32, text: &str) -> Self {
            self.push(turn, ItemBody::Assistant { text: text.into() })
        }

        fn thought(self, turn: u32) -> Self {
            self.push(turn, ItemBody::Reasoning { text: "hmm".into() })
        }

        fn tool(self, turn: u32, kind: ToolKind) -> Self {
            let call = ToolCall {
                call_id: String::new(),
                kind,
                name: "Bash".into(),
                title: "ls".into(),
                input: String::new(),
                output: String::new(),
                output_blob: None,
                exit_code: None,
                files: Vec::new(),
            };
            self.push(turn, ItemBody::Tool(call))
        }

        fn end(self, turn: u32) -> Self {
            self.push(turn, ItemBody::TurnEnd(TurnSummary::default()))
        }

        fn error(self, turn: u32) -> Self {
            self.push(turn, ItemBody::Notice { level: NoticeLevel::Error, text: "overloaded".into() })
        }

        fn ids(&self) -> Vec<ItemId> {
            self.items.iter().map(|i| i.id).collect()
        }

        fn layout(&self, busy: bool, open: &[ItemId]) -> Layout {
            let open: HashSet<ItemId> = open.iter().copied().collect();
            build(&self.items, busy, &open, |ms| ((ms - EPOCH) / 86_400_000) as i32)
        }
    }

    fn kinds(rows: &[Row]) -> Vec<&'static str> {
        rows.iter()
            .map(|r| match r {
                Row::Day(_) => "day",
                Row::User(_) => "user",
                Row::Assistant(_) => "reply",
                Row::Reasoning(_) => "thought",
                Row::Work { nested: false, .. } => "work",
                Row::Work { nested: true, .. } => "work*",
                Row::Task(_) => "task",
                Row::Plan(_) => "plan",
                Row::Request(_) => "request",
                Row::Notice(_) => "notice",
                Row::TurnEnd(_) => "end",
                Row::Fold(Fold { kind: FoldKind::Work, open: false, .. }) => "fold",
                Row::Fold(Fold { kind: FoldKind::Work, open: true, .. }) => "fold-open",
                Row::Fold(Fold { kind: FoldKind::Attempt(_), .. }) => "attempt",
                Row::Working => "working",
            })
            .collect()
    }

    fn turn() -> Builder {
        Builder::new()
            .user(1, "go")
            .thought(1)
            .tool(1, ToolKind::Command)
            .tool(1, ToolKind::Read)
            .reply(1, "looking")
            .tool(1, ToolKind::Edit)
            .reply(1, "Done: fixed it.")
            .end(1)
    }

    #[test]
    fn a_finished_turn_folds_its_work_and_never_its_answer() {
        let b = turn();
        let layout = b.layout(false, &[]);
        assert_eq!(kinds(&layout.rows), vec!["user", "fold", "reply", "end"]);
        assert_eq!(layout.rows[2], Row::Assistant(b.ids()[6]), "the last reply is the answer");
        // Everything before the answer is in the fold, for find to open.
        let key = b.ids()[1];
        assert!(b.ids()[1..6].iter().all(|id| layout.folded.get(id) == Some(&key)));

        let opened = b.layout(false, &[key]);
        assert_eq!(kinds(&opened.rows), vec!["user", "fold-open", "thought", "work*", "reply", "work*", "reply", "end"]);
        assert!(opened.folded.is_empty());
    }

    #[test]
    fn a_live_turn_shows_everything() {
        let b = Builder::new().user(1, "go").thought(1).tool(1, ToolKind::Command).reply(1, "so far");
        assert_eq!(kinds(&b.layout(true, &[]).rows), vec!["user", "thought", "work", "reply", "working"]);
    }

    #[test]
    fn a_single_work_log_folds_itself() {
        let b = Builder::new().user(1, "go").tool(1, ToolKind::Command).tool(1, ToolKind::Read).reply(1, "ok").end(1);
        assert_eq!(kinds(&b.layout(false, &[]).rows), vec!["user", "work", "reply", "end"]);
    }

    #[test]
    fn a_retry_folds_the_failed_attempt_and_shows_the_message_once() {
        let b = Builder::new()
            .user(1, "go")
            .tool(1, ToolKind::Command)
            .end(1)
            .error(1)
            .thought(1)
            .tool(1, ToolKind::Command)
            .reply(1, "worked this time")
            .end(1);
        let layout = b.layout(false, &[]);
        assert_eq!(kinds(&layout.rows), vec!["user", "attempt", "fold", "reply", "end"]);
        let Row::Fold(attempt) = &layout.rows[1] else { panic!() };
        assert_eq!(attempt.kind, FoldKind::Attempt(1));
        assert_eq!(attempt.items, b.ids()[1..4].to_vec(), "the attempt keeps its end and its error");
        let opened = b.layout(false, &[attempt.key]);
        assert_eq!(kinds(&opened.rows)[..5], ["user", "attempt", "work*", "end", "notice"]);
    }

    #[test]
    fn steering_messages_split_the_fold_and_stay_visible() {
        let b = Builder::new()
            .user(1, "go")
            .thought(1)
            .tool(1, ToolKind::Command)
            .steer(1, "use the other file")
            .thought(1)
            .tool(1, ToolKind::Edit)
            .reply(1, "done")
            .end(1);
        assert_eq!(kinds(&b.layout(false, &[]).rows), vec!["user", "fold", "user", "fold", "reply", "end"]);
    }

    #[test]
    fn a_queued_message_follows_the_turn_it_waited_for() {
        // "next" was sent (and got its id) while turn 1 was still running.
        let b = Builder::new().user(1, "first").user(2, "next").reply(1, "answer one").end(1).reply(2, "answer two").end(2);
        let rows = b.layout(false, &[]).rows;
        let ids = b.ids();
        assert_eq!(
            rows,
            vec![
                Row::User(ids[0]),
                Row::Assistant(ids[2]),
                Row::TurnEnd(ids[3]),
                Row::User(ids[1]),
                Row::Assistant(ids[4]),
                Row::TurnEnd(ids[5])
            ]
        );
    }

    #[test]
    fn subagents_get_their_own_row() {
        let b = Builder::new().user(1, "go").tool(1, ToolKind::Command).tool(1, ToolKind::Task).tool(1, ToolKind::Read);
        assert_eq!(kinds(&b.layout(true, &[]).rows), vec!["user", "work", "task", "work", "working"]);
    }

    #[test]
    fn days_are_marked_only_when_the_thread_spans_several() {
        let one_day = Builder::new().user(1, "a").reply(1, "b").end(1).user(2, "c");
        assert!(!kinds(&one_day.layout(false, &[]).rows).contains(&"day"));
        let two_days = Builder::new().user(1, "a").reply(1, "b").end(1).at(1_440 * 3).user(2, "c").reply(2, "d").end(2);
        assert_eq!(kinds(&two_days.layout(false, &[]).rows), vec!["day", "user", "reply", "end", "day", "user", "reply", "end"]);
    }

    #[test]
    fn a_splice_replaces_only_what_changed() {
        let r = |n: u64| Row::User(ItemId(n));
        assert_eq!(splice(&[r(1), r(2)], &[r(1), r(2)]), None);
        assert_eq!(splice(&[r(1), r(2)], &[r(1), r(2), r(3)]), Some((2..2, 1)));
        assert_eq!(splice(&[r(1), r(2), r(3)], &[r(1), r(3)]), Some((1..2, 0)));
        assert_eq!(splice(&[r(1), r(2), r(3)], &[r(1), r(7), r(8), r(3)]), Some((1..2, 2)));
        // Opening a fold above the reader inserts rows without touching the rest.
        assert_eq!(splice(&[r(1), r(5)], &[r(1), r(2), r(3), r(5)]), Some((1..1, 2)));
        assert_eq!(splice(&[r(1), r(1)], &[r(1)]), Some((1..2, 0)));
    }

    #[test]
    fn the_inline_peek_shows_the_end_of_long_output() {
        let call = ToolCall {
            call_id: String::new(),
            kind: ToolKind::Command,
            name: "Bash".into(),
            title: "build".into(),
            input: String::new(),
            output: (0..500).map(|i| format!("line {i}\n")).collect(),
            output_blob: None,
            exit_code: Some(1),
            files: Vec::new(),
        };
        let (peek, cut) = output_peek(&call);
        assert!(cut);
        assert!(peek.starts_with("line 440\n") && peek.ends_with("line 499\n"), "{}", &peek[..20]);
    }
}
