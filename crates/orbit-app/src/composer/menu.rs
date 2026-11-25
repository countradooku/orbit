//! The completion list above the composer: `@` files and threads, `$`
//! skills, `/` commands.
//!
//! Ranking here is pure and bounded: skills and commands are a provider's
//! list (hundreds at most) and threads are the shell's records, so filtering
//! them per keystroke costs microseconds. File ranking happens on the host
//! ([`Command::SearchFiles`]); this module only shows its answer.

use orbit_core::fuzzy;
use orbit_proto::*;

/// Rows a list shows.
pub const MAX_ROWS: usize = 8;
/// Thread rows under the files of an `@` list.
pub const MAX_THREAD_ROWS: usize = 3;

/// One row of the list.
#[derive(Clone, Debug, PartialEq)]
pub enum Suggestion {
    File(FileMatch),
    Thread { id: ThreadId, title: String, project: Option<String> },
    /// A provider command or skill. `skill` rows insert `$name`.
    Command(SlashCommand),
    /// Orbit's own commands.
    Builtin(Builtin),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// Open the model picker.
    Model,
    /// Toggle plan mode.
    Plan,
}

impl Builtin {
    pub const ALL: [Builtin; 2] = [Builtin::Model, Builtin::Plan];

    pub fn name(self) -> &'static str {
        match self {
            Builtin::Model => "model",
            Builtin::Plan => "plan",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Builtin::Model => "Choose the agent, model and effort",
            Builtin::Plan => "Turn plan mode on or off",
        }
    }
}

/// Provider commands and skills for `query`, best first. Names match
/// before descriptions; `__internal` commands are hidden; duplicates by name
/// keep the first.
pub fn rank_commands(query: &str, commands: &[SlashCommand], limit: usize) -> Vec<SlashCommand> {
    let mut seen = std::collections::HashSet::new();
    let unique = commands.iter().filter(|c| !c.name.starts_with("__") && !c.name.is_empty() && seen.insert(c.name.as_str()));
    if query.is_empty() {
        let mut all: Vec<&SlashCommand> = unique.collect();
        all.sort_by_key(|c| c.name.to_lowercase());
        return all.into_iter().take(limit).cloned().collect();
    }
    let scored = unique.filter_map(|c| {
        let by_name = fuzzy::score(query, &c.name);
        // Descriptions match words, not scattered letters, and rank below
        // any name match.
        let by_description = || contains_ignore_case(&c.description, query).then_some(-200);
        by_name.or_else(by_description).map(|s| (s, c))
    });
    fuzzy::top(scored, limit).into_iter().cloned().collect()
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.to_lowercase();
    needle.split_whitespace().all(|term| haystack.contains(&term.to_lowercase()))
}

/// Orbit's commands matching `query`.
pub fn rank_builtins(query: &str) -> Vec<Builtin> {
    let scored = Builtin::ALL.into_iter().filter_map(|b| fuzzy::score(query, b.name()).map(|s| (s, b)));
    fuzzy::top(scored, Builtin::ALL.len())
}

/// Threads whose title matches `query`, most relevant then most recent.
/// `threads` are (id, title, project name, updated ms).
pub fn rank_threads<'a>(
    query: &str,
    threads: impl Iterator<Item = (ThreadId, &'a str, Option<&'a str>, u64)>,
    exclude: Option<ThreadId>,
    limit: usize,
) -> Vec<Suggestion> {
    let mut candidates: Vec<(ThreadId, &str, Option<&str>, u64)> =
        threads.filter(|(id, title, _, _)| Some(*id) != exclude && !title.is_empty()).collect();
    candidates.sort_by_key(|(_, _, _, updated)| std::cmp::Reverse(*updated));
    let scored = candidates.into_iter().filter_map(|(id, title, project, _)| {
        if query.is_empty() {
            return Some((0, (id, title, project)));
        }
        fuzzy::score(query, title).map(|s| (s, (id, title, project)))
    });
    fuzzy::top(scored, limit)
        .into_iter()
        .map(|(id, title, project)| Suggestion::Thread { id, title: title.to_owned(), project: project.map(str::to_owned) })
        .collect()
}

/// A file match split for display: the name, and the folder it is in.
pub fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => (&path[i + 1..], &path[..i + 1]),
        None => (path, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, description: &str) -> SlashCommand {
        SlashCommand { name: name.into(), description: description.into(), argument_hint: String::new() }
    }

    #[test]
    fn names_rank_before_descriptions_and_internal_commands_hide() {
        let commands = vec![
            cmd("review", "Review a pull request"),
            cmd("compact", "Free up context by summarizing"),
            cmd("__debug", "internal"),
            cmd("pr-comments", "Show pull request comments"),
            cmd("review", "duplicate"),
        ];
        let names = |q: &str| rank_commands(q, &commands, 8).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("rev"), vec!["review"]);
        let mut pull = names("pull");
        pull.sort();
        assert_eq!(pull, vec!["pr-comments", "review"]);
        assert_eq!(names(""), vec!["compact", "pr-comments", "review"]);
        assert!(names("debug").is_empty(), "internal commands stay hidden");
        assert_eq!(names("summ"), vec!["compact"], "a description match still finds it");
    }

    #[test]
    fn many_skills_rank_quickly() {
        let skills: Vec<SlashCommand> = (0..2_000).map(|i| cmd(&format!("skill-{i}"), "does a thing")).collect();
        let start = std::time::Instant::now();
        let hits = rank_commands("skl19", &skills, 8);
        assert!(!hits.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn threads_rank_by_title_then_recency_and_skip_this_thread() {
        let a = ThreadId(1);
        let b = ThreadId(2);
        let c = ThreadId(3);
        let threads = [(a, "Fix the parser", None, 10), (b, "Parser benchmarks", Some("orbit"), 30), (c, "Unrelated", None, 20)];
        let ids = |q: &str, exclude| {
            rank_threads(q, threads.iter().map(|(i, t, p, u)| (*i, *t, *p, *u)), exclude, 5)
                .into_iter()
                .map(|s| match s {
                    Suggestion::Thread { id, .. } => id,
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("parser", None).len(), 2);
        assert_eq!(ids("", None), vec![b, c, a], "most recent first");
        assert_eq!(ids("", Some(b)), vec![c, a]);
    }

    #[test]
    fn paths_split_into_name_and_folder() {
        assert_eq!(split_path("src/engine/files.rs"), ("files.rs", "src/engine/"));
        assert_eq!(split_path("src/engine/"), ("engine/", "src/"));
        assert_eq!(split_path("README.md"), ("README.md", ""));
        assert_eq!(rank_builtins("mod"), vec![Builtin::Model]);
    }
}
