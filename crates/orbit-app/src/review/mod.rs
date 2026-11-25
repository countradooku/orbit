//! Review: what the agent changed, file by file and hunk by hunk.
//!
//! A sheet at the trailing edge keeps the thread visible beside it. One
//! request fetches the whole patch for a scope (a turn, every turn since one,
//! the whole thread, uncommitted work, or the branch against any base), and
//! it is parsed off the UI thread. Files are listed in a filterable tree that
//! drives the diff; every hunk carries a marker to mark it reviewed, revert
//! it or comment on it; reverts can be undone; comments go back to the agent
//! as one follow-up message with file, lines and the quoted change. Scopes
//! that reach the working tree refresh live while the agent works.

mod dialogs;
mod model;
mod perf;
mod state;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffDecoration, DiffGutterMarker, DiffHoverHighlight, DiffInlineUnit, DiffLinePosition,
    DiffLineRange, DiffMode, DiffSide, DiffSpanHighlight, DiffState,
};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::tree::{TreeEvent, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Placement, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::diff::Side;
use orbit_proto::*;

#[allow(unused_imports)] // for the thread view's turn and message actions
pub use dialogs::{confirm_restore, edit_message};
use model::{Comment, ReviewData};
use state::{ReviewMarks, prefs, update_prefs};

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;

gpui_kit::actions!(
    review,
    [NextFile, PreviousFile, NextChange, PreviousChange, ToggleViewed, ToggleWrap, ToggleFileTree, SendComments, Refresh]
);

const CONTEXT: &str = "Review";
/// Live refreshes are at most this frequent while files keep changing.
const LIVE_INTERVAL: Duration = Duration::from_millis(400);
/// A hovered line this recent is where the reader is; refreshes keep it.
const HOVER_ANCHOR: Duration = Duration::from_secs(20);

fn bind_keys(cx: &mut App) {
    struct Bound;
    impl Global for Bound {}
    if cx.has_global::<Bound>() {
        return;
    }
    cx.set_global(Bound);
    cx.bind_keys([
        KeyBinding::new("alt-down", NextFile, Some(CONTEXT)),
        KeyBinding::new("alt-up", PreviousFile, Some(CONTEXT)),
        KeyBinding::new("f7", NextChange, Some(CONTEXT)),
        KeyBinding::new("shift-f7", PreviousChange, Some(CONTEXT)),
        KeyBinding::new("alt-v", ToggleViewed, Some(CONTEXT)),
        KeyBinding::new("alt-z", ToggleWrap, Some(CONTEXT)),
        KeyBinding::new("alt-b", ToggleFileTree, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-enter", SendComments, Some(CONTEXT)),
        KeyBinding::new("f5", Refresh, Some(CONTEXT)),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("cmd-shift-enter", SendComments, Some(CONTEXT))]);
}

/// Which part of the thread's work the sheet shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Turn,
    Thread,
    Uncommitted,
    Branch,
}

const SCOPES: [(Scope, &str); 4] =
    [(Scope::Turn, "Turn"), (Scope::Thread, "All turns"), (Scope::Uncommitted, "Uncommitted"), (Scope::Branch, "Branch")];

type BranchSelect = SelectState<SearchableVec<SharedString>>;

/// A comment being written or edited, shown as an annotation with an editor.
struct Draft {
    comment: Comment,
    input: Entity<TextareaState>,
    /// The saved comment this edits, restored on cancel.
    original: Option<Comment>,
    _sub: Subscription,
}

pub struct ReviewView {
    app: Entity<AppState>,
    thread: ThreadId,
    focus: FocusHandle,

    scope: Scope,
    /// `None` follows the latest turn.
    turn: Option<u32>,
    /// With [`Scope::Turn`]: the turn and everything after it.
    through_now: bool,
    base: Option<SharedString>,
    branches: Option<Entity<BranchSelect>>,

    diff: Entity<DiffState>,
    data: Rc<ReviewData>,
    error: Option<String>,
    loading: bool,
    reload: bool,
    generation: u64,
    last_load: Option<Instant>,
    /// The next files shown are a new scope, not a refresh of the same one.
    fresh: bool,
    /// Turn count and busy state last seen, to follow a finishing turn.
    seen: (u32, bool),

    tree: Entity<TreeState>,
    filter: Entity<InputState>,
    filter_text: String,
    collapsed: HashSet<SharedString>,
    split: Entity<ResizableState>,
    /// Indexes into `data.files` shown after filtering, in order.
    visible: Vec<usize>,
    hide_tests: bool,
    selected: Option<SharedString>,
    hovered: Rc<RefCell<Option<(DiffLinePosition, Instant)>>>,

    reviewed: Rc<HashSet<u64>>,
    /// Hunks reverted from this sheet: hunk id → the patch that undoes it.
    reverted: Rc<HashMap<u64, String>>,
    comments: Rc<Vec<Comment>>,
    drafts: Vec<Draft>,
    next_comment: u64,
    palette: Option<Rc<Palette>>,
    perf: Option<perf::PerfLog>,
    load_started: Option<Instant>,
    _subs: Vec<Subscription>,
}

/// The color-blind palette's paint for the current files.
struct Palette {
    key: (u64, usize, bool),
    decorations: Vec<DiffDecoration>,
    spans: Vec<DiffSpanHighlight>,
}

impl ReviewView {
    pub fn new(app: Entity<AppState>, thread: ThreadId, scope: Option<DiffScope>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let p = prefs(cx);
        let split = crate::settings::get(cx).review.split;
        let diff = cx.new(|cx| {
            DiffState::new(Vec::new(), cx)
                .with_mode(if split { DiffMode::Split } else { DiffMode::Unified })
                .with_inline_unit((!p.colorblind).then_some(DiffInlineUnit::Word))
        });
        let tree = cx.new(|cx| TreeState::new(cx));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files").clean_on_escape());
        let panes = cx.new(|_| ResizableState::default());
        let mut subs = vec![
            cx.subscribe(&filter, |this, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.filter_text = input.read(cx).value().to_string();
                    this.apply_filter(cx);
                }
            }),
            cx.observe(&tree, |this, tree, cx| {
                let path = tree.read(cx).selected_entry().and_then(|e| model::item_path(&e.item().id).map(SharedString::from));
                if let Some(path) = path
                    && this.selected.as_ref() != Some(&path)
                {
                    this.selected = Some(path.clone());
                    this.diff.update(cx, |d, cx| d.scroll_to_file(&path, cx));
                    cx.notify();
                }
            }),
            cx.subscribe(&tree, |this, _, ev: &TreeEvent, _| match ev {
                TreeEvent::Collapsed(id) => {
                    this.collapsed.insert(id.clone());
                }
                TreeEvent::Expanded(id) => {
                    this.collapsed.remove(id);
                }
            }),
            cx.subscribe(&app, |this, _, ev: &AppEvent, cx| {
                if let AppEvent::Workspace(thread) = ev
                    && *thread == this.thread
                    && this.follows_working_tree(cx)
                {
                    this.refresh_live(cx);
                }
            }),
        ];
        subs.push(cx.observe(&app, |this, _, cx| this.thread_changed(cx)));
        // Settings → Review can change while the sheet is open.
        subs.push(cx.observe_global::<crate::settings::SettingsModel>(|this, cx| {
            let mode = if crate::settings::get(cx).review.split { DiffMode::Split } else { DiffMode::Unified };
            this.diff.update(cx, |d, cx| d.set_mode(mode, cx));
            cx.notify();
        }));

        let (reviewed, comments) =
            ReviewMarks::read(cx, thread, |m| (Rc::new(m.reviewed.clone()), Rc::new(m.comments.clone())));
        let next_comment = comments.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        let t = app.read(cx).state().threads.get(&thread).cloned();
        let turns = t.as_ref().map(|t| t.turns).unwrap_or(0);
        let busy = t.as_ref().is_some_and(|t| t.status.is_busy());
        let mut view = Self {
            app,
            thread,
            focus: cx.focus_handle(),
            scope: if turns > 0 { Scope::Turn } else { Scope::Uncommitted },
            turn: None,
            through_now: false,
            base: None,
            branches: None,
            diff,
            data: Rc::new(ReviewData::empty()),
            error: None,
            loading: false,
            reload: false,
            generation: 0,
            last_load: None,
            fresh: true,
            seen: (turns, busy),
            tree,
            filter,
            filter_text: String::new(),
            collapsed: HashSet::new(),
            split: panes,
            visible: Vec::new(),
            hide_tests: p.hide_tests,
            selected: None,
            hovered: Rc::default(),
            reviewed,
            reverted: Rc::default(),
            comments,
            drafts: Vec::new(),
            next_comment,
            palette: None,
            perf: perf::PerfLog::from_env(),
            load_started: None,
            _subs: subs,
        };
        match scope {
            Some(DiffScope::Turn(n)) => (view.scope, view.turn) = (Scope::Turn, Some(n)),
            Some(DiffScope::SinceTurn(n)) => (view.scope, view.turn, view.through_now) = (Scope::Turn, Some(n), true),
            Some(DiffScope::Thread) => view.scope = Scope::Thread,
            Some(DiffScope::Uncommitted) => view.scope = Scope::Uncommitted,
            Some(DiffScope::Base(base)) => (view.scope, view.base) = (Scope::Branch, Some(base.into())),
            None => {}
        }
        if view.scope == Scope::Branch {
            view.load_branches(window, cx);
        }
        view.load(cx);
        view
    }

    // ── Thread facts ─────────────────────────────────────────────────────

    fn thread_record(&self, cx: &App) -> Option<Thread> {
        self.app.read(cx).state().threads.get(&self.thread).cloned()
    }

    fn turns(&self, cx: &App) -> u32 {
        self.app.read(cx).state().threads.get(&self.thread).map(|t| t.turns).unwrap_or(0)
    }

    fn turn_number(&self, cx: &App) -> u32 {
        self.turn.unwrap_or_else(|| self.turns(cx)).max(1)
    }

    fn project(&self, cx: &App) -> Option<Project> {
        let state = self.app.read(cx).state();
        let project = state.threads.get(&self.thread)?.project?;
        state.projects.iter().find(|p| p.id == project).cloned()
    }

    /// Where the thread's files are on disk.
    fn root(&self, cx: &App) -> Option<std::path::PathBuf> {
        let thread = self.thread_record(cx)?;
        if let Some(worktree) = thread.workspace.worktree {
            return Some(worktree.into());
        }
        self.project(cx).map(|p| p.path.into())
    }

    fn diff_scope(&self, cx: &App) -> Option<DiffScope> {
        Some(match self.scope {
            Scope::Turn if self.through_now => DiffScope::SinceTurn(self.turn_number(cx)),
            Scope::Turn => DiffScope::Turn(self.turn_number(cx)),
            Scope::Thread => DiffScope::Thread,
            Scope::Uncommitted => DiffScope::Uncommitted,
            Scope::Branch => DiffScope::Base(self.base.clone()?.to_string()),
        })
    }

    /// The scope reads the working tree, so it changes as files do.
    fn follows_working_tree(&self, cx: &App) -> bool {
        match self.scope {
            Scope::Turn if !self.through_now => {
                let running = self.thread_record(cx).is_some_and(|t| t.status.is_busy());
                running && self.turn_number(cx) == self.turns(cx)
            }
            _ => true,
        }
    }

    fn thread_changed(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.thread_record(cx) else { return };
        let now = (t.turns, t.status.is_busy());
        if now == self.seen {
            return;
        }
        let was = std::mem::replace(&mut self.seen, now);
        // Following the latest turn: a new turn, or the end of one (its
        // snapshot lands a moment later), shows the latest state.
        if self.scope == Scope::Turn && self.turn.is_none() || self.scope == Scope::Thread || was.0 != now.0 {
            self.load(cx);
        }
        cx.notify();
    }

    // ── Loading ──────────────────────────────────────────────────────────

    fn load(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.reload = true;
            return;
        }
        let Some(scope) = self.diff_scope(cx) else {
            self.error = Some("Pick a branch to compare with.".into());
            cx.notify();
            return;
        };
        if matches!(scope, DiffScope::Turn(_) | DiffScope::SinceTurn(_)) && self.turns(cx) == 0 {
            self.error = Some("Nothing has run in this thread yet.".into());
            self.set_data(Rc::new(ReviewData::empty()), cx);
            return;
        }
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        self.last_load = Some(Instant::now());
        self.load_started = Some(Instant::now());
        let task = self.app.update(cx, |app, cx| app.call(Command::Diff { thread: self.thread, scope }, cx));
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(Reply::Diff(patch)) => Ok(cx.background_spawn(async move { ReviewData::parse(&patch) }).await),
                Ok(_) => Err("The engine sent an unexpected reply.".to_owned()),
                Err(message) => Err(message),
            };
            let _ = this.update(cx, |this, cx| this.loaded(generation, result, cx));
        })
        .detach();
        cx.notify();
    }

    fn loaded(&mut self, generation: u64, result: Result<ReviewData, String>, cx: &mut Context<Self>) {
        self.loading = false;
        let fetched = self.load_started.take().map(|t| t.elapsed());
        if generation == self.generation {
            match result {
                Ok(data) => {
                    self.error = None;
                    if data.fingerprint != self.data.fingerprint || self.data.is_empty() {
                        let (files, lines) =
                            (data.len(), data.files.iter().map(|f| f.additions + f.deletions).sum::<u32>());
                        let shown = Instant::now();
                        self.set_data(Rc::new(data), cx);
                        if let (Some(perf), Some(fetched)) = (&self.perf, fetched) {
                            perf.write(&format!(
                                "load files={files} changed_lines={lines} fetch+parse={:.1}ms show={:.1}ms",
                                fetched.as_secs_f32() * 1000.,
                                shown.elapsed().as_secs_f32() * 1000.
                            ));
                        }
                    }
                }
                Err(message) => {
                    self.error = Some(message);
                    self.set_data(Rc::new(ReviewData::empty()), cx);
                }
            }
        }
        if std::mem::take(&mut self.reload) {
            self.load(cx);
        }
        cx.notify();
    }

    /// A working-tree change: reload, at most every [`LIVE_INTERVAL`].
    fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let wait = self.last_load.map(|t| LIVE_INTERVAL.saturating_sub(t.elapsed())).unwrap_or_default();
        if wait.is_zero() {
            self.load(cx);
            return;
        }
        if self.reload {
            return;
        }
        self.reload = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                if std::mem::take(&mut this.reload) {
                    this.load(cx);
                }
            });
        })
        .detach();
    }

    fn set_data(&mut self, data: Rc<ReviewData>, cx: &mut Context<Self>) {
        // Keep the reader where they were: the line under the pointer, or
        // the selected file.
        let anchor = self.hovered.borrow().as_ref().filter(|(_, at)| at.elapsed() < HOVER_ANCHOR).map(|(p, _)| p.clone());
        self.data = data;
        self.palette = None;
        self.compute_visible();
        self.rebuild_tree(cx);
        let views: Vec<_> = self.visible.iter().map(|&ix| self.data.views[ix].clone()).collect();
        let selected = self.selected.clone();
        self.diff.update(cx, |d, cx| {
            d.set_files(views, cx);
            match (&anchor, &selected) {
                (Some(pos), _) if d.has_line(pos) => d.scroll_to_line(pos.clone(), cx),
                (_, Some(path)) => d.scroll_to_file(path, cx),
                _ => {}
            }
        });
        // Reviewed files start collapsed, as on a code host; with the
        // "collapse files" setting, every file of a new scope does.
        let collapse_all = std::mem::take(&mut self.fresh) && crate::settings::get(cx).review.collapse_files;
        let collapsed: Vec<SharedString> = self
            .visible
            .iter()
            .filter(|&&ix| collapse_all || self.data.file_reviewed(ix, &self.reviewed))
            .map(|&ix| self.data.path(ix))
            .collect();
        if !collapsed.is_empty() {
            self.diff.update(cx, |d, cx| {
                for path in &collapsed {
                    d.set_file_collapsed(path, true, cx);
                }
            });
        }
        cx.notify();
    }

    fn compute_visible(&mut self) {
        let hide_tests = self.hide_tests;
        let query = self.filter_text.trim().to_owned();
        self.visible = (0..self.data.len())
            .filter(|&ix| {
                let path = self.data.files[ix].path();
                (query.is_empty() || model::matches_filter(path, &query)) && !(hide_tests && model::is_test_file(path))
            })
            .collect();
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let items = model::tree_items(self.visible.iter().map(|&ix| (ix, self.data.files[ix].path())), &self.collapsed);
        let selected = self.selected.clone();
        self.tree.update(cx, |t, cx| {
            t.set_items(items, cx);
            if let Some(path) = selected
                && let Some(ix) = t.index_of(&model::file_item_id(&path))
            {
                t.set_selected_index(Some(ix), cx);
            }
        });
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        self.compute_visible();
        self.rebuild_tree(cx);
        self.palette = None;
        let views: Vec<_> = self.visible.iter().map(|&ix| self.data.views[ix].clone()).collect();
        self.diff.update(cx, |d, cx| d.set_files(views, cx));
        cx.notify();
    }

    fn load_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(cx) else {
            self.error = Some("This thread isn’t in a project, so there is no branch to compare with.".into());
            return;
        };
        let task = self.app.update(cx, |app, cx| app.call(Command::Branches { project: project.id }, cx));
        let preferred = self.base.clone().or_else(|| project.base_branch.clone().map(Into::into));
        // Comparing the thread's own branch with itself shows nothing.
        let own: Option<SharedString> = self.thread_record(cx).and_then(|t| t.workspace.branch).map(Into::into);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Reply::Branches { all, .. }) = task.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                let mut names: Vec<SharedString> = all.into_iter().map(SharedString::from).filter(|b| Some(b) != own.as_ref()).collect();
                // Local branches first, then remote ones.
                names.sort_by_key(|b| (b.contains('/'), b.to_lowercase()));
                let pick = preferred
                    .filter(|b| names.contains(b))
                    .or_else(|| names.iter().find(|b| ["main", "master", "trunk", "develop"].contains(&b.as_ref())).cloned())
                    .or_else(|| names.first().cloned());
                let ix = pick.as_ref().and_then(|p| names.iter().position(|b| b == p));
                let select = cx.new(|cx| {
                    SelectState::new(SearchableVec::new(names), ix.map(|row| IndexPath::default().row(row)), window, cx)
                        .searchable(true)
                });
                this._subs.push(cx.subscribe(&select, |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, cx| {
                    let SelectEvent::Confirm(value) = ev;
                    if let Some(value) = value
                        && this.base.as_ref() != Some(value)
                    {
                        this.base = Some(value.clone());
                        this.fresh = true;
                        this.load(cx);
                    }
                }));
                this.branches = Some(select);
                if this.base.is_none() {
                    this.base = pick;
                }
                if this.scope == Scope::Branch {
                    this.load(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.fresh = true;
        self.error = None;
        if scope == Scope::Branch && self.branches.is_none() {
            self.load_branches(window, cx);
        }
        self.load(cx);
        cx.notify();
    }

    // ── Review marks ─────────────────────────────────────────────────────

    fn set_reviewed(&mut self, ids: Vec<u64>, reviewed: bool, cx: &mut Context<Self>) {
        let mut set = (*self.reviewed).clone();
        for id in &ids {
            if reviewed {
                set.insert(*id);
            } else {
                set.remove(id);
            }
        }
        ReviewMarks::update(cx, self.thread, |m| m.reviewed = set.clone());
        self.reviewed = Rc::new(set);
        cx.notify();
    }

    fn toggle_hunk_reviewed(&mut self, hunk: u64, cx: &mut Context<Self>) {
        let reviewed = !self.reviewed.contains(&hunk);
        self.set_reviewed(vec![hunk], reviewed, cx);
        // Finishing a file's last hunk collapses it.
        if let Some(&(file, _)) = self.data.hunks.get(&hunk)
            && reviewed
            && self.data.file_reviewed(file, &self.reviewed)
        {
            let path = self.data.path(file);
            self.diff.update(cx, |d, cx| d.set_file_collapsed(&path, true, cx));
        }
    }

    fn set_file_reviewed(&mut self, file: usize, reviewed: bool, cx: &mut Context<Self>) {
        self.set_reviewed(self.data.file_marks(file), reviewed, cx);
        let path = self.data.path(file);
        self.diff.update(cx, |d, cx| d.set_file_collapsed(&path, reviewed, cx));
    }

    /// The change as text for writing a commit message or pull request:
    /// the thread's title, then the patch, cut at a size a model reads well.
    fn change_context(&self, cx: &App) -> String {
        const BUDGET: usize = 48 * 1024;
        let mut out = String::new();
        if let Some(t) = self.thread_record(cx) {
            out.push_str(&format!("Task: {}\n\n", crate::thread::display_title(&t)));
        }
        let mut left = Vec::new();
        for f in &self.data.files {
            if out.len() + f.raw.len() <= BUDGET {
                out.push_str(&f.raw);
            } else {
                left.push(f.path());
            }
        }
        if !left.is_empty() {
            out.push_str(&format!("\n…and {} more changed files: {}\n", left.len(), left.join(", ")));
        }
        out
    }

    fn reviewed_files(&self) -> usize {
        self.visible.iter().filter(|&&ix| self.data.file_reviewed(ix, &self.reviewed)).count()
    }

    // ── Revert ───────────────────────────────────────────────────────────

    fn revert_hunk(&mut self, hunk: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&(file, ix)) = self.data.hunks.get(&hunk) else { return };
        let patch = orbit_proto::hunk_patch(&self.data.files[file], ix);
        let path = self.data.files[file].path().to_owned();
        self.revert(patch, Some(hunk), format!("Reverted a change in {}", short_name(&path)), window, cx);
    }

    fn revert_file(&mut self, file: usize, window: &mut Window, cx: &mut Context<Self>) {
        let f = &self.data.files[file];
        let patch = orbit_proto::file_patch(f);
        let label = format!("Reverted {}", short_name(f.path()));
        self.revert(patch, None, label, window, cx);
    }

    /// Reverse-applies `patch` to the workspace and offers to undo it.
    fn revert(&mut self, patch: String, hunk: Option<u64>, done: String, window: &mut Window, cx: &mut Context<Self>) {
        let undo = orbit_proto::diff::invert_patch(&patch);
        let thread = self.thread;
        let task = self.app.update(cx, |app, cx| app.call(Command::RevertPatch { thread, patch }, cx));
        let app = self.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        if let (Some(hunk), Some(undo)) = (hunk, undo.clone()) {
                            let mut reverted = (*this.reverted).clone();
                            reverted.insert(hunk, undo);
                            this.reverted = Rc::new(reverted);
                        }
                        let mut note = Notification::success(done.clone());
                        if let Some(undo) = undo {
                            let (app, view) = (app.clone(), cx.weak_entity());
                            note = note.action(move |_, _, _| {
                                let (app, view, undo) = (app.clone(), view.clone(), undo.clone());
                                Button::new("undo-revert").small().label("Undo").on_click(move |_, window, cx| {
                                    undo_revert(&app, thread, undo.clone(), hunk, view.clone(), window, cx);
                                })
                            });
                        }
                        window.push_notification(note, cx);
                    }
                    Err(message) => {
                        window.push_notification(Notification::error(message).title("Couldn’t revert"), cx);
                    }
                }
                this.load(cx);
            });
        })
        .detach();
    }

    // ── Comments ─────────────────────────────────────────────────────────

    fn start_comment(&mut self, range: Option<&DiffLineRange>, path: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.data.index_of(&path) else { return };
        let side = |s: DiffSide| if s == DiffSide::Original { Side::Old } else { Side::New };
        let (start, end) = match range {
            Some(r) => {
                let mut start = (side(r.side()), r.start() as u32);
                let mut end = (side(r.end_side()), r.end() as u32);
                if start.0 == end.0 && start.1 > end.1 {
                    std::mem::swap(&mut start, &mut end);
                }
                (Some(start), Some(end))
            }
            None => (None, None),
        };
        let excerpt = match (start, end) {
            (Some(s), Some(e)) => orbit_proto::diff::excerpt(&self.data.files[file], s, e),
            _ => String::new(),
        };
        let id = self.next_comment;
        self.next_comment += 1;
        let comment = Comment { id, path: path.to_string(), start, end, excerpt, text: String::new() };
        self.open_draft(comment, None, window, cx);
        self.diff.update(cx, |d, cx| d.set_selected_lines(None, cx));
    }

    fn open_draft(&mut self, comment: Comment, original: Option<Comment>, window: &mut Window, cx: &mut Context<Self>) {
        let id = comment.id;
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .placeholder("What should change? Ctrl+Enter saves")
                .default_value(comment.text.clone())
        });
        let sub = cx.subscribe_in(&input, window, move |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: true, .. } = ev {
                this.save_draft(id, window, cx);
            }
        });
        let handle = input.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        self.drafts.push(Draft { comment, input, original, _sub: sub });
        cx.notify();
    }

    fn save_draft(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.drafts.iter().position(|d| d.comment.id == id) else { return };
        let text = self.drafts[ix].input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let draft = self.drafts.remove(ix);
        let mut comments = (*self.comments).clone();
        comments.push(Comment { text, ..draft.comment });
        comments.sort_by_key(|c| c.id);
        self.store_comments(comments, cx);
        self.focus.focus(window, cx);
    }

    fn cancel_draft(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(ix) = self.drafts.iter().position(|d| d.comment.id == id) else { return };
        let draft = self.drafts.remove(ix);
        if let Some(original) = draft.original {
            let mut comments = (*self.comments).clone();
            comments.push(original);
            comments.sort_by_key(|c| c.id);
            self.store_comments(comments, cx);
        }
        cx.notify();
    }

    fn edit_comment(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(comment) = self.comments.iter().find(|c| c.id == id).cloned() else { return };
        let comments: Vec<Comment> = self.comments.iter().filter(|c| c.id != id).cloned().collect();
        self.store_comments(comments, cx);
        self.open_draft(comment.clone(), Some(comment), window, cx);
    }

    fn delete_comment(&mut self, id: u64, cx: &mut Context<Self>) {
        let comments: Vec<Comment> = self.comments.iter().filter(|c| c.id != id).cloned().collect();
        self.store_comments(comments, cx);
    }

    fn store_comments(&mut self, comments: Vec<Comment>, cx: &mut Context<Self>) {
        ReviewMarks::update(cx, self.thread, |m| m.comments = comments.clone());
        self.comments = Rc::new(comments);
        cx.notify();
    }

    /// Sends every comment (and any draft with text) to the agent as one
    /// follow-up message. A busy thread queues it for after its turn.
    fn send_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.drafts.iter().map(|d| d.comment.id).collect();
        for id in ids {
            self.save_draft(id, window, cx);
        }
        if self.comments.is_empty() {
            return;
        }
        let text = model::comments_message(&self.comments);
        let count = self.comments.len();
        let thread = self.thread;
        let busy = self.thread_record(cx).is_some_and(|t| t.status.is_busy());
        let task = self.app.update(cx, |app, cx| app.call(Command::Send { thread, text, attachments: Vec::new(), steer: false }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(_) => {
                    this.store_comments(Vec::new(), cx);
                    let what = if count == 1 { "1 comment".to_owned() } else { format!("{count} comments") };
                    let message = if busy { format!("{what} queued for the agent") } else { format!("Sent {what} to the agent") };
                    window.push_notification(Notification::success(message), cx);
                }
                Err(message) => window.push_notification(Notification::error(message).title("Couldn’t send the comments"), cx),
            });
        })
        .detach();
    }

    // ── Navigation ───────────────────────────────────────────────────────

    fn select_file(&mut self, path: SharedString, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        let id = model::file_item_id(&path);
        self.tree.update(cx, |t, cx| t.reveal_item(&id, ScrollStrategy::Center, cx));
        self.tree.update(cx, |t, cx| {
            if let Some(ix) = t.index_of(&id) {
                t.set_selected_index(Some(ix), cx);
            }
        });
        self.diff.update(cx, |d, cx| d.scroll_to_file(&path, cx));
        cx.notify();
    }

    fn step_file(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|p| self.data.index_of(p)).and_then(|ix| self.visible.iter().position(|&v| v == ix));
        let n = self.visible.len();
        let next = match (current, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        let path = self.data.path(self.visible[next]);
        self.select_file(path, cx);
    }

    fn on_next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(true, cx);
    }

    fn on_previous_file(&mut self, _: &PreviousFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(false, cx);
    }

    fn on_next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |d, cx| d.next_change(cx));
    }

    fn on_previous_change(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |d, cx| d.previous_change(cx));
    }

    fn on_toggle_viewed(&mut self, _: &ToggleViewed, _: &mut Window, cx: &mut Context<Self>) {
        let file = self.selected.as_ref().and_then(|p| self.data.index_of(p)).or_else(|| self.visible.first().copied());
        if let Some(file) = file {
            let reviewed = !self.data.file_reviewed(file, &self.reviewed);
            self.set_file_reviewed(file, reviewed, cx);
            if reviewed {
                self.step_file(true, cx);
            }
        }
    }

    fn on_toggle_wrap(&mut self, _: &ToggleWrap, _: &mut Window, cx: &mut Context<Self>) {
        update_prefs(cx, |p| p.wrap = !p.wrap);
        cx.notify();
    }

    fn on_toggle_tree(&mut self, _: &ToggleFileTree, _: &mut Window, cx: &mut Context<Self>) {
        update_prefs(cx, |p| p.tree = !p.tree);
        cx.notify();
    }

    fn on_send(&mut self, _: &SendComments, window: &mut Window, cx: &mut Context<Self>) {
        self.send_comments(window, cx);
    }

    fn on_refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// Side by side is a saved setting: changing it here changes it for
    /// every review.
    fn set_split(&mut self, split: bool, cx: &mut Context<Self>) {
        crate::settings::update(cx, |s| s.review.split = split);
        let mode = if split { DiffMode::Split } else { DiffMode::Unified };
        self.diff.update(cx, |d, cx| d.set_mode(mode, cx));
        cx.notify();
    }

    fn set_colorblind(&mut self, on: bool, cx: &mut Context<Self>) {
        update_prefs(cx, |p| p.colorblind = on);
        self.palette = None;
        self.diff.update(cx, |d, cx| d.set_inline_unit((!on).then_some(DiffInlineUnit::Word), cx));
        cx.notify();
    }

    fn set_hide_tests(&mut self, hide: bool, cx: &mut Context<Self>) {
        update_prefs(cx, |p| p.hide_tests = hide);
        self.hide_tests = hide;
        self.apply_filter(cx);
    }
}

