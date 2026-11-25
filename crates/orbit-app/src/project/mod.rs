//! Projects: their settings page, run targets and live git status.
//!
//! * [`settings`] is Settings › Projects: name, icon, base branch, the agent
//!   new threads start with, worktree scripts, run targets, shared files,
//!   ports, environment, import from the repository, archive and remove.
//! * [`runs`] runs a project's targets in a thread's workspace: the header's
//!   Run control, the palette's rows and the terminal dock's tabs.
//! * [`GitBadge`] is a checkout's branch, ahead/behind, changes and
//!   conflicts, as the sidebar and the thread header show them.

pub mod runs;
pub mod settings;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::assets::Lucide;

pub use runs::{RunDock, RunDockEvent};

/// Binds the projects' keys.
pub fn init(cx: &mut App) {
    runs::init(cx);
}

/// A checkout's git status in one line: branch, commits ahead and behind,
/// changed files and conflicts. Words carry the meaning; color repeats it.
#[derive(IntoElement)]
pub struct GitBadge {
    id: ElementId,
    summary: Option<GitSummary>,
    /// Shown while no status has arrived (a worktree's own branch).
    fallback_branch: Option<String>,
    compact: bool,
}

impl GitBadge {
    pub fn new(id: impl Into<ElementId>, summary: Option<&GitSummary>) -> Self {
        Self { id: id.into(), summary: summary.cloned(), fallback_branch: None, compact: false }
    }

    pub fn fallback_branch(mut self, branch: Option<String>) -> Self {
        self.fallback_branch = branch;
        self
    }

    /// Counts only, for dense rows (the sidebar).
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
}

/// Shown for folders that are not git repositories.
pub const NOT_A_REPO: &str = "Not a git repository: checkpoints are kept by Orbit.";

/// What a summary says in words, for tooltips and accessible names.
pub fn describe(summary: &GitSummary) -> String {
    let mut parts = Vec::new();
    match &summary.branch {
        Some(branch) => parts.push(format!("On {branch}")),
        None => parts.push(format!("Detached at {}", summary.head.as_deref().unwrap_or("HEAD"))),
    }
    if let Some(base) = &summary.compared_to {
        let commits = |n: u32| if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") };
        match (summary.ahead, summary.behind) {
            (0, 0) => parts.push(format!("up to date with {base}")),
            (a, 0) => parts.push(format!("{} ahead of {base}", commits(a))),
            (0, b) => parts.push(format!("{} behind {base}", commits(b))),
            (a, b) => parts.push(format!("{} ahead of {base}, {b} behind", commits(a))),
        }
    }
    match summary.changed {
        0 => parts.push("no uncommitted changes".into()),
        1 => parts.push(format!("1 changed file (+{} −{})", summary.additions, summary.deletions)),
        n => parts.push(format!("{n} changed files (+{} −{})", summary.additions, summary.deletions)),
    }
    match summary.conflicts {
        0 => {}
        1 => parts.push("1 file with merge conflicts".into()),
        n => parts.push(format!("{n} files with merge conflicts")),
    }
    let mut text = parts.join(", ");
    text.push('.');
    text
}

