//! Claude Code tools: kinds, one-line titles, file changes and diff previews.
//!
//! Inputs are parsed with targeted structs over the raw JSON, so a `Write`
//! with a megabyte of content is scanned once and never built into a tree.

use std::borrow::Cow;
use std::path::Path;

use orbit_proto::*;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};

/// Largest file read from disk to build an exact diff preview.
const MAX_PREVIEW_FILE: u64 = 2 * 1024 * 1024;

/// Tools that never change anything: allowed through in readonly threads and
/// the only built-in tools a readonly session is started with.
pub const READONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "ToolSearch",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];

pub fn kind_of(name: &str) -> ToolKind {
    match name {
        "Bash" | "PowerShell" | "BashOutput" | "KillShell" => ToolKind::Command,
        "Read" | "LS" => ToolKind::Read,
        "Glob" | "Grep" | "ToolSearch" => ToolKind::Search,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "WebFetch" | "WebSearch" => ToolKind::Web,
        "Task" | "Agent" => ToolKind::Task,
        "TodoWrite" => ToolKind::Todo,
        _ if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

/// The fields of every tool input Orbit reads. Unknown fields are skipped
/// without being parsed.
#[derive(Default, Deserialize)]
struct Input<'a> {
    #[serde(borrow, default)]
    command: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    description: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    notebook_path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pattern: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    url: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    query: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    subagent_type: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    skill: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    old_string: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    new_string: Option<Cow<'a, str>>,
    #[serde(default)]
    replace_all: bool,
    #[serde(borrow, default)]
    content: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    new_source: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    edits: Vec<EditOp<'a>>,
    #[serde(borrow, default)]
    todos: Vec<Todo<'a>>,
    #[serde(borrow, default)]
    plan: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct EditOp<'a> {
    #[serde(borrow, default)]
    old_string: Cow<'a, str>,
    #[serde(borrow, default)]
    new_string: Cow<'a, str>,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct Todo<'a> {
    #[serde(borrow, default)]
    content: Cow<'a, str>,
    #[serde(borrow, default, rename = "activeForm")]
    active_form: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    status: Cow<'a, str>,
}

fn parse(input: &str) -> Input<'_> {
    sonic_rs::from_str(input).unwrap_or_default()
}

/// A path as shown to the user: relative to the session's directory when it
/// is inside it.
pub fn display_path(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    match p.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => path.to_owned(),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Builds the tool call for a `tool_use` block. `input` is compact JSON;
/// empty while the input is still streaming.
pub fn tool_call(call_id: &str, name: &str, input: &str, cwd: &Path) -> ToolCall {
    let kind = kind_of(name);
    let parsed = if input.is_empty() { Input::default() } else { parse(input) };
    let title = title(name, kind, &parsed, cwd);
    let files = if input.is_empty() { Vec::new() } else { planned_changes(name, &parsed) };
    ToolCall {
        call_id: call_id.to_owned(),
        kind,
        name: name.to_owned(),
        title,
        input: input.to_owned(),
        output: String::new(),
        output_blob: None,
        exit_code: None,
        files,
    }
}

fn title(name: &str, kind: ToolKind, i: &Input, cwd: &Path) -> String {
    let path = || i.file_path.as_deref().or(i.notebook_path.as_deref()).map(|p| display_path(p, cwd));
    let t = match kind {
        ToolKind::Command => i.command.as_deref().map(|c| first_line(c).to_owned()),
        ToolKind::Read | ToolKind::Edit => path().or_else(|| i.path.as_deref().map(|p| display_path(p, cwd))),
        ToolKind::Search => i.pattern.as_deref().or(i.query.as_deref()).map(|p| match i.path.as_deref() {
            Some(dir) => format!("{p} in {}", display_path(dir, cwd)),
            None => p.to_owned(),
        }),
        ToolKind::Web => i.url.as_deref().or(i.query.as_deref()).map(str::to_owned),
        ToolKind::Task => i.description.as_deref().map(|d| match i.subagent_type.as_deref() {
            Some(agent) => format!("{d} ({agent})"),
            None => d.to_owned(),
        }),
        ToolKind::Mcp => {
            let mut parts = name.trim_start_matches("mcp__").splitn(2, "__");
            let server = parts.next().unwrap_or_default();
            Some(match parts.next() {
                Some(tool) => format!("{server}: {}", humanize_id(tool)),
                None => humanize_id(server),
            })
        }
        ToolKind::Todo => Some("Update plan".to_owned()),
        ToolKind::Other => i
            .skill
            .as_deref()
            .or(i.description.as_deref())
            .or(i.query.as_deref())
            .map(|s| first_line(s).to_owned()),
    };
    t.filter(|t| !t.is_empty()).unwrap_or_else(|| name.to_owned())
}

fn count_lines(s: &str) -> u32 {
    s.lines().count() as u32
}

/// Added and deleted line counts between two texts.
fn line_delta(old: &str, new: &str) -> (u32, u32) {
    let diff = TextDiff::from_lines(old, new);
    let (mut add, mut del) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => add += 1,
            ChangeTag::Delete => del += 1,
            ChangeTag::Equal => {}
        }
    }
    (add, del)
}