fn short_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn undo_revert(
    app: &Entity<AppState>,
    thread: ThreadId,
    patch: String,
    hunk: Option<u64>,
    view: WeakEntity<ReviewView>,
    window: &mut Window,
    cx: &mut App,
) {
    let task = app.update(cx, |app, cx| app.call(Command::RevertPatch { thread, patch }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                if let Err(message) = result {
                    window.push_notification(Notification::error(message).title("Couldn’t undo the revert"), cx);
                }
                let _ = view.update(cx, |view, cx| {
                    if let Some(hunk) = hunk {
                        let mut reverted = (*view.reverted).clone();
                        reverted.remove(&hunk);
                        view.reverted = Rc::new(reverted);
                    }
                    view.load(cx);
                });
            });
        })
        .detach();
}

fn side_of(side: Side) -> DiffSide {
    match side {
        Side::Old => DiffSide::Original,
        Side::New => DiffSide::Modified,
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────

impl ReviewView {
    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let turns = self.turns(cx);
        let scope_ix = SCOPES.iter().position(|(s, _)| *s == self.scope).unwrap_or(0);
        let has_project = self.project(cx).is_some();
        let turn_picker = (self.scope == Scope::Turn && turns > 0).then(|| self.render_turn_picker(turns, cx));
        let branch_picker = (self.scope == Scope::Branch).then(|| match &self.branches {
            Some(select) => Select::new(select)
                .small()
                .w(px(180.))
                .flex_shrink_0()
                .menu_width(px(280.))
                .search_placeholder("Find a branch")
                .placeholder("Compare with…")
                .into_any_element(),
            None => Spinner::new().small().into_any_element(),
        });
        h_flex()
            .gap_2()
            .child(
                TabBar::new("review-scope")
                    .segmented()
                    .small()
                    .selected_index(scope_ix)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| this.set_scope(SCOPES[*ix].0, window, cx)))
                    .children(SCOPES.iter().map(|(_, label)| Tab::new().label(*label))),
            )
            .children(turn_picker)
            .children(branch_picker)
            .child(div().flex_1().min_w_0())
            .when(has_project, |el| el.child(self.render_actions(cx)))
    }

    fn render_turn_picker(&self, turns: u32, cx: &mut Context<Self>) -> AnyElement {
        let current = self.turn_number(cx);
        let labels = self.turn_labels(turns, cx);
        let view = cx.weak_entity();
        let following = self.turn.is_none();
        let through_now = self.through_now;
        let label = if through_now { format!("Turn {current} and later") } else { format!("Turn {current}") };
        Button::new("review-turn")
            .small()
            .outline()
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.scrollable(true).max_h(px(360.)).min_w(px(280.));
                let v = view.clone();
                menu = menu.item(PopupMenuItem::new("Latest turn").checked(following).on_click(move |_, _, cx| {
                    let _ = v.update(cx, |this, cx| {
                        this.turn = None;
                        this.fresh = true;
                        this.load(cx);
                    });
                }));
                menu = menu.separator();
                for (n, text) in labels.iter().rev() {
                    let (n, v) = (*n, view.clone());
                    menu = menu.item(PopupMenuItem::new(text.clone()).checked(!following && n == current).on_click(move |_, _, cx| {
                        let _ = v.update(cx, |this, cx| {
                            this.turn = Some(n);
                            this.fresh = true;
                            this.load(cx);
                        });
                    }));
                }
                let v = view.clone();
                menu.separator().item(
                    PopupMenuItem::new("Include later turns").checked(through_now).on_click(move |_, _, cx| {
                        let _ = v.update(cx, |this, cx| {
                            this.through_now = !this.through_now;
                            this.load(cx);
                        });
                    }),
                )
            })
            .into_any_element()
    }

    /// "Turn 3 · Fix the parser" for each turn, from the loaded timeline.
    fn turn_labels(&self, turns: u32, cx: &App) -> Vec<(u32, SharedString)> {
        let mut prompts: HashMap<u32, String> = HashMap::new();
        if let Some(timeline) = self.app.read(cx).state().timeline(self.thread) {
            for item in timeline.items() {
                if let ItemBody::User(message) = &item.body
                    && !message.steer
                {
                    prompts.entry(item.turn).or_insert_with(|| message.text.lines().next().unwrap_or_default().to_owned());
                }
            }
        }
        (1..=turns)
            .map(|n| {
                let label = match prompts.get(&n).map(|p| p.trim()).filter(|p| !p.is_empty()) {
                    Some(p) => {
                        let cut: String = p.chars().take(48).collect();
                        let ellipsis = if cut.len() < p.len() { "…" } else { "" };
                        format!("Turn {n} · {cut}{ellipsis}")
                    }
                    None => format!("Turn {n}"),
                };
                (n, label.into())
            })
            .collect()
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.thread_record(cx);
        let project = self.project(cx);
        let base = project.as_ref().and_then(|p| p.base_branch.clone());
        let branch = thread.as_ref().and_then(|t| t.workspace.branch.clone());
        let mergeable = thread.as_ref().is_some_and(|t| t.workspace.worktree.is_some())
            || matches!((&branch, &base), (Some(b), Some(base)) if b != base);
        let turn = (self.scope == Scope::Turn && self.turns(cx) > 0).then(|| self.turn_number(cx));
        let restored = ReviewMarks::read(cx, self.thread, |m| m.restored);
        let (app, id) = (self.app.clone(), self.thread);
        let view = cx.weak_entity();
        h_flex()
            .gap_1()
            .child(
                Button::new("commit")
                    .small()
                    .icon(Lucide::GitCommitHorizontal)
                    .label("Commit…")
                    .on_click(cx.listener(|this, _, window, cx| dialogs::commit(this.app.clone(), this.thread, this.change_context(cx), cx.weak_entity(), window, cx))),
            )
            .child(
                Button::new("pull-request")
                    .small()
                    .outline()
                    .icon(Lucide::GitPullRequest)
                    .label("Pull request…")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let files = this.data.files.iter().map(|f| f.path().to_owned()).collect();
                        dialogs::create_pr(this.app.clone(), this.thread, files, this.change_context(cx), window, cx)
                    })),
            )
            .when(mergeable, |el| {
                el.child(
                    Button::new("merge")
                        .small()
                        .outline()
                        .icon(Lucide::GitMerge)
                        .label(match &base {
                            Some(base) => format!("Merge into {base}…"),
                            None => "Merge…".to_owned(),
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            dialogs::merge(this.app.clone(), this.thread, cx.weak_entity(), window, cx);
                        })),
                )
            })
            .child(Button::new("review-more").small().ghost().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, _| {
                let mut menu = menu.min_w(px(260.));
                if let Some(turn) = turn {
                    let app = app.clone();
                    menu = menu.item(PopupMenuItem::new(format!("Restore to before turn {turn}…")).icon(Lucide::RotateCcw).on_click(
                        move |_, window, cx| confirm_restore(app.clone(), id, turn, window, cx),
                    ));
                }
                if let Some(turn) = restored {
                    let (app, view) = (app.clone(), view.clone());
                    menu = menu.item(PopupMenuItem::new(format!("Undo restore to turn {turn}")).icon(IconName::Undo2).on_click(
                        move |_, window, cx| {
                            dialogs::undo_restore(app.clone(), id, window, cx);
                            let _ = view.update(cx, |_, cx| cx.notify());
                        },
                    ));
                }
                if turn.is_none() && restored.is_none() {
                    menu = menu.label("Pick a turn to restore to it");
                }
                menu
            }))
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let p = prefs(cx);
        let split = crate::settings::get(cx).review.split;
        let total = self.visible.len();
        let reviewed = self.reviewed_files();
        let (adds, dels) = self.visible.iter().fold((0, 0), |(a, d), &ix| (a + self.data.files[ix].additions, d + self.data.files[ix].deletions));
        let (added_color, removed_color) = if p.colorblind { (theme.info, theme.warning) } else { (theme.success, theme.danger) };
        let live = self.follows_working_tree(cx) && self.error.is_none();
        let view = cx.weak_entity();
        let comparing = match (self.scope, &self.base) {
            (Scope::Branch, Some(base)) => {
                let head = self.thread_record(cx).and_then(|t| t.workspace.branch).unwrap_or_else(|| "this checkout".into());
                Some(format!("Comparing {head} with {base}"))
            }
            _ => None,
        };
        h_flex()
            .gap_3()
            .text_xs()
            .text_color(theme.muted_foreground)
            .when(total > 0, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .child(format!("{total} file{}", if total == 1 { "" } else { "s" }))
                        .child(div().text_color(added_color).child(format!("+{adds}")))
                        .child(div().text_color(removed_color).child(format!("−{dels}"))),
                )
                .child(
                    h_flex()
                        .id("review-progress")
                        .gap_1p5()
                        .child(
                            gpui_kit::component::progress::Progress::new("reviewed-progress")
                                .w(px(64.))
                                .value(if total == 0 { 0. } else { reviewed as f32 * 100. / total as f32 }),
                        )
                        .child(format!("{reviewed} of {total} reviewed"))
                        .tooltip(|window, cx| Tooltip::new("Mark a file viewed with its checkbox or Alt+V").build(window, cx)),
                )
            })
            .children(comparing)
            .when(live, |el| {
                el.child(
                    h_flex()
                        .id("review-live")
                        .gap_1()
                        .child(div().size(px(6.)).rounded_full().bg(theme.success))
                        .child("Live")
                        .tooltip(|window, cx| Tooltip::new("Updates as the files change").build(window, cx)),
                )
            })
            .when(self.loading, |el| el.child(Spinner::new().xsmall()))
            .child(div().flex_1())
            .child(
                Button::new("review-view")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Settings2)
                    .label("View")
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        let item = |label: &'static str, checked: bool, f: fn(&mut ReviewView, &mut Context<ReviewView>)| {
                            let v = view.clone();
                            PopupMenuItem::new(label).checked(checked).on_click(move |_, _, cx| {
                                let _ = v.update(cx, f);
                            })
                        };
                        menu.min_w(px(240.))
                            .item(item("Side by side", split, |this, cx| this.set_split(!crate::settings::get(cx).review.split, cx)))
                            .item(item("Wrap lines", p.wrap, |this, cx| {
                                update_prefs(cx, |p| p.wrap = !p.wrap);
                                cx.notify();
                                let _ = this;
                            }))
                            .item(item("Colour-blind palette", p.colorblind, |this, cx| this.set_colorblind(!prefs(cx).colorblind, cx)))
                            .separator()
                            .item(item("File tree", p.tree, |_, cx| {
                                update_prefs(cx, |p| p.tree = !p.tree);
                                cx.notify();
                            }))
                            .item(item("Hide test files", p.hide_tests, |this, cx| this.set_hide_tests(!prefs(cx).hide_tests, cx)))
                            .separator()
                            .item(item("Collapse all files", false, |this, cx| this.diff.update(cx, |d, cx| d.set_all_files_collapsed(true, cx))))
                            .item(item("Expand all files", false, |this, cx| this.diff.update(cx, |d, cx| d.set_all_files_collapsed(false, cx))))
                    }),
            )
            .child(
                Button::new("review-refresh")
                    .xsmall()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh (F5)")
                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
            )
    }

    /// Files this thread changed that other live worktrees changed too.
    fn conflicts(&self, cx: &App) -> HashMap<String, Vec<String>> {
        let state = self.app.read(cx).state();
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for conflict in state.conflicts.iter().filter(|c| c.threads.contains(&self.thread)) {
            let others = conflict
                .threads
                .iter()
                .filter(|t| **t != self.thread)
                .map(|t| state.threads.get(t).map(|t| crate::thread::display_title(t).to_owned()).unwrap_or_else(|| "another thread".into()))
                .collect();
            out.insert(conflict.path.clone(), others);
        }
        out
    }

    fn render_conflicts(&self, conflicts: &HashMap<String, Vec<String>>, cx: &App) -> Option<AnyElement> {
        if conflicts.is_empty() {
            return None;
        }
        let mut paths: Vec<&String> = conflicts.keys().collect();
        paths.sort();
        let mut threads: Vec<&str> = conflicts.values().flatten().map(String::as_str).collect();
        threads.sort();
        threads.dedup();
        let shown: Vec<&str> = paths.iter().take(3).map(|p| short_name(p)).collect();
        let more = paths.len().saturating_sub(shown.len());
        let files = if more > 0 { format!("{} and {more} more", shown.join(", ")) } else { shown.join(", ") };
        let message = format!(
            "{files} {} also changed in {}. Merging both may conflict.",
            if paths.len() == 1 { "is" } else { "are" },
            threads.join(", ")
        );
        let _ = cx;
        Some(
            gpui_kit::component::alert::Alert::warning("review-conflicts", message)
                .title("Overlapping work")
                .small()
                .into_any_element(),
        )
    }

    fn render_tree_panel(&self, conflicts: Rc<HashMap<String, Vec<String>>>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let data = self.data.clone();
        let reviewed = self.reviewed.clone();
        let p = prefs(cx);
        let (added_color, removed_color) = if p.colorblind { (theme.info, theme.warning) } else { (theme.success, theme.danger) };
        let view = cx.weak_entity();
        let empty = self.visible.is_empty() && !data.is_empty();
        v_flex()
            .size_full()
            .gap_1()
            .pr_1()
            .child(Input::new(&self.filter).small().cleanable(true).prefix(Icon::new(IconName::Search).xsmall().text_color(theme.muted_foreground)))
            .child(if empty {
                div().p_3().text_xs().text_color(theme.muted_foreground).child("No files match the filter.").into_any_element()
            } else {
                tree(&self.tree, move |ix, entry, _selected, _, cx| {
                    let theme = cx.theme();
                    let item = entry.item();
                    let depth = entry.depth();
                    let row = ListItem::new(ix).pl(px(8.) + px(12.) * depth as f32).pr_1().py_0p5();
                    if entry.is_folder() {
                        let icon = if entry.is_expanded() { IconName::FolderOpen } else { IconName::Folder };
                        return row.child(
                            h_flex()
                                .gap_1p5()
                                .text_xs()
                                .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
                                .child(div().truncate().child(item.label.clone())),
                        );
                    }
                    let path = model::item_path(&item.id).unwrap_or_default();
                    let Some(file) = data.index_of(path) else { return row.child(item.label.clone()) };
                    let f = &data.files[file];
                    let done = data.file_reviewed(file, &reviewed);
                    let conflict = conflicts.contains_key(path);
                    let letter_color = match f.kind {
                        orbit_proto::FileDiffKind::Added | orbit_proto::FileDiffKind::Copied => added_color,
                        orbit_proto::FileDiffKind::Deleted => removed_color,
                        _ => theme.muted_foreground,
                    };
                    let v = view.clone();
                    let path_owned = SharedString::from(path.to_owned());
                    row.on_click(move |_, _, cx| {
                        let path = path_owned.clone();
                        let _ = v.update(cx, |this, cx| {
                            this.selected = Some(path.clone());
                            this.diff.update(cx, |d, cx| d.scroll_to_file(&path, cx));
                        });
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .text_xs()
                            .child(div().w(px(10.)).flex_shrink_0().text_color(letter_color).child(model::kind_letter(f.kind)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(done, |d| d.text_color(theme.muted_foreground))
                                    .when(f.kind == orbit_proto::FileDiffKind::Deleted, |d| d.line_through())
                                    .child(item.label.clone()),
                            )
                            .when(conflict, |el| el.child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.warning)))
                            .when(done, |el| el.child(Icon::new(IconName::Check).xsmall().text_color(theme.muted_foreground)))
                            .when(!done && (f.additions > 0 || f.deletions > 0), |el| {
                                el.child(
                                    h_flex()
                                        .flex_shrink_0()
                                        .gap_0p5()
                                        .when(f.additions > 0, |el| el.child(div().text_color(added_color).child(format!("+{}", f.additions))))
                                        .when(f.deletions > 0, |el| el.child(div().text_color(removed_color).child(format!("−{}", f.deletions)))),
                                )
                            }),
                    )
                })
                .flex_1()
                .into_any_element()
            })
            .into_any_element()
    }

    fn palette(&mut self, cx: &App) -> Rc<Palette> {
        let theme = cx.theme();
        let key = (self.data.fingerprint, self.visible.len(), theme.mode.is_dark());
        if let Some(p) = &self.palette
            && p.key == key
        {
            return p.clone();
        }
        let (added, removed) = (theme.info, theme.warning);
        let mut decorations = Vec::new();
        let mut spans = Vec::new();
        for &ix in &self.visible {
            let path = self.data.path(ix);
            for (side, start, end) in self.data.change_runs(ix) {
                let color = if side == Side::New { added.opacity(0.16) } else { removed.opacity(0.13) };
                decorations.push(DiffDecoration::new(DiffLineRange::new(path.clone(), side_of(side), start as usize, end as usize)).with_background(color));
            }
            for (side, line, range) in self.data.word_spans(ix) {
                let color = if side == Side::New { added.opacity(0.4) } else { removed.opacity(0.35) };
                spans.push(
                    DiffSpanHighlight::new(DiffLinePosition::new(path.clone(), side_of(side), line as usize), range).with_background(color),
                );
            }
        }
        let palette = Rc::new(Palette { key, decorations, spans });
        self.palette = Some(palette.clone());
        palette
    }

    fn render_diff(&mut self, conflicts: Rc<HashMap<String, Vec<String>>>, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx);
        let data = self.data.clone();
        let view = cx.weak_entity();

        // One marker per hunk, at its first changed line.
        let mut markers = Vec::new();
        for &file in &self.visible {
            let path = data.path(file);
            for (h, &hid) in data.hunk_ids[file].iter().enumerate() {
                if let Some((side, line)) = data.hunk_anchor(file, h) {
                    markers.push(
                        DiffGutterMarker::new(("hunk", hid), DiffLinePosition::new(path.clone(), side_of(side), line as usize))
                            .with_label(format!("Change {} of {}", h + 1, data.hunk_ids[file].len())),
                    );
                }
            }
        }

        // Comments: saved ones, then drafts.
        let mut annotations = Vec::new();
        let anchor = |c: &Comment| -> DiffAnnotation {
            let id = ElementId::from(("comment", c.id));
            match c.end {
                Some((side, line)) => DiffAnnotation::line(id, DiffLinePosition::new(c.path.clone(), side_of(side), line as usize)),
                None => DiffAnnotation::file(id, c.path.clone()),
            }
        };
        annotations.extend(self.comments.iter().map(anchor));
        annotations.extend(self.drafts.iter().map(|d| anchor(&d.comment)));
        let comments = self.comments.clone();
        let drafts: Rc<HashMap<u64, (Comment, Entity<TextareaState>)>> =
            Rc::new(self.drafts.iter().map(|d| (d.comment.id, (d.comment.clone(), d.input.clone()))).collect());

        let reviewed = self.reviewed.clone();
        let reverted = self.reverted.clone();
        let palette = p.colorblind.then(|| self.palette(cx));

        let diff = Diff::new(&self.diff)
            .size_full()
            .soft_wrap(p.wrap)
            .hover_highlight(DiffHoverHighlight::Both)
            .change_background(!p.colorblind)
            .when_some(palette, |d, palette| d.decorations(palette.decorations.iter().cloned()).span_highlights(palette.spans.iter().cloned()))
            .gutter_markers(markers)
            .render_gutter_marker({
                let (view, reviewed, reverted, data) = (view.clone(), reviewed.clone(), reverted.clone(), data.clone());
                move |marker, _, cx| render_hunk_marker(marker, &data, &reviewed, &reverted, view.clone(), cx)
            })
            .annotations(annotations)
            .render_annotation({
                let view = view.clone();
                move |annotation, _, cx| {
                    let ElementId::NamedInteger(_, id) = annotation.id() else { return div().into_any_element() };
                    render_comment(*id, &comments, &drafts, view.clone(), cx)
                }
            })
            .on_add_annotation({
                let view = view.clone();
                move |range, window, cx| {
                    let path = range.path().clone();
                    let range = range.clone();
                    let _ = view.update(cx, |this, cx| this.start_comment(Some(&range), path, window, cx));
                }
            })
            .on_line_hover({
                let hovered = self.hovered.clone();
                move |position, _, _| {
                    if let Some(position) = position {
                        *hovered.borrow_mut() = Some((position.clone(), Instant::now()));
                    }
                }
            })
            .on_line_click({
                let view = view.clone();
                move |position, _, _, cx| {
                    let path = position.path().clone();
                    let _ = view.update(cx, |this, cx| {
                        if this.selected.as_ref() != Some(&path) {
                            this.selected = Some(path.clone());
                            let id = model::file_item_id(&path);
                            this.tree.update(cx, |t, cx| {
                                if let Some(ix) = t.index_of(&id) {
                                    t.set_selected_index(Some(ix), cx);
                                    t.scroll_to_item(ix, ScrollStrategy::Center);
                                }
                            });
                            cx.notify();
                        }
                    });
                }
            })
            .render_header_title_suffix({
                let conflicts = conflicts.clone();
                move |file, _, cx| {
                    let others = conflicts.get(file.path().as_ref());
                    h_flex().when_some(others.cloned(), |el, others| {
                        let tip = format!("Also changed in {}", others.join(", "));
                        el.child(
                            div()
                                .id(SharedString::from(format!("conflict-{}", file.path())))
                                .child(Icon::new(IconName::TriangleAlert).xsmall().text_color(cx.theme().warning))
                                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
                        )
                    })
                }
            })
            .render_header_suffix({
                let (view, data, reviewed) = (view.clone(), data.clone(), reviewed.clone());
                move |file, _, _| {
                    let path = file.path().clone();
                    let Some(ix) = data.index_of(&path) else { return h_flex().into_any_element() };
                    let done = data.file_reviewed(ix, &reviewed);
                    let id = data.file_ids[ix];
                    let deleted = data.files[ix].kind == orbit_proto::FileDiffKind::Deleted;
                    let v = view.clone();
                    h_flex()
                        .gap_1()
                        .child(
                            Checkbox::new(("viewed", id))
                                .label("Viewed")
                                .checked(done)
                                .on_click(move |checked: &bool, _, cx| {
                                    let checked = *checked;
                                    let _ = v.update(cx, |this, cx| this.set_file_reviewed(ix, checked, cx));
                                }),
                        )
                        .child(file_menu(("file-menu", id), ix, path, deleted, view.clone()))
                        .into_any_element()
                }
            });
        div().size_full().min_w_0().child(diff).into_any_element()
    }

    fn render_comment_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.comments.len() + self.drafts.iter().filter(|d| !d.comment.text.is_empty()).count();
        if count == 0 && self.drafts.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let busy = self.thread_record(cx).is_some_and(|t| t.status.is_busy());
        let saved = self.comments.len();
        Some(
            h_flex()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .child(Icon::new(Lucide::MessageSquare).small().text_color(theme.muted_foreground))
                .child(
                    v_flex()
                        .flex_1()
                        .child(div().text_sm().child(match saved {
                            0 => "Finish the comment to send it".to_owned(),
                            1 => "1 comment for the agent".to_owned(),
                            n => format!("{n} comments for the agent"),
                        }))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(if busy {
                            "They go as one message after the current turn."
                        } else {
                            "They go as one message with each file, line and change."
                        })),
                )
                .child(
                    Button::new("discard-comments")
                        .small()
                        .ghost()
                        .label("Discard")
                        .disabled(saved == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.drafts.clear();
                            this.store_comments(Vec::new(), cx);
                        })),
                )
                .child(
                    Button::new("send-comments")
                        .small()
                        .primary()
                        .icon(Lucide::Send)
                        .label(if busy { "Queue for agent" } else { "Send to agent" })
                        .disabled(saved == 0)
                        .tooltip("Ctrl+Shift+Enter")
                        .on_click(cx.listener(|this, _, window, cx| this.send_comments(window, cx))),
                )
                .into_any_element(),
        )
    }
}

