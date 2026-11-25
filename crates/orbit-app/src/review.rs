//! Review: what the agent changed, file by file, hunk by hunk.
//!
//! A sheet at the trailing edge keeps the thread visible beside it. The
//! scope switches between the last turn, the whole thread and uncommitted
//! changes; each file can be reverted, and a hunk with the line selection.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, Placement, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;

pub struct ReviewView {
    app: Entity<AppState>,
    thread: ThreadId,
    scope: usize,
    split: bool,
    diff: Option<Entity<DiffState>>,
    patch: String,
    files: Vec<FileDiff>,
    error: Option<String>,
    loading: bool,
}

const SCOPES: [&str; 3] = ["Last turn", "Whole thread", "Uncommitted"];

impl ReviewView {
    pub fn new(app: Entity<AppState>, thread: ThreadId, cx: &mut Context<Self>) -> Self {
        let mut view =
            Self { app, thread, scope: 0, split: false, diff: None, patch: String::new(), files: Vec::new(), error: None, loading: false };
        view.load(cx);
        view
    }

    fn scope(&self, cx: &App) -> DiffScope {
        let turns = self.app.read(cx).state().threads.get(&self.thread).map(|t| t.turns).unwrap_or(0);
        match self.scope {
            0 => DiffScope::Turn(turns.max(1)),
            1 => DiffScope::Thread,
            _ => DiffScope::Uncommitted,
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        let scope = self.scope(cx);
        let task = self.app.update(cx, |app, cx| app.call(Command::Diff { thread: self.thread, scope }, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(Reply::Diff(patch)) => {
                        this.error = None;
                        this.files = orbit_proto::diff::parse(&patch);
                        let files = DiffFile::parse(&patch).unwrap_or_default();
                        let mode = if this.split { DiffMode::Split } else { DiffMode::Unified };
                        this.diff = Some(cx.new(|cx| DiffState::new(files, cx).with_mode(mode)));
                        this.patch = patch;
                    }
                    Ok(_) => {}
                    Err(message) => {
                        this.error = Some(message);
                        this.diff = None;
                        this.files.clear();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn revert_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.files.iter().find(|f| f.new_path.as_deref() == Some(path) || f.old_path.as_deref() == Some(path)) else {
            return;
        };
        let patch = orbit_proto::file_patch(file);
        let thread = self.thread;
        let task = self.app.update(cx, |app, cx| app.call(Command::RevertPatch { thread, patch }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => window.push_notification(format!("Reverted {}", path_name(&this.files, thread)), cx),
                    Err(message) => window.push_notification(
                        gpui_kit::component::notification::Notification::error(message).title("Couldn’t revert"),
                        cx,
                    ),
                }
                this.load(cx);
            });
        })
        .detach();
    }
}

fn path_name(_: &[FileDiff], _: ThreadId) -> &'static str {
    "the file"
}

impl Render for ReviewView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (adds, dels) = self.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let view = cx.weak_entity();
        v_flex()
            .size_full()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        TabBar::new("review-scope")
                            .segmented()
                            .small()
                            .selected_index(self.scope)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.scope = *ix;
                                this.load(cx);
                            }))
                            .children(SCOPES.iter().map(|s| Tab::new().label(*s))),
                    )
                    .child(div().flex_1())
                    .when(!self.files.is_empty(), |el| {
                        el.child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .child(format!("{} file{}", self.files.len(), if self.files.len() == 1 { "" } else { "s" }))
                                .child(div().text_color(theme.success).child(format!("+{adds}")))
                                .child(div().text_color(theme.danger).child(format!("−{dels}"))),
                        )
                    })
                    .child(
                        Button::new("review-mode")
                            .ghost()
                            .small()
                            .label(if self.split { "Unified" } else { "Split" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.split = !this.split;
                                let mode = if this.split { DiffMode::Split } else { DiffMode::Unified };
                                if let Some(diff) = &this.diff {
                                    diff.update(cx, |d, cx| d.set_mode(mode, cx));
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(if let Some(error) = &self.error {
                crate::ui::empty_state(Icon::new(Lucide::FileDiff), "No changes to show", error.clone(), None, cx).into_any_element()
            } else if self.loading && self.diff.is_none() {
                div().flex_1().into_any_element()
            } else if self.files.is_empty() {
                crate::ui::empty_state(
                    Icon::new(Lucide::FileDiff),
                    "No changes",
                    "This scope didn’t change any files.",
                    None,
                    cx,
                )
                .into_any_element()
            } else if let Some(diff) = &self.diff {
                Diff::new(diff)
                    .flex_1()
                    .min_h_0()
                    .render_header_suffix(move |file, _, _| {
                        let path = file.path().to_string();
                        let view = view.clone();
                        Button::new(SharedString::from(format!("revert-{path}")))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::RotateCcw)
                            .label("Revert file")
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.revert_file(&path, window, cx));
                            })
                    })
                    .into_any_element()
            } else {
                div().into_any_element()
            })
    }
}

pub fn open(app: Entity<AppState>, thread: ThreadId, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ReviewView::new(app, thread, cx));
    window.open_sheet_at(Placement::Right, cx, move |sheet, _, _| {
        sheet.size(rems(52.)).resizable(true).title("Review changes").child(view.clone())
    });
}
