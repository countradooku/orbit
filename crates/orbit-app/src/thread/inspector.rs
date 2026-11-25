//! The step inspector: everything about one tool call, in a sheet beside
//! the thread.
//!
//! * The input reads as fields, strings unescaped (a Write's content, a
//!   subagent's prompt), not as one line of JSON.
//! * The output is a read-only editor (virtualized, searchable with Ctrl+F,
//!   soft-wrapped) that follows the step live; when the stored output was
//!   cut, "Load full output" fetches all of it from the host on demand.
//! * A file-changing step shows its diff in place: the turn's on-disk diff
//!   for exactly the files this step touched, with a way to the full review.
//! * A subagent shows its prompt and its result as Markdown.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Placement, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;
use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};

use super::{ThreadView, ThreadViewEvent};
use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::format;

/// One field of a tool's input, ready to read.
#[derive(Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
    /// A string value (shown as text), not a number, list or object.
    pub text: bool,
}

/// A tool's input JSON as fields: top-level strings as they are, other
/// values indented. Input still streaming (not yet valid JSON) is shown as
/// one indented block.
pub fn input_fields(input: &str) -> Vec<Field> {
    if input.trim().is_empty() {
        return Vec::new();
    }
    let parsed: Option<sonic_rs::Value> = sonic_rs::from_str(input).ok();
    match parsed.as_ref().and_then(|v| v.as_object()) {
        Some(object) => object
            .iter()
            .map(|(name, value)| match value.as_str() {
                Some(text) => Field { name: name.to_owned(), value: text.to_owned(), text: true },
                None => Field {
                    name: name.to_owned(),
                    value: sonic_rs::to_string_pretty(value).unwrap_or_default(),
                    text: false,
                },
            })
            .collect(),
        None => vec![Field { name: String::new(), value: format::pretty_json(input), text: false }],
    }
}

/// Matches a path a tool reported (absolute or relative) to a diff path
/// (relative to the repository).
fn same_file(tool_path: &str, diff_path: &str) -> bool {
    let tool = tool_path.replace('\\', "/");
    let diff = diff_path.replace('\\', "/");
    tool == diff || tool.ends_with(&format!("/{diff}"))
}

enum Full {
    None,
    Loading,
    Loaded,
    Failed(String),
}

enum DiffLoad {
    Idle,
    Loading,
    Ready(Entity<DiffState>),
    Empty,
    Failed(String),
}

pub struct InspectorView {
    app: Entity<AppState>,
    thread: ThreadId,
    item: ItemId,
    view: WeakEntity<ThreadView>,
    output: Entity<EditorState>,
    /// What the output editor shows, so live updates only replace changed text.
    shown: String,
    full: Full,
    fields: Vec<Field>,
    fields_of: String,
    result: Option<Entity<TextViewState>>,
    diff: DiffLoad,
    _subscriptions: Vec<Subscription>,
}

impl InspectorView {
    fn new(
        app: Entity<AppState>,
        thread: ThreadId,
        item: ItemId,
        view: WeakEntity<ThreadView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let output = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        output.update(cx, |state, cx| state.set_readonly(true, cx));
        let sub = cx.subscribe_in(&app, window, move |this, _, event: &AppEvent, window, cx| {
            if matches!(event, AppEvent::Timeline { thread: t, .. } if *t == this.thread) {
                this.sync(window, cx);
            }
        });
        let mut this = Self {
            app,
            thread,
            item,
            view,
            output,
            shown: String::new(),
            full: Full::None,
            fields: Vec::new(),
            fields_of: String::new(),
            result: None,
            diff: DiffLoad::Idle,
            _subscriptions: vec![sub],
        };
        this.sync(window, cx);
        this
    }

    fn call(&self, cx: &App) -> Option<(Item, ToolCall)> {
        let item = self.app.read(cx).state().timeline(self.thread)?.get(self.item)?.clone();
        match &item.body {
            ItemBody::Tool(call) => Some((item.clone(), call.clone())),
            _ => None,
        }
    }

