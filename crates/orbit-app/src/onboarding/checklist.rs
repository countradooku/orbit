//! The activation checklist on Home: five first wins, each with the button
//! that gets it done. It leaves once every step is done or it is dismissed.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::*;
use crate::assets::Lucide;

gpui_kit::actions!(
    orbit,
    [
        /// Opens the most recent thread with its review.
        ReviewLatestThread,
        /// Arms the Home composer for a two-agent race.
        SetUpRace,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    AddProject,
    RunAgent,
    ReviewDiff,
    TryRace,
    UsePalette,
}

impl Step {
    pub const ALL: [Step; 5] = [Step::AddProject, Step::RunAgent, Step::ReviewDiff, Step::TryRace, Step::UsePalette];

    fn label(self) -> &'static str {
        match self {
            Step::AddProject => "Add a project",
            Step::RunAgent => "Run your first agent",
            Step::ReviewDiff => "Review a diff",
            Step::TryRace => "Try a race",
            Step::UsePalette => "Open the command palette",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Step::AddProject => "A folder or repository for agents to work in.",
            Step::RunAgent => "Describe a task above and send it.",
            Step::ReviewDiff => "See what an agent changed, file by file, and keep or revert each part.",
            Step::TryRace => "Send one prompt to two agents and compare their work side by side.",
            Step::UsePalette => "Every command and thread, a few keys away.",
        }
    }

    fn id(self) -> usize {
        self as usize
    }
}

/// What the checklist knows, as plain data (tested without a window).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub projects: bool,
    pub threads: bool,
    pub races: bool,
    pub reviewed_diff: bool,
    pub used_palette: bool,
}

impl Facts {
    pub fn is_done(&self, step: Step) -> bool {
        match step {
            Step::AddProject => self.projects,
            Step::RunAgent => self.threads,
            Step::ReviewDiff => self.reviewed_diff,
            Step::TryRace => self.races,
            Step::UsePalette => self.used_palette,
        }
    }

    pub fn done_count(&self) -> usize {
        Step::ALL.iter().filter(|s| self.is_done(**s)).count()
    }

    /// Why a step can't be started yet, if it can't.
    pub fn blocked(&self, step: Step) -> Option<&'static str> {
        match step {
            Step::ReviewDiff if !self.threads => Some("Run an agent first: its changes are what you review."),
            Step::TryRace if !self.projects => Some("Add a project first: races run in a project."),
            _ => None,
        }
    }
}

fn facts(cx: &App) -> Option<Facts> {
    let app = super::app_state(cx)?;
    let state = app.read(cx).state();
    let progress = super::progress(cx);
    Some(Facts {
        projects: !state.projects.is_empty(),
        threads: !state.threads.is_empty(),
        races: !state.races.is_empty(),
        reviewed_diff: progress.reviewed_diff,
        used_palette: progress.used_palette,
    })
}

/// Whether Home shows the checklist.
pub fn is_visible(cx: &App) -> bool {
    let progress = super::progress(cx);
    // Before the first-run decision there is no record: nothing to show.
    let decided = cx.try_global::<super::Onboarding>().is_some_and(|o| o.progress.is_some());
    decided && !progress.checklist_dismissed && facts(cx).is_some_and(|f| f.done_count() < Step::ALL.len())
}

fn step_action(step: Step) -> (&'static str, Box<dyn Action>) {
    match step {
        Step::AddProject => ("Add project…", Box::new(AddProject)),
        Step::RunAgent => ("Write a task", Box::new(FocusComposer)),
        Step::ReviewDiff => ("Open review", Box::new(ReviewLatestThread)),
        Step::TryRace => ("Set up a race", Box::new(SetUpRace)),
        Step::UsePalette => ("Open palette", Box::new(OpenPalette)),
    }
}

/// The checklist card, or nothing.
pub fn render(cx: &App) -> Option<AnyElement> {
    if !is_visible(cx) {
        return None;
    }
    let facts = facts(cx)?;
    let theme = cx.theme().clone();
    let done = facts.done_count();
    let total = Step::ALL.len();
    let next = Step::ALL.into_iter().find(|s| !facts.is_done(*s) && facts.blocked(*s).is_none());
    let rows = Step::ALL.into_iter().map(|step| {
        let is_done = facts.is_done(step);
        let blocked = facts.blocked(step);
        let (verb, action) = step_action(step);
        let keys = (!is_done && blocked.is_none()).then(|| crate::ui::keycaps(action.as_ref(), cx)).flatten();
        h_flex()
            .id(("checklist-step", step.id()))
            .w_full()
            .min_h_10()
            .px_3()
            .gap_3()
            .child(
                div().flex_shrink_0().child(if is_done {
                    Icon::new(IconName::CircleCheck).small().text_color(theme.success)
                } else {
                    Icon::new(Lucide::Circle).small().text_color(theme.muted_foreground)
                }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .py_1p5()
                    .child(
                        div()
                            .text_sm()
                            .when(is_done, |el| el.text_color(theme.muted_foreground))
                            .when(!is_done, |el| el.font_weight(FontWeight::MEDIUM))
                            .child(step.label()),
                    )
                    .when(!is_done, |el| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(blocked.unwrap_or(step.help())))
                    }),
            )
            .when(!is_done, |el| {
                el.when_some(keys, |el, keys| el.child(div().text_xs().text_color(theme.muted_foreground).child(keys)))
                    .child(
                        Button::new(("checklist-do", step.id()))
                            .small()
                            .map(|b| if Some(step) == next { b.outline() } else { b.ghost() })
                            .label(verb)
                            .disabled(blocked.is_some())
                            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)),
                    )
            })
    });
    Some(
        v_flex()
            .id("activation-checklist")
            .w_full()
            .mt_6()
            .py_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .px_3()
                    .pb_1()
                    .gap_2()
                    .child(Icon::new(Lucide::ListChecks).small().text_color(theme.primary))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Get started"))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{done} of {total}")))
                    .child(div().flex_1())
                    .child(
                        Button::new("checklist-dismiss")
                            .ghost()
                            .xsmall()
                            .label("Dismiss")
                            .on_click(|_, _, cx| super::dismiss_checklist(cx)),
                    ),
            )
            .child(
                div().px_3().pb_1().child(
                    ProgressBar::new("checklist-progress")
                        .value(done as f32 / total as f32 * 100.0)
                        .accessibility_label(format!("{done} of {total} steps done")),
                ),
            )
            .children(rows)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would bring in `gpui_kit::test` in place of `#[test]`.
    use super::{Facts, Step};

    #[test]
    fn steps_unlock_in_order_and_count_up() {
        let mut facts = Facts::default();
        assert_eq!(facts.done_count(), 0);
        assert!(facts.blocked(Step::ReviewDiff).is_some());
        assert!(facts.blocked(Step::TryRace).is_some());
        assert!(facts.blocked(Step::AddProject).is_none());
        facts.projects = true;
        facts.threads = true;
        assert!(facts.blocked(Step::ReviewDiff).is_none());
        assert!(facts.blocked(Step::TryRace).is_none());
        assert_eq!(facts.done_count(), 2);
        facts = Facts { projects: true, threads: true, races: true, reviewed_diff: true, used_palette: true };
        assert_eq!(facts.done_count(), Step::ALL.len());
    }
}
