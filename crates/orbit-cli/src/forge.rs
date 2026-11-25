//! `orbit pr …`, `orbit issue …` and `orbit ship`: the engine's forge and
//! tracker commands ([`ForgeCommand`]).

use std::fmt::Write as _;

use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{Ctx, Outcome};
use crate::json as records;
use crate::render::ago_or_in;
use crate::select;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

/// Debug names as JSON words: `ChangesRequested` → `changes_requested`.
fn word(debug: impl std::fmt::Debug) -> String {
    let text = format!("{debug:?}");
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// `--project`, else the project containing the current folder, else the
/// only one.
fn project(ctx: &Ctx, conn: &Conn, args: &Map<String, Value>) -> Result<ProjectId, CliError> {
    let state = conn.state();
    if let Some(sel) = s(args, "project") {
        return select::project(&state, &ctx.env, sel);
    }
    if let Some(id) = select::containing(&state, &ctx.env.cwd) {
        return Ok(id);
    }
    match state.projects.as_slice() {
        [one] => Ok(one.id),
        _ => Err(CliError::usage("Pick a project with --project (the current folder is not in one).")),
    }
}

fn number(args: &Map<String, Value>) -> Result<u64, CliError> {
    let raw = s(args, "number").unwrap_or_default();
    raw.trim_start_matches(['#', '!']).parse().map_err(|_| CliError::usage(format!("“{raw}” is not a pull request number.")))
}

async fn forge(conn: &Conn, command: ForgeCommand) -> Result<Reply, CliError> {
    conn.call(Command::Forge(command)).await
}

fn pr_json(pr: &PullRequest) -> Value {
    json!({
        "number": pr.number,
        "title": pr.title,
        "author": pr.author,
        "state": word(pr.state),
        "head": pr.head,
        "base": pr.base,
        "url": pr.url,
        "checks": {"passed": pr.checks.passed, "failed": pr.checks.failed, "pending": pr.checks.pending, "other": pr.checks.other},
        "review": word(pr.review),
        "additions": pr.additions,
        "deletions": pr.deletions,
        "labels": pr.labels,
        "updated_ms": pr.updated_ms,
    })
}

fn issue_json(issue: &Issue) -> Value {
    json!({
        "tracker": word(issue.tracker),
        "key": issue.key,
        "title": issue.title,
        "state": word(issue.state),
        "status": issue.status,
        "author": issue.author,
        "assignee": issue.assignee,
        "labels": issue.labels,
        "url": issue.url,
        "comments": issue.comments,
        "branch": issue.branch,
        "updated_ms": issue.updated_ms,
    })
}

fn tracker(name: &str) -> TrackerKind {
    match name {
        "gitlab" => TrackerKind::GitLab,
        "linear" => TrackerKind::Linear,
        "jira" => TrackerKind::Jira,
        _ => TrackerKind::GitHub,
    }
}

/// `--tracker`, else the project's own host (GitHub or GitLab).
async fn tracker_for(conn: &Conn, args: &Map<String, Value>, project: Option<ProjectId>) -> Result<TrackerKind, CliError> {
    if let Some(name) = s(args, "tracker") {
        return Ok(tracker(name));
    }
    let Some(project) = project else { return Ok(TrackerKind::GitHub) };
    if let Reply::Forge(ForgeReply::Info(info)) = forge(conn, ForgeCommand::Info { project }).await?
        && info.repo.as_ref().is_some_and(|r| r.kind == ForgeKind::GitLab)
    {
        return Ok(TrackerKind::GitLab);
    }
    Ok(TrackerKind::GitHub)
}

fn unexpected() -> CliError {
    CliError::engine("Unexpected reply.")
}

fn started(conn: &Conn, id: ThreadId, what: &str) -> Outcome {
    let title = conn.state().threads.get(&id).map(|t| records::title(t).to_owned()).unwrap_or_default();
    Outcome::ok(
        json!({"thread": id.to_string()}),
        format!("{what}: {id}  {title}\nFollow it with `orbit tail {id}`."),
    )
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["pr", "list"] => {
            let project = project(ctx, conn, args)?;
            let filter = match s(args, "filter").unwrap_or("open") {
                "mine" => PrFilter::Mine,
                "merged" => PrFilter::Merged,
                "closed" => PrFilter::Closed,
                _ => PrFilter::Open,
            };
            let reply = forge(conn, ForgeCommand::PullRequests { project, filter, refresh: flag(args, "refresh") }).await?;
            let Reply::Forge(ForgeReply::PullRequests(list)) = reply else { return Err(unexpected()) };
            let mut human = String::new();
            for pr in &list.items {
                let c = &pr.checks;
                let checks = if c.failed > 0 {
                    format!("✕ {} failing", c.failed)
                } else if c.pending > 0 {
                    format!("● {} running", c.pending)
                } else if c.passed > 0 {
                    "✓ checks pass".to_owned()
                } else {
                    String::new()
                };
                let _ = writeln!(human, "#{:<6} {}  ({} → {}, {})  {}", pr.number, pr.title, pr.head, pr.base, pr.author, checks);
            }
            if list.items.is_empty() {
                human.push_str("No pull requests.");
            }
            if let Some(stale) = &list.stale {
                let _ = write!(human, "\n(Showing earlier results: {stale})");
            }
            let value = json!({"pull_requests": list.items.iter().map(pr_json).collect::<Vec<_>>(), "stale": list.stale});
            Ok(Outcome::ok(value, human.trim_end().to_owned()))
        }
        ["pr", "show"] => {
            let project = project(ctx, conn, args)?;
            let number = number(args)?;
            let reply = forge(conn, ForgeCommand::PullRequest { project, number, refresh: flag(args, "refresh") }).await?;
            let Reply::Forge(ForgeReply::PullRequest(detail)) = reply else { return Err(unexpected()) };
            let mut value = pr_json(&detail.pr);
            value["body"] = json!(detail.body);
            value["head_sha"] = json!(detail.head_sha);
            value["mergeable"] = json!(word(detail.mergeable));
            value["auto_merge"] = json!(detail.auto_merge.map(word));
            value["changed_files"] = json!(detail.changed_files);
            value["checks"] = json!(detail
                .checks
                .iter()
                .map(|c| json!({"name": c.name, "state": word(c.state), "url": c.url}))
                .collect::<Vec<_>>());
            value["reviews"] = json!(detail
                .reviews
                .iter()
                .map(|r| json!({"author": r.author, "state": word(r.state), "body": r.body}))
                .collect::<Vec<_>>());
            value["comments"] = json!(detail
                .comments
                .iter()
                .map(|c| json!({"author": c.author, "body": c.body, "path": c.path, "line": c.line}))
                .collect::<Vec<_>>());
            let mut human = format!(
                "#{} {}\n{} → {} · {} · {} · +{} -{} · {}\n{}",
                detail.pr.number,
                detail.pr.title,
                detail.pr.head,
                detail.pr.base,
                word(detail.pr.state),
                word(detail.mergeable),
                detail.pr.additions,
                detail.pr.deletions,
                ago_or_in(detail.pr.updated_ms),
                detail.pr.url
            );
            for check in &detail.checks {
                let _ = write!(human, "\n  {:<10} {}", word(check.state), check.name);
            }
            for review in &detail.reviews {
                let _ = write!(human, "\n  review: {} {}", review.author, word(review.state));
            }
            if !detail.body.trim().is_empty() {
                let _ = write!(human, "\n\n{}", detail.body.trim());
            }
            Ok(Outcome::ok(value, human))
        }
        ["pr", "merge"] => {
            let project = project(ctx, conn, args)?;
            let number = number(args)?;
            let method = match s(args, "method").unwrap_or("squash") {
                "merge" => MergeMethod::Merge,
                "rebase" => MergeMethod::Rebase,
                _ => MergeMethod::Squash,
            };
            let auto = flag(args, "auto");
            let command = ForgeCommand::Merge {
                project,
                number,
                method,
                auto,
                delete_branch: flag(args, "delete-branch"),
                head_sha: String::new(),
            };
            forge(conn, command).await?;
            let human = if auto { format!("Auto-merge is on for #{number}.") } else { format!("Merged #{number}.") };
            Ok(Outcome::ok(json!({"number": number, "auto": auto, "method": word(method)}), human))
        }
        ["pr", "checkout"] => {
            let project = project(ctx, conn, args)?;
            let number = number(args)?;
            let config = conn.state().defaults.clone();
            let Reply::Thread(id) = forge(conn, ForgeCommand::Checkout { project, number, config }).await? else {
                return Err(unexpected());
            };
            Ok(started(conn, id, &format!("#{number} is in")))
        }
        ["pr", "fix-checks"] => {
            let project = project(ctx, conn, args)?;
            let number = number(args)?;
            let thread = match s(args, "thread") {
                Some(sel) => Some(select::thread(&conn.state(), &ctx.env, sel)?),
                None => None,
            };
            let config = conn.state().defaults.clone();
            let Reply::Thread(id) = forge(conn, ForgeCommand::FixChecks { project, number, thread, config }).await? else {
                return Err(unexpected());
            };
            Ok(started(conn, id, "Sent the failing checks to"))
        }
        ["issue", "list"] => {
            let project = project(ctx, conn, args).ok();
            let tracker = tracker_for(conn, args, project).await?;
            let query = s(args, "query").unwrap_or_default().to_owned();
            let reply = forge(conn, ForgeCommand::Issues { project, tracker, query, refresh: flag(args, "refresh") }).await?;
            let Reply::Forge(ForgeReply::Issues(list)) = reply else { return Err(unexpected()) };
            let mut human = String::new();
            for issue in &list.items {
                let _ = writeln!(human, "{:<10} {}  [{}]", issue.key, issue.title, if issue.status.is_empty() { word(issue.state) } else { issue.status.clone() });
            }
            if list.items.is_empty() {
                human.push_str("No issues.");
            }
            let value = json!({"issues": list.items.iter().map(issue_json).collect::<Vec<_>>(), "stale": list.stale});
            Ok(Outcome::ok(value, human.trim_end().to_owned()))
        }
        ["issue", "show"] => {
            let project = project(ctx, conn, args).ok();
            let tracker = tracker_for(conn, args, project).await?;
            let key = s(args, "key").unwrap_or_default().to_owned();
            let reply = forge(conn, ForgeCommand::Issue { project, tracker, key, refresh: flag(args, "refresh") }).await?;
            let Reply::Forge(ForgeReply::Issue(detail)) = reply else { return Err(unexpected()) };
            let mut value = issue_json(&detail.issue);
            value["body"] = json!(detail.body);
            value["comments"] = json!(detail.comments.iter().map(|c| json!({"author": c.author, "body": c.body})).collect::<Vec<_>>());
            let human = format!("{} {}\n{}\n\n{}", detail.issue.key, detail.issue.title, detail.issue.url, detail.body.trim());
            Ok(Outcome::ok(value, human.trim_end().to_owned()))
        }
        ["issue", "start"] => {
            let project = project(ctx, conn, args)?;
            let tracker = tracker_for(conn, args, Some(project)).await?;
            let key = s(args, "key").unwrap_or_default().to_owned();
            let config = conn.state().defaults.clone();
            let command = ForgeCommand::StartFromIssue { project, tracker, key: key.clone(), config, worktree: !flag(args, "here") };
            let Reply::Thread(id) = forge(conn, command).await? else { return Err(unexpected()) };
            Ok(started(conn, id, &format!("Started {key} in")))
        }
        ["ship"] => {
            let sel = s(args, "thread").unwrap_or("latest");
            let id = select::thread(&conn.state(), &ctx.env, sel)?;
            let reply = forge(conn, ForgeCommand::Ship { thread: id, refresh: flag(args, "refresh") }).await?;
            let Reply::Forge(ForgeReply::Ship(ship)) = reply else { return Err(unexpected()) };
            let value = json!({
                "thread": id.to_string(),
                "stage": word(ship.stage),
                "branch": ship.branch,
                "files": ship.files,
                "additions": ship.additions,
                "deletions": ship.deletions,
                "ahead": ship.ahead,
                "behind": ship.behind,
                "pull_request": ship.pr.as_ref().map(pr_json),
                "mergeable": word(ship.mergeable),
                "auto_merge": ship.auto_merge,
                "note": ship.note,
            });
            let mut human = format!(
                "{}  {}  ({} files, +{} -{}, {} ahead, {} behind)",
                word(ship.stage).replace('_', " "),
                ship.branch.as_deref().unwrap_or("-"),
                ship.files,
                ship.additions,
                ship.deletions,
                ship.ahead,
                ship.behind
            );
            if let Some(pr) = &ship.pr {
                let _ = write!(human, "\n#{} {}  {}", pr.number, pr.title, pr.url);
            }
            if let Some(note) = &ship.note {
                let _ = write!(human, "\n{note}");
            }
            Ok(Outcome::ok(value, human))
        }
        _ => Err(CliError::usage("Not a forge command.")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn debug_names_become_snake_case_words() {
        assert_eq!(super::word(orbit_proto::ReviewDecision::ChangesRequested), "changes_requested");
        assert_eq!(super::word(orbit_proto::MergeMethod::Squash), "squash");
    }
}
