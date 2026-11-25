//! The file viewer: one file of the thread's workspace, read-only.
//!
//! * Code and text in the editor component the review's diffs use (the same
//!   tree-sitter grammars), with line numbers, search (`mod-f`) and go to
//!   line (`ctrl-g`). It draws only the visible lines; files beyond
//!   [`MAX_LINES`] show their beginning.
//! * Markdown rendered (Source shows the text), images drawn, CSV and TSV as
//!   a table whose rows are virtualized.
//! * The file is not reloaded under the reader: when the agent changes it, a
//!   "Changed on disk" button reloads it.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::editor;
use crate::app_state::{AppEvent, AppState};

gpui_kit::actions!(viewer, [GoToLine, CloseViewer]);

const CONTEXT: &str = "FileViewer";
/// Bytes read for a text file.
const READ_LIMIT: u32 = 4 * 1024 * 1024;
/// Lines shown at most; the editor stays fast up to about this many.
pub const MAX_LINES: usize = 50_000;
/// Rows of a CSV table kept.
const MAX_ROWS: usize = 100_000;
/// Columns of a CSV table shown.
const MAX_COLUMNS: usize = 64;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-g", GoToLine, Some(CONTEXT)),
        KeyBinding::new("escape", CloseViewer, Some("FileViewer && !Input")),
    ]);
}

pub enum FileViewerEvent {
    Close,
    Mention(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    Markdown,
    Table { separator: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Rendered,
    Source,
}

enum Content {
    Loading,
    Text { truncated: bool, lines: usize },
    Image(Arc<Image>),
    Binary,
    Error(String),
}

pub struct FileViewer {
    app: Entity<AppState>,
    thread: ThreadId,
    path: String,
    absolute: Option<String>,
    size: u64,
    kind: Kind,
    mode: Mode,
    content: Content,
    /// The text, for the rendered views.
    text: SharedString,
    table: Option<Arc<Table>>,
    editor: Entity<EditorState>,
    goto: Option<Entity<InputState>>,
    /// The line to show once the text is in.
    pending_line: Option<u32>,
    /// The file changed on disk since it was read.
    stale: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FileViewerEvent> for FileViewer {}

impl FileViewer {
    pub fn new(
        app: Entity<AppState>,
        thread: ThreadId,
        path: String,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = kind_of(&path);
        let language = language_of(&path);
        let editor = cx.new(|cx| EditorState::new(window, cx).language(language).line_number(true).soft_wrap(false));
        editor.update(cx, |state, cx| state.set_readonly(true, cx));
        let watched = path.clone();
        let subscription = cx.subscribe(&app, move |this, _, event: &AppEvent, cx| {
            if let AppEvent::Workspace(t) = event
                && *t == this.thread
                && !this.stale
                && matches!(this.content, Content::Text { .. } | Content::Image(_))
                && this
                    .app
                    .read(cx)
                    .state()
                    .workspace
                    .get(&this.thread)
                    .is_some_and(|files| files.iter().any(|f| f.path.replace('\\', "/") == watched))
            {
                // Agents edit files while they are read; reload on request.
                this.stale = true;
                cx.notify();
            }
        });
        let mut viewer = Self {
            app,
            thread,
            path,
            absolute: None,
            size: 0,
            kind,
            mode: if kind == Kind::Code { Mode::Source } else { Mode::Rendered },
            content: Content::Loading,
            text: SharedString::default(),
            table: None,
            editor,
            goto: None,
            pending_line: line,
            stale: false,
            focus: cx.focus_handle(),
            _subscriptions: vec![subscription],
        };
        viewer.load(window, cx);
        viewer
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stale = false;
        let command = Command::Workspace(WorkspaceCommand::ReadFile {
            root: WorkspaceRoot::Thread(self.thread),
            path: self.path.clone(),
            max_bytes: READ_LIMIT,
        });
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.loaded(result, window, cx));
        })
        .detach();
    }

