//! Files: a thread's worktree as a tree beside the conversation, a viewer
//! for what is in it, quick open, and opening things in the user's tools.
//!
//! * The tree lists folders as they are opened (one ignore-aware listing
//!   each, read by the engine), marks what the thread changed, and follows
//!   the agent: a burst of file changes refreshes the open folders once.
//! * A file opens in the viewer to its left: highlighted code, rendered
//!   Markdown, images and CSV tables (see [`viewer`]).
//! * `mod-p` finds a file by name with the engine's file index
//!   ([`quick_open`]); `mod-shift-e` shows or hides the tree.
//! * Dragging a file onto the composer mentions it; the context menu opens
//!   it in an editor, reveals it, copies its path or mentions it.
//! * "Memory in scope" lists the instruction files (`CLAUDE.md`,
//!   `AGENTS.md`, `.claude/`) that apply to the worktree.

pub mod editor;
mod quick_open;
pub mod viewer;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::tree::{TreeEntry, TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use viewer::{FileViewer, FileViewerEvent};

gpui_kit::actions!(
    files,
    [
        /// Shows or hides the thread's file tree.
        ToggleFiles,
        /// Finds a file of the thread's workspace by name.
        QuickOpen,
    ]
);

/// How long the tree waits after a change before listing again.
const REFRESH_DELAY: Duration = Duration::from_millis(250);

pub fn init(cx: &mut App) {
    editor::init(cx);
    viewer::init(cx);
    cx.bind_keys([
        KeyBinding::new(crate::actions::platform("cmd-shift-e", "ctrl-shift-e"), ToggleFiles, Some("Thread")),
        KeyBinding::new(crate::actions::platform("cmd-p", "ctrl-p"), QuickOpen, Some("Thread")),
    ]);
}

/// A file or folder being dragged out of the tree (onto the composer).
#[derive(Clone)]
pub struct DraggedPath {
    /// Relative to the workspace, `/`-separated.
    pub path: String,
    pub is_dir: bool,
}

struct DragPreview(DraggedPath);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .px_2()
            .py_1()
            .gap_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_xs()
            .shadow_md()
            .child(file_icon(&self.0.path, self.0.is_dir, false).xsmall().text_color(theme.muted_foreground))
            .child(format!("@{}", self.0.path))
    }
}

pub enum FilesEvent {
    /// Mention a path in the thread's composer.
    Mention(String),
}

/// The icon of a file by its kind, or of a folder.
pub fn file_icon(path: &str, is_dir: bool, open: bool) -> Icon {
    if is_dir {
        return Icon::new(if open { IconName::FolderOpen } else { IconName::Folder });
    }
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" | "tif" | "tiff" => Icon::new(Lucide::FileImage),
        "csv" | "tsv" => Icon::new(Lucide::FileSpreadsheet),
        "md" | "mdx" | "txt" | "rst" => Icon::new(IconName::FileText),
        "json" | "toml" | "yaml" | "yml" | "lock" | "ini" | "env" => Icon::new(Lucide::Braces),
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "rb" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "cc" | "cs"
        | "css" | "scss" | "html" | "sh" | "bash" | "sql" | "lua" | "zig" | "php" | "vue" | "svelte" => {
            Icon::new(Lucide::FileCode)
        }
        _ => Icon::new(IconName::File),
    }
}

/// What a thread's changes look like from the tree: each changed file's
/// kind, and every folder holding one.
#[derive(Default)]
struct Marks {
    files: HashMap<String, ChangeKind>,
    folders: HashSet<String>,
}

impl Marks {
    fn of(changes: &[FileChange]) -> Self {
        let mut marks = Marks::default();
        for change in changes {
            let path = change.path.replace('\\', "/");
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                marks.folders.insert(parent.to_owned());
                dir = parent;
            }
            marks.files.insert(path, change.kind);
        }
        marks
    }
}

