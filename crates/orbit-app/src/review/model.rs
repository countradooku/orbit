//! What the review sheet shows, computed off the UI thread from one patch:
//! files in tree order, stable hunk and file ids, the folder tree, filters,
//! and the message review comments become. No GPUI state lives here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use gpui_kit::SharedString;
use gpui_kit::component::diff::DiffFile;
use gpui_kit::component::tree::TreeItem;
use orbit_proto::diff::{self, FileDiff, FileDiffKind, LineKind, Side};

/// One parsed diff, ready to show.
pub struct ReviewData {
    /// Files in tree order: folders before files, each level by name.
    pub files: Vec<FileDiff>,
    /// The same files for the diff view, in the same order.
    pub views: Vec<DiffFile>,
    pub file_ids: Vec<u64>,
    /// Per file, per hunk.
    pub hunk_ids: Vec<Vec<u64>>,
    /// [`DiffFile::path`] → index into `files`.
    pub by_path: HashMap<SharedString, usize>,
    /// Hunk id → (file, hunk).
    pub hunks: HashMap<u64, (usize, usize)>,
    /// A cheap fingerprint of the patch, to skip no-op refreshes.
    pub fingerprint: u64,
}

impl ReviewData {
    pub fn empty() -> Self {
        Self::parse("")
    }

    /// Parses `patch` (both for review logic and for display) and orders it.
    pub fn parse(patch: &str) -> Self {
        let mut files = diff::parse(patch);
        let views = DiffFile::parse(patch).unwrap_or_default();
        // Pair the display files with ours by path; both parsers keep patch order.
        let mut view_by_path: HashMap<String, DiffFile> = HashMap::with_capacity(views.len());
        for view in views {
            view_by_path.insert(view.path().to_string(), view);
        }
        let order = tree_order(files.iter().map(|f| f.path()));
        let mut slots: Vec<Option<FileDiff>> = files.drain(..).map(Some).collect();
        let mut files = Vec::with_capacity(slots.len());
        let mut views = Vec::with_capacity(slots.len());
        for ix in order {
            let Some(file) = slots[ix].take() else { continue };
            let view = view_by_path
                .remove(file.path())
                .or_else(|| file.old_path.as_deref().and_then(|p| view_by_path.remove(p)))
                .unwrap_or_else(|| DiffFile::unchanged(file.path().to_owned(), ""));
            files.push(file);
            views.push(view);
        }
        let file_ids = files.iter().map(diff::file_id).collect();
        let hunk_ids: Vec<Vec<u64>> =
            files.iter().map(|f| (0..f.hunks.len()).map(|h| diff::hunk_id(f, h)).collect()).collect();
        let mut hunks = HashMap::new();
        for (f, ids) in hunk_ids.iter().enumerate() {
            for (h, id) in ids.iter().enumerate() {
                hunks.entry(*id).or_insert((f, h));
            }
        }
        let by_path = views.iter().enumerate().map(|(ix, v)| (v.path().clone(), ix)).collect();
        Self {
            files,
            views,
            file_ids,
            hunk_ids,
            by_path,
            hunks,
            fingerprint: fingerprint(patch),
        }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The display path of file `ix` (what the diff view calls it).
    pub fn path(&self, ix: usize) -> SharedString {
        self.views[ix].path().clone()
    }

    pub fn index_of(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// Whether every hunk of file `ix` (or the file itself, when it has no
    /// hunks: binary, rename only) is marked reviewed.
    pub fn file_reviewed(&self, ix: usize, reviewed: &HashSet<u64>) -> bool {
        let hunks = &self.hunk_ids[ix];
        if hunks.is_empty() {
            reviewed.contains(&self.file_ids[ix])
        } else {
            hunks.iter().all(|h| reviewed.contains(h))
        }
    }

    /// The ids that mark file `ix` reviewed.
    pub fn file_marks(&self, ix: usize) -> Vec<u64> {
        let mut ids = self.hunk_ids[ix].clone();
        ids.push(self.file_ids[ix]);
        ids
    }

    /// Where a hunk starts: its first changed line, on the side that has it.
    pub fn hunk_anchor(&self, file: usize, hunk: usize) -> Option<(Side, u32)> {
        let h = self.files.get(file)?.hunks.get(hunk)?;
        h.lines.iter().find_map(|l| match l.kind {
            LineKind::Added => l.new_no.map(|n| (Side::New, n)),
            LineKind::Removed => l.old_no.map(|n| (Side::Old, n)),
            _ => None,
        })
    }

    /// Contiguous runs of added (new side) and removed (old side) lines, for
    /// painting changes in a palette of our choosing.
    pub fn change_runs(&self, file: usize) -> Vec<(Side, u32, u32)> {
        let mut runs: Vec<(Side, u32, u32)> = Vec::new();
        for hunk in &self.files[file].hunks {
            for line in &hunk.lines {
                let (side, no) = match line.kind {
                    LineKind::Added => (Side::New, line.new_no),
                    LineKind::Removed => (Side::Old, line.old_no),
                    _ => continue,
                };
                let Some(no) = no else { continue };
                match runs.last_mut() {
                    Some((s, _, end)) if *s == side && *end + 1 == no => *end = no,
                    _ => runs.push((side, no, no)),
                }
            }
        }
        runs
    }

    /// Word-level change spans of file `ix`: (side, line, byte range).
    pub fn word_spans(&self, file: usize) -> Vec<(Side, u32, Range<usize>)> {
        let f = &self.files[file];
        let mut out = Vec::new();
        for (h, hunk) in f.hunks.iter().enumerate() {
            for (line, ranges) in hunk.lines.iter().zip(diff::word_diff(f, h)) {
                let (side, no) = match line.kind {
                    LineKind::Added => (Side::New, line.new_no),
                    LineKind::Removed => (Side::Old, line.old_no),
                    _ => continue,
                };
                let Some(no) = no else { continue };
                out.extend(ranges.into_iter().map(|r| (side, no, r.start as usize..r.end as usize)));
            }
        }
        out
    }
}

fn fingerprint(text: &str) -> u64 {
    // FNV-1a over the bytes; a refresh compares this before re-laying out.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in text.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^ text.len() as u64
}

// ---------------------------------------------------------------------------
// Tree

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<(String, usize)>,
}

fn build_nodes<'a>(paths: impl IntoIterator<Item = (usize, &'a str)>) -> Node {
    let mut root = Node::default();
    for (ix, path) in paths {
        let mut node = &mut root;
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        for part in parts {
            node = node.dirs.entry(part.to_owned()).or_default();
        }
        node.files.push((name.to_owned(), ix));
    }
    root
}