    fn loaded(&mut self, result: Result<Reply, String>, window: &mut Window, cx: &mut Context<Self>) {
        let file = match result {
            Ok(Reply::Workspace(WorkspaceReply::File(file))) => file,
            Ok(_) => return,
            Err(message) => {
                self.content = Content::Error(message);
                cx.notify();
                return;
            }
        };
        self.absolute = Some(file.absolute.clone());
        self.size = file.size;
        self.content = match file.body {
            FileBody::Binary => Content::Binary,
            FileBody::Image { mime, data } => Content::Image(Arc::new(Image::from_bytes(image_format(&mime), data))),
            FileBody::Text { text, truncated } => {
                let (text, cut) = first_lines(text, MAX_LINES);
                let lines = text.lines().count().max(1);
                self.table = match self.kind {
                    Kind::Table { separator } => Some(Arc::new(parse_table(&text, separator))),
                    _ => None,
                };
                self.text = SharedString::from(text);
                let shown = self.text.clone();
                self.editor.update(cx, |state, cx| state.set_value(shown, window, cx));
                Content::Text { truncated: truncated || cut, lines }
            }
        };
        if let Some(line) = self.pending_line.take() {
            self.go_to_line(Some(line), window, cx);
        }
        cx.notify();
    }

    /// Shows `line` (1-based) with the cursor on it.
    pub fn go_to_line(&mut self, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = line.filter(|l| *l > 0) else { return };
        if !matches!(self.content, Content::Text { .. }) {
            self.pending_line = Some(line);
            return;
        }
        self.mode = Mode::Source;
        let row = line.saturating_sub(1);
        self.editor.update(cx, |state, cx| state.set_cursor_position(gpui_kit::component::input::Position::new(row, 0), window, cx));
        cx.notify();
    }

    fn open_goto(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Text { lines, .. } = self.content else { return };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(format!("Line, 1 to {lines}")));
        self._subscriptions.push(cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => {
                let line = input.read(cx).value().trim().parse::<u32>().ok();
                this.goto = None;
                this.go_to_line(line, window, cx);
            }
            InputEvent::Blur => {
                this.goto = None;
                cx.notify();
            }
            _ => {}
        }));
        let focus = input.clone();
        window.defer(cx, move |window, cx| focus.update(cx, |input, cx| input.focus(window, cx)));
        self.goto = Some(input);
        cx.notify();
    }

    fn close(&mut self, _: &CloseViewer, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FileViewerEvent::Close);
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let message = |text: String, cx: &App| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_8()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text)
                .into_any_element()
        };
        match &self.content {
            Content::Loading => message("Reading…".into(), cx),
            Content::Error(error) => message(error.clone(), cx),
            Content::Binary => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                    "{} is not text Orbit can show ({}).",
                    file_name(&self.path),
                    format_size(self.size)
                )))
                .when_some(self.absolute.clone(), |el, path| {
                    el.child(
                        Button::new("binary-open")
                            .outline()
                            .small()
                            .label("Open with the default app")
                            .on_click(move |_, _, cx| cx.open_with_system(std::path::Path::new(&path))),
                    )
                })
                .into_any_element(),
            Content::Image(image) => div()
                .id("image-view")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .p_4()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.muted.opacity(0.4))
                .child(img(image.clone()).max_w_full().max_h_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            Content::Text { .. } => match (self.kind, self.mode) {
                (Kind::Markdown, Mode::Rendered) => div()
                    .flex_1()
                    .min_h_0()
                    .px_6()
                    .py_4()
                    .child(
                        TextView::markdown(SharedString::from(format!("viewer-md-{}", self.path)), self.text.clone())
                            .selectable(true)
                            .scrollable(true),
                    )
                    .into_any_element(),
                (Kind::Table { .. }, Mode::Rendered) => match &self.table {
                    Some(table) => render_table(table.clone(), cx),
                    None => message("Empty table.".into(), cx),
                },
                _ => Editor::new(&self.editor).readonly(true).bordered(false).h_full().into_any_element(),
            },
        }
    }
}

