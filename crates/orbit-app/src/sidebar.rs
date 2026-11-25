//! The sidebar: what needs you, what is working, and everything else.
//!
//! Threads live in sections by what they need from you, in this order:
//! Pinned, Needs you, Working, Recent, then the Snoozed and Settled shelves.
//! When a thread changes section (it starts working, it asks a question, it
//! finishes) its row arrives in the new section with a short slide and fade,
//! so the eye can follow it; rows that did not move never animate. An
//! alternative layout groups threads by project.
//!
//! Rows are virtualized (a uniform list), so a thousand threads cost what is
//! visible. Every row has the same lanes: status, title, project, age.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use chrono::TimeZone as _;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{VirtualListScrollHandle, v_virtual_list};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::ui::{self, Attention, StatusGlyph};
use crate::{format, thread::display_title, thread_menu};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Pinned,
    NeedsYou,
    Working,
    Recent,
    Snoozed,
    Settled,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Pinned => "Pinned",
            Section::NeedsYou => "Needs you",
            Section::Working => "Working",
            Section::Recent => "Recent",
            Section::Snoozed => "Snoozed",
            Section::Settled => "Settled",
        }
    }

    fn of(thread: &Thread) -> Section {
        if thread.settled_ms.is_some() {
            return Section::Settled;
        }
        if thread.snoozed_until.is_some() {
            return Section::Snoozed;
        }
        match Attention::of(thread) {
            Attention::Waiting | Attention::Failed | Attention::RateLimited => Section::NeedsYou,
            _ if thread.pinned => Section::Pinned,
            Attention::Working => Section::Working,
            Attention::Unread | Attention::Idle => Section::Recent,
        }
    }

    /// Shelves start closed; they are reference, not work.
    fn open_by_default(self) -> bool {
        !matches!(self, Section::Snoozed | Section::Settled)
    }
}

#[derive(Clone, Debug)]
enum Entry {
    Section { section: Section, count: usize, open: bool },
    Project { id: Option<ProjectId>, name: String, count: usize, open: bool },
    /// A thread row; `compact` rows are one line (shelves).
    Thread { id: ThreadId, compact: bool },
}

impl Entry {
    /// Row height in rems; the virtual list needs every height up front.
    fn height(&self) -> f32 {
        match self {
            Entry::Section { .. } | Entry::Project { .. } => 2.0,
            Entry::Thread { compact: true, .. } => 2.0,
            Entry::Thread { compact: false, .. } => 3.25,
        }
    }
}

/// What a thread needs, in a few words, and the color that carries it.
fn status_line(thread: &Thread, state: &orbit_net::state::ClientState, theme: &gpui_kit::component::Theme) -> (String, Hsla) {
    let muted = theme.muted_foreground;
    if let Some(pending) = thread.pending.first() {
        let text = match &pending.request.kind {
            RequestKind::Command { command, .. } => format!("Wants to run {}", command.lines().next().unwrap_or_default()),
            RequestKind::FileChange { path, .. } => format!("Wants to change {}", path.rsplit(['/', '\\']).next().unwrap_or(path)),
            RequestKind::Tool { name, .. } => format!("Wants to use {name}"),
            RequestKind::Question { .. } => "Has a question for you".into(),
            RequestKind::Plan { .. } => "Has a plan for you to approve".into(),
        };
        return (text, theme.warning);
    }
    match &thread.status {
        ThreadStatus::Starting if thread.workspace.worktree.is_none() => ("Setting up the worktree…".into(), muted),
        ThreadStatus::Running | ThreadStatus::Starting => ("Working…".into(), muted),
        ThreadStatus::Failed { message } => (format!("Failed · {message}"), theme.danger),
        ThreadStatus::RateLimited { resets_ms } => (format::resets_at(*resets_ms, now_ms()), theme.warning),
        _ if thread.snoozed_until.is_some() => {
            let until = thread.snoozed_until.unwrap_or_default();
            let local = chrono::Local
                .timestamp_millis_opt(until as i64)
                .single()
                .map(|t| t.format("%a %H:%M").to_string())
                .unwrap_or_default();
            (format!("Snoozed until {local}"), muted)
        }
        _ if thread.unread => ("New result".into(), theme.primary),
        _ => {
            let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
            let model = ui::model_label(&thread.config, &state.providers);
            match project {
                Some(p) => (format!("{} · {}", p.name, model), muted),
                None => (model, muted),
            }
        }
    }
}

/// A row that just changed section: animate its arrival.
#[derive(Clone, Copy)]
struct Move {
    generation: u32,
    /// Arrived from a section above (slides down) or below (slides up).
    from_above: bool,
    at: Instant,
}

const MOVE_DURATION: Duration = Duration::from_millis(260);