/// File indexes in tree order (folders first, then files, each by name).
pub fn tree_order<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<usize> {
    fn walk(node: &Node, out: &mut Vec<usize>) {
        for child in node.dirs.values() {
            walk(child, out);
        }
        let mut files: Vec<&(String, usize)> = node.files.iter().collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(files.iter().map(|(_, ix)| *ix));
    }
    let root = build_nodes(paths.into_iter().enumerate());
    let mut out = Vec::new();
    walk(&root, &mut out);
    out
}

/// Tree ids: folders are `d:<path>`, files `f:<path>`.
pub fn file_item_id(path: &str) -> SharedString {
    format!("f:{path}").into()
}

pub fn item_path(id: &str) -> Option<&str> {
    id.strip_prefix("f:")
}

/// The folder tree of the given files (index, path), with single-child
/// folder chains merged (`src/engine`). Folders in `collapsed` start closed.
pub fn tree_items<'a>(files: impl IntoIterator<Item = (usize, &'a str)>, collapsed: &HashSet<SharedString>) -> Vec<TreeItem> {
    fn convert(name: String, prefix: &str, node: &Node, collapsed: &HashSet<SharedString>) -> TreeItem {
        // Merge chains of folders that only hold one folder.
        let (mut name, mut node) = (name, node);
        while node.files.is_empty() && node.dirs.len() == 1 {
            let (child, next) = node.dirs.iter().next().expect("one child");
            name = format!("{name}/{child}");
            node = next;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let id: SharedString = format!("d:{path}").into();
        let open = !collapsed.contains(&id);
        TreeItem::new(id, name).expanded(open).children(children(&path, node, collapsed))
    }
    fn children(prefix: &str, node: &Node, collapsed: &HashSet<SharedString>) -> Vec<TreeItem> {
        let mut out: Vec<TreeItem> =
            node.dirs.iter().map(|(name, child)| convert(name.clone(), prefix, child, collapsed)).collect();
        let mut files: Vec<&(String, usize)> = node.files.iter().collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(files.into_iter().map(|(name, _)| {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            TreeItem::new(file_item_id(&path), name.clone())
        }));
        out
    }
    let root = build_nodes(files);
    children("", &root, collapsed)
}

// ---------------------------------------------------------------------------
// Filters

/// Whether `path` matches every space-separated term of `query`
/// (case-insensitive substrings).
pub fn matches_filter(path: &str, query: &str) -> bool {
    let path = path.to_lowercase();
    query.split_whitespace().all(|term| path.contains(&term.to_lowercase()))
}

/// Test files by the usual conventions across languages.
pub fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let in_test_dir = lower.split('/').rev().skip(1).any(|dir| {
        matches!(dir, "test" | "tests" | "__tests__" | "spec" | "specs" | "testdata" | "fixtures" | "e2e")
    });
    let stem = name.split('.').next().unwrap_or(name);
    in_test_dir
        || name.contains(".test.")
        || name.contains(".spec.")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.starts_with("test_")
        || stem.ends_with("tests") && name.ends_with(".rs")
        || stem.ends_with("test") && (name.ends_with(".java") || name.ends_with(".kt") || name.ends_with(".cs"))
}

