//! The turn navigator: a slim rail at the timeline's edge with a mark for
//! each of your messages. The mark of the turn you are reading is drawn
//! stronger; hovering a mark shows the message and its time, clicking jumps
//! there. Alt+Up and Alt+Down step between your messages from the keyboard.
//!
//! It is its own view: scrolling re-renders the rail, never the thread.

use std::rc::Rc;

use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::format;

/// One of your messages on the rail.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub row: usize,
    pub item: ItemId,
    pub preview: SharedString,
}

/// The mark of the turn being read: the last one at or above the top row.
pub fn current(marks: &[Mark], top_row: usize) -> Option<usize> {
    marks.iter().rposition(|m| m.row <= top_row).or((!marks.is_empty()).then_some(0))
}

/// The mark to jump to from `top_row`: the next one below, or the previous
/// one above (the current turn's own message first, when it is scrolled
/// past).
pub fn neighbor(marks: &[Mark], top_row: usize, forward: bool) -> Option<usize> {
    if forward {
        marks.iter().position(|m| m.row > top_row)
    } else {
        marks.iter().rposition(|m| m.row < top_row)
    }
}

/// A message's first line, short enough for a tooltip.
pub fn preview(text: &str) -> SharedString {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut preview: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        preview.push('…');
    }
    preview.into()
}

pub struct Minimap {
    scroller: Entity<MessageScrollerState>,
    marks: Rc<Vec<Mark>>,
    _subscription: Subscription,
}

impl Minimap {
    pub fn new(scroller: Entity<MessageScrollerState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&scroller, |_, _, cx| cx.notify());
        Self { scroller, marks: Rc::new(Vec::new()), _subscription: subscription }
    }

    pub fn set_marks(&mut self, marks: Vec<Mark>, cx: &mut Context<Self>) {
        if *self.marks != marks {
            self.marks = Rc::new(marks);
            cx.notify();
        }
    }

    fn top_row(&self, cx: &App) -> usize {
        self.scroller.read(cx).logical_scroll_top().item_ix
    }

    /// Scrolls to the previous or next of your messages. Returns false when
    /// there is none that way.
    pub fn step(&self, forward: bool, cx: &mut App) -> bool {
        let Some(ix) = neighbor(&self.marks, self.top_row(cx), forward) else { return false };
        let row = self.marks[ix].row;
        self.scroller.update(cx, |s, cx| s.scroll_to_item(row, cx))
    }
}

impl Render for Minimap {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.marks.len() < 2 {
            return div().id("minimap");
        }
        let theme = cx.theme().clone();
        let current = current(&self.marks, self.top_row(cx));
        let marks = self.marks.clone();
        v_flex()
            .id("minimap")
            .absolute()
            .top(px(16.))
            .bottom(px(16.))
            .right(px(14.))
            .w(px(18.))
            .justify_center()
            .children(marks.iter().enumerate().map(|(ix, mark)| {
                let is_current = Some(ix) == current;
                let row = mark.row;
                let tooltip: SharedString = format!("{}\n{}", mark.preview, format::timestamp(mark.item.created_ms(), now_ms())).into();
                let scroller = self.scroller.clone();
                div()
                    .id(("mark", mark.item.0))
                    .group("mark")
                    .flex_1()
                    .min_h(px(3.))
                    .max_h(px(14.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        scroller.update(cx, |s, cx| {
                            s.scroll_to_item(row, cx);
                        });
                    })
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(
                        div()
                            .h(px(2.))
                            .rounded_full()
                            .w(px(if is_current { 14. } else { 8. }))
                            .bg(if is_current { theme.foreground } else { theme.muted_foreground.opacity(0.4) })
                            .group_hover("mark", |s| s.w(px(14.)).bg(theme.foreground)),
                    )
                    .when(is_current, |el| el)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Mark, current, neighbor, preview};
    use orbit_proto::*;

    fn marks(rows: &[usize]) -> Vec<Mark> {
        rows.iter().map(|&row| Mark { row, item: ItemId(row as u64), preview: "".into() }).collect()
    }

    #[test]
    fn the_current_mark_is_the_turn_being_read() {
        let m = marks(&[0, 5, 9]);
        assert_eq!(current(&m, 0), Some(0));
        assert_eq!(current(&m, 7), Some(1));
        assert_eq!(current(&m, 20), Some(2));
    }

    #[test]
    fn stepping_moves_between_your_messages() {
        let m = marks(&[0, 5, 9]);
        assert_eq!(neighbor(&m, 5, true), Some(2));
        assert_eq!(neighbor(&m, 5, false), Some(0));
        assert_eq!(neighbor(&m, 7, false), Some(1), "back to this turn's message first");
        assert_eq!(neighbor(&m, 9, true), None);
        assert_eq!(preview("\n  Fix the build please  \nmore"), "Fix the build please");
    }
}