impl RenderOnce for GitBadge {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        if self.summary.as_ref().is_some_and(|s| !s.repo) && self.fallback_branch.is_none() {
            // Checkpoints still work, kept by Orbit; say so calmly.
            return h_flex()
                .id(self.id)
                .flex_none()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::Folder).xsmall())
                .child("No git")
                .tooltip(|window, cx| Tooltip::new(NOT_A_REPO).build(window, cx));
        }
        let summary = self.summary.filter(|s| s.repo);
        let branch = summary
            .as_ref()
            .and_then(|s| s.branch.clone().or_else(|| s.head.as_ref().map(|h| format!("({h})"))))
            .or(self.fallback_branch);
        let Some(branch) = branch else { return h_flex().id(self.id) };
        let tip = summary.as_ref().map(describe);
        let numbers = |el: Stateful<Div>, s: &GitSummary| {
            el.when(s.ahead > 0, |el| el.child(div().flex_none().child(format!("↑{}", s.ahead))))
                .when(s.behind > 0, |el| el.child(div().flex_none().child(format!("↓{}", s.behind))))
                .when(s.changed > 0, |el| {
                    el.child(
                        h_flex()
                            .flex_none()
                            .gap_0p5()
                            .child(div().size_1p5().rounded_full().bg(theme.warning))
                            .child(if self.compact { s.changed.to_string() } else { format!("{} changed", s.changed) }),
                    )
                })
                .when(s.conflicts > 0, |el| {
                    el.child(
                        h_flex()
                            .flex_none()
                            .gap_0p5()
                            .text_color(theme.danger)
                            .child(Icon::new(IconName::TriangleAlert).xsmall())
                            .child(if self.compact { s.conflicts.to_string() } else { format!("{} in conflict", s.conflicts) }),
                    )
                })
        };
        h_flex()
            .id(self.id)
            .min_w_0()
            .gap_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(Icon::new(Lucide::GitBranch).xsmall().flex_none())
            .child(div().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(branch))
            .when_some(summary.as_ref(), numbers)
            .when_some(tip, |el, tip| el.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)))
    }
}

/// The project's mark: its icon (an emoji or letters) when set, else the
/// monogram of its name.
pub fn mark(project: &Project, config: &ProjectConfig) -> AnyElement {
    if config.icon.trim().is_empty() {
        return crate::ui::Monogram::new(project.name.clone(), project.id.0).into_any_element();
    }
    let icon = config.icon.clone();
    let letters = icon.chars().all(|c| c.is_alphanumeric());
    h_flex()
        .flex_none()
        .size_5()
        .justify_center()
        .text_size(if letters { rems(0.6) } else { rems(0.8) })
        .font_weight(FontWeight::BOLD)
        .child(if letters { icon.to_uppercase() } else { icon })
        .into_any_element()
}

/// A project's menu (its sidebar row): new thread, settings, open it in
/// the user's tools, archive.
pub fn project_menu(menu: PopupMenu, project: &Project, cx: &App) -> PopupMenu {
    let (id, path) = (project.id, std::path::PathBuf::from(&project.path));
    let (editor_path, reveal_path, terminal_path) = (path.clone(), path.clone(), path);
    menu.item(PopupMenuItem::new("New thread").icon(Lucide::SquarePen).on_click(move |_, window, cx| {
        window.dispatch_action(Box::new(crate::sidebar::NewThreadIn { project: id.0 }), cx)
    }))
    .item(PopupMenuItem::new("Project settings…").icon(IconName::Settings2).on_click(move |_, window, cx| {
        settings::open(Some(id), window, cx)
    }))
    .separator()
    .item(
        PopupMenuItem::new(crate::files::editor::open_label(cx))
            .icon(IconName::ExternalLink)
            .on_click(move |_, _, cx| crate::files::editor::open_folder(&editor_path, cx)),
    )
    .item(
        PopupMenuItem::new(format!("Reveal in {}", crate::files::editor::file_manager()))
            .icon(IconName::FolderOpen)
            .on_click(move |_, _, cx| crate::files::editor::reveal(&reveal_path, cx)),
    )
    .item(
        PopupMenuItem::new("Open in terminal")
            .icon(Lucide::Terminal)
            .on_click(move |_, _, cx| crate::files::editor::open_terminal(&terminal_path, cx)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_read_as_sentences() {
        let summary = GitSummary {
            repo: true,
            branch: Some("topic".into()),
            compared_to: Some("main".into()),
            ahead: 2,
            behind: 1,
            changed: 3,
            additions: 12,
            deletions: 4,
            conflicts: 1,
            ..Default::default()
        };
        assert_eq!(
            describe(&summary),
            "On topic, 2 commits ahead of main, 1 behind, 3 changed files (+12 −4), 1 file with merge conflicts."
        );
        let clean = GitSummary { repo: true, head: Some("abc1234".into()), ..Default::default() };
        assert_eq!(describe(&clean), "Detached at abc1234, no uncommitted changes.");
    }
}