fn render_hunk_marker(
    marker: &DiffGutterMarker,
    data: &Rc<ReviewData>,
    reviewed: &Rc<HashSet<u64>>,
    reverted: &Rc<HashMap<u64, String>>,
    view: WeakEntity<ReviewView>,
    cx: &App,
) -> AnyElement {
    let ElementId::NamedInteger(_, hid) = marker.id() else { return div().into_any_element() };
    let hid = *hid;
    let done = reviewed.contains(&hid);
    let undo = reverted.get(&hid).cloned();
    let theme = cx.theme();
    let path = marker.position().path().clone();
    let position = marker.position().clone();
    let (icon, color, tip) = if undo.is_some() {
        (IconName::Undo2, theme.muted_foreground, "Reverted: open to undo")
    } else if done {
        (IconName::CircleCheck, theme.success, "Reviewed: open for actions")
    } else {
        (IconName::Ellipsis, theme.muted_foreground, "Review this change")
    };
    let _ = data;
    // The marker sits in a code row; its menu reads in the interface font.
    let font = theme.font_family.clone();
    let button = Button::new(("hunk-marker", hid))
        .ghost()
        .xsmall()
        .compact()
        .icon(Icon::new(icon).text_color(color))
        .tooltip(tip)
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.min_w(px(220.));
            let v = view.clone();
            menu = menu.item(
                PopupMenuItem::new(if done { "Mark unreviewed" } else { "Mark reviewed" })
                    .icon(if done { IconName::Undo2 } else { IconName::CircleCheck })
                    .on_click(move |_, _, cx| {
                        let _ = v.update(cx, |this, cx| this.toggle_hunk_reviewed(hid, cx));
                    }),
            );
            let v = view.clone();
            let (path, position) = (path.clone(), position.clone());
            menu = menu.item(PopupMenuItem::new("Comment on this change…").icon(Lucide::MessageSquare).on_click(move |_, window, cx| {
                let range = DiffLineRange::new(path.clone(), position.side(), position.line(), position.line());
                let _ = v.update(cx, |this, cx| {
                    // Quote the whole hunk.
                    let range = this.hunk_range(hid).unwrap_or(range);
                    this.start_comment(Some(&range), path.clone(), window, cx)
                });
            }));
            menu = menu.separator();
            let v = view.clone();
            match undo.clone() {
                Some(patch) => menu.item(PopupMenuItem::new("Undo revert").icon(IconName::Undo2).on_click(move |_, window, cx| {
                    let _ = v.update(cx, |this, cx| {
                        undo_revert(&this.app, this.thread, patch.clone(), Some(hid), cx.weak_entity(), window, cx);
                    });
                })),
                None => menu.item(PopupMenuItem::new("Revert this change").icon(Lucide::RotateCcw).on_click(move |_, window, cx| {
                    let _ = v.update(cx, |this, cx| this.revert_hunk(hid, window, cx));
                })),
            }
        });
    div().font_family(font).child(button).into_any_element()
}

