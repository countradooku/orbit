//! Timeline rows: how each kind of item reads.
//!
//! * Your messages sit in a quiet bubble at the trailing edge.
//! * Replies are full-width Markdown in a readable column, without a bubble.
//! * Reasoning is one line ("Thinking…" shimmering while it streams, "Thought
//!   for 12 s" after) that expands to the text.
//! * Consecutive tool calls fold into one work log: a header with the step
//!   count and the steps beneath, the running step animated. Finished turns
//!   collapse their work log so the answer is what you read first.
//! * A request is a card with the exact command or change and buttons that
//!   name the result: Deny, Allow for this session, Allow.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::ThreadView;
use crate::assets::Lucide;
use crate::format;

/// Width of the reading column.
pub const COLUMN: Rems = Rems(52.);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    User(ItemId),
    Assistant(ItemId),
    Reasoning(ItemId),
    /// Consecutive tool calls of one turn.
    Work { turn: u32, items: Vec<ItemId> },
    Plan(ItemId),
    Request(ItemId),
    Notice(ItemId),
    TurnEnd(ItemId),
}

impl Row {
    pub fn build(items: &[Item]) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::with_capacity(items.len());
        for item in items {
            if item.status == ItemStatus::Queued {
                continue; // shown above the composer instead
            }
            let row = match &item.body {
                ItemBody::User(_) => Row::User(item.id),
                ItemBody::Assistant { text } if text.is_empty() && item.status != ItemStatus::Streaming => continue,
                ItemBody::Assistant { .. } => Row::Assistant(item.id),
                ItemBody::Reasoning { text } if text.trim().is_empty() && item.status != ItemStatus::Streaming => continue,
                ItemBody::Reasoning { .. } => Row::Reasoning(item.id),
                // Questions and plan approvals are shown as request cards.
                ItemBody::Tool(call) if matches!(call.name.as_str(), "AskUserQuestion" | "ExitPlanMode") => continue,
                ItemBody::Tool(_) => {
                    if let Some(Row::Work { turn, items }) = rows.last_mut()
                        && *turn == item.turn
                    {
                        items.push(item.id);
                        continue;
                    }
                    Row::Work { turn: item.turn, items: vec![item.id] }
                }
                ItemBody::Plan { .. } => Row::Plan(item.id),
                ItemBody::Request(_) => Row::Request(item.id),
                ItemBody::Notice { .. } => Row::Notice(item.id),
                ItemBody::TurnEnd(_) => Row::TurnEnd(item.id),
            };
            rows.push(row);
        }
        rows
    }

    /// The stable key of the row (its first item).
    pub fn key(&self) -> ItemId {
        match self {
            Row::Work { items, .. } => items[0],
            Row::User(id)
            | Row::Assistant(id)
            | Row::Reasoning(id)
            | Row::Plan(id)
            | Row::Request(id)
            | Row::Notice(id)
            | Row::TurnEnd(id) => *id,
        }
    }

    pub fn items(&self) -> Vec<ItemId> {
        match self {
            Row::Work { items, .. } => items.clone(),
            other => vec![other.key()],
        }
    }
}

