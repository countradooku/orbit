//! Status glyphs for pull requests, checks and issues. Each pairs a shape
//! with a color, so state never depends on color alone, and sits in a fixed
//! slot so rows keep their columns.

use std::time::Duration;

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::*;
use orbit_proto::*;

use crate::assets::Lucide;

fn slot() -> Div {
    h_flex().size_4().flex_none().justify_center()
}

/// A slowly turning ring for work in progress.
pub fn spinner(id: impl Into<ElementId>, color: Hsla) -> AnyElement {
    Icon::new(IconName::LoaderCircle)
        .xsmall()
        .text_color(color)
        .with_animation(id.into(), Animation::new(Duration::from_millis(900)).repeat(), |icon, t| {
            icon.transform(Transformation::rotate(percentage(t)))
        })
        .into_any_element()
}

pub fn check(id: impl Into<ElementId>, state: CheckState, cx: &App) -> Div {
    let theme = cx.theme();
    slot().child(match state {
        CheckState::Passed => Icon::new(IconName::CircleCheck).xsmall().text_color(theme.success).into_any_element(),
        CheckState::Failed => Icon::new(IconName::CircleX).xsmall().text_color(theme.danger).into_any_element(),
        CheckState::Running => spinner(id, theme.warning),
        CheckState::Queued => Icon::new(Lucide::Clock).xsmall().text_color(theme.muted_foreground).into_any_element(),
        CheckState::Cancelled => Icon::new(IconName::Ban).xsmall().text_color(theme.muted_foreground).into_any_element(),
        CheckState::Skipped | CheckState::Neutral => {
            Icon::new(IconName::Minus).xsmall().text_color(theme.muted_foreground).into_any_element()
        }
    })
}

/// A pull request's state, with its checks when it is open: failing checks
/// and running checks are what a reader of the list needs first.
pub fn pull_request(id: impl Into<ElementId>, pr: &PullRequest, cx: &App) -> Div {
    let theme = cx.theme();
    if pr.state.is_open() {
        match pr.checks.overall() {
            Some(CheckState::Failed) => return check(id, CheckState::Failed, cx),
            Some(CheckState::Running | CheckState::Queued) => return check(id, CheckState::Running, cx),
            _ => {}
        }
    }
    let (icon, color) = match pr.state {
        PrState::Open => (Icon::new(Lucide::GitPullRequest), theme.success),
        PrState::Draft => (Icon::new(Lucide::GitPullRequest), theme.muted_foreground),
        PrState::Merged => (Icon::new(Lucide::GitMerge), theme.magenta),
        PrState::Closed => (Icon::new(IconName::CircleX), theme.danger),
    };
    slot().child(icon.xsmall().text_color(color))
}

pub fn pr_state_color(state: PrState, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        PrState::Open => theme.success,
        PrState::Draft => theme.muted_foreground,
        PrState::Merged => theme.magenta,
        PrState::Closed => theme.danger,
    }
}

pub fn issue(state: IssueState, cx: &App) -> Div {
    let theme = cx.theme();
    let (icon, color) = match state {
        IssueState::Open => (Icon::new(Lucide::CircleDot), theme.success),
        IssueState::Started => (Icon::new(Lucide::CircleDot), theme.warning),
        IssueState::Closed => (Icon::new(IconName::CircleCheck), theme.magenta),
    };
    slot().child(icon.xsmall().text_color(color))
}
