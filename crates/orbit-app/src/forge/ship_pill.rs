//! The ship pill: one control from an edit to a merge.
//!
//! It shows where a thread's work stands (uncommitted → committed → pushed →
//! pull request open → checks → mergeable or merged) and offers the next
//! step as its click. The pill is presentational: it takes a [`ShipState`]
//! (from `ForgeCommand::Ship`, see [`fetch`]) and reports the
//! [`ShipAction`] clicked; [`perform`] runs the forge-side actions, and the
//! mounting view handles the git-side ones (commit, push, create the pull
//! request) with its own dialogs.

use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::glyph;
use crate::app_state::AppState;
use crate::assets::Lucide;

/// The next step the pill offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShipAction {
    /// Commit the changes (the review's commit dialog).
    Commit,
    /// Push the branch.
    Push,
    /// Open a pull request (the review's create dialog).
    CreatePullRequest,
    /// Send the failing checks to the thread's agent.
    FixChecks,
    /// Merge (after a confirmation).
    Merge,
    /// Show the pull request.
    View,
}

impl ShipAction {
    pub fn label(self, forge: Option<ForgeKind>) -> String {
        match self {
            ShipAction::Commit => "Commit…".into(),
            ShipAction::Push => "Push".into(),
            ShipAction::CreatePullRequest => match forge {
                Some(ForgeKind::GitLab) => "Create merge request…".into(),
                _ => "Create pull request…".into(),
            },
            ShipAction::FixChecks => "Fix checks".into(),
            ShipAction::Merge => "Merge…".into(),
            ShipAction::View => "View".into(),
        }
    }
}

/// What clicking the pill does in `state`, if anything.
pub fn next_action(state: &ShipState) -> Option<ShipAction> {
    match state.stage {
        ShipStage::NoRepo | ShipStage::Clean => None,
        ShipStage::Uncommitted => Some(ShipAction::Commit),
        ShipStage::Committed => Some(ShipAction::Push),
        // Without a host there is nothing to open a pull request on.
        ShipStage::Pushed if state.forge.is_some() && state.note.is_none() => Some(ShipAction::CreatePullRequest),
        ShipStage::Pushed => None,
        ShipStage::ChecksFailed => Some(ShipAction::FixChecks),
        ShipStage::Mergeable if !state.auto_merge => Some(ShipAction::Merge),
        ShipStage::ChecksRunning | ShipStage::Blocked | ShipStage::Mergeable | ShipStage::Merged | ShipStage::Closed => {
            Some(ShipAction::View)
        }
    }
}

/// Reads a thread's ship state. `refresh` after the user acted (commit,
/// push) or when the window regains focus.
pub fn fetch(app: &Entity<AppState>, thread: ThreadId, refresh: bool, cx: &mut App) -> Task<Result<ShipState, String>> {
    let task = app.update(cx, |app, cx| app.call(Command::Forge(ForgeCommand::Ship { thread, refresh }), cx));
    cx.background_spawn(async move {
        match task.await? {
            Reply::Forge(ForgeReply::Ship(state)) => Ok(*state),
            _ => Err("Unexpected reply.".into()),
        }
    })
}

/// Runs the forge-side actions for `thread` (fix checks, merge, view) and
/// returns `false` for the git-side ones the caller handles.
pub fn perform(app: &Entity<AppState>, thread: ThreadId, state: &ShipState, action: ShipAction, window: &mut Window, cx: &mut App) -> bool {
    let Some(pr) = state.pr.clone() else { return false };
    let Some(project) = app.read(cx).state().threads.get(&thread).and_then(|t| t.project) else { return false };
    match action {
        ShipAction::FixChecks => {
            let config = app.read(cx).state().threads.get(&thread).map(|t| t.config.clone()).unwrap_or_default();
            let command = ForgeCommand::FixChecks { project, number: pr.number, thread: Some(thread), config };
            app.update(cx, |app, cx| app.run(Command::Forge(command), window, cx));
            true
        }
        ShipAction::Merge => {
            let app = app.clone();
            let sigil = state.forge.map(|f| f.review_sigil()).unwrap_or("#");
            window.open_alert_dialog(cx, move |alert, _, _| {
                let app = app.clone();
                let number = pr.number;
                alert
                    .title(format!("Merge {sigil}{number}?"))
                    .description(format!("“{}” goes into {} and the {} branch is deleted afterwards.", pr.title, pr.base, pr.head))
                    .show_cancel(true)
                    .ok_text("Merge")
                    .on_ok(move |_, window, cx| {
                        let command = ForgeCommand::Merge {
                            project,
                            number,
                            method: MergeMethod::Squash,
                            auto: false,
                            delete_branch: true,
                            head_sha: String::new(),
                        };
                        app.update(cx, |app, cx| app.run(Command::Forge(command), window, cx));
                        true
                    })
            });
            true
        }
        ShipAction::View => {
            cx.open_url(&pr.url);
            true
        }
        _ => false,
    }
}

