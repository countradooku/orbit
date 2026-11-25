//! The changes a file-changing step made, as a unified patch, from what the
//! agent sent: Claude's `Edit`, `MultiEdit` and `Write` inputs, and Codex's
//! per-file diffs.
//!
//! Codex diffs and whole-file writes carry real line numbers. An `Edit`
//! only carries the replaced text, so its hunks are numbered within that
//! snippet ([`EditPatch::numbered`] is false); `git apply` still finds them
//! by their context. The timeline shows these inline, and review falls back
//! to them for turns that have no checkpoint.

use std::path::Path;

use orbit_proto::{Item, ItemBody, ToolCall, ToolKind};
use serde::Deserialize;

/// Lines of context around each change in snippet hunks.
const CONTEXT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditPatch {
    /// Unified diff text with `--- a/` and `+++ b/` headers.
    pub text: String,
    /// Line numbers are the file's own, not a snippet's.
    pub numbered: bool,
}

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    file_path: String,
    #[serde(default)]
    old_string: String,
    #[serde(default)]
    new_string: String,
    #[serde(default)]
    edits: Vec<Replace>,
    #[serde(default)]
    content: Option<String>,
    /// Codex: one unified diff per changed file.
    #[serde(default)]
    changes: Vec<PatchChange>,
}

#[derive(Deserialize)]
struct Replace {
    #[serde(default)]
    old_string: String,
    #[serde(default)]
    new_string: String,
}

#[derive(Deserialize)]
struct PatchChange {
    #[serde(default)]
    path: String,
    #[serde(default)]
    diff: String,
}

/// The patch a step made, with paths relative to `root` when they are
/// inside it. `None` for steps that change no file, or whose input names no
/// change (yet, while it streams).
pub fn patch(call: &ToolCall, root: Option<&Path>) -> Option<EditPatch> {
    if call.kind != ToolKind::Edit {
        return None;
    }
    let input: Input = sonic_rs::from_str(&call.input).ok()?;
    let mut text = String::new();
    let mut numbered = false;
    if !input.changes.is_empty() {
        numbered = true;
        for change in input.changes.iter().filter(|c| !c.diff.is_empty()) {
            text.push_str(&with_headers(&relative(&change.path, root), &change.diff));
        }
    } else {
        let path = if input.file_path.is_empty() { call.title.clone() } else { relative(&input.file_path, root) };
        if !input.edits.is_empty() {
            text.push_str(&header(&path));
            for edit in &input.edits {
                text.push_str(&hunks(&edit.old_string, &edit.new_string));
            }
        } else if let Some(content) = &input.content {
            // A whole file written: every line is new, numbered from 1.
            numbered = true;
            text.push_str(&header(&path));
            text.push_str(&hunks("", content));
        } else if !input.old_string.is_empty() || !input.new_string.is_empty() {
            text.push_str(&header(&path));
            text.push_str(&hunks(&input.old_string, &input.new_string));
        }
    }
    (text.contains("\n@@ ")).then_some(EditPatch { text, numbered })
}

/// Adds file headers to a patch that has only hunks.
pub fn with_headers(path: &str, patch: &str) -> String {
    if patch.starts_with("--- ") || patch.starts_with("diff --git ") {
        return patch.to_owned();
    }
    let mut out = header(&path.replace('\\', "/"));
    out.push_str(patch);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A change to one file, as the agent described it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Replace { old: String, new: String, all: bool },
    Write { content: String },
}

/// The changes a step made, per file (relative path).
fn ops(call: &ToolCall, root: Option<&Path>) -> Vec<(String, Op)> {
    if call.kind != ToolKind::Edit {
        return Vec::new();
    }
    let Ok(input) = sonic_rs::from_str::<Input>(&call.input) else { return Vec::new() };
    if input.file_path.is_empty() {
        return Vec::new();
    }
    let path = relative(&input.file_path, root);
    let all = sonic_rs::from_str::<ReplaceAll>(&call.input).map(|r| r.replace_all).unwrap_or(false);
    if !input.edits.is_empty() {
        input
            .edits
            .into_iter()
            .map(|e| (path.clone(), Op::Replace { old: e.old_string, new: e.new_string, all: false }))
            .collect()
    } else if let Some(content) = input.content {
        vec![(path, Op::Write { content })]
    } else if !input.old_string.is_empty() || !input.new_string.is_empty() {
        vec![(path, Op::Replace { old: input.old_string, new: input.new_string, all })]
    } else {
        Vec::new()
    }
}

