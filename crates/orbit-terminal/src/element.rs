//! The terminal grid element: layout, painting and pointer wiring.
//!
//! Each frame the visible grid becomes, per row, a few shaped lines (one per
//! run of cells between wide characters, with one `TextRun` per style) and a
//! list of background rectangles merged horizontally and vertically. Glyphs
//! are placed on the cell grid with `shape_line`'s forced advance, so fallback
//! fonts and bold faces never drift off the grid, and the cell height is
//! snapped to device pixels so rows never blur or overlap on high-DPI
//! displays.

use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
// Upstream GPUI still calls the wheel event by its old name.
#[cfg(not(feature = "fast"))]
use gpui_kit::ScrollWheelEvent as ScrollEvent;

use crate::colors::Palette;
use crate::view::TerminalView;

/// Line height as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.3;
/// Beam and underline cursor thickness as a fraction of the cell.
const CURSOR_THICKNESS: f32 = 0.12;

/// The size of one cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellSize {
    pub width: Pixels,
    pub height: Pixels,
}

/// Where the grid sits in the window, for mapping pointer positions to cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub origin: Point<Pixels>,
    pub cell: CellSize,
    pub cols: usize,
    pub rows: usize,
}

impl Geometry {
    /// The viewport cell under `position` (clamped to the grid) and the half
    /// of the cell it is in.
    pub fn cell_at(&self, position: Point<Pixels>) -> (usize, usize, alacritty_terminal::index::Side) {
        let x = ((position.x - self.origin.x) / self.cell.width).max(0.);
        let y = ((position.y - self.origin.y) / self.cell.height).max(0.);
        let col = (x as usize).min(self.cols.saturating_sub(1));
        let row = (y as usize).min(self.rows.saturating_sub(1));
        let side = if x.fract() >= 0.5 && (x as usize) < self.cols {
            alacritty_terminal::index::Side::Right
        } else {
            alacritty_terminal::index::Side::Left
        };
        (col, row, side)
    }

    pub fn contains(&self, position: Point<Pixels>) -> bool {
        let width = self.cell.width * self.cols as f32;
        let height = self.cell.height * self.rows as f32;
        Bounds::new(self.origin, size(width, height)).contains(&position)
    }

    fn cell_bounds(&self, col: usize, row: usize, cols: usize, rows: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(self.origin.x + self.cell.width * col as f32, self.origin.y + self.cell.height * row as f32),
            size(self.cell.width * cols as f32, self.cell.height * rows as f32),
        )
    }
}

pub(crate) struct TerminalElement {
    view: Entity<TerminalView>,
    focus: FocusHandle,
    style: StyleRefinement,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>, focus: FocusHandle) -> Self {
        let mut style = StyleRefinement::default();
        style.size.width = Some(relative(1.).into());
        style.size.height = Some(relative(1.).into());
        Self { view, focus, style }
    }
}

/// A background rectangle in cell units.
struct BackgroundRect {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    color: Hsla,
}

/// One run of glyphs placed from `col` on `row`.
struct Segment {
    col: usize,
    row: usize,
    text: String,
    runs: Vec<TextRun>,
    /// The column the next glyph would occupy.
    next_col: usize,
    /// No more glyphs may join (it holds a wide character).
    sealed: bool,
}

impl Segment {
    fn is_blank(&self) -> bool {
        self.text.chars().all(|c| c == ' ')
    }

    /// Drops trailing undecorated spaces: they paint nothing.
    fn trim_end(&mut self) {
        while self.text.ends_with(' ') {
            let Some(run) = self.runs.last_mut() else { break };
            if run.underline.is_some() || run.strikethrough.is_some() {
                break;
            }
            self.text.pop();
            run.len -= 1;
            if run.len == 0 {
                self.runs.pop();
            }
        }
    }
}

enum CursorPaint {
    Fill(Bounds<Pixels>, Hsla),
    Outline(Bounds<Pixels>, Hsla),
}