type OnAction = Rc<dyn Fn(ShipAction, &mut Window, &mut App)>;

/// One control showing where the work stands, whose click is the next step.
#[derive(IntoElement)]
pub struct ShipPill {
    id: ElementId,
    state: ShipState,
    loading: bool,
    on_action: Option<OnAction>,
}

impl ShipPill {
    pub fn new(id: impl Into<ElementId>, state: ShipState) -> Self {
        Self { id: id.into(), state, loading: false, on_action: None }
    }

    /// Shows a spinner while the next step runs.
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    pub fn on_action(mut self, handler: impl Fn(ShipAction, &mut Window, &mut App) + 'static) -> Self {
        self.on_action = Some(Rc::new(handler));
        self
    }
}

/// The pill's status text: what is true now.
pub fn status_text(state: &ShipState) -> String {
    let sigil = state.forge.map(|f| f.review_sigil()).unwrap_or("#");
    let pr = state.pr.as_ref().map(|p| format!("{sigil}{}", p.number)).unwrap_or_default();
    let plural = |n: u32, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    match state.stage {
        ShipStage::NoRepo => String::new(),
        ShipStage::Clean => "No changes".into(),
        ShipStage::Uncommitted => plural(state.files, "file", "files"),
        ShipStage::Committed => format!("{} to push", plural(state.ahead, "commit", "commits")),
        ShipStage::Pushed => "Pushed".into(),
        ShipStage::ChecksRunning => {
            let c = state.pr.as_ref().map(|p| p.checks).unwrap_or_default();
            format!("{pr} · checks {}/{}", c.passed + c.failed + c.other, c.total())
        }
        ShipStage::ChecksFailed => {
            let failed = state.pr.as_ref().map(|p| u32::from(p.checks.failed)).unwrap_or(1);
            format!("{pr} · {} failing", failed)
        }
        ShipStage::Blocked => {
            let why = match state.pr.as_ref().map(|p| (p.state, p.review)) {
                Some((PrState::Draft, _)) => "draft".to_owned(),
                Some((_, ReviewDecision::ChangesRequested)) => "changes requested".to_owned(),
                Some((_, ReviewDecision::ReviewRequired)) => "review required".to_owned(),
                _ => state.mergeable.label().to_lowercase(),
            };
            format!("{pr} · {why}")
        }
        ShipStage::Mergeable if state.auto_merge => format!("{pr} · auto-merge on"),
        ShipStage::Mergeable => format!("{pr} · ready"),
        ShipStage::Merged => format!("{pr} merged"),
        ShipStage::Closed => format!("{pr} closed"),
    }
}

