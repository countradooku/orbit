//! What team members and judges read: the architect's and workers' team
//! context, worker briefs, delivered mail and the judge's prompt.
//!
//! Team context reaches an agent the way a provider handoff does: put in
//! front of its next prompt, not shown as something the user typed. Mail is
//! delivered as one structured message at a turn boundary, never typed into
//! a terminal.

use std::fmt::Write as _;

use orbit_proto::*;

/// Notes quoted in a new worker's brief; the rest are counted.
pub const BRIEF_NOTES: usize = 5;
/// Characters of a candidate's diff the judge sees.
const JUDGE_DIFF_CHARS: usize = 6_000;
/// Characters of a candidate's final reply the judge sees.
const JUDGE_REPLY_CHARS: usize = 1_500;

/// Context for the architect, sent with its first message.
pub fn architect_context(team: &Team, branch: &str, tools: bool) -> String {
    let l = &team.limits;
    let mut out = String::new();
    let _ = writeln!(out, "<orbit-team role=\"architect\" team=\"{}\">", team.id);
    out.push_str(
        "You lead a team of coding agents in Orbit. Plan the work as small, independent tasks and hand each one \
         to a worker; do the integration yourself.\n",
    );
    if tools {
        out.push_str("Your Orbit MCP tools:\n");
    } else {
        out.push_str("Run these with the `orbit` command line (it knows who you are):\n");
    }
    let tool = |mcp: &str, cli: &str| if tools { mcp.to_owned() } else { format!("orbit {cli}") };
    let _ = writeln!(
        out,
        "- {}: title, brief, depends_on (worker keys), scope (paths it will edit). Returns the worker's key (w1, w2…).",
        tool("team_spawn_worker", "team spawn")
    );
    let _ = writeln!(out, "- {}: mail a worker (or `@all`); `urgent` steers a working one.", tool("team_message", "team message"));
    let _ = writeln!(out, "- {} / {}: the shared notes board for API shapes and decisions.", tool("team_post_note", "team note"), tool("team_read_notes", "team notes"));
    let _ = writeln!(out, "- {}: every worker's state and why a waiting one waits.", tool("team_status", "team status"));
    let _ = writeln!(
        out,
        "- {}: merge a finished worker's branch into yours (previewed first; conflicts are reported and nothing changes).",
        tool("team_merge_worker", "team merge")
    );
    let _ = writeln!(
        out,
        "Workers start in their own worktrees branched from `{branch}` as it is committed; commit anything they need first. \
         Up to {} work at once; a worker waits while a dependency is unfinished, no slot is free, or a teammate is editing its scope.",
        l.max_workers
    );
    out.push_str(
        "After spawning, end your turn: never poll. Orbit wakes you with each worker's summary as team mail, \
         delivered between your turns. Post decisions other workers need to the notes board.\n",
    );
    if l.max_cost_micros > 0 {
        let _ = writeln!(out, "Spawning stops after {} failed workers or ${:.2} spent, until the user resumes it.", l.max_failures, l.max_cost_micros as f64 / 1e6);
    } else {
        let _ = writeln!(out, "Spawning stops after {} failed workers, until the user resumes it.", l.max_failures);
    }
    out.push_str("</orbit-team>");
    out
}

/// Context for a worker, sent with its brief.
pub fn worker_context(team: &Team, worker: &Worker, branch: &str, tools: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "<orbit-team role=\"worker\" key=\"{}\" team=\"{}\">", worker.key, team.id);
    let _ = writeln!(
        out,
        "You are worker {} on a team led by an architect. You work in your own git worktree, branched from `{branch}`.",
        worker.key
    );
    if tools {
        out.push_str(
            "Orbit MCP tools: team_post_note (share API shapes and decisions), team_read_notes, \
             team_message (to: \"architect\" or a worker key), team_status.\n",
        );
    } else {
        out.push_str("Use `orbit team note`, `orbit team notes`, `orbit team message architect …` and `orbit team status`.\n");
    }
    out.push_str(
        "Do only your task. When it is done, end your turn with a short summary of what you changed and anything \
         the architect must know: Orbit sends it to the architect. Messages from the team arrive between your turns.\n",
    );
    out.push_str("</orbit-team>");
    out
}

