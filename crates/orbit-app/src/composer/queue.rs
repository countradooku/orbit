//! Queued messages, above the composer: edit in place, send now, reorder.
//!
//! * **Edit** in the row itself (click the text, or Alt+Up from an empty
//!   composer for the latest): Enter saves, Esc cancels, an empty save
//!   removes the message. The edit is saved on the host, so it is what runs.
//! * **Send now** steers the running turn with that message (or starts it
//!   when nothing runs); with nothing typed, the composer's alternate send
//!   key does it for the oldest one.
//! * **Reorder** by dragging a row or from its menu (Move to front, up,
//!   down); the host re-issues ids so every client sees the same order.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape as InputEscape, InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::Composer;
use crate::assets::Lucide;

/// A queued message being edited in its row.
pub(super) struct QueueEdit {
    item: ItemId,
    input: Entity<TextareaState>,
    _subscription: Subscription,
}

/// The queue panel of a thread. Mount it above the thread's composer.
#[derive(IntoElement)]
pub struct QueuePanel {
    thread: ThreadId,
    composer: Entity<Composer>,
}

impl QueuePanel {
    pub fn new(thread: ThreadId, composer: &Entity<Composer>) -> Self {
        Self { thread, composer: composer.clone() }
    }
}

/// A queued message: id, text, attachment count.
type Queued = (ItemId, String, usize);

/// What a dragged row carries.
#[derive(Clone)]
struct QueueDrag {
    item: ItemId,
    text: SharedString,
}

struct QueueDragPreview(SharedString);

impl Render for QueueDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .max_w(rems(28.))
            .px_3()
            .py_1p5()
            .gap_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.drag_border)
            .bg(theme.popover)
            .shadow_md()
            .text_sm()
            .child(Icon::new(Lucide::Clock).xsmall().text_color(theme.muted_foreground))
            .child(div().truncate().child(self.0.clone()))
    }
}

impl Composer {
    fn queued(&self, cx: &App) -> Vec<Queued> {
        let Some(thread) = self.thread_id() else { return Vec::new() };
        let Some(timeline) = self.app.read(cx).state().timeline(thread) else { return Vec::new() };
        timeline
            .items()
            .iter()
            .filter(|i| i.status == ItemStatus::Queued)
            .filter_map(|i| match &i.body {
                ItemBody::User(m) => Some((i.id, m.text.clone(), m.attachments.len())),
                _ => None,
            })
            .collect()
    }

    fn run(&self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(command, window, cx));
    }

    /// Sends the oldest queued message now. Returns whether there was one.
    pub(super) fn steer_oldest_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(thread), Some((item, _, _))) = (self.thread_id(), self.queued(cx).into_iter().next()) else {
            return false;
        };
        self.run(Command::SteerQueued { thread, item }, window, cx);
        true
    }

    /// Alt+Up: edit the latest queued message. Returns whether there was one.
    pub(super) fn edit_last_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((item, text, _)) = self.queued(cx).pop() else { return false };
        self.start_queue_edit(item, &text, window, cx);
        true
    }

    fn start_queue_edit(&mut self, item: ItemId, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(1, 8).submit_on_enter(true);
            state.set_value(text.to_owned(), window, cx);
            state
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.save_queue_edit(window, cx);
            }
        });
        input.update(cx, |input, cx| {
            let end = input.value().len();
            input.set_selected_range(end..end, cx);
            input.focus(window, cx);
        });
        self.queue_edit = Some(QueueEdit { item, input, _subscription: subscription });
        cx.notify();
    }

    fn save_queue_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(thread), Some(edit)) = (self.thread_id(), self.queue_edit.take()) else { return };
        let text = edit.input.read(cx).value().trim().to_owned();
        self.run(Command::EditQueued { thread, item: edit.item, text }, window, cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn cancel_queue_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.queue_edit.take().is_some() {
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }
}