pub struct Sidebar {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    by_project: bool,
    closed_sections: HashSet<Section>,
    opened_shelves: HashSet<Section>,
    closed_projects: HashSet<Option<ProjectId>>,
    scroll: VirtualListScrollHandle,
    entries: Vec<Entry>,
    search: Entity<InputState>,
    query: String,
    sections: HashMap<ThreadId, Section>,
    moves: HashMap<ThreadId, Move>,
    _subscriptions: Vec<Subscription>,
    _clock: Task<()>,
}

impl Sidebar {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&app, |this, _, cx| {
            this.rebuild(cx);
            cx.notify();
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search threads").clean_on_escape());
        let search_sub = cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = input.read(cx).value().trim().to_lowercase();
                this.rebuild(cx);
                cx.notify();
            }
        });
        // Relative ages ("4m") stay honest with one cheap render a minute.
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(60)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let mut sidebar = Self {
            app,
            workspace,
            by_project: false,
            closed_sections: HashSet::new(),
            opened_shelves: HashSet::new(),
            closed_projects: HashSet::new(),
            scroll: VirtualListScrollHandle::new(),
            entries: Vec::new(),
            search,
            query: String::new(),
            sections: HashMap::new(),
            moves: HashMap::new(),
            _subscriptions: vec![sub, search_sub],
            _clock: clock,
        };
        sidebar.rebuild(cx);
        sidebar
    }

    fn is_open(&self, section: Section) -> bool {
        if section.open_by_default() {
            !self.closed_sections.contains(&section)
        } else {
            self.opened_shelves.contains(&section)
        }
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let mut threads: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
        threads.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));

        // Track section changes for arrival animations.
        let now = Instant::now();
        let first_build = self.sections.is_empty();
        for thread in &threads {
            let section = Section::of(thread);
            match self.sections.insert(thread.id, section) {
                Some(previous) if previous != section && !first_build => {
                    let generation = self.moves.get(&thread.id).map_or(0, |m| m.generation + 1);
                    self.moves.insert(thread.id, Move { generation, from_above: previous < section, at: now });
                }
                _ => {}
            }
        }
        let live: HashSet<ThreadId> = threads.iter().map(|t| t.id).collect();
        self.sections.retain(|id, _| live.contains(id));
        self.moves.retain(|id, m| live.contains(id) && now.duration_since(m.at) < MOVE_DURATION * 2);

        let mut entries = Vec::new();
        if !self.query.is_empty() {
            let query = &self.query;
            let hits: Vec<&&Thread> = threads
                .iter()
                .filter(|t| {
                    let project = t.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
                    t.title.to_lowercase().contains(query)
                        || project.is_some_and(|p| p.name.to_lowercase().contains(query))
                        || t.workspace.branch.as_ref().is_some_and(|b| b.to_lowercase().contains(query))
                })
                .collect();
            entries.extend(hits.iter().map(|t| Entry::Thread { id: t.id, compact: false }));
        } else if self.by_project {
            let mut groups: Vec<(Option<ProjectId>, String)> =
                state.projects.iter().map(|p| (Some(p.id), p.name.clone())).collect();
            groups.push((None, "No project".into()));
            for (project, name) in groups {
                let in_group: Vec<&&Thread> = threads
                    .iter()
                    .filter(|t| t.project == project && t.settled_ms.is_none())
                    .collect();
                if in_group.is_empty() && project.is_none() {
                    continue;
                }
                let open = !self.closed_projects.contains(&project);
                entries.push(Entry::Project { id: project, name, count: in_group.len(), open });
                if open {
                    let mut in_group = in_group;
                    in_group.sort_by_key(|t| (!t.pinned, std::cmp::Reverse(t.updated_ms)));
                    entries.extend(in_group.iter().map(|t| Entry::Thread { id: t.id, compact: false }));
                }
            }
        } else {
            for section in [
                Section::Pinned,
                Section::NeedsYou,
                Section::Working,
                Section::Recent,
                Section::Snoozed,
                Section::Settled,
            ] {
                let in_section: Vec<&&Thread> = threads.iter().filter(|t| Section::of(t) == section).collect();
                if in_section.is_empty() {
                    continue;
                }
                let open = self.is_open(section);
                entries.push(Entry::Section { section, count: in_section.len(), open });
                if open {
                    let compact = matches!(section, Section::Snoozed | Section::Settled);
                    entries.extend(in_section.iter().map(|t| Entry::Thread { id: t.id, compact }));
                }
            }
        }
        self.entries = entries;
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let set = if section.open_by_default() { &mut self.closed_sections } else { &mut self.opened_shelves };
        if !set.remove(&section) {
            set.insert(section);
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn toggle_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        if !self.closed_projects.remove(&project) {
            self.closed_projects.insert(project);
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn navigate(&self, route: Route, window: &mut Window, cx: &mut App) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(route, window, cx));
    }

    fn current(&self, cx: &App) -> Option<Route> {
        self.workspace.upgrade().map(|ws| ws.read(cx).route().clone())
    }

    fn render_entries(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let current = self.current(cx);
        let now = now_ms();
        let mut out = Vec::with_capacity(range.len());
        for ix in range {
            let Some(entry) = self.entries.get(ix).cloned() else { continue };
            let el: AnyElement = match entry {
                Entry::Section { section, count, open } => h_flex()
                    .id(("section", section as usize))
                    .h_8()
                    .px_2()
                    .gap_1p5()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if section == Section::NeedsYou { theme.warning } else { theme.muted_foreground })
                    .cursor_default()
                    .hover(|s| s.text_color(theme.sidebar_foreground))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
                    .child(div().flex_1().child(section.label()))
                    .child(count.to_string())
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                    .into_any_element(),
                Entry::Project { id, name, count, open } => {
                    let new_in = id;
                    h_flex()
                        .id(("project", id.map(|p| p.0).unwrap_or_default()))
                        .h_8()
                        .px_2()
                        .gap_1p5()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .cursor_default()
                        .hover(|s| s.text_color(theme.sidebar_foreground))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(id, cx)))
                        .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .child(count.to_string())
                        .when_some(new_in, |el, project| {
                            el.child(
                                Button::new(("new-in", project.0))
                                    .ghost()
                                    .xsmall()
                                    .icon(Lucide::SquarePen)
                                    .tooltip("New thread in this project")
                                    .on_click(move |_, window, cx| {
                                        window.dispatch_action(Box::new(NewThreadIn { project: project.0 }), cx)
                                    }),
                            )
                        })
                        .into_any_element()
                }
                Entry::Thread { id, compact } => {
                    let Some(thread) = state.threads.get(&id) else {
                        out.push(div().h(rems(entry.height())).into_any_element());
                        continue;
                    };
                    let selected = current == Some(Route::Thread(id));
                    let attention = Attention::of(thread);
                    let title = display_title(thread).to_owned();
                    let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
                    let project_path = project.map(|p| p.path.clone());
                    let (mark_name, mark_seed) = match project {
                        Some(p) => (p.name.clone(), p.id.0),
                        None => ("No project".to_owned(), 0),
                    };
                    let settled = thread.settled_ms.is_some();
                    let app = self.app.clone();
                    let menu_thread = thread.clone();
                    let age = format::relative_time(thread.updated_ms, now);
                    let base = h_flex()
                        .id(("thread-row", id.0))
                        .h(rems(entry.height()))
                        .px_2()
                        .gap_2p5()
                        .rounded(theme.radius_lg)
                        .cursor_default()
                        .when(selected, |el| el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                        .when(!selected, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.55))))
                        .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Thread(id), window, cx)))
                        .context_menu(move |menu, _, _| thread_menu::build(menu, &app, &menu_thread, project_path.clone()));
                    let row = if compact {
                        base.text_sm()
                            .when(!selected, |el| el.text_color(theme.muted_foreground))
                            .child(ui::Monogram::new(mark_name, mark_seed))
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(age))
                    } else {
                        let (status, status_color) = status_line(thread, state, &theme);
                        base.child(ui::Monogram::new(mark_name, mark_seed)).child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_sm()
                                                .when(attention == Attention::Unread || attention == Attention::Waiting, |el| {
                                                    el.font_weight(FontWeight::SEMIBOLD)
                                                })
                                                .when(settled && !selected, |el| el.text_color(theme.muted_foreground))
                                                .child(title),
                                        )
                                        .when(thread.pinned, |el| {
                                            el.child(Icon::new(Lucide::Pin).xsmall().text_color(theme.muted_foreground))
                                        })
                                        .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(age)),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .text_xs()
                                        .child(StatusGlyph::attention(("glyph", id.0), attention))
                                        .child(div().flex_1().min_w_0().truncate().text_color(status_color).child(status))
                                        .when(thread.woke, |el| {
                                            el.child(
                                                div()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(theme.primary.opacity(0.15))
                                                    .text_color(theme.primary)
                                                    .child("Woke"),
                                            )
                                        }),
                                ),
                        )
                    };
                    match self.moves.get(&id).copied() {
                        Some(mv) => {
                            let from = if mv.from_above { -1.0 } else { 1.0 };
                            row.relative()
                                .with_animation(
                                    ElementId::NamedInteger("moved".into(), id.0 ^ ((mv.generation as u64) << 56)),
                                    Animation::new(MOVE_DURATION).with_easing(ease_out_quint()),
                                    move |el, t| el.opacity(0.35 + 0.65 * t).top(px(10.0 * from * (1.0 - t))),
                                )
                                .into_any_element()
                        }
                        None => row.into_any_element(),
                    }
                }
            };
            out.push(div().px_2().child(el).into_any_element());
        }
        out
    }
}

