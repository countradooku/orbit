//! Unified diff parsing, shared by the engine and the client.
//!
//! One `git diff` for all files is parsed here in a single pass. Each file
//! owns its verbatim section of the patch (`FileDiff::raw`) and every header,
//! hunk and line is a [`Span`] into it, so a 10,000-line diff costs one
//! string copy and one `Vec` per hunk instead of a `String` per line.
//!
//! Handles git extended headers (renames, copies, mode changes, new and
//! deleted files, empty files), binary files (`Binary files … differ` and
//! `GIT binary patch`), `\ No newline at end of file`, CRLF content, C-quoted
//! paths (`"caf\303\251.txt"`) and paths with spaces, plus plain
//! `--- / +++` unified diffs without a git header.
//!
//! [`hunk_patch`], [`hunks_patch`] and [`file_patch`] rebuild minimal patches
//! that `git apply` accepts, for per-hunk and per-file revert. [`word_diff`]
//! computes intra-line change ranges for paired removed/added lines, lazily,
//! one hunk at a time (only for hunks on screen).

use std::ops::Range;

use bitcode::{Decode, Encode};

use crate::model::{ChangeKind, FileChange};

/// A byte range into [`FileDiff::raw`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn new(start: usize, end: usize) -> Self {
        Span { start: start as u32, end: end as u32 }
    }
    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }
    pub fn is_empty(self) -> bool {
        self.end == self.start
    }
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum FileDiffKind {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

impl From<FileDiffKind> for ChangeKind {
    fn from(kind: FileDiffKind) -> Self {
        match kind {
            FileDiffKind::Added | FileDiffKind::Copied => ChangeKind::Added,
            FileDiffKind::Deleted => ChangeKind::Deleted,
            FileDiffKind::Modified => ChangeKind::Modified,
            FileDiffKind::Renamed => ChangeKind::Renamed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`: the previous line has no newline.
    NoNewline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the old file (context and removed lines).
    pub old_no: Option<u32>,
    /// Line number in the new file (context and added lines).
    pub new_no: Option<u32>,
    /// The line's content without its marker and line ending.
    pub text: Span,
    /// The line ended in `\r\n` (shown as a CRLF marker; `text` excludes it).
    pub crlf: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// The section heading after the closing `@@` (often a function name).
    pub header: Span,
    /// The whole hunk, `@@` line included, verbatim.
    pub span: Span,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct FileDiff {
    /// `None` for added files.
    pub old_path: Option<String>,
    /// `None` for deleted files.
    pub new_path: Option<String>,
    pub kind: FileDiffKind,
    pub binary: bool,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    /// Similarity percentage of a rename or copy.
    pub similarity: Option<u8>,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
    /// Header lines, from `diff --git` up to the first hunk.
    pub header: Span,
    /// This file's section of the patch, verbatim. Spans index into it.
    pub raw: String,
}

impl FileDiff {
    /// The path to show: the new path, or the old one for deletions.
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or_default()
    }

    pub fn text(&self, span: Span) -> &str {
        &self.raw[span.range()]
    }

    pub fn line_text(&self, line: &DiffLine) -> &str {
        self.text(line.text)
    }

    pub fn mode_changed(&self) -> bool {
        matches!((self.old_mode, self.new_mode), (Some(a), Some(b)) if a != b)
    }

    pub fn to_change(&self) -> FileChange {
        FileChange {
            path: self.path().to_owned(),
            kind: self.kind.into(),
            additions: self.additions,
            deletions: self.deletions,
        }
    }
}

/// Parses a unified diff (normally `git diff` output) into files. Text that
/// is not part of a file diff (a commit header, a stat block) is skipped.
pub fn parse(text: &str) -> Vec<FileDiff> {
    let mut cur = Cursor { text, pos: 0 };
    let mut files = Vec::new();
    while let Some(line) = cur.peek() {
        if line.text.starts_with("diff --git ") {
            files.push(parse_file(&mut cur, true));
        } else if line.text.starts_with("--- ")
            && cur.line_at(line.next).is_some_and(|l| l.text.starts_with("+++ "))
        {
            files.push(parse_file(&mut cur, false));
        } else {
            cur.pos = line.next;
        }
    }
    files
}

/// [`parse`] for raw process output; invalid UTF-8 is replaced.
pub fn parse_bytes(bytes: &[u8]) -> Vec<FileDiff> {
    parse(&String::from_utf8_lossy(bytes))
}

// ---------------------------------------------------------------------------
// Line cursor

#[derive(Clone, Copy)]
struct Line<'a> {
    /// The line without `\n` (a trailing `\r` is kept).
    text: &'a str,
    start: usize,
    next: usize,
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn line_at(&self, pos: usize) -> Option<Line<'a>> {
        if pos >= self.text.len() {
            return None;
        }
        let rest = &self.text.as_bytes()[pos..];
        let (end, next) = match memchr::memchr(b'\n', rest) {
            Some(i) => (pos + i, pos + i + 1),
            None => (self.text.len(), self.text.len()),
        };
        Some(Line { text: &self.text[pos..end], start: pos, next })
    }

    fn peek(&self) -> Option<Line<'a>> {
        self.line_at(self.pos)
    }
}

fn trim_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

// ---------------------------------------------------------------------------
// File parsing

fn parse_file(cur: &mut Cursor<'_>, git: bool) -> FileDiff {
    let base = cur.pos;
    let mut kind = FileDiffKind::Modified;
    let mut binary = false;
    let (mut old_mode, mut new_mode, mut similarity) = (None, None, None);
    let (mut minus, mut plus): (Option<Option<String>>, Option<Option<String>>) = (None, None);
    let (mut from, mut to): (Option<String>, Option<String>) = (None, None);
    let mut header_paths = None;

    if git {
        let line = cur.peek().expect("caller checked");
        header_paths = git_header_paths(trim_cr(&line.text["diff --git ".len()..]));
        cur.pos = line.next;
    }

    // Extended header lines.
    while let Some(line) = cur.peek() {
        let t = trim_cr(line.text);
        if t.starts_with("@@ ") || (git && t.starts_with("diff --git ")) {
            break;
        }
        if let Some(rest) = t.strip_prefix("--- ") {
            minus = Some(side_path(rest, "a/"));
        } else if let Some(rest) = t.strip_prefix("+++ ") {
            plus = Some(side_path(rest, "b/"));
            cur.pos = line.next;
            break;
        } else if !git {
            break;
        } else if let Some(rest) = t.strip_prefix("old mode ") {
            old_mode = parse_mode(rest);
        } else if let Some(rest) = t.strip_prefix("new mode ") {
            new_mode = parse_mode(rest);
        } else if let Some(rest) = t.strip_prefix("deleted file mode ") {
            kind = FileDiffKind::Deleted;
            old_mode = parse_mode(rest);
        } else if let Some(rest) = t.strip_prefix("new file mode ") {
            kind = FileDiffKind::Added;
            new_mode = parse_mode(rest);
        } else if let Some(rest) = t.strip_prefix("rename from ") {
            kind = FileDiffKind::Renamed;
            from = Some(unquote_token(rest));
        } else if let Some(rest) = t.strip_prefix("rename to ") {
            to = Some(unquote_token(rest));
        } else if let Some(rest) = t.strip_prefix("copy from ") {
            kind = FileDiffKind::Copied;
            from = Some(unquote_token(rest));
        } else if let Some(rest) = t.strip_prefix("copy to ") {
            to = Some(unquote_token(rest));
        } else if let Some(rest) = t.strip_prefix("similarity index ") {
            similarity = rest.trim_end_matches('%').parse().ok();
        } else if t.starts_with("dissimilarity index ") {
        } else if let Some(rest) = t.strip_prefix("index ") {
            // `index <old>..<new>[ <mode>]`: the mode is the unchanged one.
            if let Some((_, mode)) = rest.split_once(' ') {
                let mode = parse_mode(mode);
                old_mode = old_mode.or(mode);
                new_mode = new_mode.or(mode);
            }
        } else if t.starts_with("Binary files ") {
            binary = true;
        } else if t == "GIT binary patch" {
            binary = true;
            cur.pos = line.next;
            // literal/delta blocks run until the next file.
            while let Some(l) = cur.peek() {
                if l.text.starts_with("diff --git ") {
                    break;
                }
                cur.pos = l.next;
            }
            break;
        } else {
            break;
        }
        cur.pos = line.next;
    }
    let header_end = cur.pos;

    // Hunks.
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    while let Some(line) = cur.peek() {
        let Some((old_start, old_len, new_start, new_len, heading)) = parse_hunk_header(line.text) else {
            break;
        };
        let hunk_start = line.start;
        cur.pos = line.next;
        let (mut rem_old, mut rem_new) = (old_len, new_len);
        let (mut old_no, mut new_no) = (old_start, new_start);
        let mut lines = Vec::with_capacity(old_len.max(new_len) as usize + 1);
        while let Some(l) = cur.peek() {
            let first = l.text.as_bytes().first().copied();
            let kind = match first {
                Some(b'\\') => LineKind::NoNewline,
                _ if rem_old == 0 && rem_new == 0 => break,
                Some(b' ') | None | Some(b'\r') if rem_old > 0 && rem_new > 0 => LineKind::Context,
                Some(b'-') if rem_old > 0 => LineKind::Removed,
                Some(b'+') if rem_new > 0 => LineKind::Added,
                _ => break,
            };
            let body_start = if first.is_some_and(|b| b != b'\r') { l.start + 1 } else { l.start };
            let end = l.start + l.text.len();
            let crlf = l.text.ends_with('\r') && kind != LineKind::NoNewline;
            let text_end = if l.text.ends_with('\r') { end - 1 } else { end };
            let (o, n) = match kind {
                LineKind::Context => {
                    rem_old -= 1;
                    rem_new -= 1;
                    old_no += 1;
                    new_no += 1;
                    (Some(old_no - 1), Some(new_no - 1))
                }
                LineKind::Removed => {
                    rem_old -= 1;
                    old_no += 1;
                    deletions += 1;
                    (Some(old_no - 1), None)
                }
                LineKind::Added => {
                    rem_new -= 1;
                    new_no += 1;
                    additions += 1;
                    (None, Some(new_no - 1))
                }
                LineKind::NoNewline => (None, None),
            };
            lines.push(DiffLine {
                kind,
                old_no: o,
                new_no: n,
                text: Span::new(body_start.min(text_end) - base, text_end - base),
                crlf,
            });
            cur.pos = l.next;
        }
        hunks.push(Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            header: Span::new(hunk_start + heading.start - base, hunk_start + heading.end - base),
            span: Span::new(hunk_start - base, cur.pos - base),
            lines,
        });
    }

    let end = cur.pos;
    // Plain unified diffs mark creation and deletion with /dev/null only.
    let (old_null, new_null) = (matches!(minus, Some(None)), matches!(plus, Some(None)));
    let kind = match kind {
        FileDiffKind::Modified if old_null && !new_null => FileDiffKind::Added,
        FileDiffKind::Modified if new_null && !old_null => FileDiffKind::Deleted,
        k => k,
    };
    let (head_old, head_new) = match header_paths {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    let old = from.or(minus.flatten()).or(head_old);
    let new = to.or(plus.flatten()).or(head_new);
    let (old_path, new_path) = match kind {
        FileDiffKind::Added => (None, new.or(old)),
        FileDiffKind::Deleted => (old.or(new), None),
        _ => (old.clone().or(new.clone()), new.or(old)),
    };
    FileDiff {
        old_path,
        new_path,
        kind,
        binary,
        old_mode,
        new_mode,
        similarity,
        additions,
        deletions,
        hunks,
        header: Span::new(0, header_end - base),
        raw: cur.text[base..end].to_owned(),
    }
}

fn parse_mode(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim(), 8).ok()
}

/// `@@ -a[,b] +c[,d] @@[ heading]` → (a, b, c, d, heading range in line).
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32, Range<usize>)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let close = rest.find(" @@")?;
    let new = &rest[..close];
    let range = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, ol) = range(old)?;
    let (ns, nl) = range(new)?;
    let mut h = line.len() - rest.len() + close + 3;
    if line[h..].starts_with(' ') {
        h += 1;
    }
    let mut end = line.len();
    if line.ends_with('\r') {
        end -= 1;
    }
    Some((os, ol, ns, nl, h.min(end)..end))
}

/// A `---`/`+++` path: `None` for `/dev/null`, else unquoted with the side's
/// `a/` or `b/` prefix removed.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let path = if rest.starts_with('"') {
        unquote_token(rest)
    } else {
        // Git appends a tab to names containing spaces; plain diffs put a
        // timestamp after it.
        rest.split('\t').next().unwrap_or(rest).to_owned()
    };
    if path == "/dev/null" {
        return None;
    }
    Some(match path.strip_prefix(prefix) {
        Some(p) => p.to_owned(),
        None => path,
    })
}

/// Paths from `diff --git a/<old> b/<new>`. Unquoted names with spaces are
/// ambiguous unless both are equal, which is the only case this fallback is
/// needed for (renames and copies carry `rename from`/`copy to` lines).
fn git_header_paths(rest: &str) -> Option<(String, String)> {
    if rest.starts_with('"') {
        let (a, tail) = unquote(rest)?;
        let tail = tail.strip_prefix(' ')?;
        let b = if tail.starts_with('"') { unquote(tail)?.0 } else { tail.to_owned() };
        return Some((a.strip_prefix("a/")?.to_owned(), b.strip_prefix("b/")?.to_owned()));
    }
    let len = rest.len();
    if len >= 5 && (len - 5).is_multiple_of(2) {
        let p = (len - 5) / 2;
        if rest.starts_with("a/") && rest.is_char_boundary(2 + p) && rest[2 + p..].starts_with(" b/") {
            let (a, b) = (&rest[2..2 + p], &rest[5 + p..]);
            if a == b {
                return Some((a.to_owned(), b.to_owned()));
            }
        }
    }
    let i = rest.find(" b/")?;
    Some((rest.strip_prefix("a/")?[..i - 2].to_owned(), rest[i + 3..].to_owned()))
}

/// Unquotes a C-style quoted token, or returns the text as is.
fn unquote_token(s: &str) -> String {
    match unquote(s) {
        Some((v, _)) => v,
        None => s.to_owned(),
    }
}

/// Parses a git C-quoted string at the start of `s`; returns it and the rest.
fn unquote(s: &str) -> Option<(String, &str)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &s[i + 1..])),
            b'\\' => {
                i += 1;
                let c = *b.get(i)?;
                match c {
                    b'0'..=b'7' => {
                        let mut v = 0u32;
                        let mut n = 0;
                        while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                            v = v * 8 + (b[i] - b'0') as u32;
                            i += 1;
                            n += 1;
                        }
                        out.push(v as u8);
                        continue;
                    }
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'v' => out.push(11),
                    b'f' => out.push(12),
                    b'r' => out.push(b'\r'),
                    other => out.push(other),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Patch building

/// The file's patch, for per-file revert (`git apply -R`). Binary files only
/// apply if the diff was made with `--binary`.
pub fn file_patch(file: &FileDiff) -> String {
    let mut out = file.raw.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A minimal patch holding one hunk, for per-hunk revert.
pub fn hunk_patch(file: &FileDiff, hunk_ix: usize) -> String {
    hunks_patch(file, [hunk_ix])
}

/// A minimal patch holding the given hunks of one file (in file order).
///
/// For renames, copies and mode changes the patch touches only the content
/// at the new path, so reverting a hunk never undoes the rename. Creations
/// and deletions keep their header, so reverting the only hunk of a new
/// file deletes it and reverting a deleted file's hunk recreates it.
pub fn hunks_patch(file: &FileDiff, hunk_ixs: impl IntoIterator<Item = usize>) -> String {
    let mut ixs: Vec<usize> = hunk_ixs.into_iter().filter(|&i| i < file.hunks.len()).collect();
    ixs.sort_unstable();
    ixs.dedup();
    let body: usize = ixs.iter().map(|&i| file.hunks[i].span.len()).sum();
    let mut out = String::with_capacity(body + 128 + 4 * file.path().len());
    let mode = |m: Option<u32>| format!("{:06o}", m.unwrap_or(0o100644));
    match file.kind {
        FileDiffKind::Added => {
            let p = file.path();
            push_git_line(&mut out, p, p);
            out.push_str("new file mode ");
            out.push_str(&mode(file.new_mode));
            out.push_str("\n--- /dev/null\n");
            push_side(&mut out, "+++ ", "b/", p);
        }
        FileDiffKind::Deleted => {
            let p = file.path();
            push_git_line(&mut out, p, p);
            out.push_str("deleted file mode ");
            out.push_str(&mode(file.old_mode));
            out.push('\n');
            push_side(&mut out, "--- ", "a/", p);
            out.push_str("+++ /dev/null\n");
        }
        _ => {
            let p = file.path();
            push_git_line(&mut out, p, p);
            push_side(&mut out, "--- ", "a/", p);
            push_side(&mut out, "+++ ", "b/", p);
        }
    }
    for i in ixs {
        out.push_str(file.text(file.hunks[i].span));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn push_git_line(out: &mut String, a: &str, b: &str) {
    out.push_str("diff --git ");
    push_path(out, "a/", a);
    out.push(' ');
    push_path(out, "b/", b);
    out.push('\n');
}

fn push_side(out: &mut String, marker: &str, prefix: &str, path: &str) {
    out.push_str(marker);
    push_path(out, prefix, path);
    if path.contains(' ') {
        out.push('\t');
    }
    out.push('\n');
}

fn push_path(out: &mut String, prefix: &str, path: &str) {
    let quote = path.bytes().any(|b| b < 0x20 || b == b'"' || b == b'\\' || b == 0x7f);
    if !quote {
        out.push_str(prefix);
        out.push_str(path);
        return;
    }
    out.push('"');
    out.push_str(prefix);
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The patch that undoes reverse-applying `patch`: its reverse application
/// puts back what reverse-applying `patch` took away. Works on patches from
/// [`hunks_patch`] and on text file sections from [`file_patch`] (renames,
/// mode changes, creations and deletions are swapped); binary patches and
/// copies return `None`.
pub fn invert_patch(patch: &str) -> Option<String> {
    let mut out = String::with_capacity(patch.len());
    let mut lines = patch.split_inclusive('\n');
    while let Some(raw) = lines.next() {
        let (line, eol) = split_eol(raw);
        if line == "GIT binary patch" || line.starts_with("Binary files ") || line.starts_with("copy from ") {
            return None;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (a, b) = git_header_paths(trim_cr(rest))?;
            out.push_str("diff --git ");
            push_path(&mut out, "a/", &b);
            out.push(' ');
            push_path(&mut out, "b/", &a);
        } else if let Some(mode) = line.strip_prefix("new file mode ") {
            out.push_str("deleted file mode ");
            out.push_str(mode);
        } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
            out.push_str("new file mode ");
            out.push_str(mode);
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            out.push_str("new mode ");
            out.push_str(mode);
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            out.push_str("old mode ");
            out.push_str(mode);
        } else if let Some(p) = line.strip_prefix("rename from ") {
            out.push_str("rename to ");
            out.push_str(p);
        } else if let Some(p) = line.strip_prefix("rename to ") {
            out.push_str("rename from ");
            out.push_str(p);
        } else if let Some(rest) = line.strip_prefix("index ") {
            let (ids, mode) = match rest.split_once(' ') {
                Some((ids, mode)) => (ids, Some(mode)),
                None => (rest, None),
            };
            let (a, b) = ids.split_once("..")?;
            out.push_str("index ");
            out.push_str(b);
            out.push_str("..");
            out.push_str(a);
            if let Some(mode) = mode {
                out.push(' ');
                out.push_str(mode);
            }
        } else if let Some(minus) = line.strip_prefix("--- ") {
            // `---` and `+++` come in pairs: emit them swapped.
            let (plus, plus_eol) = split_eol(lines.next()?);
            let plus = plus.strip_prefix("+++ ")?;
            out.push_str("--- ");
            out.push_str(&swap_side(plus, "b/", "a/"));
            out.push_str(plus_eol);
            out.push_str("+++ ");
            out.push_str(&swap_side(minus, "a/", "b/"));
        } else if let Some((os, ol, ns, nl, heading)) = parse_hunk_header(line) {
            out.push_str(&format!("@@ -{ns},{nl} +{os},{ol} @@"));
            if !heading.is_empty() {
                out.push(' ');
                out.push_str(&line[heading]);
            }
        } else if let Some(rest) = line.strip_prefix('+') {
            out.push('-');
            out.push_str(rest);
        } else if let Some(rest) = line.strip_prefix('-') {
            out.push('+');
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push_str(eol);
    }
    Some(out)
}

/// A line and its ending (`\n`, or `\n` after a kept `\r`, or nothing).
fn split_eol(raw: &str) -> (&str, &str) {
    match raw.strip_suffix('\n') {
        Some(line) => (line, "\n"),
        None => (raw, ""),
    }
}

/// Re-prefixes a `---`/`+++` path for the other side; `/dev/null` stays.
fn swap_side(path: &str, from: &str, to: &str) -> String {
    if path.starts_with("/dev/null") {
        return path.to_owned();
    }
    if let Some(rest) = path.strip_prefix('"').and_then(|p| p.strip_prefix(from)) {
        return format!("\"{to}{rest}");
    }
    match path.strip_prefix(from) {
        Some(rest) => format!("{to}{rest}"),
        None => path.to_owned(),
    }
}

/// A stable identity for a hunk: its file and its changed lines, not its
/// position, so it survives edits elsewhere in the file and changes when the
/// hunk itself changes. Use it to remember per-hunk review state.
pub fn hunk_id(file: &FileDiff, hunk_ix: usize) -> u64 {
    let mut h = Fnv::new();
    h.write(file.path().as_bytes());
    if let Some(hunk) = file.hunks.get(hunk_ix) {
        for line in &hunk.lines {
            let tag = match line.kind {
                LineKind::Added => b'+',
                LineKind::Removed => b'-',
                LineKind::NoNewline => b'\\',
                LineKind::Context => continue,
            };
            h.write(&[0, tag]);
            h.write(file.line_text(line).as_bytes());
        }
    }
    h.finish()
}

/// A stable identity for a whole file's change (paths, kind and every
/// hunk), for "viewed" marks that reset when the file changes again.
pub fn file_id(file: &FileDiff) -> u64 {
    let mut h = Fnv::new();
    h.write(file.old_path.as_deref().unwrap_or_default().as_bytes());
    h.write(&[0]);
    h.write(file.new_path.as_deref().unwrap_or_default().as_bytes());
    h.write(&[file.kind as u8, file.binary as u8]);
    for ix in 0..file.hunks.len() {
        h.write(&hunk_id(file, ix).to_le_bytes());
    }
    if file.binary {
        h.write(file.text(file.header).as_bytes());
    }
    h.finish()
}

/// FNV-1a: tiny, and stable across runs and platforms.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Which version of a file a line number refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Old,
    New,
}

/// The diff lines from `start` through `end` (1-based line numbers on the
/// given sides), in patch order, each with its `+`, `-` or space marker: the
/// excerpt a review comment quotes. Only lines the patch carries are known.
pub fn excerpt(file: &FileDiff, start: (Side, u32), end: (Side, u32)) -> String {
    let number = |line: &DiffLine, side: Side| match side {
        Side::Old => line.old_no,
        Side::New => line.new_no,
    };
    let mut out = String::new();
    let mut inside = false;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            if !inside {
                if number(line, start.0).is_some_and(|n| n >= start.1) {
                    inside = true;
                } else {
                    continue;
                }
            }
            if number(line, end.0).is_some_and(|n| n > end.1) {
                return out;
            }
            out.push(match line.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                LineKind::Context => ' ',
                LineKind::NoNewline => '\\',
            });
            out.push_str(file.line_text(line));
            out.push('\n');
            if number(line, end.0) == Some(end.1) {
                return out;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Word-level highlights

/// Intra-line change ranges for one hunk: one entry per line of the hunk
/// (empty for lines without emphasis). Ranges are byte offsets into the
/// line's text. Removed lines are paired in order with the added lines that
/// directly follow them; pairs that share too little are left unemphasized,
/// since highlighting a whole rewritten line only adds noise.
pub fn word_diff(file: &FileDiff, hunk_ix: usize) -> Vec<LineRanges> {
    let Some(hunk) = file.hunks.get(hunk_ix) else { return Vec::new() };
    let lines = &hunk.lines;
    let mut out = vec![Vec::new(); lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let mut removed = Vec::new();
        while i < lines.len() && matches!(lines[i].kind, LineKind::Removed | LineKind::NoNewline) {
            if lines[i].kind == LineKind::Removed {
                removed.push(i);
            }
            i += 1;
        }
        let mut added = Vec::new();
        while i < lines.len() && matches!(lines[i].kind, LineKind::Added | LineKind::NoNewline) {
            if lines[i].kind == LineKind::Added {
                added.push(i);
            }
            i += 1;
        }
        for (&r, &a) in removed.iter().zip(&added) {
            if let Some((old, new)) = inline_ranges(file.line_text(&lines[r]), file.line_text(&lines[a])) {
                out[r] = old;
                out[a] = new;
            }
        }
    }
    out
}

/// Byte ranges into one line's text.
pub type LineRanges = Vec<Range<u32>>;

/// Changed byte ranges between two versions of a line, by word, or `None`
/// when the lines are identical or too different to be worth emphasizing.
pub fn inline_ranges(old: &str, new: &str) -> Option<(LineRanges, LineRanges)> {
    if old == new {
        return None;
    }
    let a = tokenize(old);
    let b = tokenize(new);
    fn tok<'s>(s: &'s str, r: &Range<u32>) -> &'s str {
        &s[r.start as usize..r.end as usize]
    }
    let eq = |i: usize, j: usize| tok(old, &a[i]) == tok(new, &b[j]);

    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && eq(prefix, prefix) {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix && suffix < b.len() - prefix && eq(a.len() - 1 - suffix, b.len() - 1 - suffix) {
        suffix += 1;
    }
    let (na, nb) = (a.len() - prefix - suffix, b.len() - prefix - suffix);
    let mut changed_a = vec![false; a.len()];
    let mut changed_b = vec![false; b.len()];
    if na == 0 || nb == 0 || na * nb > 16_384 {
        changed_a[prefix..prefix + na].fill(true);
        changed_b[prefix..prefix + nb].fill(true);
    } else {
        // LCS over the middle tokens.
        let w = nb + 1;
        let mut dp = vec![0u16; (na + 1) * w];
        for i in (0..na).rev() {
            for j in (0..nb).rev() {
                dp[i * w + j] = if eq(prefix + i, prefix + j) {
                    dp[(i + 1) * w + j + 1] + 1
                } else {
                    dp[(i + 1) * w + j].max(dp[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < na && j < nb {
            if eq(prefix + i, prefix + j) {
                i += 1;
                j += 1;
            } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
                changed_a[prefix + i] = true;
                i += 1;
            } else {
                changed_b[prefix + j] = true;
                j += 1;
            }
        }
        changed_a[prefix + i..prefix + na].fill(true);
        changed_b[prefix + j..prefix + nb].fill(true);
    }

    let ranges = |toks: &[Range<u32>], changed: &[bool]| -> (Vec<Range<u32>>, usize) {
        let mut out: Vec<Range<u32>> = Vec::new();
        let mut bytes = 0;
        for (t, &c) in toks.iter().zip(changed) {
            if !c {
                continue;
            }
            bytes += (t.end - t.start) as usize;
            match out.last_mut() {
                Some(last) if last.end == t.start => last.end = t.end,
                _ => out.push(t.clone()),
            }
        }
        (out, bytes)
    };
    let (ra, ca) = ranges(&a, &changed_a);
    let (rb, cb) = ranges(&b, &changed_b);
    let common = (old.len() - ca) + (new.len() - cb);
    if common * 10 < (old.len() + new.len()) * 4 {
        return None;
    }
    Some((ra, rb))
}

/// Words (alphanumeric runs), whitespace runs, and single other characters.
fn tokenize(s: &str) -> Vec<Range<u32>> {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut out: Vec<Range<u32>> = Vec::with_capacity(s.len() / 3 + 1);
    let mut prev = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        let end = (i + c.len_utf8()) as u32;
        match out.last_mut() {
            Some(last) if prev == Some(k) && k != Class::Other => last.end = end,
            _ => out.push(i as u32..end),
        }
        prev = Some(k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(f: &FileDiff, h: usize) -> Vec<(LineKind, Option<u32>, Option<u32>, &str)> {
        f.hunks[h].lines.iter().map(|l| (l.kind, l.old_no, l.new_no, f.line_text(l))).collect()
    }

    const MODIFY: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@ fn main() {
 one
-two
+deux
+zwei
 three
 four
@@ -10,2 +11,2 @@
 ten
-eleven
+onze
";

    #[test]
    fn parses_a_modified_file_with_line_numbers() {
        let files = parse(MODIFY);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.kind, FileDiffKind::Modified);
        assert_eq!(f.old_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(f.new_path.as_deref(), Some("src/lib.rs"));
        assert_eq!((f.additions, f.deletions), (3, 2));
        assert_eq!(f.old_mode, Some(0o100644));
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 4, 1, 5));
        assert_eq!(f.text(h.header), "fn main() {");
        assert_eq!(
            texts(f, 0),
            vec![
                (LineKind::Context, Some(1), Some(1), "one"),
                (LineKind::Removed, Some(2), None, "two"),
                (LineKind::Added, None, Some(2), "deux"),
                (LineKind::Added, None, Some(3), "zwei"),
                (LineKind::Context, Some(3), Some(4), "three"),
                (LineKind::Context, Some(4), Some(5), "four"),
            ]
        );
        assert_eq!(f.text(f.hunks[1].header), "");
        assert_eq!(texts(f, 1)[2], (LineKind::Added, None, Some(12), "onze"));
        assert_eq!(f.raw, MODIFY, "a single file's raw section is the whole input");
        assert!(f.text(f.header).ends_with("+++ b/src/lib.rs\n"));
    }

    #[test]
    fn header_lookalikes_inside_hunks_are_content() {
        let text = "diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,3 +1,3 @@
--- not a header
-diff --git a/y b/y
+++++ still content
+@@ -1 +1 @@
 diff --git a/z b/z
diff --git a/next b/next
--- a/next
+++ b/next
@@ -1 +1 @@
-a
+b
";
        let files = parse(text);
        assert_eq!(files.len(), 2);
        let k: Vec<_> = texts(&files[0], 0).iter().map(|t| (t.0, t.3)).collect();
        assert_eq!(
            k,
            vec![
                (LineKind::Removed, "-- not a header"),
                (LineKind::Removed, "diff --git a/y b/y"),
                (LineKind::Added, "++++ still content"),
                (LineKind::Added, "@@ -1 +1 @@"),
                (LineKind::Context, "diff --git a/z b/z"),
            ]
        );
        assert_eq!(files[1].path(), "next");
        assert_eq!(files[0].raw.len() + files[1].raw.len(), text.len());
    }

    #[test]
    fn new_deleted_and_empty_files() {
        let text = "diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3b18e51
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100755
index 3b18e51..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/empty file.txt b/empty file.txt
new file mode 100644
index 0000000..e69de29
diff --git a/was empty b/was empty
deleted file mode 100644
index e69de29..0000000
";
        let f = parse(text);
        assert_eq!(f.len(), 4);
        assert_eq!((f[0].kind, f[0].old_path.as_deref(), f[0].path()), (FileDiffKind::Added, None, "new.txt"));
        assert_eq!((f[0].additions, f[0].new_mode), (2, Some(0o100644)));
        assert_eq!(texts(&f[0], 0)[1], (LineKind::Added, None, Some(2), "world"));
        assert_eq!((f[1].kind, f[1].new_path.as_deref(), f[1].path()), (FileDiffKind::Deleted, None, "gone.txt"));
        assert_eq!((f[1].deletions, f[1].old_mode), (1, Some(0o100755)));
        assert_eq!((f[2].kind, f[2].path(), f[2].hunks.len()), (FileDiffKind::Added, "empty file.txt", 0));
        assert_eq!((f[3].kind, f[3].path()), (FileDiffKind::Deleted, "was empty"));
    }

    #[test]
    fn renames_copies_and_mode_changes() {
        let text = "diff --git a/old name.rs b/new name.rs
similarity index 90%
rename from old name.rs
rename to new name.rs
index 1111111..2222222 100644
--- a/old name.rs\t
+++ b/new name.rs\t
@@ -1 +1 @@
-a
+b
diff --git a/pure.rs b/moved/pure.rs
similarity index 100%
rename from pure.rs
rename to moved/pure.rs
diff --git a/src.c b/copy.c
similarity index 100%
copy from src.c
copy to copy.c
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
";
        let f = parse(text);
        assert_eq!(f.len(), 4);
        assert_eq!(f[0].kind, FileDiffKind::Renamed);
        assert_eq!((f[0].old_path.as_deref(), f[0].new_path.as_deref()), (Some("old name.rs"), Some("new name.rs")));
        assert_eq!(f[0].similarity, Some(90));
        assert_eq!((f[0].additions, f[0].deletions), (1, 1));
        assert_eq!(f[1].kind, FileDiffKind::Renamed);
        assert_eq!((f[1].old_path.as_deref(), f[1].path(), f[1].hunks.len()), (Some("pure.rs"), "moved/pure.rs", 0));
        assert_eq!((f[2].kind, f[2].old_path.as_deref(), f[2].path()), (FileDiffKind::Copied, Some("src.c"), "copy.c"));
        assert_eq!((f[3].kind, f[3].path()), (FileDiffKind::Modified, "run.sh"));
        assert!(f[3].mode_changed());
        assert_eq!((f[3].old_mode, f[3].new_mode), (Some(0o100644), Some(0o100755)));
        assert_eq!(f[1].to_change().kind, ChangeKind::Renamed);
    }

    #[test]
    fn binary_files() {
        let text = "diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
diff --git a/blob.bin b/blob.bin
new file mode 100644
index 0000000000000000000000000000000000000000..9d4ad6e2e8f2d2c79f0a4d8b6f6c0c0d0e0f0a0b
GIT binary patch
literal 12
TcmZQzWMXDvWn<^!;^G1T0%HN!

literal 0
HcmV?d00001

diff --git a/t.txt b/t.txt
--- a/t.txt
+++ b/t.txt
@@ -1 +1 @@
-x
+y
";
        let f = parse(text);
        assert_eq!(f.len(), 3);
        assert!(f[0].binary && f[0].hunks.is_empty());
        assert_eq!((f[0].kind, f[0].path(), f[0].additions), (FileDiffKind::Modified, "img.png", 0));
        assert!(f[1].binary);
        assert_eq!((f[1].kind, f[1].path()), (FileDiffKind::Added, "blob.bin"));
        assert!(f[1].raw.contains("literal 12"));
        assert!(!f[2].binary);
        assert_eq!(f[2].hunks.len(), 1);
    }

    #[test]
    fn no_newline_markers() {
        let text = "diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,2 +1,2 @@
 a
-b
\\ No newline at end of file
+c
\\ No newline at end of file
";
        let f = &parse(text)[0];
        let k: Vec<_> = texts(f, 0).iter().map(|t| (t.0, t.3)).collect();
        assert_eq!(
            k,
            vec![
                (LineKind::Context, "a"),
                (LineKind::Removed, "b"),
                (LineKind::NoNewline, " No newline at end of file"),
                (LineKind::Added, "c"),
                (LineKind::NoNewline, " No newline at end of file"),
            ]
        );
        assert_eq!((f.additions, f.deletions), (1, 1));
        let p = hunk_patch(f, 0);
        assert!(p.ends_with("+c\n\\ No newline at end of file\n"));
    }

    #[test]
    fn crlf_content_and_crlf_patches() {
        let text = "diff --git a/w.txt b/w.txt\n--- a/w.txt\n+++ b/w.txt\n@@ -1,2 +1,2 @@\n one\r\n-two\r\n+2\r\n";
        let f = &parse(text)[0];
        let l = &f.hunks[0].lines;
        assert_eq!(l.len(), 3);
        assert!(l.iter().all(|l| l.crlf));
        assert_eq!(f.line_text(&l[1]), "two");
        // A patch that is itself CRLF-terminated.
        let crlf = MODIFY.replace('\n', "\r\n");
        let g = &parse(&crlf)[0];
        assert_eq!(g.path(), "src/lib.rs");
        assert_eq!(g.hunks.len(), 2);
        assert_eq!(g.text(g.hunks[0].header), "fn main() {");
        assert_eq!(g.line_text(&g.hunks[0].lines[2]), "deux");
    }

    #[test]
    fn quoted_unicode_and_escaped_paths() {
        let text = "diff --git \"a/caf\\303\\251 \\\"q\\\".txt\" \"b/caf\\303\\251 \\\"q\\\".txt\"
new file mode 100644
index 0000000..e69de29
diff --git a/plain.txt \"b/na\\303\\257ve\\tx.txt\"
similarity index 100%
rename from plain.txt
rename to \"na\\303\\257ve\\tx.txt\"
diff --git \"a/\\346\\227\\245\\346\\234\\254.md\" \"b/\\346\\227\\245\\346\\234\\254.md\"
--- \"a/\\346\\227\\245\\346\\234\\254.md\"
+++ \"b/\\346\\227\\245\\346\\234\\254.md\"
@@ -1 +1 @@
-a
+b
";
        let f = parse(text);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].path(), "café \"q\".txt");
        assert_eq!(f[1].old_path.as_deref(), Some("plain.txt"));
        assert_eq!(f[1].path(), "naïve\tx.txt");
        assert_eq!(f[2].path(), "日本.md");
        assert_eq!(f[2].old_path.as_deref(), Some("日本.md"));
        // Rebuilt headers quote what must be quoted.
        let p = hunk_patch(&f[2], 0);
        assert!(p.starts_with("diff --git a/日本.md b/日本.md\n--- a/日本.md\n+++ b/日本.md\n@@"));
        let mut s = String::new();
        push_path(&mut s, "b/", "naïve\tx\"\\.txt");
        assert_eq!(s, "\"b/naïve\\tx\\\"\\\\.txt\"");
        assert_eq!(unquote(&s).unwrap().0, "b/naïve\tx\"\\.txt");
    }

    #[test]
    fn plain_unified_diffs_and_preamble() {
        let text = "commit abc\nAuthor: someone\n\n 1 file changed\n--- a/old.txt\t2024-01-01 10:00\n+++ b/old.txt\t2024-01-02 10:00\n@@ -1 +1,2 @@\n x\n+y\n--- /dev/null\n+++ b/created\n@@ -0,0 +1 @@\n+z\n";
        let f = parse(text);
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].kind, f[0].path(), f[0].additions), (FileDiffKind::Modified, "old.txt", 1));
        assert_eq!((f[1].kind, f[1].path()), (FileDiffKind::Added, "created"));
    }

    #[test]
    fn omitted_hunk_lengths_default_to_one() {
        let text = "--- a/f\n+++ b/f\n@@ -3 +3 @@\n-a\n+b\n";
        let h = &parse(text)[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (3, 1, 3, 1));
        assert_eq!(h.lines[1].new_no, Some(3));
    }

    #[test]
    fn truncated_input_does_not_panic() {
        for cut in 0..MODIFY.len() {
            if MODIFY.is_char_boundary(cut) {
                let _ = parse(&MODIFY[..cut]);
            }
        }
        let _ = parse("diff --git \n@@ -x +y @@\n");
        let _ = parse("diff --git a/\"unterminated b/x\n");
        let _ = parse("@@ -1,2 +1,2 @@\n-");
    }

    #[test]
    fn hunk_patches_keep_only_selected_hunks() {
        let f = &parse(MODIFY)[0];
        let p = hunk_patch(f, 1);
        assert_eq!(p, "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,2 +11,2 @@\n ten\n-eleven\n+onze\n");
        let both = hunks_patch(f, [1, 0, 1]);
        assert!(both.find("@@ -1,4").unwrap() < both.find("@@ -10,2").unwrap());
        assert_eq!(file_patch(f), MODIFY);

        let renamed = &parse(
            "diff --git a/a b/b\nsimilarity index 80%\nrename from a\nrename to b\nindex 1..2 100755\n--- a/a\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n",
        )[0];
        assert_eq!(hunk_patch(renamed, 0), "diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n");
        let spaced = &parse("--- a/my file\t\n+++ b/my file\t\n@@ -1 +1 @@\n-x\n+y\n")[0];
        assert!(hunk_patch(spaced, 0).contains("--- a/my file\t\n+++ b/my file\t\n"));
        let added = &parse("diff --git a/n b/n\nnew file mode 100755\n--- /dev/null\n+++ b/n\n@@ -0,0 +1 @@\n+x\n")[0];
        assert_eq!(hunk_patch(added, 0), "diff --git a/n b/n\nnew file mode 100755\n--- /dev/null\n+++ b/n\n@@ -0,0 +1 @@\n+x\n");
    }

    #[test]
    fn word_diff_pairs_removed_and_added_lines() {
        let text = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n-let total = price * count;\n-completely different line here\n+let total = price * amount;\n+zzz\n same\n";
        let f = &parse(text)[0];
        let w = word_diff(f, 0);
        assert_eq!(w.len(), 5);
        let old = f.line_text(&f.hunks[0].lines[0]);
        let new = f.line_text(&f.hunks[0].lines[2]);
        let r = &w[0];
        assert_eq!(r.len(), 1);
        assert_eq!(&old[r[0].start as usize..r[0].end as usize], "count");
        assert_eq!(&new[w[2][0].start as usize..w[2][0].end as usize], "amount");
        assert!(w[1].is_empty() && w[3].is_empty() && w[4].is_empty(), "dissimilar pair is not emphasized");
    }

    #[test]
    fn inline_ranges_handle_insertions_unicode_and_identity() {
        assert_eq!(inline_ranges("same", "same"), None);
        let (a, b) = inline_ranges("foo(a, b)", "foo(a, x, b)").unwrap();
        assert!(a.is_empty());
        assert_eq!(&"foo(a, x, b)"[b[0].start as usize..b[0].end as usize], "x, ");
        let (a, b) = inline_ranges("naïve café = 1", "naïve café = 2").unwrap();
        assert_eq!(&"naïve café = 1"[a[0].start as usize..a[0].end as usize], "1");
        assert_eq!(b.len(), 1);
        // Long lines fall back to one middle range without blowing up.
        let long_a: String = (0..400).map(|i| format!("w{i} ")).collect();
        let long_b: String = (0..400).map(|i| format!("w{} ", i * 7)).collect();
        let _ = inline_ranges(&long_a, &long_b);
    }

    #[test]
    fn inverted_patches_swap_sides_and_invert_twice_to_the_original() {
        let f = &parse(MODIFY)[0];
        let p = hunk_patch(f, 0);
        let inv = invert_patch(&p).unwrap();
        assert_eq!(
            inv,
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,5 +1,4 @@ fn main() {\n one\n+two\n-deux\n-zwei\n three\n four\n"
        );
        let back = invert_patch(&inv).unwrap();
        assert_eq!(parse(&back)[0].hunks[0].lines, parse(&p)[0].hunks[0].lines);

        let added = &parse("diff --git a/n b/n\nnew file mode 100755\n--- /dev/null\n+++ b/n\n@@ -0,0 +1 @@\n+x\n")[0];
        let inv = invert_patch(&hunk_patch(added, 0)).unwrap();
        assert_eq!(inv, "diff --git a/n b/n\ndeleted file mode 100755\n--- a/n\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n");
        let renamed = "diff --git a/old b/new\nsimilarity index 90%\nrename from old\nrename to new\nindex 1111111..2222222 100644\n--- a/old\n+++ b/new\n@@ -1 +1 @@\n-a\n+b\n";
        let inv = invert_patch(renamed).unwrap();
        assert!(inv.starts_with("diff --git a/new b/old\nsimilarity index 90%\nrename to old\nrename from new\nindex 2222222..1111111 100644\n--- a/new\n+++ b/old\n"), "{inv}");
        assert!(invert_patch("diff --git a/i b/i\nindex 1..2 100644\nBinary files a/i and b/i differ\n").is_none());
    }

    #[test]
    fn hunk_ids_follow_content_not_position() {
        let a = &parse(MODIFY)[0];
        let shifted = MODIFY.replace("@@ -10,2 +11,2 @@", "@@ -40,2 +41,2 @@").replace(" ten\n", " TEN\n");
        let b = &parse(&shifted)[0];
        assert_eq!(hunk_id(a, 1), hunk_id(b, 1), "moved and with new context: same change");
        assert_ne!(hunk_id(a, 0), hunk_id(a, 1));
        let edited = MODIFY.replace("+onze", "+eleven!");
        let c = &parse(&edited)[0];
        assert_ne!(hunk_id(a, 1), hunk_id(c, 1), "a changed hunk is a new hunk");
        assert_eq!(file_id(a), file_id(b));
        assert_ne!(file_id(a), file_id(c));
    }

    #[test]
    fn excerpts_quote_the_selected_lines_with_markers() {
        let f = &parse(MODIFY)[0];
        assert_eq!(excerpt(f, (Side::New, 2), (Side::New, 3)), "+deux\n+zwei\n");
        assert_eq!(excerpt(f, (Side::Old, 2), (Side::New, 3)), "-two\n+deux\n+zwei\n");
        assert_eq!(excerpt(f, (Side::New, 4), (Side::New, 4)), " three\n");
        assert_eq!(excerpt(f, (Side::New, 12), (Side::New, 12)), "+onze\n");
        assert_eq!(excerpt(f, (Side::New, 99), (Side::New, 120)), "");
    }

    #[test]
    fn diffs_round_trip_through_bitcode() {
        let files = parse(MODIFY);
        let bytes = bitcode::encode(&files);
        let back: Vec<FileDiff> = bitcode::decode(&bytes).unwrap();
        assert_eq!(back, files);
    }
}
