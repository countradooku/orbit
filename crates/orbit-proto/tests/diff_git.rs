//! Round-trips real `git diff` output through the parser: the parse is
//! lossless, its counts match `--numstat`, and the rebuilt per-file and
//! per-hunk patches are accepted by `git apply -R` and restore the original.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use orbit_proto::diff::{self, FileDiffKind};

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    git_in(dir, args, None).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

fn git_in(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .args(["-c", "core.quotepath=true", "-c", "core.safecrlf=false"])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut input = child.stdin.take().unwrap();
    if let Some(bytes) = stdin {
        input.write_all(bytes).unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn numbered(n: usize, f: impl Fn(usize) -> String) -> String {
    (1..=n).map(|i| f(i) + "\n").collect()
}

/// A repo whose index holds a rich change set against HEAD; the working
/// tree equals the index.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["config", "user.name", "t"]);
    git(d, &["config", "user.email", "t@t"]);
    git(d, &["config", "core.autocrlf", "false"]);

    write(d, "modify.txt", numbered(60, |i| format!("line {i}")).as_bytes());
    write(d, "delete.txt", b"to be deleted\nsecond\n");
    write(d, "rename_src.txt", numbered(20, |i| format!("stable content {i}")).as_bytes());
    write(d, "space name.txt", b"alpha\nbeta\n");
    write(d, "caf\u{e9}.txt", b"un\ndeux\n");
    write(d, "\u{65e5}\u{672c}/\u{30d5}\u{30a1}\u{30a4}\u{30eb}.md", b"# title\nbody\n");
    write(d, "no_newline.txt", b"first\nlast without newline");
    write(d, "crlf.txt", b"one\r\ntwo\r\nthree\r\n");
    write(d, "bin.dat", &[0, 1, 2, 3, 255, 0, 10, 13]);
    write(d, "empty_then_full.txt", b"");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);

    let modified = numbered(60, |i| match i {
        2 => "line 2 changed".into(),
        20 => "line twenty".into(),
        21 => "inserted after twenty\nline 21".into(),
        45 => "line 45 -- tricky".into(),
        59 => "--- line 59 looks like a header".into(),
        _ => format!("line {i}"),
    });
    write(d, "modify.txt", modified.as_bytes());
    std::fs::remove_file(d.join("delete.txt")).unwrap();
    std::fs::remove_file(d.join("rename_src.txt")).unwrap();
    write(
        d,
        "moved/rename dst.txt",
        numbered(20, |i| if i == 10 { "edited in rename".into() } else { format!("stable content {i}") }).as_bytes(),
    );
    write(d, "space name.txt", b"alpha\nbeta gamma\n");
    write(d, "caf\u{e9}.txt", b"un\ntrois\n");
    write(d, "\u{65e5}\u{672c}/\u{30d5}\u{30a1}\u{30a4}\u{30eb}.md", b"# title\nbody\nmore\n");
    write(d, "no_newline.txt", b"first\nlast changed, still without newline");
    write(d, "crlf.txt", b"one\r\n2\r\nthree\r\n");
    write(d, "bin.dat", &[0, 1, 2, 4, 255, 0, 10, 13, 9]);
    write(d, "empty_then_full.txt", b"now has content\n");
    write(d, "added.txt", b"brand new\nfile\n");
    write(d, "added empty.txt", b"");
    write(d, "#hash [x] (1).txt", b"odd characters in name\n");
    git(d, &["add", "-A"]);
    tmp
}

const DIFF: &[&str] = &[
    "diff",
    "--cached",
    "-M",
    "--no-color",
    "--no-ext-diff",
    "--full-index",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

fn numstat(d: &Path) -> HashMap<String, (Option<u32>, Option<u32>)> {
    let mut args = DIFF.to_vec();
    args.extend(["--numstat", "-z"]);
    let out = git(d, &args);
    let mut fields = out.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut map = HashMap::new();
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        let mut parts = rec.splitn(3, '\t');
        let add = parts.next().unwrap().parse().ok();
        let del = parts.next().unwrap().parse().ok();
        let mut path = parts.next().unwrap().to_owned();
        if path.is_empty() {
            // Rename: old and new paths follow as separate fields.
            let _old = fields.next().unwrap();
            path = fields.next().unwrap();
        }
        map.insert(path, (add, del));
    }
    map
}