impl ReviewView {
    /// The lines a hunk changed, from its first to its last changed line.
    fn hunk_range(&self, hid: u64) -> Option<DiffLineRange> {
        let &(file, h) = self.data.hunks.get(&hid)?;
        let hunk = self.data.files[file].hunks.get(h)?;
        let changed = |l: &&orbit_proto::DiffLine| matches!(l.kind, orbit_proto::LineKind::Added | orbit_proto::LineKind::Removed);
        let pos = |l: &orbit_proto::DiffLine| match l.kind {
            orbit_proto::LineKind::Added => (DiffSide::Modified, l.new_no.unwrap_or(1) as usize),
            _ => (DiffSide::Original, l.old_no.unwrap_or(1) as usize),
        };
        let first = hunk.lines.iter().find(changed).map(pos)?;
        let last = hunk.lines.iter().rev().find(changed).map(pos)?;
        Some(DiffLineRange::new(self.data.path(file), first.0, first.1, last.1).with_end_side(last.0))
    }
}

fn file_menu(id: impl Into<ElementId>, file: usize, path: SharedString, deleted: bool, view: WeakEntity<ReviewView>) -> impl IntoElement {
    Button::new(id).ghost().xsmall().compact().icon(IconName::Ellipsis).dropdown_menu(move |menu: PopupMenu, _, _| {
        let v = view.clone();
        let p = path.clone();
        let menu = menu.min_w(px(220.)).item(PopupMenuItem::new("Comment on file…").icon(Lucide::MessageSquare).on_click(move |_, window, cx| {
            let _ = v.update(cx, |this, cx| this.start_comment(None, p.clone(), window, cx));
        }));
        let p = path.clone();
        let menu = menu.item(PopupMenuItem::new("Copy path").icon(IconName::Copy).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(p.to_string()));
        }));
        let (v, p) = (view.clone(), path.clone());
        let menu = menu.item(PopupMenuItem::new("Open file").icon(IconName::ExternalLink).disabled(deleted).on_click(move |_, _, cx| {
            if let Some(root) = v.upgrade().and_then(|v| v.read(cx).root(cx)) {
                cx.open_with_system(&root.join(p.as_ref()));
            }
        }));
        let (v, p) = (view.clone(), path.clone());
        let menu = menu.item(PopupMenuItem::new("Show in folder").icon(IconName::FolderOpen).disabled(deleted).on_click(move |_, _, cx| {
            if let Some(root) = v.upgrade().and_then(|v| v.read(cx).root(cx)) {
                cx.reveal_path(&root.join(p.as_ref()));
            }
        }));
        let v = view.clone();
        menu.separator().item(PopupMenuItem::new("Revert file").icon(Lucide::RotateCcw).on_click(move |_, window, cx| {
            let _ = v.update(cx, |this, cx| this.revert_file(file, window, cx));
        }))
    })
}

