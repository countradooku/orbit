//! Inbox: everything that needs you, answerable in place.
//!
//! Each card carries the context to decide (the exact command, the file, the
//! question) so answering never requires opening the thread. Groups are in
//! the order they block work: requests, failures, quota waits, new results.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::{self, Attention};
use crate::format;

pub struct InboxView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    _subscription: Subscription,
}

impl InboxView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&app, |_, _, cx| cx.notify());
        Self { app, workspace, _subscription: sub }
    }

    fn open(&self, id: ThreadId, window: &mut Window, cx: &mut App) {
        let _ = self.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
    }
}

pub(crate) fn request_summary(kind: &RequestKind) -> (Icon, String, Option<String>) {
    match kind {
        RequestKind::Command { command, reason, .. } => (
            Icon::new(Lucide::Terminal),
            if reason.is_empty() { "Run a command".into() } else { reason.clone() },
            Some(command.clone()),
        ),
        RequestKind::FileChange { path, .. } => (Icon::new(Lucide::FilePen), "Change a file".into(), Some(path.clone())),
        RequestKind::Tool { name, .. } => (Icon::new(Lucide::Wrench), format!("Use {name}"), None),
        RequestKind::Question { question, .. } => (Icon::new(Lucide::MessageSquare), question.clone(), None),
        RequestKind::Plan { .. } => (Icon::new(Lucide::ListChecks), "Approve a plan".into(), None),
    }
}

impl Render for InboxView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let mut threads: Vec<&Thread> = state.threads.values().collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let group = |a: Attention| threads.iter().filter(move |t| Attention::of(t) == a).copied().collect::<Vec<_>>();
        let (waiting, failed, limited, unread) =
            (group(Attention::Waiting), group(Attention::Failed), group(Attention::RateLimited), group(Attention::Unread));
        let project_name = |t: &Thread| {
            t.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.name.clone()).unwrap_or_default()
        };
        let now = now_ms();

        if waiting.is_empty() && failed.is_empty() && limited.is_empty() && unread.is_empty() {
            return v_flex()
                .size_full()
                .child(ui::empty_state(
                    Icon::new(IconName::Inbox),
                    "Nothing needs you",
                    "Approvals, questions, failures and finished work from every thread land here.",
                    None,
                    cx,
                ))
                .into_any_element();
        }

        let card = |thread: &Thread| {
            let id = thread.id;
            v_flex()
                .p_3()
                .gap_2()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(
                    h_flex()
                        .id(("inbox-head", id.0))
                        .gap_2()
                        .cursor_default()
                        .on_click(cx.listener(move |this, _, window, cx| this.open(id, window, cx)))
                        .child(ui::StatusGlyph::new(("inbox-glyph", id.0), thread))
                        .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(display_title(thread).to_owned()))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(project_name(thread)))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(format::relative_time(thread.updated_ms, now))),
                )
        };

        let mut list = v_flex().w_full().max_w(crate::settings::chat_column(cx)).gap_3().px_4().py_6();
        if !waiting.is_empty() {
            list = list.child(ui::section_label(format!("Needs you · {}", waiting.iter().map(|t| t.pending.len().max(1)).sum::<usize>()), cx));
            for thread in &waiting {
                let mut c = card(thread);
                for pending in &thread.pending {
                    let (icon, title, detail) = request_summary(&pending.request.kind);
                    let thread_id = thread.id;
                    let request = pending.request.id;
                    let app = self.app.clone();
                    let answer = move |decision: Decision| {
                        let app = app.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let decision = decision.clone();
                            app.update(cx, |app, cx| app.run(Command::Resolve { thread: thread_id, request, decision }, window, cx));
                        }
                    };
                    let simple = matches!(pending.request.kind, RequestKind::Command { .. } | RequestKind::FileChange { .. } | RequestKind::Tool { .. });
                    c = c.child(
                        v_flex()
                            .gap_1p5()
                            .child(h_flex().gap_2().text_sm().child(icon.small().text_color(theme.warning)).child(title))
                            .when_some(detail, |el, detail| {
                                el.child(
                                    div()
                                        .p_2()
                                        .rounded(theme.radius)
                                        .bg(theme.background)
                                        .border_1()
                                        .border_color(theme.border)
                                        .font_family(theme.mono_font_family.clone())
                                        .text_xs()
                                        .line_clamp(4)
                                        .child(detail),
                                )
                            })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_end()
                                    .when(simple, |el| {
                                        el.child(Button::new(("inbox-deny", request.0)).outline().small().label("Deny").on_click(answer(Decision::Deny { feedback: String::new() })))
                                            .child(Button::new(("inbox-session", request.0)).small().label("Allow for this session").on_click(answer(Decision::AllowForSession)))
                                            .child(Button::new(("inbox-allow", request.0)).primary().small().label("Allow").on_click(answer(Decision::Allow)))
                                    })
                                    .when(!simple, |el| {
                                        el.child(
                                            Button::new(("inbox-open", request.0))
                                                .small()
                                                .label("Answer in thread")
                                                .on_click(cx.listener(move |this, _, window, cx| this.open(thread_id, window, cx))),
                                        )
                                    }),
                            ),
                    );
                }
                list = list.child(c);
            }
        }
        for (label, threads, detail) in [
            ("Failed", &failed, true),
            ("Waiting for quota", &limited, true),
            ("New results", &unread, false),
        ] {
            if threads.is_empty() {
                continue;
            }
            list = list.child(ui::section_label(format!("{label} · {}", threads.len()), cx));
            for thread in threads.iter() {
                let message = match &thread.status {
                    ThreadStatus::Failed { message } if detail => Some(message.clone()),
                    ThreadStatus::RateLimited { resets_ms } if detail => Some(format::resets_at(*resets_ms, now)),
                    _ => None,
                };
                list = list.child(card(thread).when_some(message, |el, m| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).line_clamp(2).child(m))
                }));
            }
        }
        v_flex().size_full().id("inbox").overflow_y_scroll().items_center().child(list).into_any_element()
    }
}