/// The changes a file tool's input describes, before it runs. The tool
/// result replaces these with exact counts.
fn planned_changes(name: &str, i: &Input) -> Vec<FileChange> {
    let Some(path) = i.file_path.as_deref().or(i.notebook_path.as_deref()) else { return Vec::new() };
    let (kind, additions, deletions) = match name {
        "Edit" => {
            let (a, d) = line_delta(i.old_string.as_deref().unwrap_or(""), i.new_string.as_deref().unwrap_or(""));
            (ChangeKind::Modified, a, d)
        }
        "MultiEdit" => {
            let (a, d) = i.edits.iter().fold((0, 0), |(a, d), e| {
                let (ea, ed) = line_delta(&e.old_string, &e.new_string);
                (a + ea, d + ed)
            });
            (ChangeKind::Modified, a, d)
        }
        "Write" => {
            let kind = if Path::new(path).exists() { ChangeKind::Modified } else { ChangeKind::Added };
            (kind, count_lines(i.content.as_deref().unwrap_or("")), 0)
        }
        "NotebookEdit" => (ChangeKind::Modified, count_lines(i.new_source.as_deref().unwrap_or("")), 0),
        _ => return Vec::new(),
    };
    vec![FileChange { path: path.to_owned(), kind, additions, deletions }]
}

/// The agent's todo list from a `TodoWrite` input.
pub fn plan_steps(name: &str, input: &str) -> Option<Vec<PlanStep>> {
    if name != "TodoWrite" {
        return None;
    }
    let i = parse(input);
    Some(
        i.todos
            .iter()
            .map(|t| {
                let state = match t.status.as_ref() {
                    "in_progress" => StepState::Active,
                    "completed" => StepState::Done,
                    _ => StepState::Pending,
                };
                let text = match (state, t.active_form.as_deref()) {
                    (StepState::Active, Some(active)) if !active.is_empty() => active,
                    _ => t.content.as_ref(),
                };
                PlanStep { text: text.to_owned(), state }
            })
            .collect(),
    )
}

/// The subset of a file tool's `tool_use_result` that gives exact counts.
#[derive(Deserialize)]
struct FileResult<'a> {
    #[serde(borrow, default, rename = "type")]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default, rename = "filePath")]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    content: Option<Cow<'a, str>>,
    #[serde(borrow, default, rename = "structuredPatch")]
    structured_patch: Option<Vec<Hunk<'a>>>,
}

#[derive(Deserialize)]
struct Hunk<'a> {
    #[serde(borrow, default)]
    lines: Vec<Cow<'a, str>>,
}