impl RenderOnce for QueuePanel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let composer = self.composer.read(cx);
        let queued = composer.queued(cx);
        if queued.is_empty() {
            return div().into_any_element();
        }
        let editing = composer.queue_edit.as_ref().map(|e| (e.item, e.input.clone()));
        let theme = cx.theme().clone();
        let thread = self.thread;
        let count = queued.len();
        let mut list = v_flex().gap_1().mb_2();
        list = list.child(
            h_flex()
                .px_1()
                .gap_1p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::Clock).xsmall())
                .child(match count {
                    1 => "1 message waits for this turn to end".to_owned(),
                    n => format!("{n} messages wait for this turn to end, in this order"),
                })
                .child(div().flex_1())
                .child("Alt+↑ edits the last"),
        );
        for (ix, (item, text, attachments)) in queued.into_iter().enumerate() {
            let composer = self.composer.clone();
            if let Some((_, input)) = editing.as_ref().filter(|(id, _)| *id == item) {
                let (save, cancel, esc) = (composer.clone(), composer.clone(), composer.clone());
                list = list.child(
                    v_flex()
                        .id(("queue-edit", item.0))
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.ring)
                        .bg(theme.popover)
                        .capture_action(move |_: &InputEscape, window, cx| {
                            esc.update(cx, |c, cx| c.cancel_queue_edit(window, cx));
                            cx.stop_propagation();
                        })
                        .child(Textarea::new(input).appearance(false))
                        .child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Enter saves · Esc cancels · saving it empty removes it")
                                .child(div().flex_1())
                                .child(Button::new(("queue-cancel", item.0)).ghost().xsmall().label("Cancel").on_click(
                                    move |_, window, cx| cancel.update(cx, |c, cx| c.cancel_queue_edit(window, cx)),
                                ))
                                .child(Button::new(("queue-save", item.0)).primary().xsmall().label("Save").on_click(
                                    move |_, window, cx| save.update(cx, |c, cx| c.save_queue_edit(window, cx)),
                                )),
                        ),
                );
                continue;
            }
            let (edit, steer, menu) = (composer.clone(), composer.clone(), composer.clone());
            let edit_text = text.clone();
            let menu_text = text.clone();
            let drag = QueueDrag { item, text: text.clone().into() };
            let drop_target = composer.clone();
            list = list.child(
                h_flex()
                    .id(("queued", item.0))
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .py_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .border_dashed()
                    .bg(theme.muted)
                    .text_sm()
                    .cursor_grab()
                    .on_drag(drag, |drag: &QueueDrag, _, _, cx| cx.new(|_| QueueDragPreview(drag.text.clone())))
                    .drag_over::<QueueDrag>(|style, _, _, cx| style.border_color(cx.theme().drag_border))
                    .on_drop(move |drag: &QueueDrag, window, cx| {
                        if drag.item != item {
                            drop_target.update(cx, |c, cx| {
                                c.run(Command::MoveQueued { thread, item: drag.item, to: ix as u32 }, window, cx)
                            });
                        }
                    })
                    .child(
                        div()
                            .id(("queued-text", item.0))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .cursor_text()
                            .on_click(move |_, window, cx| {
                                edit.update(cx, |c, cx| c.start_queue_edit(item, &edit_text, window, cx))
                            })
                            .child(text),
                    )
                    .when(attachments > 0, |el| {
                        el.child(
                            h_flex()
                                .gap_0p5()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(Icon::new(Lucide::Paperclip).xsmall())
                                .child(attachments.to_string()),
                        )
                    })
                    .child(
                        Button::new(("queued-steer", item.0))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Lucide::Zap))
                            .label("Send now")
                            .tooltip("Send it into the running turn now")
                            .on_click(move |_, window, cx| {
                                steer.update(cx, |c, cx| c.run(Command::SteerQueued { thread, item }, window, cx))
                            }),
                    )
                    .child(Button::new(("queued-menu", item.0)).ghost().xsmall().icon(IconName::Ellipsis).dropdown_menu(
                        move |menu_builder, _, _| {
                            let move_to = |to: usize| {
                                let composer = menu.clone();
                                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                    composer.update(cx, |c, cx| {
                                        c.run(Command::MoveQueued { thread, item, to: to as u32 }, window, cx)
                                    })
                                }
                            };
                            let (edit, remove) = (menu.clone(), menu.clone());
                            let text = menu_text.clone();
                            menu_builder
                                .item(PopupMenuItem::new("Edit").icon(Lucide::Pencil).on_click(move |_, window, cx| {
                                    let text = text.clone();
                                    edit.update(cx, |c, cx| c.start_queue_edit(item, &text, window, cx))
                                }))
                                .separator()
                                .item(PopupMenuItem::new("Move to front").disabled(ix == 0).on_click(move_to(0)))
                                .item(PopupMenuItem::new("Move up").disabled(ix == 0).on_click(move_to(ix.saturating_sub(1))))
                                .item(PopupMenuItem::new("Move down").disabled(ix + 1 >= count).on_click(move_to(ix + 1)))
                                .separator()
                                .item(PopupMenuItem::new("Remove").icon(Lucide::Trash).on_click(move |_, window, cx| {
                                    remove.update(cx, |c, cx| {
                                        c.run(Command::EditQueued { thread, item, text: String::new() }, window, cx)
                                    })
                                }))
                        },
                    )),
            );
        }
        list.into_any_element()
    }
}