fn render_comment(
    id: u64,
    comments: &Rc<Vec<Comment>>,
    drafts: &Rc<HashMap<u64, (Comment, Entity<TextareaState>)>>,
    view: WeakEntity<ReviewView>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let shell = |location: String| {
        v_flex()
            .mx_2()
            .my_1()
            .p_2()
            .gap_1p5()
            .max_w(rems(44.))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .font_family(theme.font_family.clone())
            .text_sm()
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::MessageSquare).xsmall())
                    .child(format!("Comment on {location}")),
            )
    };
    if let Some((comment, input)) = drafts.get(&id) {
        let (v1, v2) = (view.clone(), view.clone());
        return shell(comment.location())
            .child(Textarea::new(input))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(Button::new(("cancel-comment", id)).small().ghost().label("Cancel").on_click(move |_, _, cx| {
                        let _ = v1.update(cx, |this, cx| this.cancel_draft(id, cx));
                    }))
                    .child(Button::new(("save-comment", id)).small().primary().label("Save comment").on_click(move |_, window, cx| {
                        let _ = v2.update(cx, |this, cx| this.save_draft(id, window, cx));
                    })),
            )
            .into_any_element();
    }
    let Some(comment) = comments.iter().find(|c| c.id == id) else { return div().into_any_element() };
    let (v1, v2) = (view.clone(), view);
    shell(comment.location())
        .child(div().whitespace_normal().child(comment.text.clone()))
        .child(
            h_flex()
                .gap_1()
                .justify_end()
                .child(Button::new(("edit-comment", id)).xsmall().ghost().label("Edit").on_click(move |_, window, cx| {
                    let _ = v1.update(cx, |this, cx| this.edit_comment(id, window, cx));
                }))
                .child(Button::new(("delete-comment", id)).xsmall().ghost().label("Delete").on_click(move |_, _, cx| {
                    let _ = v2.update(cx, |this, cx| this.delete_comment(id, cx));
                })),
        )
        .into_any_element()
}

impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(perf) = &mut self.perf {
            perf.frame();
            window.request_animation_frame();
        }
        let p = prefs(cx);
        let conflicts = Rc::new(self.conflicts(cx));
        let body = if let Some(error) = &self.error {
            crate::ui::empty_state(Icon::new(Lucide::FileDiff), "No changes to show", error.clone(), None, cx).into_any_element()
        } else if self.loading && self.data.is_empty() {
            v_flex().flex_1().items_center().justify_center().child(Spinner::new()).into_any_element()
        } else if self.data.is_empty() {
            let help = match self.scope {
                Scope::Uncommitted => "Everything is committed.",
                Scope::Branch => "This branch has nothing the other one doesn’t.",
                _ => "This scope didn’t change any files.",
            };
            crate::ui::empty_state(Icon::new(Lucide::FileDiff), "No changes", help, None, cx).into_any_element()
        } else {
            let tree_panel = self.render_tree_panel(conflicts.clone(), cx);
            let diff = self.render_diff(conflicts.clone(), cx);
            h_resizable("review-split")
                .with_state(&self.split)
                .child(resizable_panel().size(px(240.)).size_range(px(160.)..px(520.)).visible(p.tree).child(tree_panel))
                .child(resizable_panel().child(diff))
                .into_any_element()
        };
        v_flex()
            .id("review")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_next_file))
            .on_action(cx.listener(Self::on_previous_file))
            .on_action(cx.listener(Self::on_next_change))
            .on_action(cx.listener(Self::on_previous_change))
            .on_action(cx.listener(Self::on_toggle_viewed))
            .on_action(cx.listener(Self::on_toggle_wrap))
            .on_action(cx.listener(Self::on_toggle_tree))
            .on_action(cx.listener(Self::on_send))
            .on_action(cx.listener(Self::on_refresh))
            .size_full()
            .gap_2()
            .child(self.render_toolbar(cx))
            .child(self.render_summary(cx))
            .children(self.render_conflicts(&conflicts, cx))
            .child(div().flex_1().min_h_0().child(body))
            .children(self.render_comment_bar(cx))
    }
}