/// Exact file changes from a file tool's `tool_use_result`.
pub fn result_changes(name: &str, result: &str) -> Option<Vec<FileChange>> {
    if kind_of(name) != ToolKind::Edit {
        return None;
    }
    let r: FileResult = sonic_rs::from_str(result).ok()?;
    let path = r.file_path?.into_owned();
    let created = r.ty.as_deref() == Some("create");
    let (mut additions, mut deletions) = (0u32, 0u32);
    match r.structured_patch {
        Some(hunks) if !hunks.is_empty() => {
            for line in hunks.iter().flat_map(|h| h.lines.iter()) {
                match line.as_bytes().first() {
                    Some(b'+') => additions += 1,
                    Some(b'-') => deletions += 1,
                    _ => {}
                }
            }
        }
        _ if created => additions = count_lines(r.content.as_deref().unwrap_or("")),
        _ => return None,
    }
    let kind = if created { ChangeKind::Added } else { ChangeKind::Modified };
    Some(vec![FileChange { path, kind, additions, deletions }])
}

/// A unified diff previewing what a file tool will do. Uses the file on disk
/// for real line numbers and context when it can, else the input alone.
pub fn diff_preview(name: &str, input: &str, cwd: &Path) -> Option<(String, String)> {
    let i = parse(input);
    let path = i.file_path.as_deref().or(i.notebook_path.as_deref())?.to_owned();
    let shown = display_path(&path, cwd);
    let current = read_small(&path);
    let (old, new): (String, String) = match name {
        "Edit" => {
            let (from, to) = (i.old_string.as_deref().unwrap_or(""), i.new_string.as_deref().unwrap_or(""));
            match current.as_deref() {
                Some(text) if !from.is_empty() && text.contains(from) => {
                    let after = if i.replace_all { text.replace(from, to) } else { text.replacen(from, to, 1) };
                    (text.to_owned(), after)
                }
                _ => (from.to_owned(), to.to_owned()),
            }
        }
        "MultiEdit" => match current.as_deref() {
            Some(text) if i.edits.iter().all(|e| !e.old_string.is_empty()) => {
                let mut after = text.to_owned();
                for e in &i.edits {
                    after = if e.replace_all {
                        after.replace(e.old_string.as_ref(), &e.new_string)
                    } else {
                        after.replacen(e.old_string.as_ref(), &e.new_string, 1)
                    };
                }
                (text.to_owned(), after)
            }
            _ => {
                let old = i.edits.iter().map(|e| e.old_string.as_ref()).collect::<Vec<_>>().join("\n");
                let new = i.edits.iter().map(|e| e.new_string.as_ref()).collect::<Vec<_>>().join("\n");
                (old, new)
            }
        },
        "Write" => (current.unwrap_or_default(), i.content.as_deref().unwrap_or("").to_owned()),
        "NotebookEdit" => (String::new(), i.new_source.as_deref().unwrap_or("").to_owned()),
        _ => return None,
    };
    let diff = TextDiff::from_lines(&old, &new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{shown}"), &format!("b/{shown}"))
        .to_string();
    Some((path, diff))
}

fn read_small(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PREVIEW_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The request shown for a `can_use_tool` that is not a question or a plan.
pub fn request_kind(name: &str, input: &str, reason: Option<&str>, cwd: &Path) -> RequestKind {
    match kind_of(name) {
        ToolKind::Command if name == "Bash" || name == "PowerShell" => {
            let i = parse(input);
            RequestKind::Command {
                command: i.command.as_deref().unwrap_or_default().to_owned(),
                cwd: cwd.display().to_string(),
                reason: i.description.as_deref().or(reason).unwrap_or_default().to_owned(),
            }
        }
        ToolKind::Edit => match diff_preview(name, input, cwd) {
            Some((path, diff)) => RequestKind::FileChange { path, diff },
            None => RequestKind::Tool { name: name.to_owned(), input: input.to_owned() },
        },
        _ => RequestKind::Tool { name: name.to_owned(), input: input.to_owned() },
    }
}

/// The plan text of an `ExitPlanMode` input.
pub fn plan_text(input: &str) -> String {
    parse(input).plan.map(Cow::into_owned).unwrap_or_default()
}

/// The `command` of a Bash or PowerShell input.
pub fn command_of(input: &str) -> Option<String> {
    parse(input).command.map(Cow::into_owned)
}

/// The file a file tool's input targets.
pub fn file_of(input: &str) -> Option<String> {
    let i = parse(input);
    i.file_path.or(i.notebook_path).map(Cow::into_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tools() {
        assert_eq!(kind_of("Bash"), ToolKind::Command);
        assert_eq!(kind_of("Grep"), ToolKind::Search);
        assert_eq!(kind_of("LS"), ToolKind::Read);
        assert_eq!(kind_of("MultiEdit"), ToolKind::Edit);
        assert_eq!(kind_of("mcp__github__create_issue"), ToolKind::Mcp);
        assert_eq!(kind_of("Agent"), ToolKind::Task);
        assert_eq!(kind_of("Skill"), ToolKind::Other);
    }

    #[test]
    fn titles_are_short_and_relative() {
        let cwd = Path::new("/repo");
        let call = tool_call("t1", "Read", r#"{"file_path":"/repo/src/main.rs"}"#, cwd);
        assert_eq!(call.title, "src/main.rs");
        let call = tool_call("t2", "Bash", r#"{"command":"cargo test\n--all","description":"x"}"#, cwd);
        assert_eq!(call.title, "cargo test");
        let call = tool_call("t3", "mcp__github__create_issue", "{}", cwd);
        assert_eq!(call.title, "github: Create issue");
        let call = tool_call("t4", "Grep", r#"{"pattern":"fn main","path":"/repo/src"}"#, cwd);
        assert_eq!(call.title, "fn main in src");
    }

    #[test]
    fn edit_counts_lines() {
        let call = tool_call(
            "t",
            "Edit",
            r#"{"file_path":"/nope/a.rs","old_string":"a\nb\n","new_string":"a\nc\nd\n"}"#,
            Path::new("/"),
        );
        assert_eq!(
            call.files,
            vec![FileChange { path: "/nope/a.rs".into(), kind: ChangeKind::Modified, additions: 2, deletions: 1 }]
        );
    }

    #[test]
    fn diff_preview_uses_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let input = format!(
            r#"{{"file_path":{},"old_string":"two","new_string":"2"}}"#,
            sonic_rs::to_string(&file.display().to_string()).unwrap()
        );
        let (_, diff) = diff_preview("Edit", &input, dir.path()).unwrap();
        assert!(diff.contains("--- a/a.txt"), "{diff}");
        assert!(diff.contains("-two\n+2\n"), "{diff}");
        assert!(diff.contains(" one\n"), "{diff}");
    }

    #[test]
    fn result_changes_count_the_structured_patch() {
        let r = r#"{"filePath":"/r/x.rs","oldString":"a","structuredPatch":[{"oldStart":1,"lines":[" k","-a","+b","+c"]}],"originalFile":"big"}"#;
        let files = result_changes("Edit", r).unwrap();
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        let r = r#"{"type":"create","filePath":"/r/n.txt","content":"hi\nthere","structuredPatch":[],"originalFile":null}"#;
        let files = result_changes("Write", r).unwrap();
        assert_eq!(files[0].kind, ChangeKind::Added);
        assert_eq!(files[0].additions, 2);
    }

    #[test]
    fn todo_write_becomes_plan_steps() {
        let steps = plan_steps(
            "TodoWrite",
            r#"{"todos":[{"content":"Write tests","activeForm":"Writing tests","status":"in_progress"},{"content":"Ship","status":"pending"},{"content":"Plan","status":"completed"}]}"#,
        )
        .unwrap();
        assert_eq!(steps[0], PlanStep { text: "Writing tests".into(), state: StepState::Active });
        assert_eq!(steps[1].state, StepState::Pending);
        assert_eq!(steps[2].state, StepState::Done);
    }
}