/// A short status letter for a file: A, D, M or R.
pub fn kind_letter(kind: FileDiffKind) -> &'static str {
    match kind {
        FileDiffKind::Added | FileDiffKind::Copied => "A",
        FileDiffKind::Deleted => "D",
        FileDiffKind::Modified => "M",
        FileDiffKind::Renamed => "R",
    }
}

// ---------------------------------------------------------------------------
// Comments

/// A review comment on a range of lines, or on a whole file (`start ==
/// None`). The excerpt is captured when the comment is made, so it still
/// says what was meant after the file changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub path: String,
    pub start: Option<(Side, u32)>,
    pub end: Option<(Side, u32)>,
    pub excerpt: String,
    pub text: String,
}

impl Comment {
    /// "line 12", "lines 12–14", "line 7 (before)", or "file".
    pub fn location(&self) -> String {
        let (Some((ss, s)), Some((es, e))) = (self.start, self.end) else { return "whole file".into() };
        let before = |side: Side| if side == Side::Old { " (before)" } else { "" };
        if ss == es && s == e {
            format!("line {s}{}", before(ss))
        } else if ss == es {
            format!("lines {s}–{e}{}", before(ss))
        } else {
            format!("line {s}{} to line {e}{}", before(ss), before(es))
        }
    }
}

