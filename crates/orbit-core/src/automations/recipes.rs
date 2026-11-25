//! The recipe gallery: ready-made automations that fill in the editor.

use orbit_proto::*;

/// A built-in automation template.
pub struct Recipe {
    pub id: &'static str,
    pub title: &'static str,
    /// One line for the gallery card.
    pub blurb: &'static str,
    /// What it needs from the user before it can be saved, if anything.
    pub needs: &'static str,
    /// A Lucide icon name for the card.
    pub icon: &'static str,
    build: fn() -> AutomationSpec,
}

impl Recipe {
    /// The recipe's automation for `project`, on the user's agent `config`
    /// (mode and access are the recipe's own).
    pub fn spec(&self, project: Option<ProjectId>, config: &ThreadConfig) -> AutomationSpec {
        let mut spec = (self.build)();
        spec.project = project;
        spec.config = ThreadConfig { mode: spec.config.mode, access: spec.config.access, ..config.clone() };
        spec.recipe = self.id.to_owned();
        spec
    }
}

fn rule(rule: &str) -> Trigger {
    Trigger::Rule { rule: rule.into(), tz: String::new() }
}

fn agent(mode: Mode, access: Access) -> ThreadConfig {
    ThreadConfig { mode, access, ..Default::default() }
}

pub static RECIPES: &[Recipe] = &[
    Recipe {
        id: "nightly-deps",
        title: "Nightly dependency updates",
        blurb: "Bumps outdated dependencies, runs the tests and opens a pull request when something changed.",
        needs: "",
        icon: "package",
        build: || AutomationSpec {
            name: "Nightly dependency updates".into(),
            target: RunTarget::Worktree,
            config: agent(Mode::Build, Access::AutoEdit),
            prompt: "Update this project's outdated dependencies ({date}).\n\n\
                     1. Find what is outdated with the project's own package manager.\n\
                     2. Update patch and minor versions; leave major versions alone and list them at the end.\n\
                     3. Build and run the tests. If an update breaks them, revert that one update.\n\
                     4. Finish with a short summary: what changed, what you reverted and why."
                .into(),
            triggers: vec![rule("FREQ=DAILY;BYHOUR=2;BYMINUTE=0")],
            checks: PreChecks { changes_since_last_run: false, ..Default::default() },
            limits: Limits { max_runs_per_day: 1, timeout_minutes: 45, grace_minutes: 180 },
            on_finish: OnFinish { notify: Notify::Failures, settle_if_unchanged: true, open_pr: true },
            ..Default::default()
        },
    },
    Recipe {
        id: "fix-ci",
        title: "Fix failing CI on my pull requests",
        blurb: "When checks fail on a pull request of yours, an agent reads the logs and pushes a fix to its branch.",
        needs: "A GitHub or GitLab project",
        icon: "wrench",
        build: || AutomationSpec {
            name: "Fix failing CI".into(),
            target: RunTarget::PullRequest,
            config: agent(Mode::Build, Access::AutoEdit),
            prompt: "Checks are failing on {pr}.\n\n\
                     Read the failing checks' logs (`gh pr checks` / `glab ci view`), find the cause, fix it on this branch, \
                     run the relevant tests locally, then commit and push. If the failure is flaky or unrelated to the change, \
                     say so instead of changing code."
                .into(),
            triggers: vec![Trigger::Event(EventTrigger::ChecksFailed { mine: true })],
            limits: Limits { max_runs_per_day: 6, timeout_minutes: 40, grace_minutes: 30 },
            on_finish: OnFinish { notify: Notify::Always, settle_if_unchanged: false, open_pr: false },
            ..Default::default()
        },
    },
    Recipe {
        id: "triage-issues",
        title: "Triage new issues",
        blurb: "Every new issue with a label gets a first look: a likely cause, the files involved and a suggested size.",
        needs: "The label to watch",
        icon: "inbox",
        build: || AutomationSpec {
            name: "Triage new issues".into(),
            target: RunTarget::Project,
            config: agent(Mode::Plan, Access::Readonly),
            prompt: "Triage {issue}.\n\n\
                     Read it, then look through the code for where it happens. Reply with: a one-line summary, the likely \
                     cause, the files involved, how to reproduce it, and a size (small, medium, large). Don't change any files."
                .into(),
            triggers: vec![Trigger::Event(EventTrigger::IssueLabeled { label: "needs-triage".into() })],
            limits: Limits { max_runs_per_day: 20, timeout_minutes: 20, grace_minutes: 30 },
            on_finish: OnFinish { notify: Notify::Always, settle_if_unchanged: false, open_pr: false },
            ..Default::default()
        },
    },
    Recipe {
        id: "standup",
        title: "Daily standup summary",
        blurb: "Each weekday morning, a short summary of what yesterday's threads did, what shipped and what is stuck.",
        needs: "",
        icon: "sunrise",
        build: || AutomationSpec {
            name: "Standup summary".into(),
            target: RunTarget::Project,
            config: agent(Mode::Plan, Access::Readonly),
            prompt: "Write my standup for {date}.\n\n\
                     Use `orbit threads --all --json` and `orbit log <thread>` to see what the threads in {project} did since \
                     the last working day, and `git log --since=yesterday` for commits. Group it as Done, In progress and \
                     Blocked, one line each, with thread titles. Keep it under 15 lines.\n\n\
                     Last time: {last_run_summary}"
                .into(),
            triggers: vec![rule("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0")],
            limits: Limits { max_runs_per_day: 1, timeout_minutes: 15, grace_minutes: 120 },
            on_finish: OnFinish { notify: Notify::Always, settle_if_unchanged: false, open_pr: false },
            ..Default::default()
        },
    },
    Recipe {
        id: "keep-rebased",
        title: "Keep a branch rebased",
        blurb: "When main moves, rebase the project's branch onto it, fix conflicts and run the tests.",
        needs: "The branch to follow",
        icon: "git-branch",
        build: || AutomationSpec {
            name: "Keep rebased on main".into(),
            target: RunTarget::Project,
            config: agent(Mode::Build, Access::AutoEdit),
            prompt: "`{branch}` moved. Fetch, then rebase the current branch onto `origin/{branch}`.\n\n\
                     Resolve conflicts keeping both sides' intent, build and run the tests, and push with \
                     `--force-with-lease` only if they pass. If a conflict needs a decision, stop and explain it."
                .into(),
            triggers: vec![Trigger::Event(EventTrigger::BranchPushed { branch: "main".into() })],
            checks: PreChecks { clean_repo: true, ..Default::default() },
            limits: Limits { max_runs_per_day: 8, timeout_minutes: 30, grace_minutes: 60 },
            on_finish: OnFinish { notify: Notify::Failures, settle_if_unchanged: true, open_pr: false },
            ..Default::default()
        },
    },
    Recipe {
        id: "review-requests",
        title: "Review pull requests that request me",
        blurb: "When someone asks for your review, an agent reads the change and drafts a review for you to send.",
        needs: "A GitHub or GitLab project",
        icon: "eye",
        build: || AutomationSpec {
            name: "Draft my reviews".into(),
            target: RunTarget::PullRequest,
            config: agent(Mode::Plan, Access::Readonly),
            prompt: "I was asked to review {pr}.\n\n\
                     Read the diff against its base and the code around it. Draft a review: what the change does in two \
                     sentences, then numbered findings (bugs first, then risks, then nits) with file and line. Don't post \
                     anything and don't change files: I'll send it myself."
                .into(),
            triggers: vec![Trigger::Event(EventTrigger::ReviewRequested)],
            limits: Limits { max_runs_per_day: 10, timeout_minutes: 25, grace_minutes: 60 },
            on_finish: OnFinish { notify: Notify::Always, settle_if_unchanged: false, open_pr: false },
            ..Default::default()
        },
    },
    Recipe {
        id: "watch-pr",
        title: "Babysit a pull request until it merges",
        blurb: "Fixes failing checks and answers requested changes on one pull request, then stops when it merges.",
        needs: "The pull request number",
        icon: "shield-check",
        build: || AutomationSpec {
            name: "Babysit pull request".into(),
            target: RunTarget::PullRequest,
            config: agent(Mode::Build, Access::AutoEdit),
            prompt: "{event} on {pr}.\n\n\
                     If checks failed, read their logs and fix the cause. If a reviewer asked for changes, address each \
                     comment. Run the tests, commit and push. Reply with what you changed."
                .into(),
            triggers: vec![Trigger::Event(EventTrigger::WatchPr { number: 0 })],
            limits: Limits { max_runs_per_day: 8, timeout_minutes: 40, grace_minutes: 30 },
            on_finish: OnFinish { notify: Notify::Always, settle_if_unchanged: false, open_pr: false },
            ..Default::default()
        },
    },
];

pub fn find(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::{schedule::Schedule, template};

    #[test]
    fn every_recipe_is_valid() {
        let mut ids = std::collections::HashSet::new();
        for recipe in RECIPES {
            assert!(ids.insert(recipe.id), "duplicate {}", recipe.id);
            let spec = recipe.spec(Some(ProjectId(1)), &ThreadConfig { model: "opus".into(), ..Default::default() });
            assert_eq!(spec.recipe, recipe.id);
            assert_eq!(spec.config.model, "opus", "the user's agent");
            assert!(!spec.name.is_empty() && !spec.prompt.is_empty() && !spec.triggers.is_empty());
            assert!(template::unknown(&spec.prompt).is_empty(), "{}: {:?}", recipe.id, template::unknown(&spec.prompt));
            for trigger in &spec.triggers {
                Schedule::of(trigger).unwrap_or_else(|e| panic!("{}: {e}", recipe.id));
            }
            assert!(!recipe.blurb.ends_with(' '));
        }
        assert!(RECIPES.len() >= 6);
    }
}