/// The files panel and its viewer, on the trailing side of a thread.
pub struct FilesDock {
    app: Entity<AppState>,
    thread: ThreadId,
    open: bool,
    tree: Entity<TreeState>,
    /// Listed folders by path (`""` is the root).
    dirs: HashMap<String, Vec<DirEntry>>,
    expanded: HashSet<String>,
    loading: HashSet<String>,
    error: Option<String>,
    memory: Option<Vec<MemoryFile>>,
    memory_open: bool,
    viewer: Option<Entity<FileViewer>>,
    split: Entity<ResizableState>,
    /// Files opened lately, newest first (quick open's empty query).
    recent: Vec<String>,
    refresh: Task<()>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FilesEvent> for FilesDock {}

impl FilesDock {
    pub fn new(app: Entity<AppState>, thread: ThreadId, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let subscriptions = vec![
            cx.subscribe(&tree, |this, _, event: &TreeEvent, cx| match event {
                TreeEvent::Expanded(id) => {
                    if let Some(path) = id.strip_prefix("d:") {
                        this.expanded.insert(path.to_owned());
                        if !this.dirs.contains_key(path) {
                            this.load(path.to_owned(), cx);
                        }
                    }
                }
                TreeEvent::Collapsed(id) => {
                    if let Some(path) = id.strip_prefix("d:") {
                        this.expanded.remove(path);
                    }
                }
            }),
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| match event {
                AppEvent::Workspace(thread) if *thread == this.thread => this.schedule_refresh(cx),
                AppEvent::Git(WorkspaceRoot::Thread(thread)) if *thread == this.thread => this.schedule_refresh(cx),
                _ => {}
            }),
        ];
        Self {
            app,
            thread,
            open: false,
            tree,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            loading: HashSet::new(),
            error: None,
            memory: None,
            memory_open: true,
            viewer: None,
            split: cx.new(|_| ResizableState::default()),
            recent: Vec::new(),
            refresh: Task::ready(()),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn has_viewer(&self) -> bool {
        self.viewer.is_some()
    }

    /// Shows or hides the tree (and the viewer with it).
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_open(!self.open, window, cx);
    }

