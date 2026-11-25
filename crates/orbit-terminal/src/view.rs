//! The terminal view entity: emulator state, input and selection.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Processor};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use crate::colors::Palette;
use crate::element::{Geometry, TerminalElement};
use crate::keys::{encode_keystroke, encode_paste};
use crate::mouse::{MouseAction, report};
use crate::{
    Clear, Copy, DEFAULT_SCROLLBACK, KEY_CONTEXT, Paste, ScrollLineDown, ScrollLineUp, ScrollPageDown, ScrollPageUp,
    ScrollToBottom, ScrollToTop, SelectAll, links,
};

/// What a [`TerminalView`] asks of its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes for the program: keystrokes, pastes, and replies to its queries.
    Input(Vec<u8>),
    /// The view's grid changed size; resize the PTY to match.
    Resize { cols: u16, rows: u16 },
    /// The program set (`Some`) or reset (`None`) its title.
    TitleChanged(Option<String>),
    /// The program rang the bell.
    Bell,
}

/// Collects the emulator's requests while bytes are parsed; drained after.
#[derive(Clone, Default)]
pub(crate) struct Listener(Rc<RefCell<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            // Redraws are driven by `feed`, not by the emulator.
            TermEvent::Wakeup | TermEvent::MouseCursorDirty => {}
            event => self.0.borrow_mut().push(event),
        }
    }
}

/// The grid size handed to the emulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A link under the pointer while the link modifier is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoveredLink {
    pub line: Line,
    pub cols: Range<usize>,
    pub uri: String,
}