/// The worker's first message: its task, what it builds on, its scope and
/// the newest notes.
pub fn worker_brief(team: &Team, worker: &Worker) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} ({})\n", worker.title, worker.key);
    out.push_str(worker.brief.trim());
    out.push('\n');
    let deps: Vec<&Worker> = worker.depends_on.iter().filter_map(|k| team.worker(k)).collect();
    if !deps.is_empty() {
        out.push_str("\n## Builds on\n");
        for d in deps {
            let summary = first_chars(d.summary.trim(), 600);
            let _ = writeln!(out, "- {} ({}): {}", d.key, d.title, if summary.is_empty() { "done" } else { summary });
        }
    }
    if !worker.scope.is_empty() {
        let _ = writeln!(out, "\n## Your scope\n{}", worker.scope.join(", "));
    }
    let (notes, more) = team.latest_notes(BRIEF_NOTES);
    if !notes.is_empty() {
        out.push_str("\n## Team notes (newest first)\n");
        for n in notes {
            let _ = writeln!(out, "- [{}] {}", n.author_label, first_chars(n.text.trim(), 800));
        }
        if more > 0 {
            let _ = writeln!(out, "{more} more notes: read them with team_read_notes.");
        }
    }
    out
}

/// One delivery of mail as the recipient's next message.
pub fn mail_message(team: TeamId, mail: &[&Mail]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "<orbit-team-mail team=\"{team}\" count=\"{}\">", mail.len());
    for m in mail {
        let _ = writeln!(
            out,
            "<mail id=\"{}\" from=\"{}\" kind=\"{}\"{}>\n{}\n</mail>",
            m.id,
            escape_attr(&m.from_label),
            m.kind.as_str(),
            if m.urgent { " urgent=\"true\"" } else { "" },
            m.text.trim()
        );
    }
    out.push_str("</orbit-team-mail>\n");
    out.push_str("Team mail, delivered by Orbit between your turns. Act on it, and answer with team_message when someone asked you something.");
    out
}

/// The mail a parent gets when its worker finishes.
pub fn done_mail(worker: &Worker) -> String {
    let summary = worker.summary.trim();
    let mut out = format!("{} finished “{}”.", worker.key, worker.title);
    if !summary.is_empty() {
        let _ = write!(out, "\n\n{summary}");
    }
    let _ = write!(out, "\n\nMerge it with team_merge_worker (worker: \"{}\") when you are ready.", worker.key);
    out
}

pub fn failed_mail(worker: &Worker, message: &str) -> String {
    format!("{} failed “{}”: {message}", worker.key, worker.title)
}

/// What a candidate looked like when it settled, for the judge.
pub struct JudgeEntry {
    pub label: String,
    pub agent: String,
    pub candidate: Candidate,
    pub reply: String,
    pub diff: String,
}

pub const JUDGE_INSTRUCTIONS: &str = "You judge a race: several coding agents were given the same task, each in \
    its own copy of the repository. Compare their results for correctness, completeness, test results and how \
    small and clean the change is. Answer with exactly two lines: `Winner: <label>` (one of the labels given), \
    then `Reason: <one or two sentences>`. Nothing else.";

