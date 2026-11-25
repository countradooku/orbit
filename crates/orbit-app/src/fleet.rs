//! Fleet: every thread at a glance, as a status board, not a kanban.
//!
//! One row per thread with fixed columns (status, title, project, agent,
//! context, cost, activity), virtualized, sorted by what needs attention.

use std::ops::Range;

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::{self, Attention};
use crate::format;

pub struct FleetView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    rows: Vec<ThreadId>,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl FleetView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&app, |this, _, cx| {
            this.sort(cx);
            cx.notify();
        });
        let mut view = Self { app, workspace, rows: Vec::new(), scroll: UniformListScrollHandle::new(), _subscription: sub };
        view.sort(cx);
        view
    }

    fn sort(&mut self, cx: &App) {
        let state = self.app.read(cx).state();
        let rank = |a: Attention| match a {
            Attention::Waiting => 0,
            Attention::Failed => 1,
            Attention::RateLimited => 2,
            Attention::Working => 3,
            Attention::Unread => 4,
            Attention::Idle => 5,
        };
        let mut threads: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
        threads.sort_by(|a, b| {
            rank(Attention::of(a)).cmp(&rank(Attention::of(b))).then(b.updated_ms.cmp(&a.updated_ms))
        });
        self.rows = threads.iter().map(|t| t.id).collect();
    }

    fn render_rows(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let now = now_ms();
        range
            .filter_map(|ix| self.rows.get(ix).copied())
            .filter_map(|id| state.threads.get(&id).map(|t| (id, t)))
            .map(|(id, thread)| {
                let usage = state.usage(id);
                let project = thread
                    .project
                    .and_then(|p| state.projects.iter().find(|x| x.id == p))
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "—".into());
                h_flex()
                    .id(("fleet-row", id.0))
                    .w_full()
                    .h_9()
                    .px_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_sm()
                    .cursor_default()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let _ = this.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
                    }))
                    .child(ui::StatusGlyph::new(("fleet-glyph", id.0), thread))
                    .child(div().w(rems(7.)).text_xs().text_color(theme.muted_foreground).child(ui::Glyph::of(thread).label()))
                    .child(div().flex_1().min_w_0().truncate().child(display_title(thread).to_owned()))
                    .child(div().w(rems(9.)).truncate().text_xs().text_color(theme.muted_foreground).child(project))
                    .child(
                        div()
                            .w(rems(10.))
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(ui::model_label(&thread.config, &state.providers)),
                    )
                    .child(div().w(rems(4.5)).child(ui::ContextMeter::new(("fleet-meter", id.0), usage)))
                    .child(div().w(rems(4.)).text_right().text_xs().text_color(theme.muted_foreground).child(format::cost(usage.cost_micros)))
                    .child(div().w(rems(3.)).text_right().text_xs().text_color(theme.muted_foreground).child(format::relative_time(thread.updated_ms, now)))
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for FleetView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        if self.rows.is_empty() {
            return v_flex().size_full().child(ui::empty_state(
                Icon::new(IconName::LayoutDashboard),
                "No threads yet",
                "Every thread shows here with its status, agent, context and cost.",
                None,
                cx,
            ));
        }
        let head = |w: Option<Rems>, label: &'static str, right: bool| {
            let mut el = div().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(label);
            el = match w {
                Some(w) => el.w(w),
                None => el.flex_1(),
            };
            if right { el.text_right() } else { el }
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h_9()
                    .px_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.list_head)
                    .child(div().size_4())
                    .child(head(Some(rems(7.)), "Status", false))
                    .child(head(None, "Thread", false))
                    .child(head(Some(rems(9.)), "Project", false))
                    .child(head(Some(rems(10.)), "Agent", false))
                    .child(head(Some(rems(4.5)), "Context", false))
                    .child(head(Some(rems(4.)), "Cost", true))
                    .child(head(Some(rems(3.)), "Active", true)),
            )
            .child(
                uniform_list("fleet-rows", self.rows.len(), cx.processor(Self::render_rows))
                    .track_scroll(&self.scroll)
                    .flex_1(),
            )
    }
}