impl Focusable for FileViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FileViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (dir, name) = match self.path.rsplit_once('/') {
            Some((dir, name)) => (Some(format!("{dir}/")), name.to_owned()),
            None => (None, self.path.clone()),
        };
        let has_modes = matches!(self.kind, Kind::Markdown | Kind::Table { .. }) && matches!(self.content, Content::Text { .. });
        let info = match &self.content {
            Content::Text { lines, truncated, .. } => {
                let lines = if *lines == 1 { "1 line".to_owned() } else { format!("{lines} lines") };
                Some((format!("{lines} · {}", format_size(self.size)), *truncated))
            }
            Content::Image(_) | Content::Binary => Some((format_size(self.size), false)),
            _ => None,
        };
        let absolute = self.absolute.clone();
        let path = self.path.clone();
        let view = cx.entity().downgrade();
        v_flex()
            .size_full()
            .bg(theme.background)
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_goto))
            .on_action(cx.listener(Self::close))
            .child(
                h_flex()
                    .h_8()
                    .flex_none()
                    .pl_3()
                    .pr_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(super::file_icon(&self.path, false, false).xsmall().flex_none().text_color(theme.muted_foreground))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .child(div().flex_shrink(1.).min_w_0().truncate().text_color(theme.muted_foreground).children(dir))
                            .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(name)),
                    )
                    .when(self.stale, |el| {
                        el.child(
                            Button::new("viewer-reload")
                                .ghost()
                                .xsmall()
                                .icon(IconName::RefreshCw)
                                .label("Changed on disk")
                                .tooltip("Read the file again")
                                .on_click(cx.listener(|this, _, window, cx| this.load(window, cx))),
                        )
                    })
                    .when(has_modes, |el| {
                        let selected = if self.mode == Mode::Rendered { 0 } else { 1 };
                        let rendered = if matches!(self.kind, Kind::Markdown) { "Preview" } else { "Table" };
                        let view = view.clone();
                        el.child(
                            TabBar::new("viewer-mode")
                                .xsmall()
                                .segmented()
                                .child(Tab::new().label(rendered))
                                .child(Tab::new().label("Source"))
                                .selected_index(selected)
                                .on_click(move |ix, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.mode = if *ix == 0 { Mode::Rendered } else { Mode::Source };
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .when(matches!(self.content, Content::Text { .. }), |el| {
                        el.child(
                            Button::new("viewer-goto")
                                .ghost()
                                .xsmall()
                                .icon(IconName::ArrowDown)
                                .tooltip_with_action("Go to line…", &GoToLine, Some(CONTEXT))
                                .on_click(cx.listener(|this, _, window, cx| this.open_goto(&GoToLine, window, cx))),
                        )
                    })
                    .when_some(absolute.clone(), |el, absolute| {
                        let editor_view = cx.entity().downgrade();
                        el.child(
                            Button::new("viewer-open")
                                .ghost()
                                .xsmall()
                                .icon(IconName::ExternalLink)
                                .tooltip(editor::open_label(cx))
                                .on_click(move |_, _, cx| {
                                    let line = editor_view
                                        .upgrade()
                                        .map(|v| v.read(cx).editor.read(cx).cursor_position().line + 1);
                                    editor::open_file(std::path::Path::new(&absolute), line, cx);
                                }),
                        )
                    })
                    .child({
                        let (absolute, path) = (absolute.clone(), path.clone());
                        let view = view.clone();
                        Button::new("viewer-more").ghost().xsmall().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, _| {
                            let mut menu = menu;
                            if let Some(absolute) = absolute.clone() {
                                let reveal = absolute.clone();
                                menu = menu
                                    .item(
                                        PopupMenuItem::new(format!("Reveal in {}", editor::file_manager()))
                                            .icon(IconName::FolderOpen)
                                            .on_click(move |_, _, cx| editor::reveal(std::path::Path::new(&reveal), cx)),
                                    )
                                    .item(PopupMenuItem::new("Copy path").icon(IconName::Copy).on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(absolute.clone()))
                                    }));
                            }
                            let (relative, mention, view) = (path.clone(), path.clone(), view.clone());
                            menu.item(PopupMenuItem::new("Copy relative path").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()))
                            }))
                            .separator()
                            .item(PopupMenuItem::new("Mention in composer").on_click(move |_, _, cx| {
                                let mention = mention.clone();
                                let _ = view.update(cx, |_, cx| cx.emit(FileViewerEvent::Mention(mention)));
                            }))
                        })
                    })
                    .child(
                        Button::new("viewer-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close the file (Esc)")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(FileViewerEvent::Close))),
                    ),
            )
            .when_some(self.goto.clone(), |el, input| {
                el.child(
                    h_flex()
                        .flex_none()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Go to line")
                        .child(Input::new(&input).xsmall().w(rems(10.))),
                )
            })
            .child(div().flex_1().min_h_0().flex().flex_col().child(self.render_body(cx)))
            .when_some(info, |el, (info, truncated)| {
                el.child(
                    h_flex()
                        .h_6()
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(info)
                        .when(truncated, |el| {
                            el.child("·").child(div().text_color(theme.warning).child("Showing the beginning; open it in your editor for the rest"))
                        }),
                )
            })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn kind_of(path: &str) -> Kind {
    let name = file_name(path).to_ascii_lowercase();
    match name.rsplit_once('.').map(|(_, e)| e) {
        Some("md" | "mdx" | "markdown") => Kind::Markdown,
        Some("csv") => Kind::Table { separator: b',' },
        Some("tsv") => Kind::Table { separator: b'\t' },
        _ => Kind::Code,
    }
}