#[test]
fn real_git_diff_parses_losslessly_and_matches_numstat() {
    let repo = fixture();
    let d = repo.path();
    let text = String::from_utf8(git(d, DIFF)).unwrap();
    let files = diff::parse(&text);

    let joined: String = files.iter().map(|f| f.raw.as_str()).collect();
    assert_eq!(joined, text, "file sections reassemble the patch exactly");

    let stats = numstat(d);
    assert_eq!(files.len(), stats.len(), "one FileDiff per numstat entry: {:?}", files.iter().map(|f| f.path()).collect::<Vec<_>>());
    for f in &files {
        let (add, del) = stats.get(f.path()).unwrap_or_else(|| panic!("{} missing from numstat", f.path()));
        if f.binary {
            assert_eq!((*add, *del), (None, None), "{}", f.path());
        } else {
            assert_eq!((Some(f.additions), Some(f.deletions)), (*add, *del), "{}", f.path());
        }
    }

    let by_path: HashMap<&str, &diff::FileDiff> = files.iter().map(|f| (f.path(), f)).collect();
    assert_eq!(by_path["delete.txt"].kind, FileDiffKind::Deleted);
    assert_eq!(by_path["added.txt"].kind, FileDiffKind::Added);
    assert_eq!(by_path["added empty.txt"].kind, FileDiffKind::Added);
    assert!(by_path["added empty.txt"].hunks.is_empty());
    assert!(by_path["bin.dat"].binary);
    assert_eq!(by_path["#hash [x] (1).txt"].kind, FileDiffKind::Added);
    let renamed = by_path["moved/rename dst.txt"];
    assert_eq!(renamed.kind, FileDiffKind::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("rename_src.txt"));
    assert!(by_path.contains_key("caf\u{e9}.txt"));
    assert!(by_path.contains_key("\u{65e5}\u{672c}/\u{30d5}\u{30a1}\u{30a4}\u{30eb}.md"));
    assert!(by_path["crlf.txt"].hunks[0].lines.iter().all(|l| l.crlf));
    assert!(by_path["modify.txt"].hunks.len() >= 3);
    let nn = by_path["no_newline.txt"];
    assert_eq!(nn.hunks[0].lines.iter().filter(|l| l.kind == diff::LineKind::NoNewline).count(), 2);
}

#[test]
fn every_rebuilt_patch_reverts_cleanly() {
    let repo = fixture();
    let d = repo.path();
    let mut args = DIFF.to_vec();
    args.push("--binary");
    let text = String::from_utf8(git(d, &args)).unwrap();
    let files = diff::parse(&text);
    assert!(files.len() >= 12);

    // Each file patch passes git's own precheck.
    for f in &files {
        let patch = diff::file_patch(f);
        git_in(d, &["apply", "-R", "--check"], Some(patch.as_bytes()))
            .unwrap_or_else(|e| panic!("file patch for {} rejected: {e}\n{patch}", f.path()));
    }

    // Revert modify.txt hunk by hunk, in a scrambled order: every hunk applies
    // against a tree where some of the others are already reverted.
    let m = files.iter().find(|f| f.path() == "modify.txt").unwrap();
    let mut order: Vec<usize> = (0..m.hunks.len()).collect();
    order.rotate_left(1);
    order.swap(0, m.hunks.len() - 1);
    for ix in order {
        let patch = diff::hunk_patch(m, ix);
        git_in(d, &["apply", "-R"], Some(patch.as_bytes())).unwrap_or_else(|e| panic!("hunk {ix}: {e}\n{patch}"));
    }
    let head = git(d, &["show", "HEAD:modify.txt"]);
    assert_eq!(std::fs::read(d.join("modify.txt")).unwrap(), head);

    // Single-hunk reverts of the other text files, then whole-file reverts of
    // the rest; the working tree must end up equal to HEAD.
    for f in files.iter().filter(|f| f.path() != "modify.txt") {
        let patch = if f.hunks.len() == 1 && f.kind == FileDiffKind::Modified {
            diff::hunk_patch(f, 0)
        } else {
            diff::file_patch(f)
        };
        git_in(d, &["apply", "-R"], Some(patch.as_bytes())).unwrap_or_else(|e| panic!("{}: {e}\n{patch}", f.path()));
    }
    git(d, &["add", "-A"]);
    let left = git(d, &["diff", "--cached", "--name-only", "HEAD"]);
    assert!(left.is_empty(), "not reverted: {}", String::from_utf8_lossy(&left));
}

#[test]
fn hunk_revert_keeps_a_rename() {
    let repo = fixture();
    let d = repo.path();
    let text = String::from_utf8(git(d, DIFF)).unwrap();
    let files = diff::parse(&text);
    let r = files.iter().find(|f| f.kind == FileDiffKind::Renamed).unwrap();
    git_in(d, &["apply", "-R"], Some(diff::hunk_patch(r, 0).as_bytes())).unwrap();
    assert!(d.join("moved/rename dst.txt").exists());
    assert!(!d.join("rename_src.txt").exists());
    let content = std::fs::read_to_string(d.join("moved/rename dst.txt")).unwrap();
    assert!(content.contains("stable content 10") && !content.contains("edited in rename"));
}

#[test]
fn parsing_a_large_diff_is_fast() {
    let mut text = String::new();
    for f in 0..300 {
        text.push_str(&format!("diff --git a/f{f}.rs b/f{f}.rs\nindex 1..2 100644\n--- a/f{f}.rs\n+++ b/f{f}.rs\n"));
        for h in 0..10 {
            text.push_str(&format!("@@ -{0},7 +{0},7 @@ fn x()\n a\n b\n c\n-old {h}\n+new {h}\n d\n e\n f\n", h * 20 + 1));
        }
    }
    let start = std::time::Instant::now();
    let files = diff::parse(&text);
    let took = start.elapsed();
    assert_eq!(files.len(), 300);
    assert_eq!(files.iter().map(|f| f.hunks.len()).sum::<usize>(), 3000);
    // ~2.7 MB, 27k lines; generous bound for debug builds on CI.
    assert!(took.as_millis() < 500, "parse took {took:?}");
}