// ── Opening ───────────────────────────────────────────────────────────────

/// The sheet that is open, so the shortcut can close it again.
#[derive(Default)]
struct OpenSheet(Option<(ThreadId, WeakEntity<ReviewView>)>);

impl Global for OpenSheet {}

/// Opens the review sheet for `thread`, or closes it when it is already
/// open for that thread.
pub fn open(app: Entity<AppState>, thread: ThreadId, window: &mut Window, cx: &mut App) {
    let open_for = cx.try_global::<OpenSheet>().and_then(|s| s.0.as_ref()).filter(|(_, v)| v.upgrade().is_some()).map(|(t, _)| *t);
    if open_for == Some(thread) && window.has_active_sheet(cx) {
        window.close_sheet(cx);
        cx.set_global(OpenSheet(None));
        return;
    }
    open_at(app, thread, None, window, cx);
}

/// Opens the review sheet on a given scope (a turn's diff from its
/// summary, for example).
pub fn open_at(app: Entity<AppState>, thread: ThreadId, scope: Option<DiffScope>, window: &mut Window, cx: &mut App) {
    bind_keys(cx);
    let view = cx.new(|cx| ReviewView::new(app, thread, scope, window, cx));
    cx.set_global(OpenSheet(Some((thread, view.downgrade()))));
    let focus = view.read(cx).focus.clone();
    window.open_sheet_at(Placement::Right, cx, move |sheet, _, _| {
        sheet.size(rems(68.)).resizable(true).title("Review changes").child(view.clone())
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}