/// The highlighter's language for a path: the same names the review's
/// diffs use; anything else is plain text.
pub fn language_of(path: &str) -> &'static str {
    let name = file_name(path).to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" => return "bash",
        "cargo.lock" | "pipfile" => return "toml",
        _ => {}
    }
    match name.rsplit_once('.').map(|(_, e)| e).unwrap_or("") {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdx" | "markdown" => "markdown",
        "css" | "scss" => "css",
        "html" | "htm" => "html",
        "sh" | "bash" | "zsh" => "bash",
        "json" | "jsonc" => "json",
        _ => "text",
    }
}

fn image_format(mime: &str) -> ImageFormat {
    match mime {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/svg+xml" => ImageFormat::Svg,
        "image/bmp" => ImageFormat::Bmp,
        "image/tiff" => ImageFormat::Tiff,
        "image/x-icon" => ImageFormat::Ico,
        _ => ImageFormat::Png,
    }
}

/// The first `max` lines of `text`; `true` when some were cut.
fn first_lines(text: String, max: usize) -> (String, bool) {
    let end = text.bytes().enumerate().filter(|(_, b)| *b == 10).map(|(ix, _)| ix).nth(max.saturating_sub(1));
    match end {
        Some(end) if end + 1 < text.len() => (text[..end + 1].to_owned(), true),
        _ => (text, false),
    }
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / KB / KB)
    }
}

/// A parsed CSV or TSV file.
#[derive(Debug, PartialEq)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Characters of the widest cell per column, capped.
    pub widths: Vec<usize>,
}

/// Parses delimited text: quoted fields with doubled quotes and line
/// breaks inside quotes, as RFC 4180 writes them.
pub fn parse_table(text: &str, separator: u8) -> Table {
    let separator = separator as char;
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == separator => record.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                if records.len() > MAX_ROWS {
                    break;
                }
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    let mut records = records.into_iter();
    let header: Vec<String> = records.next().unwrap_or_default().into_iter().take(MAX_COLUMNS).collect();
    let rows: Vec<Vec<String>> = records.map(|r| r.into_iter().take(MAX_COLUMNS).collect()).collect();
    let columns = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(0);
    let mut widths = vec![3usize; columns];
    for row in std::iter::once(&header).chain(rows.iter().take(2_000)) {
        for (ix, cell) in row.iter().enumerate() {
            widths[ix] = widths[ix].max(cell.chars().count().min(48));
        }
    }
    Table { header, rows, widths }
}

