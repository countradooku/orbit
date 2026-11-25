//! Automations: schedules, the scheduler's timing rules, prompt templates,
//! the recipe gallery, and the store's records. The engine side is
//! `engine/automations.rs`.

pub mod recipes;
pub mod records;
pub mod schedule;
pub mod scheduler;
pub mod template;

use orbit_proto::*;

/// Checks a spec before it is saved; the message says what to fix.
pub fn validate(spec: &AutomationSpec) -> Result<(), String> {
    if spec.name.trim().is_empty() {
        return Err("Give the automation a name.".into());
    }
    if spec.prompt.trim().is_empty() {
        return Err("Write what the agent should do.".into());
    }
    if spec.triggers.is_empty() {
        return Err("Add a schedule or an event that starts it.".into());
    }
    for trigger in &spec.triggers {
        schedule::Schedule::of(trigger)?;
        match trigger {
            Trigger::Event(EventTrigger::IssueLabeled { label }) if label.trim().is_empty() => {
                return Err("Say which label to watch for.".into());
            }
            Trigger::Event(EventTrigger::BranchPushed { branch }) if branch.trim().is_empty() => {
                return Err("Say which branch to watch.".into());
            }
            Trigger::Event(EventTrigger::WatchPr { number: 0 }) => {
                return Err("Say which pull request to watch.".into());
            }
            _ if spec.project.is_none() && needs_forge(trigger) => {
                return Err("Pull request, issue and branch events need a project.".into());
            }
            _ => {}
        }
    }
    if matches!(spec.target, RunTarget::Worktree | RunTarget::PullRequest) && spec.project.is_none() {
        return Err("A worktree needs a project: pick one.".into());
    }
    let unknown = template::unknown(&spec.prompt);
    if !unknown.is_empty() {
        let names: Vec<String> = unknown.iter().map(|n| format!("{{{n}}}")).collect();
        return Err(format!("The prompt uses {} that Orbit doesn't know.", names.join(", ")));
    }
    Ok(())
}

/// Whether a trigger is answered by watching the project's forge or remote.
pub fn needs_forge(trigger: &Trigger) -> bool {
    matches!(
        trigger,
        Trigger::Event(
            EventTrigger::ChecksFailed { .. }
                | EventTrigger::ReviewRequested
                | EventTrigger::IssueLabeled { .. }
                | EventTrigger::BranchPushed { .. }
                | EventTrigger::WatchPr { .. }
        )
    )
}

/// A short description of a trigger: `weekdays 9:00`, `when checks fail`.
pub fn describe(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Event(event) => match event {
            EventTrigger::ChecksFailed { mine: true } => "when checks fail on my pull requests".into(),
            EventTrigger::ChecksFailed { mine: false } => "when checks fail on a pull request".into(),
            EventTrigger::ReviewRequested => "when my review is requested".into(),
            EventTrigger::IssueLabeled { label } => format!("on new issues labeled “{label}”"),
            EventTrigger::BranchPushed { branch } => format!("when {branch} is pushed"),
            EventTrigger::Finished { source, when } => {
                let what = match source {
                    ChainSource::Automation(_) => "another automation",
                    ChainSource::Thread(_) => "a thread",
                };
                match when {
                    ChainWhen::Succeeded => format!("after {what} succeeds"),
                    ChainWhen::Failed => format!("after {what} fails"),
                    ChainWhen::Either => format!("after {what} finishes"),
                }
            }
            EventTrigger::WatchPr { number } => format!("watching #{number} until it merges"),
        },
        other => match schedule::Schedule::of(other) {
            Ok(Some(s)) => s.describe(),
            Ok(None) => String::new(),
            Err(_) => "invalid schedule".into(),
        },
    }
}