/// Starts a thread in a given project (from a project row or the palette).
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct NewThreadIn {
    pub project: u64,
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let needs_you = state
            .threads
            .values()
            .filter(|t| !t.archived && matches!(Attention::of(t), Attention::Waiting | Attention::Failed | Attention::RateLimited))
            .count();
        let current = self.current(cx);
        let connection = state.connection.clone();
        let has_projects = !state.projects.is_empty();
        let nav_item = |id: &'static str, icon: Icon, label: &'static str, route: Route, badge: Option<usize>| {
            let selected = current.as_ref() == Some(&route);
            h_flex()
                .id(id)
                .h_8()
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .text_sm()
                .cursor_default()
                .when(selected, |el| el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                .when(!selected, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.6))))
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(route.clone(), window, cx)))
                .child(icon.small().text_color(theme.muted_foreground))
                .child(div().flex_1().child(label))
                .when_some(badge.filter(|n| *n > 0), |el, n| {
                    el.child(
                        div()
                            .px_1p5()
                            .rounded_full()
                            .bg(theme.warning.opacity(0.18))
                            .text_color(theme.warning)
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .child(n.to_string()),
                    )
                })
        };
        let count = self.entries.len();
        let by_project = self.by_project;
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        Button::new("new-thread")
                            .flex_1()
                            .small()
                            .icon(Icon::new(Lucide::SquarePen))
                            .label("New thread")
                            .tooltip_with_action("New thread", &NewThread, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewThread), cx)),
                    )
                    .child(
                        Button::new("palette")
                            .ghost()
                            .small()
                            .icon(Lucide::Command)
                            .tooltip_with_action("Commands", &OpenPalette, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx)),
                    ),
            )
            .child(
                div().px_2().pb_1().child(
                    Input::new(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(theme.muted_foreground))
                        .cleanable(true),
                ),
            )
            .child(
                v_flex()
                    .px_2()
                    .gap_0p5()
                    .child(nav_item("nav-inbox", Icon::new(IconName::Inbox), "Inbox", Route::Inbox, Some(needs_you)))
                    .child(nav_item("nav-fleet", Icon::new(IconName::LayoutDashboard), "Fleet", Route::Fleet, None)),
            )
            .child(
                h_flex()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .child(ui::section_label("Threads", cx).px_2().pt_0().pb_0().flex_1())
                    .child(
                        Button::new("group-toggle")
                            .ghost()
                            .xsmall()
                            .icon(if by_project { IconName::Folder } else { IconName::Menu })
                            .tooltip(if by_project { "Group by status" } else { "Group by project" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.by_project = !this.by_project;
                                this.rebuild(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("add-project")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FolderPlus)
                            .tooltip_with_action("Add project…", &AddProject, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(AddProject), cx)),
                    ),
            )
            .child(if count == 0 {
                v_flex()
                    .flex_1()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if has_projects {
                        "No threads yet. Start one from New thread."
                    } else {
                        "Add a folder to start working in it. Orbit uses your existing checkout."
                    })
                    .when(!has_projects, |el| {
                        el.child(
                            Button::new("add-first-project")
                                .small()
                                .outline()
                                .icon(Lucide::FolderPlus)
                                .label("Add project…")
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(AddProject), cx)),
                        )
                    })
                    .into_any_element()
            } else {
                {
                    let rem = window.rem_size();
                    let sizes: Rc<Vec<Size<Pixels>>> =
                        Rc::new(self.entries.iter().map(|e| size(px(0.), rem * e.height())).collect());
                    v_virtual_list(cx.entity(), "sidebar-entries", sizes, |this, range, window, cx| {
                        this.render_entries(range, window, cx)
                    })
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .into_any_element()
                }
            })
            .child(
                h_flex()
                    .h_10()
                    .px_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(connection_pill(connection.as_ref(), &theme))
                    .child(div().flex_1())
                    .child(
                        Button::new("settings")
                            .ghost()
                            .small()
                            .icon(IconName::Settings)
                            .tooltip_with_action("Settings", &OpenSettings, None)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
                    ),
            )
    }
}

fn connection_pill(status: Option<&ConnectionStatus>, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let (color, label) = match status {
        None | Some(ConnectionStatus::Connected) => (theme.success, "This computer".to_string()),
        Some(ConnectionStatus::Connecting) => (theme.warning, "Connecting…".to_string()),
        Some(ConnectionStatus::Disconnected { reason }) => (theme.danger, reason.clone()),
    };
    h_flex()
        .min_w_0()
        .gap_1p5()
        .px_2()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().flex_none().size_1p5().rounded_full().bg(color))
        .child(div().truncate().child(label))
}
