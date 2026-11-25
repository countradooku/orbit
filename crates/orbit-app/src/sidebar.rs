//! The sidebar: where you are, what needs you, and every thread.
//!
//! Rows are virtualized (a uniform list), so a thousand threads cost what is
//! visible. Every thread row has the same lanes: status, title, age. A status
//! appearing never moves the title, and "waiting on you" never looks like
//! "working".

use std::collections::HashSet;
use std::ops::Range;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
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
use crate::{format, thread::display_title};

#[derive(Clone, Debug)]
enum Entry {
    Project { id: ProjectId, name: String, count: usize, open: bool },
    Scratch { count: usize, open: bool },
    Thread(ThreadId),
    Empty(Option<ProjectId>),
}

pub struct Sidebar {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    collapsed: HashSet<Option<ProjectId>>,
    scroll: UniformListScrollHandle,
    entries: Vec<Entry>,
    _subscriptions: Vec<Subscription>,
    _clock: Task<()>,
}

impl Sidebar {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&app, |this, _, cx| {
            this.rebuild(cx);
            cx.notify();
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
            collapsed: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            entries: Vec::new(),
            _subscriptions: vec![sub],
            _clock: clock,
        };
        sidebar.rebuild(cx);
        sidebar
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let mut entries = Vec::new();
        for project in &state.projects {
            let threads = state.sorted_threads(Some(project.id));
            let open = !self.collapsed.contains(&Some(project.id));
            entries.push(Entry::Project { id: project.id, name: project.name.clone(), count: threads.len(), open });
            if open {
                if threads.is_empty() {
                    entries.push(Entry::Empty(Some(project.id)));
                }
                entries.extend(threads.iter().map(|t| Entry::Thread(t.id)));
            }
        }
        let scratch = state.sorted_threads(None);
        if !scratch.is_empty() {
            let open = !self.collapsed.contains(&None);
            entries.push(Entry::Scratch { count: scratch.len(), open });
            if open {
                entries.extend(scratch.iter().map(|t| Entry::Thread(t.id)));
            }
        }
        self.entries = entries;
    }

    fn toggle_group(&mut self, group: Option<ProjectId>, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&group) {
            self.collapsed.insert(group);
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
        range
            .filter_map(|ix| self.entries.get(ix).cloned().map(|e| (ix, e)))
            .map(|(_, entry)| match entry {
                Entry::Project { id, name, count, open } => h_flex()
                    .id(("project", id.0))
                    .h_8()
                    .px_2()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .cursor_default()
                    .hover(|s| s.text_color(theme.sidebar_foreground))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(Some(id), cx)))
                    .context_menu(move |menu, _, _| {
                        menu.item(PopupMenuItem::new("New thread here").icon(Lucide::SquarePen).on_click(move |_, window, cx| {
                            window.dispatch_action(Box::new(NewThreadIn { project: id.0 }), cx)
                        }))
                    })
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(div().child(count.to_string()))
                    .into_any_element(),
                Entry::Scratch { count, open } => h_flex()
                    .id("scratch")
                    .h_8()
                    .px_2()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .cursor_default()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_group(None, cx)))
                    .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                    .child(div().flex_1().child("No project"))
                    .child(count.to_string())
                    .into_any_element(),
                Entry::Empty(project) => h_flex()
                    .id(("empty", project.map(|p| p.0).unwrap_or_default()))
                    .h_8()
                    .pl_7()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No threads yet")
                    .into_any_element(),
                Entry::Thread(id) => {
                    let Some(thread) = state.threads.get(&id) else { return div().h_8().into_any_element() };
                    let selected = current == Some(Route::Thread(id));
                    let attention = Attention::of(thread);
                    let title = display_title(thread).to_owned();
                    let pinned = thread.pinned;
                    let app = self.app.clone();
                    h_flex()
                        .id(("thread-row", id.0))
                        .h_8()
                        .pl_2()
                        .pr_2()
                        .gap_2()
                        .rounded(theme.radius)
                        .text_sm()
                        .cursor_default()
                        .when(selected, |el| el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                        .when(!selected, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.6))))
                        .when(attention == Attention::Unread && !selected, |el| el.font_weight(FontWeight::MEDIUM))
                        .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Thread(id), window, cx)))
                        .context_menu(move |menu, _, _| {
                            let (pin_app, archive_app, delete_app) = (app.clone(), app.clone(), app.clone());
                            menu.item(PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" }).icon(Lucide::Pin).on_click(
                                move |_, window, cx| {
                                    pin_app.update(cx, |app, cx| {
                                        app.run(
                                            Command::UpdateThread {
                                                id,
                                                patch: ThreadPatch { pinned: Some(!pinned), ..Default::default() },
                                            },
                                            window,
                                            cx,
                                        )
                                    })
                                },
                            ))
                            .item(PopupMenuItem::new("Archive").icon(Lucide::Archive).on_click(move |_, window, cx| {
                                archive_app.update(cx, |app, cx| {
                                    app.run(
                                        Command::UpdateThread {
                                            id,
                                            patch: ThreadPatch { archived: Some(true), ..Default::default() },
                                        },
                                        window,
                                        cx,
                                    )
                                })
                            }))
                            .separator()
                            .item(PopupMenuItem::new("Delete").icon(Lucide::Trash).on_click(move |_, window, cx| {
                                delete_app.update(cx, |app, cx| app.run(Command::DeleteThread { id }, window, cx))
                            }))
                        })
                        .child(StatusGlyph::attention(("glyph", id.0), attention))
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .when(pinned, |el| el.child(Icon::new(Lucide::Pin).xsmall().text_color(theme.muted_foreground)))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format::relative_time(thread.updated_ms, now)),
                        )
                        .into_any_element()
                }
            })
            .map(|el| div().px_2().child(el).into_any_element())
            .collect()
    }
}

/// Starts a thread in a given project (from a project's context menu).
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct NewThreadIn {
    pub project: u64,
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let needs_you: usize = state
            .threads
            .values()
            .filter(|t| matches!(Attention::of(t), Attention::Waiting | Attention::Failed | Attention::RateLimited))
            .count();
        let unread = state.threads.values().filter(|t| Attention::of(t) == Attention::Unread).count();
        let current = self.current(cx);
        let connection = state.connection.clone();
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
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex().p_2().gap_1().child(
                    Button::new("new-thread")
                        .w_full()
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
                        .icon(IconName::Search)
                        .tooltip_with_action("Search and commands", &OpenPalette, None)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx)),
                ),
            )
            .child(
                v_flex()
                    .px_2()
                    .gap_0p5()
                    .child(nav_item(
                        "nav-inbox",
                        Icon::new(IconName::Inbox),
                        "Inbox",
                        Route::Inbox,
                        Some(needs_you),
                    ))
                    .child(nav_item("nav-fleet", Icon::new(IconName::LayoutDashboard), "Fleet", Route::Fleet, None))
                    .when(unread > 0, |el| {
                        el.child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{unread} new result{}", if unread == 1 { "" } else { "s" })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .child(ui::section_label("Projects", cx).px_2().pt_0().pb_0().flex_1())
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
                    .child("Add a folder to start working in it. Orbit uses your existing checkout.")
                    .child(
                        Button::new("add-first-project")
                            .small()
                            .outline()
                            .icon(Lucide::FolderPlus)
                            .label("Add project…")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(AddProject), cx)),
                    )
                    .into_any_element()
            } else {
                uniform_list("sidebar-entries", count, cx.processor(Self::render_entries))
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .into_any_element()
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
