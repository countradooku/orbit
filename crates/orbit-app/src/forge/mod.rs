//! Pull requests and issues: the project's host and the user's trackers in
//! one place, next to the threads that work on them.
//!
//! A master–detail page. The list on the leading side (pull requests by
//! filter, or issues by tracker and search) stays scannable; the detail
//! takes the rest: checks with their durations, the description, reviews
//! and comments as Markdown, and the actions that move the work on (merge,
//! auto-merge, check out into a thread, fix failing checks, start a thread
//! from an issue).
//!
//! Data comes from the engine's forge cache: switching back to the page or
//! to the window asks for a refresh, which the engine turns into a request
//! only when its data is more than a few seconds old.

mod detail;
mod glyph;
pub mod integrations;
mod issues;
mod list;
// Mounted by the thread header (see its module docs); until then only its
// tests use it.
#[allow(dead_code)]
pub mod ship_pill;

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::resizable::ResizableState;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::ui;

#[allow(unused_imports)]
pub use ship_pill::{ShipAction, ShipPill};

gpui_kit::actions!(orbit_forge, [GoPullRequests, RefreshForge, SelectNextItem, SelectPreviousItem]);

/// Installs the integration settings model and the page's key bindings.
pub fn init(app: Entity<AppState>, cx: &mut App) {
    integrations::init(app, cx);
    cx.bind_keys([
        KeyBinding::new(crate::actions::platform("cmd-shift-g", "ctrl-shift-g"), GoPullRequests, None),
        KeyBinding::new("f5", RefreshForge, Some("Forge")),
        KeyBinding::new("down", SelectNextItem, Some("Forge")),
        KeyBinding::new("up", SelectPreviousItem, Some("Forge")),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    PullRequests,
    Issues,
}

/// A value the page loads: the last good value stays on screen while a
/// refresh runs, and a failure keeps it too when there is one.
pub struct Slot<T> {
    pub value: Option<T>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped per request; a reply for an older request is dropped.
    generation: u64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self { value: None, loading: false, error: None, generation: 0 }
    }
}

impl<T> Slot<T> {
    fn begin(&mut self) -> u64 {
        self.loading = true;
        self.generation += 1;
        self.generation
    }

    fn clear(&mut self) {
        self.value = None;
        self.error = None;
        self.loading = false;
        self.generation += 1;
    }

    /// Applies a reply; false when it was for an older request.
    fn finish(&mut self, generation: u64, result: Result<T, String>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        true
    }
}

pub struct ForgeView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    pub(crate) page: Page,
    pub(crate) project: Option<ProjectId>,
    pub(crate) info: Slot<ForgeInfo>,
    pub(crate) filter: PrFilter,
    pub(crate) prs: Slot<PullRequestList>,
    pub(crate) selected_pr: Option<u64>,
    pub(crate) pr: Slot<PullRequestDetail>,
    pub(crate) tracker: TrackerKind,
    pub(crate) query: Entity<InputState>,
    pub(crate) issues: Slot<IssueList>,
    pub(crate) selected_issue: Option<String>,
    pub(crate) issue: Slot<IssueDetail>,
    /// The action running now (`merge`, `checkout`…), for its spinner.
    pub(crate) busy: Option<&'static str>,
    pub(crate) method: MergeMethod,
    pub(crate) on_worktree: bool,
    pub(crate) split: Entity<ResizableState>,
    pub(crate) scroll: UniformListScrollHandle,
    search: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ForgeView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search issues"));
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => this.search_soon(cx),
                InputEvent::PressEnter { .. } => this.load_issues(true, cx),
                _ => {}
            }),
            // Back in the window: refresh what is on screen (the engine
            // serves seconds-old data without asking the host).
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() && this.is_shown(cx) {
                    this.refresh(false, cx);
                }
            }),
            cx.observe(&app, |this, app, cx| {
                // The project may have been removed.
                if let Some(id) = this.project
                    && !app.read(cx).state().projects.iter().any(|p| p.id == id)
                {
                    this.project = None;
                    this.reset(cx);
                }
            }),
        ];
        let on_worktree = crate::settings::get(cx).workspace.worktree_by_default;
        // Debug builds: `ORBIT_DEBUG_FORGE` = `issues`, `pr:<number>` or
        // `filter:<mine|merged|closed>` opens that view, for visual checks.
        let debug = if cfg!(debug_assertions) { std::env::var("ORBIT_DEBUG_FORGE").unwrap_or_default() } else { String::new() };
        let (page, selected_pr, filter) = match debug.split_once(':') {
            Some(("pr", n)) => (Page::PullRequests, n.parse().ok(), PrFilter::Open),
            Some(("filter", f)) => {
                (Page::PullRequests, None, PrFilter::ALL.into_iter().find(|x| x.label().eq_ignore_ascii_case(f)).unwrap_or_default())
            }
            _ if debug == "issues" => (Page::Issues, None, PrFilter::Open),
            _ => (Page::PullRequests, None, PrFilter::Open),
        };
        Self {
            app,
            workspace,
            focus: cx.focus_handle(),
            page,
            project: None,
            info: Slot::default(),
            filter,
            prs: Slot::default(),
            selected_pr,
            pr: Slot::default(),
            tracker: TrackerKind::GitHub,
            query,
            issues: Slot::default(),
            selected_issue: None,
            issue: Slot::default(),
            busy: None,
            method: MergeMethod::Squash,
            on_worktree,
            split: cx.new(|_| ResizableState::default()),
            scroll: UniformListScrollHandle::new(),
            search: None,
            _subscriptions: subscriptions,
        }
    }

    fn is_shown(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|w| *w.read(cx).route() == Route::Forge)
    }

    /// The page became visible: pick a project if none, and refresh.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.is_none() {
            self.project = self.default_project(cx);
        }
        self.focus.focus(window, cx);
        if self.info.value.is_none() && !self.info.loading {
            self.load_info(cx);
        } else {
            self.refresh(false, cx);
        }
    }

    /// The project of the most recently active thread, else the first one.
    fn default_project(&self, cx: &App) -> Option<ProjectId> {
        let state = self.app.read(cx).state();
        state
            .threads
            .values()
            .filter(|t| t.project.is_some())
            .max_by_key(|t| t.updated_ms)
            .and_then(|t| t.project)
            .or_else(|| state.projects.first().map(|p| p.id))
    }

    pub fn select_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if self.project != Some(project) {
            self.project = Some(project);
            self.reset(cx);
        }
    }

    /// Clears everything and loads the project's host from scratch.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.info.clear();
        self.prs.clear();
        self.pr.clear();
        self.issues.clear();
        self.issue.clear();
        self.selected_pr = None;
        self.selected_issue = None;
        self.load_info(cx);
        cx.notify();
    }

    /// Refreshes what the page shows. `force` comes from the Refresh
    /// button: it also rechecks the sign-in state.
    pub fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if force || self.info.value.as_ref().is_none_or(|i| !matches!(i.auth, ForgeAuth::Ready { .. })) {
            self.load_info(cx);
            return;
        }
        match self.page {
            Page::PullRequests => {
                self.load_prs(true, cx);
                if self.selected_pr.is_some() {
                    self.load_pr(true, cx);
                }
            }
            Page::Issues => {
                self.load_issues(true, cx);
                if self.selected_issue.is_some() {
                    self.load_issue(true, cx);
                }
            }
        }
    }

    // ── Loading ──────────────────────────────────────────────────────────

    fn call(&self, command: ForgeCommand, cx: &mut Context<Self>) -> Task<Result<Reply, String>> {
        self.app.update(cx, |app, cx| app.call(Command::Forge(command), cx))
    }

    fn load_info(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project else { return };
        let generation = self.info.begin();
        let task = self.call(ForgeCommand::Info { project }, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|reply| match reply {
                Reply::Forge(ForgeReply::Info(info)) => Ok(info),
                _ => Err("Unexpected reply.".into()),
            });
            let _ = this.update(cx, |this, cx| {
                if this.info.finish(generation, result) {
                    if let Some(info) = &this.info.value {
                        // Default to the project's own tracker.
                        if this.tracker.is_forge() {
                            this.tracker = match info.repo.as_ref().map(|r| r.kind) {
                                Some(ForgeKind::GitLab) => TrackerKind::GitLab,
                                _ => TrackerKind::GitHub,
                            };
                        }
                    }
                    match this.page {
                        Page::PullRequests if this.ready() => this.load_prs(true, cx),
                        Page::Issues => this.load_issues(true, cx),
                        _ => {}
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The host is reachable and signed in.
    pub(crate) fn ready(&self) -> bool {
        self.info.value.as_ref().is_some_and(|i| i.repo.is_some() && matches!(i.auth, ForgeAuth::Ready { .. }))
    }

    pub(crate) fn forge_kind(&self) -> ForgeKind {
        self.info.value.as_ref().and_then(|i| i.repo.as_ref()).map(|r| r.kind).unwrap_or(ForgeKind::GitHub)
    }

    fn load_prs(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(project) = self.project else { return };
        let generation = self.prs.begin();
        let task = self.call(ForgeCommand::PullRequests { project, filter: self.filter, refresh }, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|reply| match reply {
                Reply::Forge(ForgeReply::PullRequests(list)) => Ok(list),
                _ => Err("Unexpected reply.".into()),
            });
            let _ = this.update(cx, |this, cx| {
                if this.prs.finish(generation, result) {
                    // Keep the selection while it is in the list, else take
                    // the first one, so the detail is never blank.
                    let items = this.prs.value.as_ref().map(|l| l.items.as_slice()).unwrap_or(&[]);
                    let keep = this.selected_pr.filter(|n| items.iter().any(|p| p.number == *n));
                    let next = keep.or(items.first().map(|p| p.number));
                    if next != this.selected_pr || (next.is_some() && this.pr.value.is_none() && !this.pr.loading) {
                        this.selected_pr = next;
                        this.pr.clear();
                        this.load_pr(false, cx);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn load_pr(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let (Some(project), Some(number)) = (self.project, self.selected_pr) else { return };
        let generation = self.pr.begin();
        let task = self.call(ForgeCommand::PullRequest { project, number, refresh }, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|reply| match reply {
                Reply::Forge(ForgeReply::PullRequest(detail)) => Ok(*detail),
                _ => Err("Unexpected reply.".into()),
            });
            let _ = this.update(cx, |this, cx| {
                if this.pr.finish(generation, result) {
                    if let Some(d) = &this.pr.value
                        && !d.merge_methods.is_empty()
                        && !d.merge_methods.contains(&this.method)
                    {
                        this.method = d.merge_methods[0];
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn load_issues(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let tracker = self.tracker;
        if !integrations::get(cx).configured(tracker) {
            self.issues.clear();
            cx.notify();
            return;
        }
        let query = self.query.read(cx).value().to_string();
        let generation = self.issues.begin();
        let task = self.call(ForgeCommand::Issues { project: self.project, tracker, query, refresh }, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|reply| match reply {
                Reply::Forge(ForgeReply::Issues(list)) => Ok(list),
                _ => Err("Unexpected reply.".into()),
            });
            let _ = this.update(cx, |this, cx| {
                if this.issues.finish(generation, result) {
                    let items = this.issues.value.as_ref().map(|l| l.items.as_slice()).unwrap_or(&[]);
                    let keep = this.selected_issue.clone().filter(|k| items.iter().any(|i| &i.key == k));
                    let next = keep.or(items.first().map(|i| i.key.clone()));
                    if next != this.selected_issue || (next.is_some() && this.issue.value.is_none() && !this.issue.loading) {
                        this.selected_issue = next;
                        this.issue.clear();
                        this.load_issue(false, cx);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn load_issue(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(key) = self.selected_issue.clone() else { return };
        let generation = self.issue.begin();
        let task = self.call(ForgeCommand::Issue { project: self.project, tracker: self.tracker, key, refresh }, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|reply| match reply {
                Reply::Forge(ForgeReply::Issue(detail)) => Ok(*detail),
                _ => Err("Unexpected reply.".into()),
            });
            let _ = this.update(cx, |this, cx| {
                if this.issue.finish(generation, result) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Searches a moment after typing stops.
    fn search_soon(&mut self, cx: &mut Context<Self>) {
        self.search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(350)).await;
            let _ = this.update(cx, |this, cx| this.load_issues(false, cx));
        }));
    }

    // ── Selection ────────────────────────────────────────────────────────

    pub(crate) fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        match page {
            Page::PullRequests if self.prs.value.is_none() && self.ready() => self.load_prs(false, cx),
            Page::Issues if self.issues.value.is_none() => self.load_issues(false, cx),
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn set_filter(&mut self, filter: PrFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.prs.clear();
            self.load_prs(false, cx);
        }
    }

    pub(crate) fn set_tracker(&mut self, tracker: TrackerKind, cx: &mut Context<Self>) {
        if self.tracker != tracker {
            self.tracker = tracker;
            self.issues.clear();
            self.issue.clear();
            self.selected_issue = None;
            self.load_issues(false, cx);
        }
    }

    pub(crate) fn select_pr(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.selected_pr != Some(number) {
            self.selected_pr = Some(number);
            self.pr.clear();
            self.load_pr(false, cx);
        }
    }

    pub(crate) fn select_issue(&mut self, key: String, cx: &mut Context<Self>) {
        if self.selected_issue.as_ref() != Some(&key) {
            self.selected_issue = Some(key);
            self.issue.clear();
            self.load_issue(false, cx);
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let at = |ix: Option<usize>, len: usize| -> Option<usize> {
            if len == 0 {
                return None;
            }
            Some(match ix {
                Some(ix) => (ix as isize + delta).clamp(0, len as isize - 1) as usize,
                None => 0,
            })
        };
        match self.page {
            Page::PullRequests => {
                let items: Vec<u64> = self.prs.value.as_ref().map(|l| l.items.iter().map(|p| p.number).collect()).unwrap_or_default();
                let current = self.selected_pr.and_then(|n| items.iter().position(|x| *x == n));
                if let Some(ix) = at(current, items.len()) {
                    self.select_pr(items[ix], cx);
                    self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
                }
            }
            Page::Issues => {
                let items: Vec<String> =
                    self.issues.value.as_ref().map(|l| l.items.iter().map(|i| i.key.clone()).collect()).unwrap_or_default();
                let current = self.selected_issue.as_ref().and_then(|k| items.iter().position(|x| x == k));
                if let Some(ix) = at(current, items.len()) {
                    self.select_issue(items[ix].clone(), cx);
                    self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
                }
            }
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────

    /// Runs an action that ends in a thread, then opens it.
    fn run_to_thread(&mut self, label: &'static str, command: ForgeCommand, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label);
        let task = self.call(command, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Reply::Thread(id)) => {
                        let _ = this.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
                    }
                    Ok(_) => {}
                    Err(message) => window.push_notification(
                        gpui_kit::component::notification::Notification::error(message).title(match label {
                            "checkout" => "Couldn’t check out the branch",
                            "fix" => "Couldn’t send the failing checks",
                            _ => "Couldn’t start the thread",
                        }),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn config(&self, cx: &App) -> ThreadConfig {
        self.app.read(cx).state().defaults.clone()
    }

    pub(crate) fn checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(number)) = (self.project, self.selected_pr) else { return };
        let config = self.config(cx);
        self.run_to_thread("checkout", ForgeCommand::Checkout { project, number, config }, window, cx);
    }

    pub(crate) fn fix_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(number)) = (self.project, self.selected_pr) else { return };
        let config = self.config(cx);
        self.run_to_thread("fix", ForgeCommand::FixChecks { project, number, thread: None, config }, window, cx);
    }

    pub(crate) fn start_from_issue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(key)) = (self.project, self.selected_issue.clone()) else { return };
        let config = self.config(cx);
        let command = ForgeCommand::StartFromIssue { project, tracker: self.tracker, key, config, worktree: self.on_worktree };
        self.run_to_thread("start", command, window, cx);
    }

    /// Merges after a confirmation naming the pull request and the method.
    pub(crate) fn confirm_merge(&mut self, method: MergeMethod, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(detail)) = (self.project, self.pr.value.clone()) else { return };
        let sigil = self.forge_kind().review_sigil();
        let view = cx.weak_entity();
        let title = format!("{} {sigil}{}?", method.label(), detail.pr.number);
        let description = format!(
            "“{}” goes into {} as it is now ({}). The {} branch is deleted afterwards.",
            detail.pr.title,
            detail.pr.base,
            short_sha(&detail.head_sha),
            detail.pr.head
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            let head_sha = detail.head_sha.clone();
            let number = detail.pr.number;
            alert
                .title(title.clone())
                .description(description.clone())
                .show_cancel(true)
                .ok_text(method.label())
                .on_ok(move |_, window, cx| {
                    let command = ForgeCommand::Merge { project, number, method, auto: false, delete_branch: true, head_sha: head_sha.clone() };
                    let _ = view.update(cx, |this, cx| this.write("merge", command, window, cx));
                    true
                })
        });
    }

    pub(crate) fn set_auto_merge(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(number)) = (self.project, self.selected_pr) else { return };
        let command = if on {
            ForgeCommand::Merge { project, number, method: self.method, auto: true, delete_branch: true, head_sha: String::new() }
        } else {
            ForgeCommand::DisableAutoMerge { project, number }
        };
        self.write(if on { "auto" } else { "auto-off" }, command, window, cx);
    }

    /// Runs a write to the host, then reloads the list and the detail.
    fn write(&mut self, label: &'static str, command: ForgeCommand, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label);
        let task = self.call(command, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if let Err(message) = result {
                    let title = match label {
                        "merge" => "Couldn’t merge",
                        _ => "Couldn’t change auto-merge",
                    };
                    window.push_notification(gpui_kit::component::notification::Notification::error(message).title(title), cx);
                }
                this.load_prs(true, cx);
                this.load_pr(true, cx);
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn open_url(url: &str, cx: &mut App) {
        if url.starts_with("https://") || url.starts_with("http://") {
            cx.open_url(url);
        }
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let projects: Vec<(ProjectId, String)> = state.projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let current = self.project;
        let name = current.and_then(|id| projects.iter().find(|(p, _)| *p == id)).map(|(_, n)| n.clone()).unwrap_or_else(|| "Pick a project".into());
        let view = cx.weak_entity();
        let picker = Button::new("forge-project")
            .ghost()
            .small()
            .icon(Icon::new(IconName::Folder))
            .label(name)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                use gpui_kit::component::menu::PopupMenuItem;
                for (id, name) in &projects {
                    let (view, id) = (view.clone(), *id);
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(current == Some(id)).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.select_project(id, cx));
                    }));
                }
                menu
            });
        let (fetched, stale, loading) = match self.page {
            Page::PullRequests => (
                self.prs.value.as_ref().map(|l| l.fetched_ms),
                self.prs.value.as_ref().and_then(|l| l.stale.clone()),
                self.prs.loading || self.pr.loading || self.info.loading,
            ),
            Page::Issues => (
                self.issues.value.as_ref().map(|l| l.fetched_ms),
                self.issues.value.as_ref().and_then(|l| l.stale.clone()),
                self.issues.loading || self.issue.loading,
            ),
        };
        let repo = self.info.value.as_ref().and_then(|i| i.repo.clone());
        h_flex()
            .flex_none()
            .h_11()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(picker)
            .child(
                TabBar::new("forge-page")
                    .segmented()
                    .small()
                    .selected_index(if self.page == Page::PullRequests { 0 } else { 1 })
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.set_page(if *ix == 0 { Page::PullRequests } else { Page::Issues }, cx)
                    }))
                    .child(Tab::new().label(if self.forge_kind() == ForgeKind::GitLab { "Merge requests" } else { "Pull requests" }))
                    .child(Tab::new().label("Issues")),
            )
            .child(div().flex_1())
            .when_some(stale, |el, note| {
                el.child(
                    h_flex()
                        .id("forge-stale")
                        .gap_1()
                        .max_w(rems(24.))
                        .text_xs()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::TriangleAlert).xsmall())
                        .child(div().truncate().child(note.clone()))
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(note.clone()).build(window, cx)),
                )
            })
            .when_some(fetched.filter(|f| *f > 0), |el, at| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child(format!("Updated {}", crate::format::relative_time(at, now_ms()))))
            })
            .when_some(repo, |el, repo| {
                let url = repo.web_url.clone();
                el.child(
                    Button::new("forge-open-repo")
                        .ghost()
                        .small()
                        .icon(IconName::ExternalLink)
                        .label(repo.path.clone())
                        .tooltip(format!("Open on {}", repo.kind.label()))
                        .on_click(move |_, _, cx| Self::open_url(&url, cx)),
                )
            })
            .child(
                Button::new("forge-refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .loading(loading)
                    .tooltip_with_action("Refresh", &RefreshForge, Some("Forge"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx))),
            )
    }

    /// The page body when the host can't be used, else `None`.
    fn render_blocked(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.app.read(cx).state();
        if state.projects.is_empty() {
            return Some(
                ui::empty_state(
                    Icon::new(Lucide::GitPullRequest),
                    "No projects yet",
                    "Add a project to see its pull requests and issues.",
                    Some(
                        Button::new("forge-add-project")
                            .label("Add project…")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::AddProject), cx))
                            .into_any_element(),
                    ),
                    cx,
                )
                .into_any_element(),
            );
        }
        if self.page == Page::Issues && !self.tracker.is_forge() {
            return None; // Linear and Jira don't need the host.
        }
        let Some(info) = &self.info.value else {
            if let Some(error) = &self.info.error {
                return Some(ui::empty_state(Icon::new(IconName::CircleAlert), "Couldn’t read the project", error.clone(), None, cx).into_any_element());
            }
            return Some(list::skeleton(cx).into_any_element());
        };
        let check_again = || {
            Button::new("forge-check-again")
                .outline()
                .small()
                .icon(IconName::RefreshCw)
                .label("Check again")
                .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx)))
        };
        let Some(repo) = &info.repo else {
            return Some(
                ui::empty_state(
                    Icon::new(Lucide::GitBranch),
                    "No hosted repository",
                    "This project’s remote isn’t on GitHub or GitLab, so it has no pull requests here. Linear and Jira issues still work.",
                    None,
                    cx,
                )
                .into_any_element(),
            );
        };
        let element = match &info.auth {
            ForgeAuth::Ready { .. } => return None,
            ForgeAuth::NotInstalled { program, fix } => ui::empty_state(
                Icon::new(IconName::SquareTerminal),
                format!("{program} isn’t installed"),
                format!("Orbit reads {} through your own {program}. {fix}", repo.kind.label()),
                Some(check_again().into_any_element()),
                cx,
            ),
            ForgeAuth::SignedOut { program, fix } => {
                let command: SharedString = fix.clone().into();
                ui::empty_state(
                    Icon::new(IconName::CircleUser),
                    format!("{program} isn’t signed in to {}", repo.host),
                    "Sign in once in a terminal; Orbit uses your own sign-in and never asks for a token.",
                    Some(
                        v_flex()
                            .items_center()
                            .gap_3()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .px_3()
                                    .py_1p5()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_sm()
                                    .child(command.clone())
                                    .child(Clipboard::new("forge-copy-login").value(command)),
                            )
                            .child(check_again())
                            .into_any_element(),
                    ),
                    cx,
                )
            }
            ForgeAuth::Unsupported => ui::empty_state(
                Icon::new(Lucide::GitPullRequest),
                format!("{} isn’t supported yet", repo.kind.label()),
                format!("Orbit reads pull requests from GitHub and GitLab today; {} support is planned.", repo.kind.label()),
                None,
                cx,
            ),
        };
        Some(element.into_any_element())
    }
}

pub(crate) fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

impl Focusable for ForgeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ForgeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = match self.render_blocked(cx) {
            Some(blocked) => v_flex().flex_1().min_h_0().child(blocked).into_any_element(),
            None => match self.page {
                Page::PullRequests => list::render_pull_requests(self, window, cx).into_any_element(),
                Page::Issues => issues::render(self, window, cx).into_any_element(),
            },
        };
        v_flex()
            .size_full()
            .key_context("Forge")
            .track_focus(&self.focus)
            .bg(theme.background)
            .on_action(cx.listener(|this, _: &RefreshForge, _, cx| this.refresh(true, cx)))
            .on_action(cx.listener(|this, _: &SelectNextItem, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPreviousItem, _, cx| this.step(-1, cx)))
            .child(self.render_toolbar(cx))
            .child(body)
    }
}