/// A row centered in the reading column.
fn column(content: impl IntoElement) -> Div {
    h_flex().w_full().justify_center().px_4().child(div().w_full().max_w(COLUMN).child(content))
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

    let content: AnyElement = match row {
        Row::User(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let ItemBody::User(message) = &item.body else { return div().into_any_element() };
            let text = message.text.clone();
            let edit_text = text.clone();
            let app = this.app.clone();
            let (thread_id, item_id) = (this.id, *id);
            v_flex()
                .items_end()
                .gap_1()
                .pt_5()
                .group("user-msg")
                .child(
                    div()
                        .max_w(relative(0.85))
                        .px_4()
                        .py_2p5()
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .text_sm()
                        .when(message.steer, |el| el.border_1().border_color(theme.ring))
                        .child(text),
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
                    })))
                })
                .child(
                    h_flex()
                        .gap_0p5()
                        .opacity(0.)
                        .group_hover("user-msg", |s| s.opacity(1.))
                        .when(message.steer, |el| {
                            el.child(div().text_xs().text_color(theme.muted_foreground).mr_1().child("Steered"))
                        })
                        .child(gpui_kit::component::clipboard::Clipboard::new(("copy-user", id.0)).value(edit_text.clone()))
                        .when(!busy, |el| {
                            el.child(
                                Button::new(("edit-user", id.0))
                                    .ghost()
                                    .xsmall()
                                    .icon(Lucide::Pencil)
                                    .tooltip("Edit and resend from here")
                                    .on_click(move |_, window, cx| {
                                        edit_and_resend(&app, thread_id, item_id, edit_text.clone(), window, cx)
                                    }),
                            )
                        }),
                )
                .into_any_element()
        }
        Row::Assistant(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let streaming = item.status == ItemStatus::Streaming;
            let Some(md) = this.markdown.get(id).cloned() else { return div().into_any_element() };
            let text = match &item.body {
                ItemBody::Assistant { text } => text.clone(),
                _ => String::new(),
            };
            v_flex()
                .pt_3()
                .gap_1()
                .group("agent-msg")
                .text_sm()
                .child(TextView::new(&md).selectable(true).stream_fade(streaming))
                .when(!streaming, |el| {
                    el.child(
                        h_flex()
                            .h_6()
                            .opacity(0.)
                            .group_hover("agent-msg", |s| s.opacity(1.))
                            .child(gpui_kit::component::clipboard::Clipboard::new(("copy-agent", id.0)).value(text)),
                    )
                })
                .into_any_element()
        }
        Row::Reasoning(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
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
                    div().child("Reasoning").into_any_element()
                })
                .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall());
            v_flex()
                .pt_2()
                .child(header)
                .when(open, |el| {
                    if let Some(md) = this.markdown.get(id).cloned() {
                        el.child(
                            div()
                                .pl_4()
                                .ml_1p5()
                                .border_l_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(TextView::new(&md).selectable(true)),
                        )
                    } else {
                        el
                    }
                })
                .into_any_element()
        }
        Row::Work { items, .. } => {
            let calls: Vec<&Item> = items.iter().filter_map(|id| item(*id)).collect();
            let live = calls.iter().any(|i| i.status == ItemStatus::Streaming);
            let key = items[0];
            // A live work log stays open so you can follow it; a finished one
            // folds away unless you open it.
            let open = live || this.expanded.contains(&key);
            let failed = calls.iter().filter(|i| i.status == ItemStatus::Failed).count();
            let summary = if live {
                format!("Working · {} step{}", calls.len(), if calls.len() == 1 { "" } else { "s" })
            } else {
                format!("Worked · {} step{}", calls.len(), if calls.len() == 1 { "" } else { "s" })
            };
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
                .when(!live, |el| {
                    el.child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                });
            let mut list = v_flex().gap_0p5().pl_1();
            if open {
                for call in &calls {
                    list = list.child(render_tool(&weak, call, this.expanded.contains(&call.id), &theme));
                }
            }
            v_flex().pt_2().child(header).when(open, |el| el.child(list)).into_any_element()
        }
        Row::Plan(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let ItemBody::Plan { steps } = &item.body else { return div().into_any_element() };
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
                .children(steps.iter().map(|step| {
                    let (icon, color) = match step.state {
                        StepState::Done => (Icon::new(IconName::CircleCheck), theme.success),
                        StepState::Active => (Icon::new(Lucide::CircleDot), theme.primary),
                        StepState::Pending => (Icon::new(Lucide::CircleDot), theme.muted_foreground),
                    };
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .when(step.state == StepState::Done, |el| el.text_color(theme.muted_foreground).line_through())
                        .child(icon.xsmall().text_color(color))
                        .child(step.text.clone())
                }))
                .into_any_element()
        }
        Row::Request(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let ItemBody::Request(request) = &item.body else { return div().into_any_element() };
            render_request(&this.app, this.id, request, &theme).into_any_element()
        }
        Row::Notice(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let ItemBody::Notice { level, text } = &item.body else { return div().into_any_element() };
            let (icon, color) = match level {
                NoticeLevel::Info => (Icon::new(IconName::Info), theme.muted_foreground),
                NoticeLevel::Warning => (Icon::new(IconName::TriangleAlert), theme.warning),
                NoticeLevel::Error => (Icon::new(IconName::CircleAlert), theme.danger),
            };
            h_flex()
                .mt_2()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(theme.radius)
                .when(*level == NoticeLevel::Error, |el| el.bg(theme.danger.opacity(0.08)))
                .text_sm()
                .text_color(color)
                .child(icon.small())
                .child(div().flex_1().child(text.clone()))
                .into_any_element()
        }
        Row::TurnEnd(id) => {
            let Some(item) = item(*id) else { return div().into_any_element() };
            let ItemBody::TurnEnd(summary) = &item.body else { return div().into_any_element() };
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
            let review = weak.clone();
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
                                    .on_click(move |_, _, cx| {
                                        let _ = review.update(cx, |_, cx| cx.emit(super::ThreadViewEvent::Review));
                                    }),
                            )
                            .when(restorable, |el| {
                                el.child(
                                    Button::new(("turn-undo", id.0))
                                        .ghost()
                                        .xsmall()
                                        .icon(Lucide::RotateCcw)
                                        .label("Undo turn")
                                        .tooltip("Put the files back the way they were before this turn. You can undo this too.")
                                        .on_click(move |_, window, cx| {
                                            app.update(cx, |app, cx| {
                                                app.run(Command::RestoreCheckpoint { thread: thread_id, turn }, window, cx)
                                            })
                                        }),
                                )
                            }),
                    )
                })
                .into_any_element()
        }
    };
    column(content).into_any_element()
}

