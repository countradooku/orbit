//! The thread's workspace panels: the file tree and viewer on the trailing
//! side, and the dock at the bottom that holds the shell and the project's
//! run targets. The thread view only mounts them; they live here.

use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
use orbit_proto::*;

use super::ThreadView;
use crate::composer::{Composer, ContextChip};
use crate::files::{DraggedPath, FilesDock, FilesEvent, QuickOpen, ToggleFiles};
use crate::project::runs::{RunDefaultTarget, RunTarget, StopTarget};
use crate::project::{RunDock, RunDockEvent};

gpui_kit::actions!(
    thread,
    [
        /// Opens Settings › Projects on the thread's project.
        ProjectSettings,
    ]
);

pub(super) struct Panels {
    pub dock: Entity<RunDock>,
    pub files: Entity<FilesDock>,
    files_split: Entity<ResizableState>,
    /// The split while a file is open beside the tree (wider).
    viewer_split: Entity<ResizableState>,
}

impl Panels {
    pub fn new(app: &Entity<crate::app_state::AppState>, thread: ThreadId, window: &mut Window, cx: &mut Context<ThreadView>) -> Self {
        let dock = cx.new(|cx| RunDock::new(app.clone(), thread, window, cx));
        let files = cx.new(|cx| FilesDock::new(app.clone(), thread, cx));
        Self { dock, files, files_split: cx.new(|_| ResizableState::default()), viewer_split: cx.new(|_| ResizableState::default()) }
    }

    /// Subscriptions that connect the panels to the thread view.
    pub fn subscribe(&self, window: &mut Window, cx: &mut Context<ThreadView>) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(&self.dock, window, |this, _, event: &RunDockEvent, window, cx| match event {
                RunDockEvent::Urls(urls) => this.offer_urls(urls.clone(), cx),
                RunDockEvent::Preview(url) => this.open_preview(url.clone(), window, cx),
                RunDockEvent::CloseShell => {
                    if this.terminal.is_some() {
                        this.toggle_terminal(window, cx);
                    }
                }
            }),
            cx.subscribe_in(&self.files, window, |this, _, event: &FilesEvent, window, cx| match event {
                FilesEvent::Mention(path) => this.mention(path.clone(), window, cx),
            }),
        ]
    }

    /// The conversation (with the bottom dock) and, beside it, the files.
    pub fn wrap(&self, conversation: AnyElement, terminal_split: &Entity<ResizableState>, cx: &mut Context<ThreadView>) -> AnyElement {
        let main = if self.dock.read(cx).visible() {
            v_resizable("thread-split")
                .with_state(terminal_split)
                .child(resizable_panel().child(conversation))
                .child(resizable_panel().size(px(300.)).size_range(px(120.)..px(900.)).child(self.dock.clone()))
                .into_any_element()
        } else {
            conversation
        };
        if !self.files.read(cx).is_open() {
            return main;
        }
        // A tree alone is narrow; with a file open beside it, wider.
        let (id, state, width) = if self.files.read(cx).has_viewer() {
            ("thread-files-viewer-split", &self.viewer_split, px(760.))
        } else {
            ("thread-files-split", &self.files_split, px(260.))
        };
        h_resizable(id)
            .with_state(state)
            .child(resizable_panel().size_range(px(320.)..Pixels::MAX).child(main))
            .child(resizable_panel().size(width).size_range(px(200.)..px(1400.)).child(self.files.clone()))
            .into_any_element()
    }
}

impl ThreadView {
    pub(super) fn toggle_files(&mut self, _: &ToggleFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.files.update(cx, |f, cx| f.toggle(window, cx));
        cx.notify();
    }

    pub(super) fn quick_open(&mut self, _: &QuickOpen, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.files.update(cx, |f, cx| f.quick_open(window, cx));
    }

    pub(super) fn run_target(&mut self, action: &RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.dock.update(cx, |d, cx| d.run(&action.name, window, cx));
    }

    pub(super) fn stop_target(&mut self, action: &StopTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.dock.update(cx, |d, cx| d.stop(&action.name, window, cx));
    }

    pub(super) fn run_default_target(&mut self, _: &RunDefaultTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.dock.update(cx, |d, cx| d.toggle_default(window, cx));
    }

    pub(super) fn project_settings(&mut self, _: &ProjectSettings, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.thread(cx).and_then(|t| t.project);
        crate::project::settings::open(project, window, cx);
    }

    /// Puts `@path` in the composer.
    pub(super) fn mention(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let composer = self.composer.clone();
        composer.update(cx, |composer, cx| composer.add_context(ContextChip::Path(path), window, cx));
        Composer::focus(&composer, window, cx);
    }

    /// A file dropped from the tree onto the composer.
    pub(super) fn drop_path(&mut self, dragged: &DraggedPath, window: &mut Window, cx: &mut Context<Self>) {
        let mut path = dragged.path.clone();
        if dragged.is_dir && !path.ends_with('/') {
            path.push('/');
        }
        self.mention(path, window, cx);
    }

    /// The terminal toggle: also brings back the targets' tabs.
    pub(super) fn sync_dock_shell(&mut self, cx: &mut Context<Self>) {
        let shell = self.terminal.clone();
        self.panels.dock.update(cx, |d, cx| d.set_shell(shell, cx));
    }
}

impl ThreadView {
    /// Debug builds: screenshot steps for the panels (see `run_debug_steps`).
    pub(super) fn panels_debug_step(&mut self, step: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match step.split_once(':') {
            _ if step == "files" => self.toggle_files(&ToggleFiles, window, cx),
            _ if step == "quickopen" => self.quick_open(&QuickOpen, window, cx),
            _ if step == "project" => self.project_settings(&ProjectSettings, window, cx),
            Some(("open", path)) => {
                let (path, line) = match path.rsplit_once(':') {
                    Some((p, l)) if l.chars().all(|c| c.is_ascii_digit()) => (p, l.parse().ok()),
                    _ => (path, None),
                };
                let path = path.to_owned();
                self.panels.files.update(cx, |f, cx| f.open_file(&path, line, window, cx));
            }
            Some(("run", target)) => self.run_target(&RunTarget { name: target.to_owned() }, window, cx),
            _ => return false,
        }
        true
    }
}

/// Shows that a dragged file can be dropped on the composer.
pub(super) fn drop_target(el: Div, cx: &mut Context<ThreadView>) -> Div {
    el.drag_over::<DraggedPath>(|style, _, _, cx| style.bg(cx.theme().drag_border.opacity(0.08)))
        .on_drop(cx.listener(|this, dragged: &DraggedPath, window, cx| this.drop_path(dragged, window, cx)))
}