pub(crate) struct LayoutState {
    geometry: Geometry,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<CursorPaint>,
    ime: Option<(Bounds<Pixels>, ShapedLine)>,
    background: Hsla,
    cursor_style: CursorStyle,
    hitbox: Hitbox,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Terminal)
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LayoutState {
        let theme = cx.theme();
        let palette = Palette::from_theme(theme);
        // The terminal zooms with the interface: its font scales with rem.
        let font_size = theme.mono_font_size * (window.rem_size() / theme.font_size);
        let font = Font {
            family: theme.mono_font_family.clone(),
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system.em_advance(font_id, font_size).unwrap_or(font_size * 0.6);
        // Whole device pixels per row: fractional rows blur glyphs and leave
        // seams between background rectangles.
        let scale = window.scale_factor();
        let cell_height = px((f32::from(font_size * LINE_HEIGHT) * scale).ceil() / scale);
        let cols = ((bounds.size.width / cell_width) as usize).max(2);
        let rows = ((bounds.size.height / cell_height) as usize).max(1);
        let geometry =
            Geometry { origin: bounds.origin, cell: CellSize { width: cell_width, height: cell_height }, cols, rows };
        self.view.update(cx, |view, cx| view.set_geometry(geometry, cx));

        let view = self.view.read(cx);
        let mut grid = GridLayout::new(&palette, &font);
        grid.build(view);
        let GridLayout { backgrounds, segments, cursor, .. } = grid;

        let backgrounds =
            backgrounds.into_iter().map(|r| (geometry.cell_bounds(r.col, r.row, r.cols, r.rows), r.color)).collect();
        let text_system = window.text_system();
        let lines = segments
            .into_iter()
            .map(|segment| {
                let origin = geometry.cell_bounds(segment.col, segment.row, 1, 1).origin;
                let line = text_system.shape_line(segment.text.into(), font_size, &segment.runs, Some(cell_width));
                (origin, line)
            })
            .collect();

        let cursor = cursor.map(|c| {
            let cell = geometry.cell_bounds(c.col, c.row, c.cols, 1);
            let thickness = (cell_width * CURSOR_THICKNESS).max(px(1.));
            match c.kind {
                CursorKind::Beam => {
                    CursorPaint::Fill(Bounds::new(cell.origin, size(thickness, cell.size.height)), c.color)
                }
                CursorKind::Underline => CursorPaint::Fill(
                    Bounds::new(point(cell.origin.x, cell.bottom() - thickness), size(cell.size.width, thickness)),
                    c.color,
                ),
                CursorKind::Hollow => CursorPaint::Outline(cell, c.color),
            }
        });

        let ime = view.ime_marked().zip(grid.cursor_cell).map(|(text, (col, row))| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: palette.foreground,
                background_color: None,
                underline: Some(UnderlineStyle { thickness: px(1.), color: Some(palette.foreground), wavy: false }),
                strikethrough: None,
            };
            let line = text_system.shape_line(text.to_owned().into(), font_size, &[run], None);
            let origin = geometry.cell_bounds(col, row, 1, 1).origin;
            (Bounds::new(origin, size(line.width(), cell_height)), line)
        });

        let mode = *view.term().mode();
        let cursor_style = if view.hovered_link().is_some() {
            CursorStyle::PointingHand
        } else if mode.intersects(TermMode::MOUSE_MODE) {
            CursorStyle::Arrow
        } else {
            CursorStyle::IBeam
        };

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        LayoutState { geometry, backgrounds, lines, cursor, ime, background: palette.background, cursor_style, hitbox }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut LayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (rect, color) in &layout.backgrounds {
                window.paint_quad(fill(*rect, *color));
            }
            let line_height = layout.geometry.cell.height;
            for (origin, line) in &layout.lines {
                let _ = line.paint(*origin, line_height, TextAlign::Left, None, window, cx);
            }
            match layout.cursor {
                Some(CursorPaint::Fill(rect, color)) => window.paint_quad(fill(rect, color)),
                Some(CursorPaint::Outline(rect, color)) => window.paint_quad(outline(rect, color, BorderStyle::Solid)),
                None => {}
            }
            if let Some((rect, line)) = &layout.ime {
                window.paint_quad(fill(*rect, layout.background));
                let _ = line.paint(rect.origin, line_height, TextAlign::Left, None, window, cx);
            }
        });

        window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        window.set_cursor_style(layout.cursor_style, &layout.hitbox);
        self.register_mouse_listeners(layout, window);
    }
}

impl TerminalElement {
    fn register_mouse_listeners(&self, layout: &LayoutState, window: &mut Window) {
        let geometry = layout.geometry;
        let hitbox = layout.hitbox.clone();
        {
            let view = self.view.clone();
            let focus = self.focus.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                focus.focus(window, cx);
                view.update(cx, |view, cx| view.mouse_down(event, &geometry, cx));
            });
        }
        {
            let view = self.view.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    view.update(cx, |view, cx| view.mouse_move(event, &geometry, cx));
                }
            });
        }
        {
            let view = self.view.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    view.update(cx, |view, cx| view.mouse_up(event, &geometry, cx));
                }
            });
        }
        {
            let view = self.view.clone();
            window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                    return;
                }
                view.update(cx, |view, cx| view.scroll_wheel(event, &geometry, cx));
                cx.stop_propagation();
            });
        }
    }
}

enum CursorKind {
    Beam,
    Underline,
    Hollow,
}

struct CursorLayout {
    col: usize,
    row: usize,
    cols: usize,
    kind: CursorKind,
    color: Hsla,
}