#[derive(Deserialize)]
struct ReplaceAll {
    #[serde(default)]
    replace_all: bool,
}

/// Undoes one change; `None` when the text it wrote is no longer there.
fn undo(state: &str, op: &Op, earliest: bool) -> Option<String> {
    match op {
        Op::Replace { old, new, all } => {
            if new.is_empty() || !state.contains(new.as_str()) {
                return None;
            }
            Some(if *all { state.replace(new.as_str(), old) } else { state.replacen(new.as_str(), old, 1) })
        }
        // What a write replaced is unknown, unless it created the file.
        Op::Write { .. } => earliest.then(String::new),
    }
}

/// The review of the turns `in_scope` accepts, rebuilt from the agent's
/// edits: each changed file's text before and after the scope is found by
/// undoing, newest first, the edits made since (from `read`'s current
/// text), so the diff has the file's real lines and line numbers. A file
/// whose edits cannot be undone (changed since by other means) falls back
/// to the edits' own hunks.
pub fn scope_patch(
    items: &[Item],
    in_scope: impl Fn(u32) -> bool,
    root: Option<&Path>,
    read: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // Every edit, oldest first, with whether it is in the scope.
    let mut edits: Vec<(String, Op, bool, &ToolCall)> = Vec::new();
    for item in items {
        let ItemBody::Tool(call) = &item.body else { continue };
        for (path, op) in ops(call, root) {
            edits.push((path, op, in_scope(item.turn), call));
        }
    }
    let mut paths: Vec<&str> = Vec::new();
    for (path, _, scoped, _) in &edits {
        if *scoped && !paths.contains(&path.as_str()) {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return turn_patch(items.iter().filter(|i| in_scope(i.turn)), root);
    }
    let mut out = String::new();
    for path in paths {
        let file_edits: Vec<&(String, Op, bool, &ToolCall)> = edits.iter().filter(|e| e.0 == path).collect();
        let exact = (|| {
            let mut state = read(path).unwrap_or_default();
            let first_scoped = file_edits.iter().position(|e| e.2)?;
            let last_scoped = file_edits.iter().rposition(|e| e.2)?;
            let mut after = None;
            for (i, (_, op, _, _)) in file_edits.iter().enumerate().rev() {
                if i < first_scoped {
                    break;
                }
                if i == last_scoped {
                    after = Some(state.clone());
                }
                state = undo(&state, op, i == 0)?;
            }
            Some((state, after?))
        })();
        match exact {
            Some((before, after)) if before != after => {
                let diff = similar::TextDiff::from_lines(&before, &after);
                out.push_str(&format!("diff --git a/{path} b/{path}\n"));
                if before.is_empty() {
                    out.push_str("new file mode 100644\n");
                }
                out.push_str(&diff.unified_diff().context_radius(3).header(&format!("a/{path}"), &format!("b/{path}")).to_string());
            }
            Some(_) => {}
            None => {
                let calls: Vec<Item> = file_edits
                    .iter()
                    .filter(|e| e.2)
                    .map(|e| Item { id: orbit_proto::ItemId(0), turn: 0, status: orbit_proto::ItemStatus::Done, body: ItemBody::Tool(e.3.clone()) })
                    .collect();
                if let Some(patch) = turn_patch_in(&calls, root, &read) {
                    out.push_str(&patch);
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// One patch of every edit in `items` (one turn, or several), grouped by
/// file in the order files were first changed. `None` when they changed
/// nothing the agent described.
pub fn turn_patch<'a>(items: impl IntoIterator<Item = &'a Item>, root: Option<&Path>) -> Option<String> {
    turn_patch_in(items, root, |_| None)
}

/// [`turn_patch`], with each file's hunks placed where their text is in
/// the file's current content (`read`), or else one after another: snippet
/// hunks are all numbered from line 1, and a diff viewer drops hunks that
/// overlap.
fn turn_patch_in<'a>(
    items: impl IntoIterator<Item = &'a Item>,
    root: Option<&Path>,
    read: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut files: Vec<(String, String)> = Vec::new();
    for item in items {
        let ItemBody::Tool(call) = &item.body else { continue };
        let Some(patch) = patch(call, root) else { continue };
        for (path, hunks) in split_files(&patch.text) {
            match files.iter_mut().find(|(p, _)| *p == path) {
                Some((_, all)) => all.push_str(&hunks),
                None => files.push((path, hunks)),
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (path, hunks) in files {
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        out.push_str(&header(&path));
        out.push_str(&renumber(&hunks, read(&path).as_deref()));
    }
    Some(out)
}

/// Rewrites `@@` headers so hunks follow each other: each starts where its
/// new-side text is found in `current` (after the previous hunk), or right
/// after the previous hunk.
fn renumber(hunks: &str, current: Option<&str>) -> String {
    let file: Vec<&str> = current.map(|c| c.lines().collect()).unwrap_or_default();
    // Split into (body lines) per hunk.
    let mut parsed: Vec<Vec<&str>> = Vec::new();
    for line in hunks.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            parsed.push(Vec::new());
        } else if let Some(body) = parsed.last_mut() {
            body.push(line);
        }
    }
    let mut out = String::new();
    // Next free line on the new side, and old minus new so far.
    let (mut next_new, mut shift) = (1usize, 0isize);
    for body in parsed {
        let old_len = body.iter().filter(|l| l.starts_with(' ') || l.starts_with('-')).count();
        let new_len = body.iter().filter(|l| l.starts_with(' ') || l.starts_with('+')).count();
        let new_side: Vec<&str> = body
            .iter()
            .filter(|l| l.starts_with(' ') || l.starts_with('+'))
            .map(|l| l[1..].trim_end_matches(['\n', '\r']))
            .collect();
        let found = (!new_side.is_empty())
            .then(|| (next_new.saturating_sub(1)..file.len()).find(|&i| file[i..].starts_with(&new_side)))
            .flatten();
        let new_start = found.map_or(next_new, |i| i + 1);
        let old_start = (new_start as isize + shift).max(1) as usize;
        out.push_str(&format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"));
        for line in &body {
            out.push_str(line);
        }
        next_new = new_start + new_len;
        shift += old_len as isize - new_len as isize;
    }
    out
}

/// `(path, hunks)` per file section of a patch made by [`patch`].
fn split_files(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        if let Some(path) = line.strip_prefix("+++ b/") {
            out.push((path.trim_end().to_owned(), String::new()));
        } else if line.starts_with("--- ") || line.starts_with("diff --git ") {
            continue;
        } else if let Some((_, hunks)) = out.last_mut() {
            hunks.push_str(line);
        }
    }
    out
}

fn header(path: &str) -> String {
    format!("--- a/{path}\n+++ b/{path}\n")
}

/// The hunks of a snippet replacement. A snippet that stops mid-file has
/// no final newline; that is not the file's "no newline at end of file".
fn hunks(old: &str, new: &str) -> String {
    let (old, new) = if !old.ends_with('\n') && !new.ends_with('\n') {
        (format!("{old}\n"), format!("{new}\n"))
    } else {
        (old.to_owned(), new.to_owned())
    };
    let (old, new) = (if old == "\n" { String::new() } else { old }, if new == "\n" { String::new() } else { new });
    let diff = similar::TextDiff::from_lines(&old, &new);
    let mut out = String::new();
    for hunk in diff.unified_diff().context_radius(CONTEXT).iter_hunks() {
        out.push_str(&hunk.to_string());
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// `path` relative to `root` with forward slashes, when it is inside it.
fn relative(path: &str, root: Option<&Path>) -> String {
    let p = Path::new(path);
    let rel = root.and_then(|root| p.strip_prefix(root).ok()).unwrap_or(p);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_proto::{ItemId, ItemStatus};

    fn edit(input: &str) -> ToolCall {
        ToolCall {
            call_id: "c".into(),
            kind: ToolKind::Edit,
            name: "Edit".into(),
            title: "README.md".into(),
            input: input.into(),
            output: String::new(),
            output_blob: None,
            exit_code: None,
            files: vec![],
        }
    }

    #[test]
    fn edits_become_relative_patches_and_a_turn_groups_them_by_file() {
        let root = Path::new(if cfg!(windows) { "C:\\p" } else { "/p" });
        let path = root.join("README.md").to_string_lossy().replace('\\', "\\\\");
        let one = edit(&format!(r#"{{"file_path":"{path}","old_string":"a\nb\n","new_string":"a\nc\n"}}"#));
        let two = edit(&format!(r#"{{"file_path":"{path}","old_string":"x\n","new_string":"y\n"}}"#));
        let patch = patch(&one, Some(root)).unwrap();
        assert!(patch.text.starts_with("--- a/README.md\n+++ b/README.md\n@@ "), "{}", patch.text);
        assert!(!patch.numbered);

        let items: Vec<Item> = [one, two]
            .into_iter()
            .map(|call| Item { id: ItemId::new(), turn: 1, status: ItemStatus::Done, body: ItemBody::Tool(call) })
            .collect();
        let whole = turn_patch(&items, Some(root)).unwrap();
        assert_eq!(whole.matches("diff --git a/README.md b/README.md").count(), 1, "{whole}");
        assert_eq!(whole.matches("\n@@ ").count(), 2);
        assert!(whole.contains("-b\n+c\n") && whole.contains("-x\n+y\n"));
    }

    #[test]
    fn review_rebuilds_the_files_real_lines_from_disk() {
        let tool = |turn: u32, old: &str, new: &str| Item {
            id: ItemId::new(),
            turn,
            status: ItemStatus::Done,
            body: ItemBody::Tool(edit(&format!(
                r#"{{"file_path":"README.md","old_string":{},"new_string":{}}}"#,
                sonic_rs::to_string(old).unwrap(),
                sonic_rs::to_string(new).unwrap()
            ))),
        };
        // Turn 1 changed line 5; turn 2 changed line 9. The file on disk is
        // after both.
        let lines: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
        let mut now = lines.join("\n") + "\n";
        now = now.replace("line 5", "line five").replace("line 9", "line nine");
        let items = vec![tool(1, "line 5", "line five"), tool(2, "line 9", "line nine")];
        let read = |p: &str| (p == "README.md").then(|| now.clone());

        let turn1 = scope_patch(&items, |t| t == 1, None, read).unwrap();
        assert!(turn1.contains("@@ -2,7 +2,7 @@"), "real line numbers: {turn1}");
        assert!(turn1.contains("-line 5\n+line five\n"));
        assert!(!turn1.contains("nine"), "turn 2's change is not turn 1's: {turn1}");

        let both = scope_patch(&items, |_| true, None, read).unwrap();
        assert!(both.contains("+line five") && both.contains("+line nine"));
        assert_eq!(orbit_proto::diff::parse(&both).len(), 1, "one file, not one per edit");

        // Changed since by something else: the edits' own hunks instead,
        // numbered so they follow each other (a viewer drops overlaps).
        let other = |_: &str| Some("unrelated\n".to_owned());
        let fallback = scope_patch(&items, |t| t == 1, None, other).unwrap();
        assert!(fallback.contains("-line 5\n+line five\n"));
        let both = scope_patch(&items, |_| true, None, other).unwrap();
        let file = orbit_proto::diff::parse(&both).remove(0);
        assert_eq!(file.hunks.len(), 2, "{both}");
        assert!(file.hunks[1].new_start > file.hunks[0].new_start + file.hunks[0].new_len - 1, "{both}");

        // Hunks found in the current file take their real place.
        let moved: String = (1..=30).map(|i| format!("row {i}\n")).collect::<String>().replace("row 20", "line five");
        let placed = scope_patch(&items, |t| t == 1, None, |_| Some(moved.clone())).unwrap();
        assert!(placed.contains("+20,"), "{placed}");
    }

    #[test]
    fn writes_codex_changes_and_non_edits() {
        let write = edit(r#"{"file_path":"new.txt","content":"hello\nworld\n"}"#);
        let p = patch(&write, None).unwrap();
        assert!(p.numbered && p.text.contains("@@ -0,0 +1,2 @@"));
        let codex = edit(r#"{"changes":[{"path":"src/x.rs","diff":"@@ -10,2 +10,2 @@\n keep\n-old\n+new\n"}]}"#);
        assert!(patch(&codex, None).unwrap().text.starts_with("--- a/src/x.rs"));
        assert!(patch(&edit(r#"{"file_path":"x"}"#), None).is_none());
        let mut read = edit(r#"{"file_path":"x","old_string":"a","new_string":"b"}"#);
        read.kind = ToolKind::Read;
        assert!(patch(&read, None).is_none());
    }
}