    pub fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            if !self.dirs.contains_key("") {
                self.load(String::new(), cx);
            }
            if self.memory.is_none() {
                self.load_memory(cx);
            }
            let tree = self.tree.clone();
            window.defer(cx, move |window, cx| tree.update(cx, |t, cx| t.focus(window, cx)));
        }
        cx.notify();
    }

    /// The thread's workspace folder on this computer.
    fn root_path(&self, cx: &App) -> Option<PathBuf> {
        let state = self.app.read(cx).state();
        let thread = state.threads.get(&self.thread)?;
        let path = thread
            .workspace
            .worktree
            .clone()
            .or_else(|| thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.path.clone()))?;
        Some(PathBuf::from(path))
    }

    fn absolute(&self, relative: &str, cx: &App) -> Option<PathBuf> {
        let root = self.root_path(cx)?;
        Some(if relative.is_empty() { root } else { root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)) })
    }

    fn load(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.loading.insert(path.clone()) {
            return;
        }
        let command = Command::Workspace(WorkspaceCommand::ListDir { root: WorkspaceRoot::Thread(self.thread), path: path.clone() });
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading.remove(&path);
                match result {
                    Ok(Reply::Workspace(WorkspaceReply::Dir(entries))) => {
                        if path.is_empty() {
                            this.error = None;
                        }
                        this.dirs.insert(path, entries);
                    }
                    Ok(_) => {}
                    Err(message) => {
                        if path.is_empty() {
                            this.error = Some(message);
                        } else {
                            // A folder that went away closes.
                            this.dirs.remove(&path);
                            this.expanded.remove(&path);
                        }
                    }
                }
                this.rebuild(cx);
            });
        })
        .detach();
    }

    fn load_memory(&mut self, cx: &mut Context<Self>) {
        let command = Command::Workspace(WorkspaceCommand::MemoryFiles { root: WorkspaceRoot::Thread(self.thread) });
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.memory = Some(match result {
                    Ok(Reply::Workspace(WorkspaceReply::Memory(files))) => files,
                    _ => Vec::new(),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Lists the open folders again shortly (changes come in bursts).
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            // Listed again when the panel next opens.
            self.dirs.clear();
            self.memory = None;
            return;
        }
        self.refresh = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                // Folders not open any more are listed when reopened.
                let keep: Vec<String> =
                    this.dirs.keys().filter(|p| p.is_empty() || this.expanded.contains(*p)).cloned().collect();
                this.dirs.retain(|p, _| keep.contains(p));
                for path in keep {
                    this.load(path, cx);
                }
                cx.notify();
            });
        });
    }

    /// Rebuilds the tree's items from the listings, keeping what is open
    /// and selected.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let items = self.items_of("");
        let selected = self.tree.read(cx).selected_item().map(|i| i.id.clone());
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            if let Some(id) = selected
                && let Some(ix) = tree.index_of(&id)
            {
                tree.set_selected_index(Some(ix), cx);
            }
        });
        cx.notify();
    }

    fn items_of(&self, dir: &str) -> Vec<TreeItem> {
        let Some(entries) = self.dirs.get(dir) else { return Vec::new() };
        entries
            .iter()
            .map(|entry| {
                let path = if dir.is_empty() { entry.name.clone() } else { format!("{dir}/{}", entry.name) };
                if !entry.is_dir {
                    return TreeItem::new(format!("f:{path}"), entry.name.clone());
                }
                let children = if self.dirs.contains_key(&path) {
                    let children = self.items_of(&path);
                    if children.is_empty() {
                        vec![TreeItem::new(format!("x:{path}"), "Empty").disabled(true)]
                    } else {
                        children
                    }
                } else {
                    // Listed when opened; the placeholder makes it a folder.
                    vec![TreeItem::new(format!("x:{path}"), "Loading…").disabled(true)]
                };
                TreeItem::new(format!("d:{path}"), entry.name.clone()).expanded(self.expanded.contains(&path)).children(children)
            })
            .collect()
    }

    /// Opens a file of the workspace in the viewer, at `line` (1-based).
    pub fn open_file(&mut self, path: &str, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        if !self.dirs.contains_key("") {
            self.load(String::new(), cx);
        }
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_owned());
        self.recent.truncate(20);
        match &self.viewer {
            Some(viewer) if viewer.read(cx).path() == path => {
                viewer.update(cx, |v, cx| v.go_to_line(line, window, cx));
            }
            _ => {
                let viewer = cx.new(|cx| FileViewer::new(self.app.clone(), self.thread, path.to_owned(), line, window, cx));
                self._subscriptions.push(cx.subscribe(&viewer, |this, _, event: &FileViewerEvent, cx| match event {
                    FileViewerEvent::Close => {
                        this.viewer = None;
                        cx.notify();
                    }
                    FileViewerEvent::Mention(path) => cx.emit(FilesEvent::Mention(path.clone())),
                }));
                self.viewer = Some(viewer);
            }
        }
        // Show where it is in the tree.
        let id: SharedString = format!("f:{path}").into();
        self.reveal_in_tree(path, cx);
        self.tree.update(cx, |tree, cx| {
            if let Some(ix) = tree.index_of(&id) {
                tree.set_selected_index(Some(ix), cx);
                tree.scroll_to_item(ix, ScrollStrategy::Center);
            }
        });
        cx.notify();
    }

    /// Opens (lists) every folder above `path`.
    fn reveal_in_tree(&mut self, path: &str, cx: &mut Context<Self>) {
        let mut dir = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            dir = if dir.is_empty() { (*part).to_owned() } else { format!("{dir}/{part}") };
            self.expanded.insert(dir.clone());
            if !self.dirs.contains_key(&dir) {
                self.load(dir.clone(), cx);
            }
        }
        self.rebuild(cx);
    }

    pub fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dock = cx.entity();
        quick_open::open(self.app.clone(), self.thread, self.recent.clone(), dock, window, cx);
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.tree.read(cx).selected_item().map(|i| i.id.clone()) else { return false };
        let Some(path) = id.strip_prefix("f:") else { return false };
        let path = path.to_owned();
        self.open_file(&path, None, window, cx);
        true
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild(cx);
    }

    /// Opens an instruction file: in the viewer when it is in the
    /// workspace, in the editor otherwise.
    fn open_memory(&mut self, file: &MemoryFile, window: &mut Window, cx: &mut Context<Self>) {
        let inside = matches!(file.scope, MemoryScope::Root | MemoryScope::Nested);
        match (inside, file.is_dir) {
            (true, true) => {
                self.expanded.insert(file.label.clone());
                if !self.dirs.contains_key(&file.label) {
                    self.load(file.label.clone(), cx);
                }
                self.rebuild(cx);
            }
            (true, false) => self.open_file(&file.label, None, window, cx),
            (false, _) => editor::open_file(Path::new(&file.absolute), None, cx),
        }
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if let Some(error) = &self.error {
            return div().p_3().text_xs().text_color(theme.danger).child(error.clone()).into_any_element();
        }
        if !self.dirs.contains_key("") {
            return div().p_3().text_xs().text_color(theme.muted_foreground).child("Reading the folder…").into_any_element();
        }
        if self.dirs.get("").is_some_and(Vec::is_empty) {
            return div().p_3().text_xs().text_color(theme.muted_foreground).child("This folder is empty.").into_any_element();
        }
        let changes = self.app.read(cx).state().workspace.get(&self.thread).map(|c| Marks::of(c)).unwrap_or_default();
        let marks = Rc::new(changes);
        let dock = cx.weak_entity();
        let menu_dock = dock.clone();
        tree(&self.tree, move |ix, entry, _, _, cx| render_row(ix, entry, &marks, &dock, cx))
            .context_menu(move |_, entry, menu, _, cx| {
                let id = entry.item().id.clone();
                let (is_dir, path) = match (id.strip_prefix("d:"), id.strip_prefix("f:")) {
                    (Some(path), _) => (true, path.to_owned()),
                    (_, Some(path)) => (false, path.to_owned()),
                    _ => return menu,
                };
                let Some(dock) = menu_dock.upgrade() else { return menu };
                let absolute = dock.read(cx).absolute(&path, cx);
                build_menu(menu_dock.clone(), path, is_dir, absolute, menu, cx)
            })
            .flex_1()
            .into_any_element()
    }

    fn render_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let files = self.memory.clone().unwrap_or_default();
        let open = self.memory_open;
        v_flex()
            .flex_none()
            .max_h(relative(0.4))
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("memory-header")
                    .h_8()
                    .flex_none()
                    .px_3()
                    .gap_1p5()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .cursor_default()
                    .hover(|s| s.text_color(theme.foreground))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.memory_open = !this.memory_open;
                        cx.notify();
                    }))
                    .child(Icon::new(IconName::BookOpen).xsmall())
                    .child(div().flex_1().child("Memory in scope"))
                    .child(if self.memory.is_none() { "…".to_owned() } else { files.len().to_string() })
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall()),
            )
            .when(open, |el| {
                el.child(
                    v_flex()
                        .id("memory-list")
                        .overflow_y_scroll()
                        .pb_1()
                        .when(files.is_empty() && self.memory.is_some(), |el| {
                            el.child(
                                div()
                                    .px_3()
                                    .pb_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("No CLAUDE.md, AGENTS.md or .claude folder applies here."),
                            )
                        })
                        .children(files.into_iter().enumerate().map(|(ix, file)| {
                            let scope = match file.scope {
                                MemoryScope::Root => "This folder",
                                MemoryScope::Nested => "Subfolder",
                                MemoryScope::Parent => "Folder above",
                                MemoryScope::User => "Yours",
                            };
                            let icon = match file.kind {
                                MemoryKind::ClaudeFolder => Icon::new(IconName::Settings2),
                                _ => Icon::new(IconName::FileText),
                            };
                            let tip = file.absolute.clone();
                            let label = file_label(&file);
                            h_flex()
                                .id(("memory", ix))
                                .h_7()
                                .mx_1()
                                .px_2()
                                .gap_1p5()
                                .rounded(theme.radius)
                                .text_xs()
                                .cursor_default()
                                .hover(|s| s.bg(theme.muted))
                                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                                .on_click(cx.listener(move |this, _, window, cx| this.open_memory(&file, window, cx)))
                                .child(icon.xsmall().text_color(theme.muted_foreground))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .child(div().flex_none().text_color(theme.muted_foreground).child(scope))
                        })),
                )
            })
            .into_any_element()
    }
}