    /// Follows the step as it streams (until the full output was loaded).
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, call)) = self.call(cx) else { return };
        if call.input != self.fields_of {
            self.fields = input_fields(&call.input);
            self.fields_of = call.input.clone();
        }
        if call.kind == ToolKind::Task {
            let done = !call.output.is_empty();
            match &self.result {
                Some(md) => md.update(cx, |md, cx| md.set_text(&call.output, cx)),
                None if done => self.result = Some(cx.new(|cx| TextViewState::markdown(&call.output, cx))),
                None => {}
            }
        }
        if matches!(self.full, Full::None | Full::Failed(_)) && call.output != self.shown {
            self.shown = call.output.clone();
            let text = self.shown.clone();
            self.output.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        cx.notify();
    }

    fn load_full(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, call)) = self.call(cx) else { return };
        let Some(hash) = call.output_blob else { return };
        self.full = Full::Loading;
        let task = self.app.update(cx, |app, cx| app.call(Command::ReadBlob { hash }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Reply::Bytes(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        this.output.update(cx, |state, cx| state.set_value(text, window, cx));
                        this.full = Full::Loaded;
                    }
                    Ok(_) => this.full = Full::Failed("The host sent something unexpected.".into()),
                    Err(message) => this.full = Full::Failed(message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The turn's diff, narrowed to the files this step changed.
    pub fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some((item, call)) = self.call(cx) else { return };
        if call.files.is_empty() || matches!(self.diff, DiffLoad::Loading | DiffLoad::Ready(_)) {
            return;
        }
        self.diff = DiffLoad::Loading;
        let scope = DiffScope::Turn(item.turn);
        let task = self.app.update(cx, |app, cx| app.call(Command::Diff { thread: self.thread, scope }, cx));
        let paths: Vec<String> = call.files.iter().map(|f| f.path.clone()).collect();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.diff = match result {
                    Ok(Reply::Diff(patch)) => {
                        let files: Vec<DiffFile> = DiffFile::parse(&patch)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|f| paths.iter().any(|p| same_file(p, f.path())))
                            .collect();
                        if files.is_empty() {
                            DiffLoad::Empty
                        } else {
                            DiffLoad::Ready(cx.new(|cx| DiffState::new(files, cx)))
                        }
                    }
                    Ok(_) => DiffLoad::Empty,
                    Err(message) => DiffLoad::Failed(message),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn section(title: impl Into<SharedString>, cx: &App) -> Div {
        h_flex()
            .gap_2()
            .pt_2()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(cx.theme().muted_foreground)
            .child(title.into())
    }
}

impl Render for InspectorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_body(cx)
    }
}

impl InspectorView {
    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some((item, call)) = self.call(cx) else {
            return v_flex().p_4().text_sm().text_color(theme.muted_foreground).child("This step is no longer in the thread.").into_any_element();
        };
        let (status, color) = match item.status {
            ItemStatus::Streaming => ("Running", theme.muted_foreground),
            ItemStatus::Failed => ("Failed", theme.danger),
            ItemStatus::Cancelled => ("Stopped", theme.muted_foreground),
            _ => ("Done", theme.success),
        };
        let title = if call.title.is_empty() { call.name.clone() } else { call.title.clone() };
        let task = call.kind == ToolKind::Task;
        let cut = call.output_blob.is_some();
        let mono = theme.mono_font_family.clone();