fn render_table(table: Arc<Table>, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let char_width = 0.45;
    let widths: Vec<Rems> = table.widths.iter().map(|w| rems(*w as f32 * char_width + 1.0)).collect();
    let total: f32 = table.widths.iter().map(|w| *w as f32 * char_width + 1.0).sum::<f32>() + 3.0;
    let cell = |text: String, width: Rems, header: bool, theme: &gpui_kit::component::Theme| {
        div()
            .flex_none()
            .w(width)
            .px_2()
            .truncate()
            .border_r_1()
            .border_color(theme.border.opacity(0.5))
            .when(header, |el| el.font_weight(FontWeight::SEMIBOLD))
            .child(text)
    };
    let header_widths = widths.clone();
    let header = h_flex()
        .h_7()
        .flex_none()
        .w(rems(total))
        .bg(theme.muted)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .child(div().flex_none().w(rems(3.)).px_2().text_color(theme.muted_foreground).child("#"))
        .children(table.header.iter().enumerate().map(|(ix, h)| {
            cell(h.clone(), header_widths.get(ix).copied().unwrap_or(rems(4.)), true, &theme)
        }));
    let rows = table.clone();
    let list = uniform_list("csv-rows", table.rows.len(), move |range, _, cx| {
        let theme = cx.theme().clone();
        range
            .map(|ix| {
                let row = &rows.rows[ix];
                h_flex()
                    .h_6()
                    .w(rems(total))
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .border_b_1()
                    .border_color(theme.border.opacity(0.35))
                    .when(ix % 2 == 1, |el| el.bg(theme.muted.opacity(0.35)))
                    .child(div().flex_none().w(rems(3.)).px_2().text_color(theme.muted_foreground).child((ix + 1).to_string()))
                    .children(row.iter().enumerate().map(|(c, text)| cell(text.clone(), widths.get(c).copied().unwrap_or(rems(4.)), false, &theme)))
            })
            .collect()
    })
    .w(rems(total))
    .flex_1();
    div()
        .id("csv-scroll")
        .flex_1()
        .min_h_0()
        .overflow_x_scroll()
        .child(v_flex().h_full().w(rems(total)).child(header).child(list))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Kind, Table, first_lines, format_size, kind_of, language_of, parse_table};

    #[test]
    fn csv_quotes_and_line_breaks_parse() {
        let table = parse_table("name,note\r\nAda,\"likes, commas\"\nBob,\"says \"\"hi\"\"\nthen leaves\"\n", b',');
        assert_eq!(table.header, vec!["name", "note"]);
        assert_eq!(table.rows, vec![vec!["Ada", "likes, commas"], vec!["Bob", "says \"hi\"\nthen leaves"]]);
        let tsv = parse_table("a\tb\n1\t2", b'\t');
        assert_eq!(tsv.rows, vec![vec!["1", "2"]]);
        assert_eq!(parse_table("", b','), Table { header: vec![], rows: vec![], widths: vec![] });
    }

    #[test]
    fn long_files_show_their_beginning() {
        let text: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let (head, cut) = first_lines(text.clone(), 3);
        assert_eq!((head.as_str(), cut), ("line 0\nline 1\nline 2\n", true));
        assert_eq!(first_lines(text.clone(), 10), (text.clone(), false));
        assert_eq!(first_lines("one".into(), 1), ("one".into(), false));
    }

    #[test]
    fn languages_and_kinds_follow_the_file_name() {
        assert_eq!(language_of("src/main.rs"), "rust");
        assert_eq!(language_of("web/App.TSX"), "tsx");
        assert_eq!(language_of("notes.txt"), "text");
        assert!(matches!(kind_of("README.md"), Kind::Markdown));
        assert!(matches!(kind_of("data/x.tsv"), Kind::Table { separator: b'\t' }));
        assert_eq!(format_size(1536), "1.5 KB");
    }
}
