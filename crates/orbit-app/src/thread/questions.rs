//! The question panel: the agent's questions, answered where you type.
//!
//! * Every pending question of the thread is one step of a questionnaire:
//!   "2 of 3", Previous and Next, one Submit at the end that answers them
//!   all at once (the agent waits for the whole set).
//! * Choices take a number key (1–9); a single choice confirms itself and
//!   moves on; several choices are checkboxes when the agent allows it.
//! * Every question also takes your own words, so a reply is never limited
//!   to the agent's guesses.
//! * Skip tells the agent you chose not to answer, instead of leaving it
//!   waiting.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::questionnaire::*;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use orbit_proto::*;

use super::ThreadView;
use crate::assets::Lucide;

/// One pending question.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub request: RequestId,
    pub text: String,
    pub options: Vec<String>,
    pub multi: bool,
}

impl Question {
    fn name(&self) -> SharedString {
        self.request.0.to_string().into()
    }
}

/// The thread's unanswered questions, oldest first, with blank and repeated
/// options dropped.
pub fn pending(thread: &Thread) -> Vec<Question> {
    thread
        .pending
        .iter()
        .filter_map(|p| match &p.request.kind {
            RequestKind::Question { question, options, multi_select } => {
                let mut unique: Vec<String> = Vec::with_capacity(options.len());
                for option in options {
                    if !option.trim().is_empty() && !unique.contains(option) {
                        unique.push(option.clone());
                    }
                }
                Some(Question { request: p.request.id, text: question.clone(), options: unique, multi: *multi_select })
            }
            _ => None,
        })
        .collect()
}

/// The answer the agent receives: chosen labels one per line (the protocol's
/// multi-select form), then your own words.
pub fn answer_text(choices: &[SharedString], freeform: Option<&str>) -> String {
    let mut lines: Vec<&str> = choices.iter().map(|c| c.as_ref()).collect();
    if let Some(own) = freeform.map(str::trim).filter(|t| !t.is_empty()) {
        lines.push(own);
    }
    lines.join("\n")
}

pub struct Questions {
    pub questions: Vec<Question>,
    state: Entity<QuestionnaireState>,
    _subscription: Subscription,
}

impl Questions {
    pub fn new(questions: Vec<Question>, window: &mut Window, cx: &mut Context<ThreadView>) -> Option<Self> {
        let items: Vec<QuestionnaireItemDefinition> = questions
            .iter()
            .map(|q| {
                let placeholder = if q.options.is_empty() { "Your answer" } else { "Or answer in your own words" };
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                QuestionnaireItemDefinition::new(q.name(), q.text.clone())
                    .with_required(true)
                    .with_multiple(q.multi)
                    .with_choices(q.options.iter().map(|o| QuestionnaireChoiceDefinition::new(o.clone(), o.clone())))
                    .with_input(QuestionnaireInputDefinition::new(input, "Your own answer"))
            })
            .collect();
        let state = cx.new(|cx| match QuestionnaireState::new(items, cx) {
            Ok(state) => state.with_shortcuts(QuestionnaireShortcutMode::Numbers),
            Err(err) => {
                log::warn!("question panel: {err:?}");
                QuestionnaireState::new(Vec::new(), cx).expect("an empty questionnaire is valid")
            }
        });
        if state.read(cx).total() == 0 {
            return None;
        }
        let subscription = cx.subscribe_in(&state, window, |this, _, event: &QuestionnaireEvent, window, cx| {
            if let QuestionnaireEvent::Submit(submission) = event {
                this.answer_questions(submission, window, cx);
            }
        });
        Some(Self { questions, state, _subscription: subscription })
    }

    /// Each question's answer, ready to send.
    pub fn answers(&self, submission: &QuestionnaireSubmission) -> Vec<(RequestId, String)> {
        self.questions
            .iter()
            .map(|q| {
                let answer = submission.answer(&q.name());
                let text = answer.map(|a| answer_text(a.choices(), a.freeform().map(|s| s.as_ref()))).unwrap_or_default();
                (q.request, text)
            })
            .collect()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let state = self.state.read(cx);
        let target = state.current_item().and_then(|item| state.item_focus_handle(item)).cloned();
        if let Some(handle) = target {
            handle.focus(window, cx);
        }
    }

    pub fn render(&self, cx: &mut Context<ThreadView>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = &self.state;
        let count = self.questions.len();
        let header = h_flex()
            .gap_2()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .child(Icon::new(Lucide::MessageSquare).small().text_color(theme.warning))
            .child(if count == 1 { "The agent has a question".to_owned() } else { format!("The agent has {count} questions") })
            .child(div().flex_1())
            .when(count > 1, |el| el.child(QuestionnaireProgress::new(state).xsmall()))
            .child(
                Button::new("skip-questions")
                    .ghost()
                    .xsmall()
                    .label(if count == 1 { "Skip" } else { "Skip all" })
                    .tooltip("Tell the agent you’d rather not answer")
                    .on_click(cx.listener(|this, _, window, cx| this.skip_questions(window, cx))),
            );
        let mut questionnaire = Questionnaire::new(state).small();
        for q in &self.questions {
            let name = q.name();
            questionnaire = questionnaire.child(
                QuestionnaireItem::new(state, name.clone())
                    .child(QuestionnaireTitle::new(state, name.clone()))
                    .child(QuestionnaireChoices::new(state, name.clone()).children(
                        q.options.iter().map(|o| QuestionnaireChoice::new(state, name.clone(), o.clone()).child(o.clone())),
                    ))
                    .child(QuestionnaireInput::new(state, name.clone()))
                    .child(QuestionnaireError::new(state, name)),
            );
        }
        questionnaire = questionnaire.child(
            QuestionnaireActions::new(state)
                .child(QuestionnairePrevious::new(state))
                .child(QuestionnaireNext::new(state))
                .child(QuestionnaireSubmit::new(state)),
        );
        v_flex()
            .mb_2()
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.warning.opacity(0.55))
            .bg(theme.background)
            .child(header)
            .child(questionnaire)
            .into_any_element()
    }
}

use gpui_kit::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::{answer_text, pending};
    use gpui_kit::SharedString;
    use orbit_proto::*;

    #[test]
    fn answers_put_each_choice_on_its_own_line_then_your_words() {
        let choices: Vec<SharedString> = vec!["Red, warm".into(), "Blue".into()];
        assert_eq!(answer_text(&choices, Some("  and green ")), "Red, warm\nBlue\nand green");
        assert_eq!(answer_text(&[], Some("just this")), "just this");
        assert_eq!(answer_text(&choices[..1], Some("   ")), "Red, warm");
    }

    #[test]
    fn pending_questions_drop_blank_and_repeated_options() {
        let request = |kind| PendingRequest {
            item: ItemId(1),
            request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
            created_ms: 0,
        };
        let thread = Thread {
            pending: vec![
                request(RequestKind::Command { command: "ls".into(), cwd: String::new(), reason: String::new() }),
                request(RequestKind::Question {
                    question: "Which?".into(),
                    options: vec!["A".into(), " ".into(), "A".into(), "B".into()],
                    multi_select: true,
                }),
            ],
            ..Default::default()
        };
        let questions = pending(&thread);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].options, vec!["A", "B"]);
        assert!(questions[0].multi);
    }
}