/// The follow-up message that hands review comments to the agent: each
/// with its file, lines and the quoted diff it is about.
pub fn comments_message(comments: &[Comment]) -> String {
    let mut out = String::from(if comments.len() == 1 {
        "I reviewed your changes and left a comment. Please address it.\n"
    } else {
        "I reviewed your changes and left some comments. Please address each of them.\n"
    });
    for (n, c) in comments.iter().enumerate() {
        out.push('\n');
        if comments.len() > 1 {
            out.push_str(&format!("{}. ", n + 1));
        }
        let line = c.start.map(|(_, s)| format!(":{s}")).unwrap_or_default();
        out.push_str(&format!("`{}{line}` ({})\n", c.path, c.location()));
        if !c.excerpt.trim().is_empty() {
            let fence = if c.excerpt.contains("```") { "````" } else { "```" };
            out.push_str(fence);
            out.push_str("diff\n");
            out.push_str(&c.excerpt);
            if !c.excerpt.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(fence);
            out.push('\n');
        }
        out.push_str(c.text.trim());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,3 +1,3 @@
 one
-two
+deux
 three
@@ -10,2 +10,3 @@
 ten
+ten and a half
 eleven
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-old
+new
diff --git a/src/a/deep/x.rs b/src/a/deep/x.rs
new file mode 100644
--- /dev/null
+++ b/src/a/deep/x.rs
@@ -0,0 +1 @@
+x
";

    #[test]
    fn files_come_in_tree_order_with_ids_and_totals() {
        let data = ReviewData::parse(PATCH);
        let paths: Vec<&str> = data.files.iter().map(|f| f.path()).collect();
        assert_eq!(paths, vec!["src/a/deep/x.rs", "src/b.rs", "README.md"]);
        let views: Vec<String> = data.views.iter().map(|v| v.path().to_string()).collect();
        assert_eq!(views, paths, "display files line up with ours");
        assert_eq!(data.index_of("README.md"), Some(2));
        assert_eq!(data.hunk_ids[1].len(), 2);
        assert_eq!(data.hunks[&data.hunk_ids[1][1]], (1, 1));
        assert_eq!(data.hunk_anchor(1, 1), Some((Side::New, 11)));
        assert_eq!(data.change_runs(1), vec![(Side::Old, 2, 2), (Side::New, 2, 2), (Side::New, 11, 11)]);
        assert_ne!(data.fingerprint, ReviewData::parse(&PATCH.replace("deux", "zwei")).fingerprint);
    }

    #[test]
    fn a_file_is_reviewed_when_all_its_hunks_are() {
        let data = ReviewData::parse(PATCH);
        let mut reviewed = HashSet::new();
        reviewed.insert(data.hunk_ids[1][0]);
        assert!(!data.file_reviewed(1, &reviewed));
        reviewed.extend(data.file_marks(1));
        assert!(data.file_reviewed(1, &reviewed));
        // Reviewing a changed version of the hunk does not carry over.
        let changed = ReviewData::parse(&PATCH.replace("+ten and a half", "+ten and three quarters"));
        assert!(!changed.file_reviewed(1, &reviewed));
        assert!(changed.hunk_ids[1][0] == data.hunk_ids[1][0], "the untouched hunk keeps its id");
    }

    #[test]
    fn the_tree_merges_single_child_folders() {
        let paths = ["src/a/deep/x.rs", "src/b.rs", "README.md"];
        let items = tree_items(paths.iter().copied().enumerate(), &HashSet::new());
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_ref()).collect();
        assert_eq!(labels, vec!["src", "README.md"]);
        let src = &items[0];
        let inner: Vec<&str> = src.children.iter().map(|i| i.label.as_ref()).collect();
        assert_eq!(inner, vec!["a/deep", "b.rs"]);
        assert_eq!(src.children[0].children[0].id.as_ref(), "f:src/a/deep/x.rs");
        assert!(src.is_expanded());
        let closed: HashSet<SharedString> = [SharedString::from("d:src")].into();
        assert!(!tree_items(paths.iter().copied().enumerate(), &closed)[0].is_expanded());
        assert_eq!(item_path("f:src/b.rs"), Some("src/b.rs"));
        assert_eq!(item_path("d:src"), None);
    }

    #[test]
    fn filters_and_test_detection() {
        assert!(matches_filter("crates/orbit-app/src/Review.rs", "review app"));
        assert!(!matches_filter("crates/orbit-app/src/review.rs", "review core"));
        for p in ["src/foo.test.ts", "pkg/foo_test.go", "tests/git.rs", "a/__tests__/x.js", "src/engine/tests.rs", "test_api.py", "src/FooTest.java"] {
            assert!(is_test_file(p), "{p}");
        }
        for p in ["src/testing.rs", "src/contest.rs", "latest/notes.md", "src/attest.ts"] {
            assert!(!is_test_file(p), "{p}");
        }
    }

    #[test]
    fn comments_become_one_message_with_file_line_and_quote() {
        let data = ReviewData::parse(PATCH);
        let f = &data.files[1];
        let c1 = Comment {
            id: 1,
            path: "src/b.rs".into(),
            start: Some((Side::Old, 2)),
            end: Some((Side::New, 2)),
            excerpt: diff::excerpt(f, (Side::Old, 2), (Side::New, 2)),
            text: "Keep it in English.".into(),
        };
        let c2 = Comment { id: 2, path: "README.md".into(), start: None, end: None, excerpt: String::new(), text: " Explain why. ".into() };
        let one = comments_message(std::slice::from_ref(&c1));
        assert_eq!(
            one,
            "I reviewed your changes and left a comment. Please address it.\n\n`src/b.rs:2` (line 2 (before) to line 2)\n```diff\n-two\n+deux\n```\nKeep it in English.\n"
        );
        let both = comments_message(&[c1, c2]);
        assert!(both.contains("1. `src/b.rs:2`"));
        assert!(both.ends_with("2. `README.md` (whole file)\nExplain why.\n"), "{both}");
    }
}