pub fn judge_input(prompt: &str, entries: &[JudgeEntry]) -> String {
    let mut out = format!("## The task\n{}\n", prompt.trim());
    for e in entries {
        let c = &e.candidate;
        let _ = writeln!(out, "\n## Candidate {} ({})", e.label, e.agent);
        let outcome = match &c.finished {
            Some(CandidateEnd::Completed) => "finished".to_owned(),
            Some(CandidateEnd::Failed { message }) => format!("failed: {message}"),
            None => "did not finish".to_owned(),
        };
        let tests = match &c.test {
            TestState::None => "no test command".to_owned(),
            TestState::Passed { .. } => "tests passed".to_owned(),
            TestState::Failed { code, .. } => format!("tests failed (exit {})", code.map_or("?".into(), |c| c.to_string())),
            TestState::Pending | TestState::Running => "tests did not run".to_owned(),
        };
        let _ = writeln!(
            out,
            "{outcome}; {tests}; {} files, +{} −{}; {} s",
            c.files.len(),
            c.additions(),
            c.deletions(),
            c.duration_ms / 1000
        );
        if let TestState::Failed { tail, .. } = &c.test
            && !tail.trim().is_empty()
        {
            let _ = writeln!(out, "Test output (end):\n```\n{}\n```", last_chars(tail.trim(), 800));
        }
        let _ = writeln!(out, "Its final reply:\n{}", first_chars(e.reply.trim(), JUDGE_REPLY_CHARS));
        let _ = writeln!(out, "Its diff:\n```diff\n{}\n```", first_chars(&e.diff, JUDGE_DIFF_CHARS));
    }
    out
}

/// Reads `Winner: B` / `Reason: …` from the judge's answer.
pub fn parse_verdict(text: &str, labels: &[&str]) -> Option<(String, String)> {
    let mut winner = None;
    let mut reason = String::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['*', '#', '-', ' ']);
        let lower = line.to_lowercase();
        if winner.is_none() && lower.starts_with("winner") {
            let value = line.split_once(':').map(|(_, v)| v).unwrap_or(&line[6..]);
            let value = value.trim().trim_matches(['*', '`', '"', '.', ' ']);
            let value = value.strip_prefix("Candidate ").unwrap_or(value);
            winner = labels.iter().find(|l| l.eq_ignore_ascii_case(value)).map(|l| (*l).to_owned());
        } else if lower.starts_with("reason") {
            reason = line.split_once(':').map(|(_, v)| v.trim().to_owned()).unwrap_or_default();
        } else if winner.is_some() && reason.is_empty() && !line.is_empty() {
            reason = line.to_owned();
        }
    }
    winner.map(|w| (w, reason))
}

pub fn first_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

fn last_chars(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let (at, _) = text.char_indices().nth(count - max).unwrap();
    &text[at..]
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_are_read_leniently() {
        let labels = ["A", "B", "C"];
        assert_eq!(parse_verdict("Winner: B\nReason: Smaller and passes.", &labels), Some(("B".into(), "Smaller and passes.".into())));
        assert_eq!(parse_verdict("**Winner:** Candidate c\nIt is the only one with tests.", &labels).unwrap().0, "C");
        assert_eq!(parse_verdict("I can't decide.", &labels), None);
        assert_eq!(parse_verdict("Winner: D", &labels), None);
    }

    #[test]
    fn a_brief_quotes_the_newest_notes_and_counts_the_rest() {
        let mut team = Team::default();
        for i in 0..8 {
            team.notes.push(Note { id: i, author: ThreadId(1), author_label: "architect".into(), text: format!("note {i}"), at_ms: i });
        }
        team.workers.push(Worker { key: "w1".into(), title: "API".into(), summary: "Added /users.".into(), state: WorkerState::Done, ..Default::default() });
        let w = Worker { key: "w2".into(), title: "UI".into(), brief: "Build the page.".into(), depends_on: vec!["w1".into()], scope: vec!["web/".into()], ..Default::default() };
        let brief = worker_brief(&team, &w);
        assert!(brief.contains("- w1 (API): Added /users."), "{brief}");
        assert!(brief.contains("note 7") && brief.contains("note 3") && !brief.contains("note 2"), "{brief}");
        assert!(brief.contains("3 more notes"), "{brief}");
        assert!(brief.contains("web/"));
    }

    #[test]
    fn mail_is_one_structured_block() {
        let m = Mail {
            id: 4,
            from: None,
            from_label: "w1".into(),
            to: ThreadId(1),
            kind: MailKind::Question,
            text: "Which port?".into(),
            urgent: true,
            sent_ms: 0,
            delivered_ms: None,
        };
        let text = mail_message(TeamId(1), &[&m]);
        assert!(text.starts_with("<orbit-team-mail"));
        assert!(text.contains("kind=\"question\" urgent=\"true\">\nWhich port?\n</mail>"), "{text}");
    }
}