fn file_label(file: &MemoryFile) -> String {
    if file.is_dir { format!("{}/", file.label) } else { file.label.clone() }
}

/// One row of the tree: chevron, icon, name and the change mark.
fn render_row(ix: usize, entry: &TreeEntry, marks: &Marks, dock: &WeakEntity<FilesDock>, cx: &App) -> ListItem {
    let theme = cx.theme();
    let item = entry.item();
    let depth = entry.depth() as f32;
    let row = ListItem::new(ix).pl(rems(0.375 + 0.875 * depth)).pr_2().py_0p5();
    if item.id.starts_with("x:") {
        return row.child(
            div().pl(rems(1.25)).text_xs().italic().text_color(theme.muted_foreground).child(item.label.clone()),
        );
    }
    let is_dir = item.id.starts_with("d:");
    let path = item.id[2..].to_owned();
    let open = entry.is_expanded();
    let (letter, color) = if is_dir {
        (None, None)
    } else {
        match marks.files.get(&path) {
            Some(ChangeKind::Added) => (Some("A"), Some(theme.success)),
            Some(ChangeKind::Modified) => (Some("M"), Some(theme.warning)),
            Some(ChangeKind::Deleted) => (Some("D"), Some(theme.danger)),
            Some(ChangeKind::Renamed) => (Some("R"), Some(theme.info)),
            None => (None, None),
        }
    };
    let folder_changed = is_dir && marks.folders.contains(&path);
    let drag = DraggedPath { path: path.clone(), is_dir };
    let row = if is_dir {
        row
    } else {
        let (dock, path) = (dock.clone(), path.clone());
        row.on_click(move |_, window, cx| {
            let _ = dock.update(cx, |d, cx| d.open_file(&path, None, window, cx));
        })
    };
    row.child(
        h_flex()
            .id(SharedString::from(format!("row:{path}")))
            .w_full()
            .min_w_0()
            .gap_1()
            .text_xs()
            .on_drag(drag, |drag: &DraggedPath, _, _, cx| cx.new(|_| DragPreview(drag.clone())))
            .child(
                div().flex_none().w(rems(0.875)).child(if is_dir {
                    Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                        .xsmall()
                        .text_color(theme.muted_foreground)
                        .into_any_element()
                } else {
                    div().into_any_element()
                }),
            )
            .child(file_icon(&path, is_dir, open).xsmall().flex_none().text_color(theme.muted_foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when_some(color, |el, color| el.text_color(color))
                    .child(item.label.clone()),
            )
            .when(folder_changed, |el| el.child(div().flex_none().size_1p5().rounded_full().bg(theme.warning.opacity(0.8))))
            .when_some(letter.zip(color), |el, (letter, color)| {
                el.child(div().flex_none().w(rems(0.75)).text_right().font_weight(FontWeight::MEDIUM).text_color(color).child(letter))
            }),
    )
}

/// The tree's context menu for one file or folder.
fn build_menu(
    dock: WeakEntity<FilesDock>,
    path: String,
    is_dir: bool,
    absolute: Option<PathBuf>,
    mut menu: gpui_kit::component::menu::PopupMenu,
    cx: &App,
) -> gpui_kit::component::menu::PopupMenu {
    if !is_dir {
        let (dock, open_path) = (dock.clone(), path.clone());
        menu = menu.item(PopupMenuItem::new("Open").icon(IconName::FileText).on_click(move |_, window, cx| {
            let _ = dock.update(cx, |d, cx| d.open_file(&open_path, None, window, cx));
        }));
    }
    if let Some(absolute) = absolute.clone() {
        let target = absolute.clone();
        menu = menu.item(PopupMenuItem::new(editor::open_label(cx)).icon(IconName::ExternalLink).on_click(move |_, _, cx| {
            if is_dir {
                editor::open_folder(&target, cx);
            } else {
                editor::open_file(&target, None, cx);
            }
        }));
        let reveal = absolute.clone();
        menu = menu.item(
            PopupMenuItem::new(format!("Reveal in {}", editor::file_manager()))
                .icon(IconName::FolderOpen)
                .on_click(move |_, _, cx| editor::reveal(&reveal, cx)),
        );
        if is_dir {
            let folder = absolute.clone();
            menu = menu.item(
                PopupMenuItem::new("Open in terminal").icon(Lucide::Terminal).on_click(move |_, _, cx| editor::open_terminal(&folder, cx)),
            );
        }
    }
    let mention = path.clone();
    let mention_dock = dock.clone();
    menu = menu.separator().item(PopupMenuItem::new("Mention in composer").icon(Lucide::AtSign).on_click(move |_, _, cx| {
        let mention = mention.clone();
        let _ = mention_dock.update(cx, |_, cx| cx.emit(FilesEvent::Mention(mention)));
    }));
    if let Some(absolute) = absolute {
        let text = absolute.display().to_string();
        menu = menu.item(
            PopupMenuItem::new("Copy path")
                .icon(IconName::Copy)
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))),
        );
    }
    menu.item(
        PopupMenuItem::new("Copy relative path").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))),
    )
}