        let header = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().text_sm().font_family(mono.clone()).child(title.clone()))
                    .child(div().text_xs().text_color(color).child(status))
                    .when_some(call.exit_code, |el, code| {
                        el.child(div().text_xs().text_color(if code == 0 { theme.muted_foreground } else { theme.danger }).child(format!("exit {code}")))
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} · turn {} · {}", call.name, item.turn, format::timestamp(item.id.created_ms(), now_ms()))),
            );

        let input = v_flex()
            .gap_1()
            .child(Self::section(if task { "Prompt" } else { "Input" }, cx))
            .when(self.fields.is_empty(), |el| el.child(div().text_xs().text_color(theme.muted_foreground).child("No input")))
            .children(self.fields.iter().enumerate().map(|(ix, field)| {
                v_flex()
                    .gap_0p5()
                    .when(!field.name.is_empty(), |el| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(field.name.clone()))
                    })
                    .child(
                        div()
                            .id(("field", ix))
                            .p_2()
                            .max_h(rems(16.))
                            .overflow_y_scroll()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_xs()
                            .when(!field.text || call.kind != ToolKind::Task, |el| el.font_family(mono.clone()))
                            .child(field.value.clone()),
                    )
            }));

        let changes = (!call.files.is_empty()).then(|| {
            let view = cx.entity().downgrade();
            let review = self.view.clone();
            v_flex()
                .gap_1()
                .child(
                    Self::section("Changes", cx).child(div().flex_1()).child(
                        Button::new("review-all").ghost().xsmall().icon(Lucide::FileDiff).label("Review all changes").on_click(
                            move |_, _, cx| {
                                let _ = review.update(cx, |_, cx| cx.emit(ThreadViewEvent::Review));
                            },
                        ),
                    ),
                )
                .children(call.files.iter().map(|f| {
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).child(f.path.clone()))
                        .child(div().text_color(theme.success).child(format!("+{}", f.additions)))
                        .child(div().text_color(theme.danger).child(format!("−{}", f.deletions)))
                }))
                .child(match &self.diff {
                    DiffLoad::Idle => h_flex()
                        .child(Button::new("open-diff").small().icon(Lucide::FileDiff).label("Open diff").on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.load_diff(cx));
                        }))
                        .into_any_element(),
                    DiffLoad::Loading => div().text_xs().text_color(theme.muted_foreground).child("Loading the diff…").into_any_element(),
                    DiffLoad::Ready(state) => div()
                        .h(rems(30.))
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(Diff::new(state).size_full())
                        .into_any_element(),
                    DiffLoad::Empty => div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("These files have no changes left in this turn’s diff.")
                        .into_any_element(),
                    DiffLoad::Failed(message) => {
                        div().text_xs().text_color(theme.danger).child(message.clone()).into_any_element()
                    }
                })
        });

        let output = if task {
            v_flex().gap_1().child(Self::section("Result", cx)).child(match &self.result {
                Some(md) => div().text_sm().child(TextView::new(md).selectable(true)).into_any_element(),
                None => div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if item.status == ItemStatus::Streaming { "The subagent is still working." } else { "No result" })
                    .into_any_element(),
            })
        } else {
            let view = cx.entity().downgrade();
            v_flex()
                .gap_1()
                .child(
                    Self::section("Output", cx)
                        .when(!call.output.is_empty(), |el| el.child(format::bytes(call.output.len() as u64)))
                        .child(div().flex_1())
                        .when(cut, |el| match &self.full {
                            Full::None | Full::Failed(_) => el.child(div().font_weight(FontWeight::NORMAL).child("Showing the end")).child(
                                Button::new("load-full").ghost().xsmall().label("Load full output").on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.load_full(window, cx));
                                }),
                            ),
                            Full::Loading => el.child("Loading…"),
                            Full::Loaded => el.child(div().font_weight(FontWeight::NORMAL).child("Full output")),
                        })
                        .when(!call.output.is_empty(), |el| el.child(Clipboard::new("copy-output").value(call.output.clone()))),
                )
                .when_some(
                    match &self.full {
                        Full::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                    |el, message| el.child(div().text_xs().text_color(theme.danger).child(message)),
                )
                .child(if call.output.is_empty() && !matches!(self.full, Full::Loaded) {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if item.status == ItemStatus::Streaming { "No output yet" } else { "No output" })
                        .into_any_element()
                } else {
                    div()
                        .h(rems(26.))
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(Editor::new(&self.output).readonly(true).h_full())
                        .into_any_element()
                })
        };

        v_flex()
            .id("inspector")
            .size_full()
            .overflow_y_scroll()
            .gap_3()
            .pb_4()
            .child(header)
            .child(input)
            .children(changes)
            .child(output)
            .into_any_element()
    }
}

/// Opens the inspector for one step; `diff` opens it on the step's diff.
pub fn open(
    app: Entity<AppState>,
    thread: ThreadId,
    item: ItemId,
    diff: bool,
    view: WeakEntity<ThreadView>,
    window: &mut Window,
    cx: &mut App,
) {
    let inspector = cx.new(|cx| InspectorView::new(app, thread, item, view, window, cx));
    if diff {
        inspector.update(cx, |this, cx| this.load_diff(cx));
    }
    let kind = inspector.read(cx).call(cx).map(|(_, c)| c.kind);
    let title = match kind {
        Some(ToolKind::Task) => "Subagent",
        Some(ToolKind::Command) => "Command",
        Some(ToolKind::Edit) => "File change",
        _ => "Step",
    };
    window.open_sheet_at(Placement::Right, cx, move |sheet, _, _| {
        sheet
            .size(rems(46.))
            .resizable(true)
            .title(h_flex().gap_2().child(Icon::new(IconName::Inspector).small()).child(title))
            .child(inspector.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::{Field, input_fields, same_file};

    #[test]
    fn input_reads_as_fields_with_strings_unescaped() {
        let fields = input_fields(r#"{"file_path":"src/a.rs","content":"fn main() {\n}\n","limit":20}"#);
        assert_eq!(fields[0], Field { name: "file_path".into(), value: "src/a.rs".into(), text: true });
        assert_eq!(fields[1].value, "fn main() {\n}\n");
        assert_eq!(fields[2], Field { name: "limit".into(), value: "20".into(), text: false });
        // Still streaming: not JSON yet, shown as one block.
        let partial = input_fields(r#"{"command":"cargo te"#);
        assert_eq!(partial.len(), 1);
        assert!(partial[0].name.is_empty());
        assert!(input_fields("").is_empty());
    }

    #[test]
    fn tool_paths_match_repository_paths() {
        assert!(same_file("C:\\work\\orbit\\src\\main.rs", "src/main.rs"));
        assert!(same_file("src/main.rs", "src/main.rs"));
        assert!(!same_file("/work/orbit/src/xmain.rs", "main.rs"));
    }
}