impl RenderOnce for ShipPill {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.state;
        if state.stage == ShipStage::NoRepo {
            return div().into_any_element();
        }
        let action = next_action(&state);
        let glyph_id = ElementId::NamedChild(std::sync::Arc::new(self.id.clone()), "glyph".into());
        let glyph: AnyElement = match state.stage {
            ShipStage::Clean => Icon::new(IconName::Check).xsmall().text_color(theme.muted_foreground).into_any_element(),
            ShipStage::Uncommitted => Icon::new(Lucide::GitCommitHorizontal).xsmall().text_color(theme.warning).into_any_element(),
            ShipStage::Committed => Icon::new(IconName::ArrowUp).xsmall().text_color(theme.info).into_any_element(),
            ShipStage::Pushed => Icon::new(Lucide::GitBranch).xsmall().text_color(theme.muted_foreground).into_any_element(),
            ShipStage::ChecksRunning => glyph::spinner(glyph_id, theme.warning),
            ShipStage::ChecksFailed => Icon::new(IconName::CircleX).xsmall().text_color(theme.danger).into_any_element(),
            ShipStage::Blocked => Icon::new(Lucide::GitPullRequest).xsmall().text_color(theme.warning).into_any_element(),
            ShipStage::Mergeable => Icon::new(Lucide::GitPullRequest).xsmall().text_color(theme.success).into_any_element(),
            ShipStage::Merged => Icon::new(Lucide::GitMerge).xsmall().text_color(theme.magenta).into_any_element(),
            ShipStage::Closed | ShipStage::NoRepo => Icon::new(IconName::CircleX).xsmall().text_color(theme.muted_foreground).into_any_element(),
        };
        let counts = (state.stage == ShipStage::Uncommitted && state.additions + state.deletions > 0).then(|| {
            h_flex()
                .gap_1()
                .child(div().text_color(theme.success).child(format!("+{}", state.additions)))
                .child(div().text_color(theme.danger).child(format!("−{}", state.deletions)))
        });
        let tooltip = match (action, &state.note) {
            (_, Some(note)) if state.stage == ShipStage::Pushed => note.clone(),
            (Some(a), _) => format!("Next: {}", a.label(state.forge).trim_end_matches('…')),
            (None, _) => status_text(&state),
        };
        let on_action = self.on_action.clone();
        Button::new(self.id)
            .small()
            .outline()
            .loading(self.loading)
            .disabled(action.is_none() || on_action.is_none())
            .tooltip(tooltip)
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .child(h_flex().size_4().flex_none().justify_center().child(glyph))
                    .children(counts)
                    .when(state.stage == ShipStage::Uncommitted && state.additions + state.deletions > 0, |el| {
                        el.child(div().text_color(theme.muted_foreground).child("·"))
                    })
                    .child(status_text(&state))
                    .when_some(action, |el, a| {
                        el.child(div().h_3().w(px(1.)).bg(theme.border)).child(
                            div().font_weight(FontWeight::MEDIUM).child(a.label(state.forge)),
                        )
                    }),
            )
            .when_some(action.zip(on_action), |button, (a, handler)| {
                button.on_click(move |_, window, cx| handler(a, window, cx))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use orbit_proto::*;

    use super::{ShipAction, next_action, status_text};

    fn with(stage: ShipStage) -> ShipState {
        ShipState { stage, forge: Some(ForgeKind::GitHub), ..Default::default() }
    }

    #[test]
    fn every_stage_offers_its_next_step() {
        assert_eq!(next_action(&with(ShipStage::NoRepo)), None);
        assert_eq!(next_action(&with(ShipStage::Clean)), None);
        assert_eq!(next_action(&with(ShipStage::Uncommitted)), Some(ShipAction::Commit));
        assert_eq!(next_action(&with(ShipStage::Committed)), Some(ShipAction::Push));
        assert_eq!(next_action(&with(ShipStage::Pushed)), Some(ShipAction::CreatePullRequest));
        let mut signed_out = with(ShipStage::Pushed);
        signed_out.note = Some("gh isn’t signed in".into());
        assert_eq!(next_action(&signed_out), None, "no pull request without a host");
        assert_eq!(next_action(&with(ShipStage::ChecksFailed)), Some(ShipAction::FixChecks));
        assert_eq!(next_action(&with(ShipStage::Mergeable)), Some(ShipAction::Merge));
        let mut auto = with(ShipStage::Mergeable);
        auto.auto_merge = true;
        assert_eq!(next_action(&auto), Some(ShipAction::View));
        assert_eq!(next_action(&with(ShipStage::Merged)), Some(ShipAction::View));
    }

    #[test]
    fn status_reads_like_the_state() {
        let mut s = with(ShipStage::Uncommitted);
        s.files = 6;
        assert_eq!(status_text(&s), "6 files");
        s.stage = ShipStage::Committed;
        s.ahead = 1;
        assert_eq!(status_text(&s), "1 commit to push");
        s.stage = ShipStage::ChecksRunning;
        s.pr = Some(PullRequest { number: 12, checks: ChecksSummary { passed: 3, pending: 2, ..Default::default() }, ..Default::default() });
        assert_eq!(status_text(&s), "#12 · checks 3/5");
        s.stage = ShipStage::Blocked;
        s.pr.as_mut().unwrap().review = ReviewDecision::ChangesRequested;
        assert_eq!(status_text(&s), "#12 · changes requested");
        s.forge = Some(ForgeKind::GitLab);
        s.stage = ShipStage::Merged;
        assert_eq!(status_text(&s), "!12 merged");
    }
}