/// Turns the visible grid into background rectangles and glyph segments.
struct GridLayout<'a> {
    palette: &'a Palette,
    font: &'a Font,
    backgrounds: Vec<BackgroundRect>,
    /// Indices into `backgrounds` of the rectangles reaching the previous
    /// row, for vertical merging.
    previous_row: Vec<usize>,
    /// The current row's rectangles, merged horizontally.
    row_rects: Vec<BackgroundRect>,
    segments: Vec<Segment>,
    cursor: Option<CursorLayout>,
    /// The cursor's viewport cell when it is on screen.
    cursor_cell: Option<(usize, usize)>,
    /// Whether a solid block cursor is drawn by inverting its cell.
    block_cursor: bool,
    /// The last contrast correction: neighboring cells share colors.
    readable: Option<((Hsla, Hsla), Hsla)>,
}

impl<'a> GridLayout<'a> {
    fn new(palette: &'a Palette, font: &'a Font) -> Self {
        Self {
            palette,
            font,
            backgrounds: Vec::new(),
            previous_row: Vec::new(),
            row_rects: Vec::new(),
            segments: Vec::new(),
            cursor: None,
            cursor_cell: None,
            block_cursor: false,
            readable: None,
        }
    }

    fn build(&mut self, view: &TerminalView) {
        let term = view.term();
        let content = term.renderable_content();
        let overrides = content.colors;
        let offset = content.display_offset as i32;
        let cursor = content.cursor;
        let selection = content.selection;
        let link = view.hovered_link();

        let cursor_row = cursor.point.line.0 + offset;
        let cursor_on_screen = cursor.shape != CursorShape::Hidden && cursor_row >= 0;
        if cursor_on_screen {
            self.cursor_cell = Some((cursor.point.column.0, cursor_row as usize));
        }
        let focused = view.is_focused();
        let show = view.is_cursor_visible();
        self.block_cursor = cursor_on_screen && focused && show && cursor.shape == CursorShape::Block;
        let cursor_wide = cursor_on_screen && term.grid()[cursor.point].flags.contains(Flags::WIDE_CHAR);
        if cursor_on_screen {
            let kind = if !focused || cursor.shape == CursorShape::HollowBlock {
                Some(CursorKind::Hollow)
            } else if !show {
                None
            } else {
                match cursor.shape {
                    CursorShape::Beam => Some(CursorKind::Beam),
                    CursorShape::Underline => Some(CursorKind::Underline),
                    _ => None,
                }
            };
            self.cursor = kind.map(|kind| CursorLayout {
                col: cursor.point.column.0,
                row: cursor_row as usize,
                cols: if cursor_wide { 2 } else { 1 },
                kind,
                color: self.palette.cursor,
            });
        }

        let mut row_ix = usize::MAX;
        for indexed in content.display_iter {
            let row = (indexed.point.line.0 + offset) as usize;
            if row != row_ix {
                self.finish_row();
                row_ix = row;
            }
            let cell = indexed.cell;
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let col = indexed.point.column.0;
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            let width = if wide { 2 } else { 1 };

            let mut fg = self.palette.resolve(cell.fg, overrides);
            let mut bg = (!Palette::is_default_background(cell.bg)).then(|| self.palette.resolve(cell.bg, overrides));
            if cell.flags.contains(Flags::INVERSE) {
                let new_bg = fg;
                fg = bg.unwrap_or(self.palette.background);
                bg = Some(new_bg);
            }
            fg = self.readable(fg, bg.unwrap_or(self.palette.background));
            if cell.flags.contains(Flags::DIM) {
                fg = self.palette.dim(fg);
            }
            if selection.is_some_and(|s| s.contains_cell(&indexed, cursor.point, cursor.shape)) {
                bg = Some(bg.unwrap_or(self.palette.background).blend(self.palette.selection));
            }
            if self.block_cursor && indexed.point == cursor.point {
                fg = bg.unwrap_or(self.palette.background);
                bg = Some(self.palette.cursor);
            }
            let linked = link.is_some_and(|l| l.line == indexed.point.line && l.cols.contains(&col));
            if linked {
                fg = self.palette.link;
            }

            if let Some(color) = bg {
                match self.row_rects.last_mut() {
                    Some(r) if r.color == color && r.col + r.cols == col => r.cols += width,
                    _ => self.row_rects.push(BackgroundRect { col, row, cols: width, rows: 1, color }),
                }
            }

            let hidden = cell.flags.contains(Flags::HIDDEN);
            let c = if hidden || cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
            let underline = (cell.flags.intersects(Flags::ALL_UNDERLINES) || linked).then(|| UnderlineStyle {
                thickness: px(1.),
                color: Some(if linked {
                    self.palette.link
                } else {
                    cell.underline_color().map_or(fg, |u| self.palette.resolve(u, overrides))
                }),
                wavy: cell.flags.contains(Flags::UNDERCURL),
            });
            let strikethrough = cell
                .flags
                .contains(Flags::STRIKEOUT)
                .then_some(StrikethroughStyle { thickness: px(1.), color: Some(fg) });
            let weight = if cell.flags.contains(Flags::BOLD) { FontWeight::BOLD } else { FontWeight::NORMAL };
            let style = if cell.flags.contains(Flags::ITALIC) { FontStyle::Italic } else { FontStyle::Normal };
            let zerowidth = if hidden { None } else { cell.zerowidth() };
            self.push_glyph(row, col, wide, c, zerowidth, fg, weight, style, underline, strikethrough);
        }
        self.finish_row();
    }