fn tool_icon(kind: ToolKind) -> Icon {
    Icon::new(match kind {
        ToolKind::Command => Lucide::Terminal,
        ToolKind::Read => Lucide::FileSearch,
        ToolKind::Edit => Lucide::FilePen,
        ToolKind::Search => Lucide::SearchCode,
        ToolKind::Web => Lucide::Globe,
        ToolKind::Task => Lucide::Layers,
        ToolKind::Mcp => Lucide::Plug,
        ToolKind::Todo => Lucide::ListTodo,
        ToolKind::Other => Lucide::Wrench,
    })
}

fn render_tool(
    weak: &WeakEntity<ThreadView>,
    item: &Item,
    open: bool,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let ItemBody::Tool(call) = &item.body else { return div() };
    let running = item.status == ItemStatus::Streaming;
    let failed = item.status == ItemStatus::Failed;
    let key = item.id;
    let has_output = !call.output.is_empty();
    let changes: (u32, u32) = call.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let status: AnyElement = if running {
        Icon::new(IconName::LoaderCircle)
            .xsmall()
            .text_color(theme.muted_foreground)
            .with_animation(("tool-spin", key.0), Animation::new(Duration::from_millis(900)).repeat(), |icon, t| {
                icon.transform(Transformation::rotate(percentage(t)))
            })
            .into_any_element()
    } else if failed {
        Icon::new(IconName::CircleX).xsmall().text_color(theme.danger).into_any_element()
    } else {
        div().size_3().into_any_element()
    };
    let title = if call.title.is_empty() { call.name.clone() } else { call.title.clone() };
    let mono = matches!(call.kind, ToolKind::Command | ToolKind::Read | ToolKind::Edit | ToolKind::Search);
    let weak = weak.clone();
    v_flex()
        .child(
            h_flex()
                .id(("tool", key.0))
                .gap_2()
                .py_0p5()
                .px_1()
                .rounded(theme.radius)
                .text_xs()
                .cursor_default()
                .when(has_output, |el| {
                    el.hover(|s| s.bg(theme.muted)).on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_expanded(key, cx));
                    })
                })
                .child(tool_icon(call.kind).xsmall().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                        .text_color(if failed { theme.danger } else { theme.foreground })
                        .child(title),
                )
                .when(changes != (0, 0), |el| {
                    el.child(div().text_color(theme.success).child(format!("+{}", changes.0)))
                        .child(div().text_color(theme.danger).child(format!("−{}", changes.1)))
                })
                .when_some(call.exit_code.filter(|c| *c != 0), |el, code| {
                    el.child(div().text_color(theme.danger).child(format!("exit {code}")))
                })
                .child(status),
        )
        .when(open && has_output, |el| {
            el.child(
                div()
                    .id(("tool-output", key.0))
                    .ml_6()
                    .mt_0p5()
                    .mb_1()
                    .p_2()
                    .max_h(rems(18.))
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(call.output.clone()),
            )
        })
}