/// A terminal emulator view. It owns the screen, scrollback, selection and
/// cursor of one terminal; the bytes come from, and go to, its owner.
///
/// * Focus: the view tracks its own [`FocusHandle`] with the
///   [`KEY_CONTEXT`] key context. While focused, keys go to the program, the
///   cursor is solid (and blinks when the program and the theme ask for it,
///   unless motion is reduced); unfocused, it is hollow.
/// * Redraws: [`TerminalView::feed`] parses a whole batch and notifies once.
pub struct TerminalView {
    term: Term<Listener>,
    listener: Listener,
    processor: Processor,
    focus_handle: FocusHandle,
    size: GridSize,
    /// Measured by the element; used for IME placement and size reports.
    geometry: Option<Geometry>,
    alt_is_meta: bool,
    /// The left button is selecting text.
    selecting: bool,
    /// The button whose press was reported to the program.
    reported_button: Option<MouseButton>,
    last_reported_cell: Option<(usize, usize)>,
    /// Wheel movement not yet worth a whole line.
    scroll_remainder: Pixels,
    hovered_link: Option<HoveredLink>,
    ime_marked: Option<String>,
    title: Option<String>,
    focused: bool,
    cursor_visible: bool,
    blink_epoch: usize,
    last_activity: Instant,
    _blink_task: Task<()>,
    _sync_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    /// A terminal of 80×24 cells until it is first laid out, with
    /// [`DEFAULT_SCROLLBACK`] lines of scrollback.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let size = GridSize { cols: 80, rows: 24 };
        let listener = Listener::default();
        let config = Self::config(DEFAULT_SCROLLBACK, cx);
        let term = Term::new(config, &size, listener.clone());
        let subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |this, _, cx| this.set_focused(true, cx)),
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| this.set_focused(false, cx)),
            cx.observe_global::<gpui_kit::component::Theme>(|this, cx| this.restart_blink(cx)),
        ];
        Self {
            term,
            listener,
            processor: Processor::new(),
            focus_handle,
            size,
            geometry: None,
            alt_is_meta: !cfg!(target_os = "macos"),
            selecting: false,
            reported_button: None,
            last_reported_cell: None,
            scroll_remainder: Pixels::ZERO,
            hovered_link: None,
            ime_marked: None,
            title: None,
            focused: false,
            cursor_visible: true,
            blink_epoch: 0,
            last_activity: Instant::now(),
            _blink_task: Task::ready(()),
            _sync_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn config(scrollback: usize, cx: &App) -> Config {
        Config {
            scrolling_history: scrollback,
            default_cursor_style: CursorStyle {
                shape: CursorShape::Block,
                blinking: !cx.theme().cursor_blink.is_steady(),
            },
            ..Config::default()
        }
    }

    /// Keeps at most `lines` of scrollback (default [`DEFAULT_SCROLLBACK`]).
    pub fn scrollback(mut self, lines: usize) -> Self {
        let config =
            Config { scrolling_history: lines, default_cursor_style: self.term.cursor_style(), ..Config::default() };
        self.term.set_options(config);
        self.listener.0.borrow_mut().clear();
        self
    }

    /// Whether Alt+key sends ESC+key (default on, except on macOS where
    /// Option types characters).
    pub fn alt_is_meta(mut self, alt_is_meta: bool) -> Self {
        self.alt_is_meta = alt_is_meta;
        self
    }

    /// Parses program output and redraws once for the whole batch.
    pub fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.processor.advance(&mut self.term, bytes);
        self.schedule_sync_timeout(cx);
        self.drain_term_events(cx);
        cx.notify();
    }

    /// The grid size in cells (columns, rows).
    pub fn grid_size(&self) -> (u16, u16) {
        (self.size.cols as u16, self.size.rows as u16)
    }

    /// The title the program set, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// The selected text, if anything is selected.
    pub fn selected_text(&self) -> Option<String> {
        self.has_selection().then(|| self.term.selection_to_string()).flatten()
    }

    /// Lines scrolled back from the live screen (0 when following output).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// The text on screen, one line per row, without trailing blanks.
    pub fn visible_text(&self) -> String {
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        let mut out = String::new();
        for row in 0..self.size.rows {
            let line = Line(row as i32 - offset);
            let mut text = String::new();
            for col in 0..self.size.cols {
                let cell = &grid[GridPoint::new(line, Column(col))];
                if !cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                    text.push(if cell.c == '\0' { ' ' } else { cell.c });
                }
            }
            out.push_str(text.trim_end());
            out.push('\n');
        }
        out.truncate(out.trim_end_matches('\n').len());
        out
    }

    /// Sends `text` to the program as a paste.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = encode_paste(text, *self.term.mode());
        self.send_user_input(bytes, cx);
    }

    pub(crate) fn term(&self) -> &Term<Listener> {
        &self.term
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn is_cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub(crate) fn hovered_link(&self) -> Option<&HoveredLink> {
        self.hovered_link.as_ref()
    }

    pub(crate) fn ime_marked(&self) -> Option<&str> {
        self.ime_marked.as_deref()
    }

    /// Applies the grid the element measured. Runs in prepaint, and only
    /// changes state (and emits [`TerminalEvent::Resize`]) when the size did.
    pub(crate) fn set_geometry(&mut self, geometry: Geometry, cx: &mut Context<Self>) {
        if self.geometry != Some(geometry) {
            self.geometry = Some(geometry);
        }
        let size = GridSize { cols: geometry.cols, rows: geometry.rows };
        if size != self.size {
            self.size = size;
            self.term.resize(size);
            cx.emit(TerminalEvent::Resize { cols: size.cols as u16, rows: size.rows as u16 });
        }
    }

    /// Bytes typed or pasted by the user: follow the live screen again.
    fn send_user_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
        self.term.selection = None;
        self.restart_blink(cx);
        cx.emit(TerminalEvent::Input(bytes));
        cx.notify();
    }

    fn drain_term_events(&mut self, cx: &mut Context<Self>) {
        let events = std::mem::take(&mut *self.listener.0.borrow_mut());
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => cx.emit(TerminalEvent::Input(text.into_bytes())),
                TermEvent::Title(title) => {
                    if self.title.as_deref() != Some(&title) {
                        self.title = Some(title.clone());
                        cx.emit(TerminalEvent::TitleChanged(Some(title)));
                    }
                }
                TermEvent::ResetTitle => {
                    if self.title.take().is_some() {
                        cx.emit(TerminalEvent::TitleChanged(None));
                    }
                }
                TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                TermEvent::ClipboardLoad(_, format) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        cx.emit(TerminalEvent::Input(format(&text).into_bytes()));
                    }
                }
                TermEvent::ColorRequest(ix, format) => {
                    let rgb = Palette::from_theme(cx.theme()).rgb_for_index(ix, self.term.colors());
                    cx.emit(TerminalEvent::Input(format(rgb).into_bytes()));
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let (cell_width, cell_height) = self.geometry.map_or((0, 0), |g| {
                        (f32::from(g.cell.width).round() as u16, f32::from(g.cell.height).round() as u16)
                    });
                    let size = WindowSize {
                        num_lines: self.size.rows as u16,
                        num_cols: self.size.cols as u16,
                        cell_width,
                        cell_height,
                    };
                    cx.emit(TerminalEvent::Input(format(size).into_bytes()));
                }
                TermEvent::Bell => cx.emit(TerminalEvent::Bell),
                TermEvent::CursorBlinkingChange => self.restart_blink(cx),
                TermEvent::Wakeup | TermEvent::MouseCursorDirty | TermEvent::Exit | TermEvent::ChildExit(_) => {}
            }
        }
    }

    /// A synchronized update (DEC 2026) holds the screen until it ends or
    /// times out; make sure the timeout is honored without more output.
    fn schedule_sync_timeout(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.processor.sync_timeout().sync_timeout() else {
            self._sync_task = None;
            return;
        };
        if self._sync_task.is_some() {
            return;
        }
        let delay = deadline.saturating_duration_since(Instant::now());
        self._sync_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this._sync_task = None;
                if this.processor.sync_timeout().sync_timeout().is_some() {
                    this.processor.stop_sync(&mut this.term);
                    this.drain_term_events(cx);
                    cx.notify();
                }
            });
        }));
    }

    // Focus and cursor blink.

    fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.term.is_focused = focused;
        if self.term.mode().contains(TermMode::FOCUS_IN_OUT) {
            let report: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
            cx.emit(TerminalEvent::Input(report.to_vec()));
        }
        if !focused {
            self.hovered_link = None;
        }
        self.restart_blink(cx);
        cx.notify();
    }

    /// Shows the cursor and restarts its blink phase. Blinks only while
    /// focused, when the program's cursor style and the theme's blink policy
    /// both say so, and never with reduced motion.
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        self.last_activity = Instant::now();
        if !self.cursor_visible {
            self.cursor_visible = true;
            cx.notify();
        }
        let policy = cx.theme().cursor_blink;
        let interval =
            policy.interval().filter(|_| self.focused && self.term.cursor_style().blinking && !cx.reduce_motion());
        let Some(interval) = interval else {
            self._blink_task = Task::ready(());
            return;
        };
        let idle_timeout = policy.idle_timeout();
        let epoch = self.blink_epoch;
        self._blink_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.blink_epoch != epoch {
                            return false;
                        }
                        if idle_timeout.is_some_and(|t| this.last_activity.elapsed() >= t) {
                            if !this.cursor_visible {
                                this.cursor_visible = true;
                                cx.notify();
                            }
                            return false;
                        }
                        this.cursor_visible = !this.cursor_visible;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
    }

    // Keyboard.

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // AltGr and similar: the layout's character wins over the modifiers.
        if event.prefer_character_input && keystroke.key_char.is_some() {
            return;
        }
        let m = &keystroke.modifiers;
        if keystroke.key == "c" && m.control && !m.shift && !m.alt && self.has_selection() {
            self.copy_selection(cx);
            self.term.selection = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(bytes) = encode_keystroke(keystroke, *self.term.mode(), self.alt_is_meta) {
            cx.stop_propagation();
            self.send_user_input(bytes.into_owned(), cx);
        }
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    // Actions.

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    /// Drops the scrollback and the selection, then sends a form feed so the
    /// shell clears the screen and redraws its prompt.
    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.term.selection = None;
        self.term.scroll_display(Scroll::Bottom);
        self.term.grid_mut().clear_history();
        cx.emit(TerminalEvent::Input(b"\x0c".to_vec()));
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let grid = self.term.grid();
        let start = GridPoint::new(grid.topmost_line(), Column(0));
        let end = GridPoint::new(grid.bottommost_line(), grid.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
        cx.notify();
    }

    fn scroll_to(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        let before = self.term.grid().display_offset();
        self.term.scroll_display(scroll);
        if self.term.grid().display_offset() != before {
            cx.notify();
        }
    }

    // Pointer.

    pub(crate) fn mouse_down(&mut self, event: &MouseDownEvent, geometry: &Geometry, cx: &mut Context<Self>) {
        let (col, row, side) = geometry.cell_at(event.position);
        let mode = *self.term.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            if let Some(bytes) = report(MouseAction::Press(event.button), col, row, event.modifiers, mode) {
                self.reported_button = Some(event.button);
                self.last_reported_cell = Some((col, row));
                cx.emit(TerminalEvent::Input(bytes));
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        if event.modifiers.secondary(cx)
            && let Some(link) = self.link_at(col, row)
        {
            cx.open_url(&link.uri);
            return;
        }
        let point = self.grid_point(col, row);
        let kind = match event.click_count {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        match (&mut self.term.selection, event.modifiers.shift && kind == SelectionType::Simple) {
            (Some(selection), true) => selection.update(point, side),
            _ => self.term.selection = Some(Selection::new(kind, point, side)),
        }
        self.selecting = true;
        cx.notify();
    }

    pub(crate) fn mouse_move(&mut self, event: &MouseMoveEvent, geometry: &Geometry, cx: &mut Context<Self>) {
        let (col, row, side) = geometry.cell_at(event.position);
        let mode = *self.term.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift && !self.selecting {
            if self.last_reported_cell != Some((col, row))
                && let Some(bytes) = report(MouseAction::Move(self.reported_button), col, row, event.modifiers, mode)
            {
                self.last_reported_cell = Some((col, row));
                cx.emit(TerminalEvent::Input(bytes));
            }
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            // Dragging past the top or bottom edge scrolls the scrollback.
            let top = geometry.origin.y;
            let bottom = top + geometry.cell.height * geometry.rows as f32;
            if event.position.y < top {
                self.term.scroll_display(Scroll::Delta(1));
            } else if event.position.y > bottom {
                self.term.scroll_display(Scroll::Delta(-1));
            }
            let point = self.grid_point(col, row);
            if let Some(selection) = &mut self.term.selection {
                selection.update(point, side);
            }
            cx.notify();
            return;
        }
        let link = if event.modifiers.secondary(cx) && geometry.contains(event.position) {
            self.link_at(col, row)
        } else {
            None
        };
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent, geometry: &Geometry, cx: &mut Context<Self>) {
        if let Some(button) = self.reported_button.take() {
            let (col, row, _) = geometry.cell_at(event.position);
            if let Some(bytes) = report(MouseAction::Release(button), col, row, event.modifiers, *self.term.mode()) {
                cx.emit(TerminalEvent::Input(bytes));
            }
            return;
        }
        if self.selecting && event.button == MouseButton::Left {
            self.selecting = false;
            if self.term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                self.term.selection = None;
            }
            cx.notify();
        }
    }

    pub(crate) fn scroll_wheel(&mut self, event: &ScrollEvent, geometry: &Geometry, cx: &mut Context<Self>) {
        let line_height = geometry.cell.height;
        self.scroll_remainder += event.delta.pixel_delta(line_height).y;
        let lines = (self.scroll_remainder / line_height) as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= line_height * lines as f32;
        let mode = *self.term.mode();
        let (col, row, _) = geometry.cell_at(event.position);
        let count = lines.unsigned_abs().min(64) as usize;
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            let action = if lines > 0 { MouseAction::WheelUp } else { MouseAction::WheelDown };
            if let Some(one) = report(action, col, row, event.modifiers, mode) {
                cx.emit(TerminalEvent::Input(one.repeat(count)));
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !event.modifiers.shift {
            // Full-screen programs without mouse support scroll by arrow keys.
            let app = mode.contains(TermMode::APP_CURSOR);
            let key: &[u8] = match (lines > 0, app) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            cx.emit(TerminalEvent::Input(key.repeat(count)));
        } else {
            self.scroll_to(Scroll::Delta(lines), cx);
        }
    }

    fn grid_point(&self, col: usize, row: usize) -> GridPoint {
        let offset = self.term.grid().display_offset() as i32;
        let row = row.min(self.size.rows.saturating_sub(1));
        GridPoint::new(Line(row as i32 - offset), Column(col.min(self.size.cols.saturating_sub(1))))
    }

    /// The OSC 8 hyperlink or plain-text URL at a viewport cell.
    fn link_at(&self, col: usize, row: usize) -> Option<HoveredLink> {
        let point = self.grid_point(col, row);
        let grid = self.term.grid();
        let cols = self.size.cols;
        if let Some(hyperlink) = grid[point].hyperlink() {
            if !links::is_openable(hyperlink.uri()) {
                return None;
            }
            let same = |c: usize| grid[GridPoint::new(point.line, Column(c))].hyperlink().as_ref() == Some(&hyperlink);
            let mut start = point.column.0;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = point.column.0 + 1;
            while end < cols && same(end) {
                end += 1;
            }
            return Some(HoveredLink { line: point.line, cols: start..end, uri: hyperlink.uri().to_owned() });
        }
        let chars: Vec<char> = (0..cols).map(|c| grid[GridPoint::new(point.line, Column(c))].c).collect();
        let (range, uri) = links::url_at(&chars, point.column.0)?;
        Some(HoveredLink { line: point.line, cols: range, uri })
    }
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(|this, _: &ScrollLineUp, _, cx| this.scroll_to(Scroll::Delta(1), cx)))
            .on_action(cx.listener(|this, _: &ScrollLineDown, _, cx| this.scroll_to(Scroll::Delta(-1), cx)))
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| this.scroll_to(Scroll::PageUp, cx)))
            .on_action(cx.listener(|this, _: &ScrollPageDown, _, cx| this.scroll_to(Scroll::PageDown, cx)))
            .on_action(cx.listener(|this, _: &ScrollToTop, _, cx| this.scroll_to(Scroll::Top, cx)))
            .on_action(cx.listener(|this, _: &ScrollToBottom, _, cx| this.scroll_to(Scroll::Bottom, cx)))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .p_1()
            .child(TerminalElement::new(cx.entity(), self.focus_handle.clone()))
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.ime_marked.as_ref().map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.ime_marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.ime_marked = None;
        self.send_user_input(text.as_bytes().to_vec(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_marked = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cell = self.geometry?.cell;
        let cursor = self.term.grid().cursor.point;
        let row = cursor.line.0 + self.term.grid().display_offset() as i32;
        let origin = element_bounds.origin + point(cell.width * cursor.column.0 as f32, cell.height * row as f32);
        Some(Bounds::new(origin, size(cell.width, cell.height)))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}
