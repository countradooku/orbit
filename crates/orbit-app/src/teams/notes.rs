//! The notes board: the team's shared blackboard for API shapes and
//! decisions. Agents post with `team_post_note`; the user posts here. New
//! workers' briefs quote the newest notes.

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::{AppEvent, AppState};
use crate::format;

pub struct NotesBoard {
    app: Entity<AppState>,
    team: TeamId,
    input: Entity<InputState>,
    posting: bool,
    _subscriptions: Vec<Subscription>,
}

impl NotesBoard {
    pub fn new(app: Entity<AppState>, team: TeamId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Post a decision or an API shape…"));
        let subs = vec![
            cx.subscribe(&app, |_, _, event: &AppEvent, cx| {
                if matches!(event, AppEvent::Teams) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.post(window, cx);
                }
            }),
        ];
        Self { app, team, input, posting: false, _subscriptions: subs }
    }

    fn post(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || self.posting {
            return;
        }
        self.posting = true;
        cx.notify();
        let task = self
            .app
            .update(cx, |app, cx| app.call(Command::Team(TeamCommand::PostNote { actor: None, team: Some(self.team), text }), cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.posting = false;
                match result {
                    Ok(_) => this.input.update(cx, |input, cx| input.set_value("", window, cx)),
                    Err(message) => {
                        use gpui_kit::component::WindowExt as _;
                        use gpui_kit::component::notification::Notification;
                        window.push_notification(Notification::error(message), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for NotesBoard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let notes: Vec<Note> = state.teams.get(&self.team).map(|t| t.notes.iter().rev().cloned().collect()).unwrap_or_default();
        let now = now_ms();
        let empty = notes.is_empty();
        v_flex()
            .size_full()
            .min_h_0()
            .bg(theme.background)
            .child(
                h_flex()
                    .h_9()
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(IconName::FileText).xsmall().text_color(theme.muted_foreground))
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Notes"))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(if empty { String::new() } else { notes.len().to_string() })),
            )
            .child(
                h_flex()
                    .flex_none()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Input::new(&self.input).small()))
                    .child(
                        Button::new("note-post")
                            .small()
                            .label("Post")
                            .loading(self.posting)
                            .on_click(cx.listener(|this, _, window, cx| this.post(window, cx))),
                    ),
            )
            .child(
                div()
                    .id(("notes", self.team.0))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(empty, |el| {
                        el.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("Decisions every worker should know land here: API shapes, names, conventions."),
                        )
                    })
                    .children(notes.into_iter().map(|n| {
                        v_flex()
                            .px_3()
                            .py_2()
                            .gap_1()
                            .border_b_1()
                            .border_color(theme.border.opacity(0.6))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(n.author_label.clone()))
                                    .child(format::relative_time(n.at_ms, now)),
                            )
                            .child(div().text_sm().child(n.text.clone()))
                    })),
            )
    }
}