impl Focusable for FilesDock {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesDock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let root = self.root_path(cx);
        let root_name = root
            .as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Files".to_owned());
        let reveal = root.clone();
        let panel = v_flex()
            .size_full()
            .bg(theme.background)
            .border_l_1()
            .border_color(theme.border)
            .key_context("FilesPanel")
            .track_focus(&self.focus)
            // Enter on a file opens it; on a folder the tree opens or closes it.
            .capture_action(cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                if this.open_selected(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                h_flex()
                    .h_8()
                    .flex_none()
                    .pl_3()
                    .pr_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(Lucide::FolderTree).xsmall().text_color(theme.muted_foreground))
                    .child(div().flex_1().min_w_0().truncate().text_xs().font_weight(FontWeight::MEDIUM).child(root_name))
                    .child(
                        Button::new("files-quick-open")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Search)
                            .tooltip_with_action("Open file…", &QuickOpen, Some("Thread"))
                            .on_click(cx.listener(|this, _, window, cx| this.quick_open(window, cx))),
                    )
                    .child(
                        Button::new("files-collapse")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ListCollapse)
                            .tooltip("Collapse all folders")
                            .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
                    )
                    .when_some(reveal, |el, root| {
                        el.child(
                            Button::new("files-reveal")
                                .ghost()
                                .xsmall()
                                .icon(IconName::FolderOpen)
                                .tooltip(format!("Reveal in {}", editor::file_manager()))
                                .on_click(move |_, _, cx| editor::reveal(&root, cx)),
                        )
                    })
                    .child(
                        Button::new("files-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip_with_action("Hide files", &ToggleFiles, Some("Thread"))
                            .on_click(cx.listener(|this, _, window, cx| this.set_open(false, window, cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().py_1().child(self.render_tree(cx)))
            .child(self.render_memory(cx));
        match self.viewer.clone() {
            None => div().size_full().child(panel).into_any_element(),
            Some(viewer) => h_resizable("files-dock")
                .with_state(&self.split)
                .child(resizable_panel().size_range(px(280.)..Pixels::MAX).child(div().size_full().border_l_1().border_color(theme.border).child(viewer)))
                .child(resizable_panel().size(px(240.)).size_range(px(180.)..px(520.)).child(panel))
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Marks;
    use orbit_proto::{ChangeKind, FileChange};

    #[test]
    fn marks_cover_files_and_their_folders() {
        let marks = Marks::of(&[
            FileChange { path: "src/app/main.rs".into(), kind: ChangeKind::Modified, additions: 1, deletions: 0 },
            FileChange { path: "README.md".into(), kind: ChangeKind::Added, additions: 1, deletions: 0 },
        ]);
        assert_eq!(marks.files.get("src/app/main.rs"), Some(&ChangeKind::Modified));
        assert!(marks.folders.contains("src") && marks.folders.contains("src/app"));
        assert!(!marks.folders.contains(""));
    }
}
