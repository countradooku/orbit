//! Tiles: up to six threads side by side, all live.
//!
//! Every tile is a full [`ThreadView`]: the focused one takes the keyboard,
//! the others keep streaming. The grid shape follows the count and the
//! window: two tiles sit side by side, three sit in a row on a wide window,
//! four make a square, five and six make two rows of three (three rows of
//! two on a narrow window).
//!
//! The layout is owned by the workspace ([`crate::shell::Workspace`]); this
//! module holds the geometry and the tile chrome.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::{CloseTile, MaximizeTile};
use crate::assets::Lucide;
use crate::thread::{ThreadView, display_title};
use crate::ui::StatusGlyph;

/// At most this many tiles: past six a tile is too small to read.
pub const MAX_TILES: usize = 6;

/// Below this window width, five or six tiles stack in two columns.
const WIDE: f32 = 1500.;

/// How many tiles each row holds, top to bottom.
pub fn rows(count: usize, wide: bool) -> Vec<usize> {
    match count {
        0 => vec![],
        1 => vec![1],
        2 => vec![2],
        3 if wide => vec![3],
        3 => vec![2, 1],
        4 => vec![2, 2],
        5 if wide => vec![3, 2],
        5 => vec![2, 2, 1],
        _ if wide => vec![3, 3],
        _ => vec![2, 2, 2],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The tile next to `ix` in `dir`, in a grid with the given rows. Moving up
/// or down keeps the column where it can and clamps to a shorter row.
pub fn neighbor(rows: &[usize], ix: usize, dir: Direction) -> Option<usize> {
    let mut start = 0;
    let mut place = None;
    for (r, &len) in rows.iter().enumerate() {
        if ix < start + len {
            place = Some((r, ix - start));
            break;
        }
        start += len;
    }
    let (row, col) = place?;
    let row_start = |r: usize| rows[..r].iter().sum::<usize>();
    match dir {
        Direction::Left => (col > 0).then(|| ix - 1),
        Direction::Right => (col + 1 < rows[row]).then(|| ix + 1),
        Direction::Up => (row > 0).then(|| row_start(row - 1) + col.min(rows[row - 1] - 1)),
        Direction::Down => (row + 1 < rows.len()).then(|| row_start(row + 1) + col.min(rows[row + 1] - 1)),
    }
}

/// Whether the window is wide enough for rows of three.
pub fn is_wide(window: &Window) -> bool {
    window.viewport_size().width > px(WIDE)
}

/// One tile: a slim bar (number, status, title, maximize, close) over the
/// thread view. The focused tile's frame carries the focus ring.
pub fn tile(
    number: usize,
    thread: Option<&Thread>,
    view: &Entity<ThreadView>,
    focused: bool,
    maximized: bool,
    on_focus: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let id = view.read(cx).id();
    let title = thread.map(display_title).unwrap_or("Thread").to_owned();
    let bar = h_flex()
        .id(("tile-bar", id.0))
        .h_7()
        .flex_none()
        .pl_2()
        .pr_1()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(if focused { theme.list_active } else { theme.group_box })
        .text_xs()
        .cursor_default()
        .on_click(move |_, window, cx| on_focus(window, cx))
        .child(
            div()
                .flex_none()
                .text_color(if focused { theme.foreground } else { theme.muted_foreground })
                .font_weight(FontWeight::SEMIBOLD)
                .child(number.to_string()),
        )
        .when_some(thread, |el, thread| el.child(StatusGlyph::new(("tile-glyph", id.0), thread, view.read(cx).app().read(cx).state())))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if focused { theme.foreground } else { theme.muted_foreground })
                .child(title),
        )
        .child(
            Button::new(("tile-max", id.0))
                .ghost()
                .xsmall()
                .icon(Icon::new(if maximized { Lucide::Minimize2 } else { Lucide::Maximize2 }))
                .tooltip_with_action(if maximized { "Show all tiles" } else { "Maximize" }, &MaximizeTile, None)
                .on_click(move |_, window, cx| window.dispatch_action(Box::new(MaximizeTile), cx)),
        )
        .child(
            Button::new(("tile-close", id.0))
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit::component::IconName::Close))
                .tooltip_with_action("Close tile", &CloseTile, None)
                .on_click(move |_, window, cx| window.dispatch_action(Box::new(CloseTile), cx)),
        );
    v_flex()
        .id(("tile", id.0))
        .flex_1()
        .min_w_0()
        .min_h_0()
        .rounded(theme.radius_lg)
        .overflow_hidden()
        .border_1()
        .border_color(if focused { theme.ring } else { theme.border })
        .bg(theme.background)
        .child(bar)
        .child(div().flex_1().min_h_0().child(view.clone()))
}

#[cfg(test)]
mod tests {
    use super::{Direction, neighbor, rows};

    #[test]
    fn grids_follow_the_count_and_the_window() {
        assert_eq!(rows(2, false), vec![2]);
        assert_eq!(rows(3, true), vec![3]);
        assert_eq!(rows(3, false), vec![2, 1]);
        assert_eq!(rows(5, true), vec![3, 2]);
        assert_eq!(rows(6, false), vec![2, 2, 2]);
        assert_eq!(rows(9, true).iter().sum::<usize>(), 6, "never more than six");
    }

    #[test]
    fn focus_moves_like_the_eye_expects() {
        let grid = rows(5, true); // [0 1 2] [3 4]
        assert_eq!(neighbor(&grid, 0, Direction::Right), Some(1));
        assert_eq!(neighbor(&grid, 0, Direction::Left), None);
        assert_eq!(neighbor(&grid, 2, Direction::Down), Some(4), "clamps to the shorter row");
        assert_eq!(neighbor(&grid, 4, Direction::Up), Some(1));
        assert_eq!(neighbor(&grid, 3, Direction::Down), None);
        assert_eq!(neighbor(&grid, 7, Direction::Up), None);
    }
}
