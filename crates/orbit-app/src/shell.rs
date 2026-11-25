//! The window: title bar, sidebar, and the routed main area.
//!
//! Every command is an Action handled here, so a button, a menu item, the
//! palette and a key binding all run the same code.

use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::fleet::FleetView;
use crate::home::HomeView;
use crate::inbox::InboxView;
use crate::sidebar::{NewThreadIn, Sidebar};
use crate::thread::{ThreadView, ThreadViewEvent};
use crate::ui::Attention;
use crate::{format, palette, settings};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Thread(ThreadId),
    Inbox,
    Fleet,
}

/// Threads whose views stay alive for instant switching. Older views are
/// dropped and their timelines released.
const LIVE_VIEWS: usize = 6;

pub struct Workspace {
    app: Entity<AppState>,
    route: Route,
    history: Vec<ThreadId>,
    sidebar: Entity<Sidebar>,
    sidebar_open: bool,
    split: Entity<ResizableState>,
    home: Entity<HomeView>,
    inbox: Entity<InboxView>,
    fleet: Entity<FleetView>,
    threads: Vec<(ThreadId, Entity<ThreadView>)>,
    focus: FocusHandle,
    /// Debug builds: a screen to open once data arrives (`ORBIT_DEBUG_ROUTE`
    /// = `latest`, `inbox`, `fleet`), for screenshots and visual checks.
    debug_route: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let sidebar = cx.new(|cx| Sidebar::new(app.clone(), weak.clone(), window, cx));
        let home = cx.new(|cx| HomeView::new(app.clone(), weak.clone(), window, cx));
        let inbox = cx.new(|cx| InboxView::new(app.clone(), weak.clone(), cx));
        let fleet = cx.new(|cx| FleetView::new(app.clone(), weak.clone(), cx));
        let split = cx.new(|_| ResizableState::default());
        let subs = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                if settings::appearance(cx) == crate::theme::Appearance::System {
                    crate::theme::apply(crate::theme::Appearance::System, window, cx);
                }
            }),
            // Follow deletions: never leave the user on a thread that is gone.
            cx.observe_in(&app, window, |this, app, window, cx| {
                if let Some(route) = this.debug_route.take() {
                    let state = app.read(cx).state();
                    let target = match route.as_str() {
                        "inbox" => Some(Route::Inbox),
                        "fleet" => Some(Route::Fleet),
                        "latest" => state.threads.values().max_by_key(|t| t.updated_ms).map(|t| Route::Thread(t.id)),
                        _ => None,
                    };
                    match target {
                        Some(route) => this.navigate(route, window, cx),
                        None if state.threads.is_empty() => this.debug_route = Some(route),
                        None => {}
                    }
                }
                if let Route::Thread(id) = this.route
                    && !app.read(cx).state().threads.contains_key(&id)
                    && app.read(cx).connection().is_none_or(|c| *c == orbit_net::ConnectionStatus::Connected)
                {
                    this.threads.retain(|(t, _)| *t != id);
                    this.route = Route::Home;
                }
                cx.notify();
            }),
        ];
        Self {
            app,
            route: Route::Home,
            history: Vec::new(),
            sidebar,
            sidebar_open: true,
            split,
            home,
            inbox,
            fleet,
            threads: Vec::new(),
            focus: cx.focus_handle(),
            debug_route: if cfg!(debug_assertions) { std::env::var("ORBIT_DEBUG_ROUTE").ok() } else { None },
            _subscriptions: subs,
        }
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn app(&self) -> &Entity<AppState> {
        &self.app
    }

    pub fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if let Route::Thread(id) = route {
            self.ensure_thread_view(id, window, cx);
            self.history.retain(|t| *t != id);
            self.history.push(id);
            if let Some((_, view)) = self.threads.iter().find(|(t, _)| *t == id) {
                let view = view.clone();
                window.defer(cx, move |window, cx| ThreadView::focus_composer(&view, window, cx));
            }
        }
        if route == Route::Home {
            let home = self.home.clone();
            window.defer(cx, move |window, cx| HomeView::focus_composer(&home, window, cx));
        }
        self.route = route;
        cx.notify();
    }

    fn ensure_thread_view(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.threads.iter().position(|(t, _)| *t == id) {
            let entry = self.threads.remove(ix);
            self.threads.push(entry);
            return;
        }
        let app = self.app.clone();
        let view = cx.new(|cx| ThreadView::new(id, app, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event: &ThreadViewEvent, window, cx| match event {
            ThreadViewEvent::Review => this.toggle_review(&ToggleReview, window, cx),
        });
        self._subscriptions.push(sub);
        self.threads.push((id, view));
        while self.threads.len() > LIVE_VIEWS {
            self.threads.remove(0);
        }
    }

    fn current_thread_view(&self) -> Option<&Entity<ThreadView>> {
        match self.route {
            Route::Thread(id) => self.threads.iter().find(|(t, _)| *t == id).map(|(_, v)| v),
            _ => None,
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────

    fn new_thread(&mut self, _: &NewThread, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Home, window, cx);
    }

    fn new_thread_in(&mut self, action: &NewThreadIn, window: &mut Window, cx: &mut Context<Self>) {
        let project = ProjectId(action.project);
        self.home.update(cx, |home, cx| home.select_project(Some(project), cx));
        self.navigate(Route::Home, window, cx);
    }

    fn go_inbox(&mut self, _: &GoInbox, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Inbox, window, cx);
    }

    fn go_fleet(&mut self, _: &GoFleet, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Fleet, window, cx);
    }

    fn go_home(&mut self, _: &GoHome, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Home, window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn toggle_review(&mut self, _: &ToggleReview, window: &mut Window, cx: &mut Context<Self>) {
        let Route::Thread(id) = self.route else { return };
        crate::review::open(self.app.clone(), id, window, cx);
    }

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        palette::open(cx.entity(), window, cx);
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        settings::open(self.app.clone(), window, cx);
    }

    pub fn add_project(&mut self, _: &AddProject, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        let app = self.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let task = app.update(cx, |app, cx| app.call(Command::AddProject { path: path.display().to_string() }, cx));
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Reply::Project(id)) => {
                    this.home.update(cx, |home, cx| home.select_project(Some(id), cx));
                    this.navigate(Route::Home, window, cx);
                }
                Ok(_) => {}
                Err(message) => window.push_notification(
                    gpui_kit::component::notification::Notification::error(message).title("Couldn’t add the project"),
                    cx,
                ),
            });
        })
        .detach();
    }

    fn stop_turn(&mut self, _: &StopTurn, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.current_thread_view().cloned() {
            view.update(cx, |view, cx| view.stop(window, cx));
        }
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        match self.current_thread_view() {
            Some(view) => ThreadView::focus_composer(&view.clone(), window, cx),
            None => HomeView::focus_composer(&self.home.clone(), window, cx),
        }
    }

    fn cycle_thread(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let mut ordered: Vec<ThreadId> = Vec::new();
        for project in &state.projects {
            ordered.extend(state.sorted_threads(Some(project.id)).iter().map(|t| t.id));
        }
        ordered.extend(state.sorted_threads(None).iter().map(|t| t.id));
        if ordered.is_empty() {
            return;
        }
        let current = match self.route {
            Route::Thread(id) => ordered.iter().position(|t| *t == id),
            _ => None,
        };
        let next = match current {
            Some(ix) => (ix as isize + step).rem_euclid(ordered.len() as isize) as usize,
            None => 0,
        };
        self.navigate(Route::Thread(ordered[next]), window, cx);
    }

    fn next_thread(&mut self, _: &NextThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread(1, window, cx);
    }

    fn previous_thread(&mut self, _: &PreviousThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread(-1, window, cx);
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let needs_you = state
            .threads
            .values()
            .filter(|t| matches!(Attention::of(t), Attention::Waiting))
            .count();
        let working = state.threads.values().filter(|t| Attention::of(t) == Attention::Working).count();
        let quota = state
            .providers
            .iter()
            .flat_map(|p| p.quota.iter().map(move |q| (p.kind, q)))
            .filter(|(_, q)| q.utilization.is_some() || q.rejected)
            .max_by(|a, b| a.1.utilization.unwrap_or(1.0).total_cmp(&b.1.utilization.unwrap_or(1.0)));
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_3()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(Lucide::Orbit).small().text_color(theme.primary))
                        .child("Orbit"),
                )
                .child(div().flex_1())
                .when(working > 0, |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(crate::ui::StatusGlyph::attention("title-working", Attention::Working))
                            .child(format!("{working} working")),
                    )
                })
                .when(needs_you > 0, |el| {
                    el.child(
                        h_flex()
                            .id("title-needs-you")
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme.warning.opacity(0.15))
                            .text_xs()
                            .text_color(theme.warning)
                            .cursor_default()
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(GoInbox), cx))
                            .child(Icon::new(Lucide::Hand).xsmall())
                            .child(format!("{needs_you} need{} you", if needs_you == 1 { "s" } else { "" })),
                    )
                })
                .when_some(quota, |el, (kind, q)| {
                    let pct = q.utilization.map(|u| (u * 100.0).round() as u32);
                    let label = match (q.rejected, pct) {
                        (true, _) => format!("{} quota used up · {}", kind.label(), format::resets_at(q.resets_ms, now_ms())),
                        (false, Some(p)) => format!("{} {}% of {} quota", kind.label(), p, humanize_window(&q.window)),
                        _ => String::new(),
                    };
                    el.child(div().text_xs().text_color(if q.rejected { theme.danger } else { theme.muted_foreground }).child(label))
                }),
        )
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.route {
            Route::Home => self.home.clone().into_any_element(),
            Route::Inbox => self.inbox.clone().into_any_element(),
            Route::Fleet => self.fleet.clone().into_any_element(),
            Route::Thread(_) => match self.current_thread_view() {
                Some(view) => view.clone().into_any_element(),
                None => crate::ui::empty_state(
                    Icon::new(IconName::Inbox),
                    "That thread is gone",
                    "It was deleted, perhaps from another window.",
                    None,
                    cx,
                )
                .into_any_element(),
            },
        }
    }
}

fn humanize_window(window: &str) -> String {
    match window {
        "five_hour" => "5-hour".into(),
        "seven_day" => "weekly".into(),
        "seven_day_opus" => "weekly Opus".into(),
        other => other.replace('_', " "),
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let main = self.render_main(cx);
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::new_thread))
            .on_action(cx.listener(Self::new_thread_in))
            .on_action(cx.listener(Self::go_inbox))
            .on_action(cx.listener(Self::go_fleet))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_review))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::add_project))
            .on_action(cx.listener(Self::stop_turn))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(Self::next_thread))
            .on_action(cx.listener(Self::previous_thread))
            .on_action(cx.listener(Self::quit))
            .child(self.render_title_bar(cx))
            .child(
                div().flex_1().min_h_0().child(
                h_resizable("shell")
                    .with_state(&self.split)
                    .child(
                        resizable_panel()
                            .size(px(272.))
                            .size_range(px(208.)..px(440.))
                            .visible(self.sidebar_open)
                            .child(self.sidebar.clone()),
                    )
                    .child(resizable_panel().child(div().size_full().min_w_0().child(main))),
                ),
            )
    }
}