    fn readable(&mut self, fg: Hsla, bg: Hsla) -> Hsla {
        match self.readable {
            Some((pair, fixed)) if pair == (fg, bg) => fixed,
            _ => {
                let fixed = Palette::readable(fg, bg);
                self.readable = Some(((fg, bg), fixed));
                fixed
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_glyph(
        &mut self,
        row: usize,
        col: usize,
        wide: bool,
        c: char,
        zerowidth: Option<&[char]>,
        color: Hsla,
        weight: FontWeight,
        style: FontStyle,
        underline: Option<UnderlineStyle>,
        strikethrough: Option<StrikethroughStyle>,
    ) {
        let joins = self.segments.last().is_some_and(|s| s.row == row && !s.sealed && s.next_col == col) && !wide;
        if !joins {
            self.segments.push(Segment {
                col,
                row,
                text: String::new(),
                runs: Vec::new(),
                next_col: col,
                sealed: false,
            });
        }
        let Some(segment) = self.segments.last_mut() else { return };
        let start = segment.text.len();
        segment.text.push(c);
        if let Some(marks) = zerowidth {
            segment.text.extend(marks.iter());
        }
        let len = segment.text.len() - start;
        segment.next_col = col + if wide { 2 } else { 1 };
        segment.sealed = wide;
        if let Some(run) = segment.runs.last_mut()
            && run.color == color
            && run.font.weight == weight
            && run.font.style == style
            && run.underline == underline
            && run.strikethrough == strikethrough
        {
            run.len += len;
            return;
        }
        let mut font = self.font.clone();
        font.weight = weight;
        font.style = style;
        segment.runs.push(TextRun { len, font, color, background_color: None, underline, strikethrough });
    }

    /// Ends a row: trims its segments and merges its rectangles into the
    /// ones above when they line up exactly.
    fn finish_row(&mut self) {
        if let Some(row) = self.segments.last().map(|s| s.row) {
            let first = self.segments.iter().rposition(|s| s.row != row).map_or(0, |ix| ix + 1);
            let finished: Vec<Segment> = self.segments.drain(first..).collect();
            for mut segment in finished {
                segment.trim_end();
                let decorated = segment.runs.iter().any(|r| r.underline.is_some() || r.strikethrough.is_some());
                if !segment.text.is_empty() && (decorated || !segment.is_blank()) {
                    self.segments.push(segment);
                }
            }
        }

        let mut reaching = Vec::with_capacity(self.row_rects.len());
        for rect in self.row_rects.drain(..) {
            let above = self.previous_row.iter().copied().find(|&ix| {
                let r = &self.backgrounds[ix];
                r.col == rect.col && r.cols == rect.cols && r.color == rect.color && r.row + r.rows == rect.row
            });
            match above {
                Some(ix) => {
                    self.backgrounds[ix].rows += 1;
                    reaching.push(ix);
                }
                None => {
                    self.backgrounds.push(rect);
                    reaching.push(self.backgrounds.len() - 1);
                }
            }
        }
        self.previous_row = reaching;
    }
}

#[cfg(test)]
mod tests {
    use super::{CellSize, Geometry};
    use gpui_kit::{point, px};

    #[test]
    fn cells_map_to_pointer_positions() {
        let g = Geometry {
            origin: point(px(10.), px(20.)),
            cell: CellSize { width: px(8.), height: px(16.) },
            cols: 80,
            rows: 24,
        };
        let (col, row, side) = g.cell_at(point(px(10. + 8. * 3. + 6.), px(20. + 16. * 2. + 1.)));
        assert_eq!((col, row), (3, 2));
        assert_eq!(side, alacritty_terminal::index::Side::Right);
        assert_eq!(g.cell_at(point(px(0.), px(0.))).0, 0);
        assert_eq!(g.cell_at(point(px(5000.), px(5000.))).1, 23);
        assert!(g.contains(point(px(11.), px(21.))));
        assert!(!g.contains(point(px(9.), px(21.))));
    }
}