fn render_request(
    app: &Entity<crate::app_state::AppState>,
    thread: ThreadId,
    request: &AgentRequest,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let (title, body): (String, Option<String>) = match &request.kind {
        RequestKind::Command { command, cwd, reason } => (
            if reason.is_empty() { "Run this command?".into() } else { reason.clone() },
            Some(if cwd.is_empty() { command.clone() } else { format!("{command}\n  in {cwd}") }),
        ),
        RequestKind::FileChange { path, diff } => (
            format!("Change {path}?"),
            (!diff.is_empty()).then(|| diff.lines().take(24).collect::<Vec<_>>().join("\n")),
        ),
        RequestKind::Tool { name, input } => (format!("Use {name}?"), Some(input.clone())),
        RequestKind::Question { question, .. } => (question.clone(), None),
        RequestKind::Plan { plan } => ("Carry out this plan?".into(), Some(plan.clone())),
    };
    if let Some(decision) = &request.resolution {
        let (icon, text, color) = match decision {
            Decision::Allow => (IconName::Check, "Allowed", theme.muted_foreground),
            Decision::AllowForSession => (IconName::Check, "Allowed for this session", theme.muted_foreground),
            Decision::Deny { .. } => (IconName::Close, "Denied", theme.muted_foreground),
            Decision::Answer { .. } => (IconName::Check, "Answered", theme.muted_foreground),
        };
        return column(
            h_flex()
                .mt_1()
                .gap_1p5()
                .text_xs()
                .text_color(color)
                .child(Icon::new(icon).xsmall())
                .child(text)
                .child("·")
                .child(div().truncate().child(title)),
        );
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
        RequestKind::Question { options, .. } if !options.is_empty() => {
            for (ix, option) in options.iter().enumerate() {
                actions = actions.child(
                    Button::new(("answer", request.id.0 ^ ix as u64))
                        .small()
                        .when(ix == 0, |b| b.primary())
                        .label(option.clone())
                        .on_click(resolve(Decision::Answer { text: option.clone() })),
                );
            }
        }
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
    column(
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
            .child(actions),
    )
}

fn edit_and_resend(
    app: &Entity<crate::app_state::AppState>,
    thread: ThreadId,
    item: ItemId,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::input::{Textarea, TextareaState};
    let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 16).default_value(text));
    let app = app.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let (input, app) = (input.clone(), app.clone());
        dialog
            .title("Edit message")
            .w(px(600.))
            .child(
                v_flex()
                    .gap_2()
                    .child(Textarea::new(&input))
                    .child(div().text_xs().child("Later messages in this thread are replaced by the new reply.")),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(gpui_kit::component::dialog::DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(
                        gpui_kit::component::dialog::DialogAction::new()
                            .child(Button::new("resend").primary().label("Resend")),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let text = input.read(cx).value().trim().to_owned();
                if text.is_empty() {
                    return false;
                }
                app.update(cx, |app, cx| app.run(Command::EditAndResend { thread, item, text }, window, cx));
                true
            })
    });
}
