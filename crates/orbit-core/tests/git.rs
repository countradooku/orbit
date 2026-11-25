//! The git layer against real repositories in temp directories.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use orbit_core::git::*;
use orbit_proto::diff::{FileDiffKind, file_patch, hunk_patch};
use orbit_proto::{ChangeKind, DiffScope, ThreadId};

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(dir: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

/// A repo on `main` with one commit; autocrlf off unless a test turns it on.
fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    sh(&dir, &["init", "-q", "-b", "main"]);
    sh(&dir, &["config", "user.name", "Test"]);
    sh(&dir, &["config", "user.email", "test@example.com"]);
    sh(&dir, &["config", "core.autocrlf", "false"]);
    sh(&dir, &["config", "commit.gpgsign", "false"]);
    for (p, c) in files {
        write(&dir, p, c);
    }
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "initial", "--allow-empty"]);
    (tmp, dir)
}

fn layer(tmp: &tempfile::TempDir) -> Git {
    Git::new(GitConfig::new(tmp.path().join("data")))
}

fn tree_files(dir: &Path, tree: &TreeId) -> BTreeSet<String> {
    sh(dir, &["-c", "core.quotepath=false", "ls-tree", "-r", "--name-only", tree.as_str()])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn lines(n: usize, f: impl Fn(usize) -> String) -> String {
    (1..=n).map(|i| f(i) + "\n").collect()
}

#[tokio::test]
async fn checkpoint_includes_untracked_excludes_ignored_and_never_touches_user_state() {
    let (tmp, dir) = repo(&[("a.txt", "a\n"), ("b.txt", "b\n"), (".gitignore", "target/\n*.log\n")]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    write(&dir, "b.txt", "b staged\n");
    sh(&dir, &["add", "b.txt"]);
    write(&dir, "a.txt", "a unstaged\n");
    write(&dir, "u.txt", "untracked\n");
    write(&dir, "target/out.bin", "build output");
    write(&dir, "x.log", "log");

    let index_before = std::fs::read(dir.join(".git/index")).unwrap();
    let head_before = sh(&dir, &["rev-parse", "HEAD"]);
    let status_before = sh(&dir, &["status", "--porcelain"]);

    let cp0 = git.checkpoint(&dir, t).await.unwrap();
    assert_eq!(cp0.n, 0);
    let files = tree_files(&dir, &cp0.tree);
    assert_eq!(files, ["a.txt", "b.txt", "u.txt", ".gitignore"].into_iter().map(String::from).collect());
    assert_eq!(sh(&dir, &["show", &format!("{}:a.txt", cp0.tree)]), "a unstaged");
    assert_eq!(sh(&dir, &["rev-parse", &format!("refs/orbit/checkpoints/{t}/0")]), cp0.tree.as_str());

    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index_before, "user index untouched");
    assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(sh(&dir, &["status", "--porcelain"]), status_before);
    assert!(dir.join(format!(".git/orbit/index-{t}")).is_file());

    let cp1 = git.checkpoint(&dir, t).await.unwrap();
    assert_eq!((cp1.n, &cp1.tree), (1, &cp0.tree), "unchanged tree, next number");
    write(&dir, "u.txt", "changed\n");
    let cp2 = git.checkpoint(&dir, t).await.unwrap();
    assert_ne!(cp2.tree, cp0.tree);

    // A fresh layer (engine restart) finds the existing checkpoints.
    let again = layer(&tmp);
    let list = again.checkpoints(&dir, t).await.unwrap();
    assert_eq!(list.iter().map(|c| c.n).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(again.checkpoint(&dir, t).await.unwrap().n, 3);

    // GC keeps checkpoint objects.
    sh(&dir, &["gc", "-q", "--prune=now"]);
    assert_eq!(sh(&dir, &["cat-file", "-t", cp2.tree.as_str()]), "tree");
}

#[tokio::test]
async fn checkpoints_are_incremental_capped_and_cheap_in_processes() {
    let files: Vec<(String, String)> =
        (0..3000).map(|i| (format!("src/m{}/f{i}.rs", i % 30), format!("// file {i}\n{}", "x\n".repeat(20)))).collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (tmp, dir) = repo(&refs);
    let mut cfg = GitConfig::new(tmp.path().join("data"));
    cfg.max_untracked_file = 4096;
    let git = Git::new(cfg);
    let t = ThreadId::new();

    let start = Instant::now();
    git.checkpoint(&dir, t).await.unwrap();
    let first = start.elapsed();

    write(&dir, "src/m3/f3.rs", "changed\n");
    write(&dir, "small_new.txt", "small\n");
    write(&dir, "huge.bin", vec![7u8; 10_000]);
    let before = git.processes_spawned();
    let start = Instant::now();
    let cp = git.checkpoint(&dir, t).await.unwrap();
    let second = start.elapsed();
    assert_eq!(git.processes_spawned() - before, 4, "ls-files, add, write-tree, update-ref");
    let tree = tree_files(&dir, &cp.tree);
    assert!(tree.contains("small_new.txt"));
    assert!(!tree.contains("huge.bin"), "untracked file over the cap is skipped");
    assert_eq!(tree.len(), 3001);
    eprintln!("checkpoint of 3000 files: first {first:?}, incremental {second:?}");
    assert!(second < Duration::from_secs(10));

    let d = git.diff(&dir, &git.checkpoint_tree(&dir, t, 0).await.unwrap(), Some(&cp.tree)).await.unwrap();
    assert_eq!(d.files.len(), 2);
}

#[tokio::test]
async fn turn_thread_base_and_uncommitted_scopes() {
    let (tmp, dir) = repo(&[("keep.txt", "keep\n"), ("edit.txt", "one\ntwo\nthree\n")]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    git.checkpoint(&dir, t).await.unwrap();

    // Turn 1 edits a file and adds 300 more.
    write(&dir, "edit.txt", "one\nTWO\nthree\n");
    for i in 0..300 {
        write(&dir, &format!("gen/g{i}.txt"), format!("generated {i}\n"));
    }
    git.checkpoint(&dir, t).await.unwrap();
    // Turn 2 deletes a file.
    std::fs::remove_file(dir.join("keep.txt")).unwrap();
    git.checkpoint(&dir, t).await.unwrap();
    // Turn 3 is still running.
    write(&dir, "running.txt", "wip\n");

    let before = git.processes_spawned();
    let turn1 = git.diff_scope(&dir, t, &DiffScope::Turn(1)).await.unwrap();
    assert_eq!(git.processes_spawned() - before, 1, "one git diff for 301 files");
    assert_eq!(turn1.files.len(), 301);
    let edit = turn1.file("edit.txt").unwrap();
    assert_eq!((edit.additions, edit.deletions), (1, 1));
    assert_eq!(turn1.changes().iter().filter(|c| c.kind == ChangeKind::Added).count(), 300);

    let before = git.processes_spawned();
    let again = git.diff_scope(&dir, t, &DiffScope::Turn(1)).await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&turn1, &again));
    assert_eq!(git.processes_spawned(), before, "cached by tree id: no process at all");

    let turn2 = git.diff_scope(&dir, t, &DiffScope::Turn(2)).await.unwrap();
    assert_eq!(turn2.changes().iter().map(|c| (c.path.as_str(), c.kind)).collect::<Vec<_>>(), vec![("keep.txt", ChangeKind::Deleted)]);

    let turn3 = git.diff_scope(&dir, t, &DiffScope::Turn(3)).await.unwrap();
    assert_eq!(turn3.files.iter().map(|f| f.path()).collect::<Vec<_>>(), vec!["running.txt"], "running turn diffs against the working tree");

    let thread = git.diff_scope(&dir, t, &DiffScope::Thread).await.unwrap();
    assert_eq!(thread.files.len(), 303);
    let before = git.processes_spawned();
    git.diff_scope(&dir, t, &DiffScope::Thread).await.unwrap();
    assert_eq!(git.processes_spawned() - before, 3, "an unchanged working tree costs a snapshot, not a diff");

    let uncommitted = git.diff_scope(&dir, t, &DiffScope::Uncommitted).await.unwrap();
    assert_eq!(uncommitted.files.len(), 303, "untracked files are part of uncommitted changes");

    sh(&dir, &["checkout", "-q", "-b", "feature"]);
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "feature work"]);
    write(&dir, "after_commit.txt", "x\n");
    let base = git.diff_scope(&dir, t, &DiffScope::Base("main".into())).await.unwrap();
    assert_eq!(base.files.len(), 304, "committed plus uncommitted work since the merge-base");
    assert!(matches!(git.diff_scope(&dir, t, &DiffScope::Turn(9)).await, Err(GitError::NoCheckpoint { n: 8, .. })));
}

#[tokio::test]
async fn hunk_and_file_revert_with_precheck() {
    let original = lines(40, |i| format!("line {i}"));
    let (tmp, dir) = repo(&[("f.txt", &original)]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    let cp0 = git.checkpoint(&dir, t).await.unwrap();
    let changed = original.replace("line 2\n", "line two\n").replace("line 20\n", "line twenty\n").replace("line 38\n", "line 38!\n");
    write(&dir, "f.txt", &changed);
    write(&dir, "new.txt", "added by the agent\n");

    let d = git.diff(&dir, &cp0.tree, None).await.unwrap();
    let f = d.file("f.txt").unwrap();
    assert_eq!(f.hunks.len(), 3);
    git.revert_patch(&dir, &hunk_patch(f, 1)).await.unwrap();
    let now = read(&dir, "f.txt");
    assert!(now.contains("line two\n") && now.contains("line 20\n") && now.contains("line 38!\n"));

    // Reverting the same hunk again fails the precheck and changes nothing.
    let err = git.revert_patch(&dir, &hunk_patch(f, 1)).await.unwrap_err();
    assert!(matches!(err, GitError::PatchDoesNotApply(_)), "{err}");
    assert_eq!(read(&dir, "f.txt"), now);

    let added = d.file("new.txt").unwrap();
    assert_eq!(added.kind, FileDiffKind::Added);
    git.revert_patch(&dir, &file_patch(added)).await.unwrap();
    assert!(!dir.join("new.txt").exists());

    // The other hunks still apply at their shifted positions.
    git.revert_patch(&dir, &hunk_patch(f, 2)).await.unwrap();
    git.revert_patch(&dir, &hunk_patch(f, 0)).await.unwrap();
    assert_eq!(read(&dir, "f.txt"), original);
}

#[tokio::test]
async fn restore_deletes_added_files_keeps_ignored_and_is_undoable() {
    let (tmp, dir) = repo(&[("a.txt", "original\n"), ("sub/b.txt", "b\n"), (".gitignore", "*.log\n")]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    write(&dir, "notes.txt", "untracked before the thread\n");
    write(&dir, "keep.log", "ignored before\n");
    let cp0 = git.checkpoint(&dir, t).await.unwrap();

    write(&dir, "a.txt", "agent edit\n");
    std::fs::remove_file(dir.join("sub/b.txt")).unwrap();
    write(&dir, "new.txt", "new\n");
    write(&dir, "new_dir/deep/c.txt", "c\n");
    write(&dir, "other.log", "ignored, created later\n");
    let head = sh(&dir, &["rev-parse", "HEAD"]);
    let index = std::fs::read(dir.join(".git/index")).unwrap();

    let restored = git.restore(&dir, t, &cp0.tree).await.unwrap();
    assert_eq!(read(&dir, "a.txt"), "original\n");
    assert_eq!(read(&dir, "sub/b.txt"), "b\n");
    assert!(!dir.join("new.txt").exists());
    assert!(!dir.join("new_dir").exists(), "emptied directories are removed");
    assert_eq!(read(&dir, "notes.txt"), "untracked before the thread\n");
    assert_eq!(read(&dir, "keep.log"), "ignored before\n");
    assert_eq!(read(&dir, "other.log"), "ignored, created later\n", "ignored files are never touched");
    assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index);
    assert_eq!(git.snapshot(&dir).await.unwrap(), cp0.tree);

    let undone = git.undo_restore(&dir, t).await.unwrap();
    assert_eq!(undone.restored, restored.undo);
    assert_eq!(read(&dir, "a.txt"), "agent edit\n");
    assert!(!dir.join("sub/b.txt").exists());
    assert_eq!(read(&dir, "new_dir/deep/c.txt"), "c\n");
}

#[tokio::test]
async fn prune_and_delete_thread_drop_refs() {
    let (tmp, dir) = repo(&[("a", "a\n")]);
    let git = layer(&tmp);
    let (t, other) = (ThreadId::new(), ThreadId::new());
    for i in 0..6 {
        write(&dir, "a", format!("{i}\n"));
        git.checkpoint(&dir, t).await.unwrap();
    }
    git.checkpoint(&dir, other).await.unwrap();
    assert_eq!(git.prune(&dir, t, 2).await.unwrap(), 3);
    let kept: Vec<u32> = git.checkpoints(&dir, t).await.unwrap().iter().map(|c| c.n).collect();
    assert_eq!(kept, vec![0, 4, 5], "baseline and newest two survive");
    assert_eq!(layer(&tmp).checkpoints(&dir, t).await.unwrap().len(), 3, "pruned on disk too");
    git.restore(&dir, t, &git.checkpoint_tree(&dir, t, 0).await.unwrap()).await.unwrap();

    git.delete_thread(&dir, t).await.unwrap();
    assert!(git.checkpoints(&dir, t).await.unwrap().is_empty());
    assert!(!dir.join(format!(".git/orbit/index-{t}")).exists());
    assert!(sh(&dir, &["for-each-ref", &format!("refs/orbit/undo/{t}")]).is_empty());
    assert_eq!(git.checkpoints(&dir, other).await.unwrap().len(), 1);

    assert_eq!(git.prune_orphans(&dir, &[]).await.unwrap(), 1);
    assert!(sh(&dir, &["for-each-ref", "refs/orbit/"]).is_empty());
}

#[tokio::test]
async fn crlf_spaces_and_unicode_paths_survive_diff_revert_and_status() {
    let (tmp, dir) = repo(&[]);
    sh(&dir, &["config", "core.autocrlf", "true"]);
    let crlf = "one\r\ntwo\r\nthree\r\n";
    write(&dir, "win.txt", crlf);
    write(&dir, "dir with space/na\u{ef}ve caf\u{e9}.txt", "alpha\nbeta\n");
    write(&dir, "\u{65e5}\u{672c}\u{8a9e}.md", "# t\n");
    sh(&dir, &["-c", "core.safecrlf=false", "add", "-A"]);
    sh(&dir, &["commit", "-qm", "files"]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    let cp0 = git.checkpoint(&dir, t).await.unwrap();

    write(&dir, "win.txt", "one\r\nTWO\r\nthree\r\n");
    write(&dir, "dir with space/na\u{ef}ve caf\u{e9}.txt", "alpha\nbeta\ngamma\n");
    write(&dir, "\u{65e5}\u{672c}\u{8a9e}.md", "# t\nbody\n");
    write(&dir, "new dir/\u{fc}ber file.txt", "neu\n");
    let d = git.diff(&dir, &cp0.tree, None).await.unwrap();
    let paths: BTreeSet<&str> = d.files.iter().map(|f| f.path()).collect();
    assert_eq!(
        paths,
        ["win.txt", "dir with space/na\u{ef}ve caf\u{e9}.txt", "\u{65e5}\u{672c}\u{8a9e}.md", "new dir/\u{fc}ber file.txt"]
            .into_iter()
            .collect()
    );

    let st = git.status(&dir).await.unwrap();
    let mut status_paths: Vec<&str> = st.files.iter().map(|f| f.path.as_str()).collect();
    status_paths.sort();
    let mut expected: Vec<&str> = paths.iter().copied().collect();
    expected.sort();
    assert_eq!(status_paths, expected);

    for f in &d.files {
        git.revert_patch(&dir, &file_patch(f)).await.unwrap_or_else(|e| panic!("{}: {e}", f.path()));
    }
    assert_eq!(std::fs::read(dir.join("win.txt")).unwrap(), crlf.as_bytes(), "CRLF restored byte for byte");
    // autocrlf=true: git writes the working-tree eol; the content is the same.
    assert_eq!(read(&dir, "dir with space/na\u{ef}ve caf\u{e9}.txt").replace('\r', ""), "alpha\nbeta\n");
    assert!(!dir.join("new dir/\u{fc}ber file.txt").exists());
    assert_eq!(git.snapshot(&dir).await.unwrap(), cp0.tree);

    // Restore also writes CRLF back through the user's eol settings.
    write(&dir, "win.txt", "changed\r\n");
    git.restore(&dir, t, &cp0.tree).await.unwrap();
    assert_eq!(std::fs::read(dir.join("win.txt")).unwrap(), crlf.as_bytes());
}

#[tokio::test]
async fn status_branches_and_merge_base() {
    let (tmp, dir) = repo(&[("a.txt", "1\n2\n3\n"), ("gone.txt", "x\n")]);
    let git = layer(&tmp);
    write(&dir, "a.txt", "1\nchanged\n3\nadded\n");
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    write(&dir, "staged.txt", "s\n");
    sh(&dir, &["add", "staged.txt"]);
    write(&dir, "untracked.txt", "u1\nu2\nu3");

    let st = git.status(&dir).await.unwrap();
    assert_eq!(st.branch.as_deref(), Some("main"));
    assert!(st.head.is_some());
    let get = |p: &str| st.files.iter().find(|f| f.path == p).cloned().unwrap();
    assert_eq!((get("a.txt").kind, get("a.txt").additions, get("a.txt").deletions), (ChangeKind::Modified, 2, 1));
    assert_eq!(get("gone.txt").kind, ChangeKind::Deleted);
    assert_eq!((get("staged.txt").kind, get("staged.txt").additions), (ChangeKind::Added, 1));
    assert_eq!((get("untracked.txt").kind, get("untracked.txt").additions), (ChangeKind::Added, 3));

    assert_eq!(git.current_branch(&dir).await.unwrap().as_deref(), Some("main"));
    let main_head = sh(&dir, &["rev-parse", "HEAD"]);
    sh(&dir, &["checkout", "-q", "-b", "topic"]);
    sh(&dir, &["commit", "-qam", "topic"]);
    let branches = git.branches(&dir).await.unwrap();
    let names: Vec<(&str, bool)> = branches.iter().map(|b| (b.name.as_str(), b.current)).collect();
    assert_eq!(names, vec![("main", false), ("topic", true)]);
    assert!(branches[1].worktree.is_some());
    assert_eq!(git.merge_base(&dir, "main", "topic").await.unwrap().as_deref(), Some(main_head.as_str()));
    sh(&dir, &["checkout", "-q", "--detach"]);
    assert_eq!(git.current_branch(&dir).await.unwrap(), None);

    // A repository with no commits yet.
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    sh(&empty, &["init", "-q", "-b", "trunk"]);
    write(&empty, "first.txt", "a\nb\n");
    let st = git.status(&empty).await.unwrap();
    assert_eq!((st.branch.as_deref(), st.head.as_deref()), (Some("trunk"), None));
    assert_eq!(st.files[0].additions, 2);
    let d = git.diff_scope(&empty, ThreadId::new(), &DiffScope::Uncommitted).await.unwrap();
    assert_eq!(d.files.len(), 1);
}

fn script(windows: &str, unix: &str) -> String {
    if cfg!(windows) { windows.to_owned() } else { unix.to_owned() }
}

#[tokio::test]
async fn worktrees_copy_includes_get_ports_run_scripts_and_clean_up() {
    let (tmp, dir) = repo(&[
        (".gitignore", ".env*\nsecrets/\n"),
        (".worktreeinclude", ".env*\n!.env.example\nsecrets/\n"),
        ("shared.txt", "shared\n"),
        ("a_only.txt", "a\n"),
    ]);
    write(&dir, ".env", "TOKEN=1\n");
    write(&dir, ".env.local", "LOCAL=1\n");
    write(&dir, ".env.example", "EXAMPLE=1\n");
    write(&dir, "secrets/key.txt", "k\n");
    write(&dir, "notes.txt", "not included\n");
    let mut cfg = GitConfig::new(tmp.path().join("data"));
    cfg.worktree_root = Some(tmp.path().join("wt root"));
    cfg.port_first = 47_100;
    let git = Git::new(cfg.clone());
    let (ta, tb) = (ThreadId::new(), ThreadId::new());

    let mut lines_seen = Vec::new();
    let setup = script("echo %PORT%> port.txt && echo hello from setup", "echo $PORT > port.txt && echo hello from setup");
    let a = git
        .create_worktree(
            NewWorktree {
                project_root: dir.clone(),
                thread: ta,
                branch: "orbit/feature-a".into(),
                base: Some("main".into()),
                path: None,
                setup: Some(setup.clone()),
            },
            &mut |stream, line| lines_seen.push((stream, line.to_owned())),
        )
        .await
        .unwrap();
    let pa = a.record.path.clone();
    assert!(pa.starts_with(tmp.path().join("wt root").join("repo")));
    assert!(pa.ends_with("orbit-feature-a"));
    let mut copied = a.copied.clone();
    copied.sort();
    assert_eq!(copied, vec![".env", ".env.local", "secrets/key.txt"]);
    assert_eq!(read(&pa, ".env"), "TOKEN=1\n");
    assert!(!pa.join(".env.example").exists() && !pa.join("notes.txt").exists());
    assert!(a.setup.as_ref().unwrap().ok());
    assert!(lines_seen.iter().any(|(s, l)| *s == OutputStream::Stdout && l.trim() == "hello from setup"));
    assert_eq!(read(&pa, "port.txt").trim(), a.record.port_base.to_string());
    assert_eq!(git.current_branch(&pa).await.unwrap().as_deref(), Some("orbit/feature-a"));

    let b = git
        .create_worktree(
            NewWorktree {
                project_root: dir.clone(),
                thread: tb,
                branch: "orbit/feature-b".into(),
                base: None,
                path: Some(tmp.path().join("explicit b")),
                setup: None,
            },
            &mut |_, _| {},
        )
        .await
        .unwrap();
    let pb = b.record.path.clone();
    assert!(b.setup.is_none());
    let (ra, rb) = (a.record.port_base, b.record.port_base);
    assert!(ra + a.record.port_count <= rb || rb + b.record.port_count <= ra, "port ranges are disjoint");
    assert!(b.record.env().contains(&("PORT".into(), rb.to_string())));

    // The registry survives a restart.
    let restarted = Git::new(cfg);
    assert_eq!(restarted.worktrees().unwrap().len(), 2);
    assert_eq!(restarted.worktree_for(tb).unwrap().unwrap().port_base, rb);

    // Both threads touch shared.txt; only A touches a_only.txt.
    write(&pa, "shared.txt", "from a\n");
    write(&pa, "a_only.txt", "a2\n");
    write(&pb, "shared.txt", "from b\n");
    sh(&pb, &["commit", "-qam", "b commits its edit"]);
    write(&pb, "b_new.txt", "untracked in b\n");
    let live = [
        LiveWorktree { thread: ta, path: pa.clone(), base: "main".into() },
        LiveWorktree { thread: tb, path: pb.clone(), base: "main".into() },
    ];
    let conflicts = git.conflicts(&live).await.unwrap();
    let mut both = vec![ta, tb];
    both.sort();
    assert_eq!(conflicts, vec![Conflict { path: "shared.txt".into(), threads: both }]);

    assert!(disk_usage(&pa).await.unwrap() > 0);

    let teardown = script("echo bye> \"%ORBIT_ROOT_PATH%/teardown.txt\"", "echo bye > \"$ORBIT_ROOT_PATH/teardown.txt\"");
    let removed = git
        .remove_worktree(
            &pa,
            RemoveWorktree { teardown: Some(teardown), branch: BranchCleanup::Delete, force: true, ..Default::default() },
            &mut |_, _| {},
        )
        .await
        .unwrap();
    assert!(removed.teardown.unwrap().ok());
    assert_eq!(read(&dir, "teardown.txt").trim(), "bye");
    assert_eq!(removed.deleted_branch.as_deref(), Some("orbit/feature-a"));
    assert!(!pa.exists());
    assert!(sh(&dir, &["branch", "--list", "orbit/feature-a"]).is_empty());
    assert_eq!(git.worktrees().unwrap().len(), 1);

    // Unmerged branch with the safe cleanup: worktree goes, branch stays.
    let removed = git
        .remove_worktree(&pb, RemoveWorktree { branch: BranchCleanup::DeleteIfMerged, force: true, ..Default::default() }, &mut |_, _| {})
        .await
        .unwrap();
    assert!(removed.deleted_branch.is_none() && removed.branch_error.is_some());
    assert!(!sh(&dir, &["branch", "--list", "orbit/feature-b"]).is_empty());
    assert!(git.worktrees().unwrap().is_empty());
    assert!(!sh(&dir, &["worktree", "list"]).contains("explicit b"));

    // The freed branch can get a new worktree immediately.
    let c = git
        .create_worktree(
            NewWorktree {
                project_root: dir.clone(),
                thread: ThreadId::new(),
                branch: "orbit/feature-b".into(),
                base: None,
                path: None,
                setup: None,
            },
            &mut |_, _| {},
        )
        .await
        .unwrap();
    assert_eq!(read(&c.record.path, "shared.txt").replace('\r', ""), "from b\n", "existing branch is checked out");
}

async fn collect(w: &RepoWatcher, window: Duration) -> Vec<Changes> {
    let mut out = Vec::new();
    let end = Instant::now() + window;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, w.recv()).await {
            Ok(Some(c)) => out.push(c),
            _ => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_coalesces_bursts_and_ignores_noise() {
    let (tmp, dir) = repo(&[(".gitignore", "target/\n"), ("a.txt", "a\n")]);
    write(&dir, "target/existing.o", "o");
    let git = layer(&tmp);
    let w = git.watch(&dir).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    for i in 0..60 {
        write(&dir, &format!("burst/f{i}.txt"), "x");
    }
    let got = collect(&w, Duration::from_millis(1500)).await;
    assert!(!got.is_empty(), "a burst is reported");
    assert!(got.len() <= 3, "60 writes coalesce into a few notifications, got {}", got.len());
    let all: BTreeSet<PathBuf> = got.iter().flat_map(|c| c.paths.iter().cloned()).collect();
    for i in 0..60 {
        assert!(all.contains(&Path::new("burst").join(format!("f{i}.txt"))), "missing f{i}");
    }

    // Ignored build output and Orbit's own checkpoint writes stay silent.
    for i in 0..20 {
        write(&dir, &format!("target/obj{i}.o"), "o");
    }
    git.checkpoint(&dir, ThreadId::new()).await.unwrap();
    let noise = collect(&w, Duration::from_millis(600)).await;
    // Under heavy load the tail of the burst can still be arriving; those
    // paths are expected. Anything else (build output, checkpoint refs) is not.
    assert!(
        noise.iter().all(|c| !c.git_refs && c.paths.iter().all(|p| p.starts_with("burst"))),
        "unexpected: {noise:?}"
    );

    // A commit moves a ref.
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-qm", "burst"]);
    let got = collect(&w, Duration::from_millis(1500)).await;
    assert!(got.iter().any(|c| c.git_refs), "commit reported as a ref change: {got:?}");
}

#[tokio::test]
async fn commit_push_and_pull_request_errors() {
    let (tmp, dir) = repo(&[("a.txt", "a\n")]);
    let remote = tmp.path().join("remote.git");
    sh(tmp.path(), &["init", "-q", "--bare", remote.to_str().unwrap()]);
    sh(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let mut cfg = GitConfig::new(tmp.path().join("data"));
    cfg.gh = Some(tmp.path().join("no-such-gh.exe"));
    let git = Git::new(cfg);

    write(&dir, "a.txt", "changed\n");
    write(&dir, "b.txt", "new\n");
    let result = git.commit(&dir, "Orbit commit\n\nwith a body", true).await.unwrap();
    assert_eq!(result.commit, sh(&dir, &["rev-parse", "HEAD"]));
    let pushed = result.pushed.unwrap();
    assert_eq!((pushed.remote.as_str(), pushed.branch.as_str(), pushed.set_upstream), ("origin", "main", true));
    assert_eq!(sh(&remote, &["rev-parse", "main"]), result.commit);
    assert_eq!(sh(&dir, &["log", "-1", "--format=%B"]), "Orbit commit\n\nwith a body");

    assert!(matches!(git.commit(&dir, "again", false).await, Err(GitError::NothingToCommit)));
    write(&dir, "c.txt", "c\n");
    git.commit(&dir, "second", false).await.unwrap();
    let again = git.push(&dir).await.unwrap();
    assert!(!again.set_upstream);

    let err = git.create_pull_request(&dir, "title", "body", None).await.unwrap_err();
    assert!(matches!(err, GitError::NotInstalled("gh")));
    assert!(err.to_string().contains("GitHub CLI"));
}

#[tokio::test]
async fn concurrent_requests_never_exceed_the_process_limit() {
    let (tmp, dir) = repo(&[("a.txt", "a\n")]);
    let mut cfg = GitConfig::new(tmp.path().join("data"));
    cfg.max_processes = 2;
    let git = Git::new(cfg);
    let t = ThreadId::new();
    let cp = git.checkpoint(&dir, t).await.unwrap();
    write(&dir, "a.txt", "b\n");
    let to = git.snapshot(&dir).await.unwrap();
    let before = git.processes_spawned();
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let (git, dir, from, to) = (git.clone(), dir.clone(), cp.tree.clone(), to.clone());
        tasks.push(tokio::spawn(async move { git.diff(&dir, &from, Some(&to)).await.unwrap() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().files.len(), 1);
    }
    assert_eq!(git.processes_spawned() - before, 1, "50 concurrent identical requests share one git diff");
    assert!(matches!(git.status(tmp.path()).await, Err(GitError::NotARepo(_))));
}

#[tokio::test]
async fn failed_worktree_creation_leaves_nothing_behind() {
    let (tmp, dir) = repo(&[("a.txt", "a\n")]);
    let git = layer(&tmp);
    let spec = |branch: &str, base: Option<&str>| NewWorktree {
        project_root: dir.clone(),
        thread: ThreadId::new(),
        branch: branch.into(),
        base: base.map(str::to_owned),
        path: Some(tmp.path().join("wt")),
        setup: None,
    };
    assert!(git.create_worktree(spec("orbit/x", Some("no-such-ref")), &mut |_, _| {}).await.is_err());
    assert!(sh(&dir, &["branch", "--list", "orbit/x"]).is_empty());
    assert!(!tmp.path().join("wt").exists());
    // A branch checked out elsewhere is refused, and of course kept.
    assert!(git.create_worktree(spec("main", None), &mut |_, _| {}).await.is_err());
    assert!(!sh(&dir, &["branch", "--list", "main"]).is_empty());
    assert!(git.worktrees().unwrap().is_empty());
}

/// A folder that is not a git repository still gets checkpoints, turn diffs
/// and restore, through a shadow repository in Orbit's data folder. The
/// folder never gains a `.git`, and operations that need a real repository
/// keep refusing it.
#[tokio::test]
async fn a_folder_without_git_gets_shadow_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("plain");
    write(&dir, "README.md", "old intro\nsecond line\n");
    write(&dir, ".gitignore", "secret.env\n");
    write(&dir, "secret.env", "TOKEN=1\n");
    write(&dir, "node_modules/x/index.js", "x\n");
    let git = layer(&tmp);
    let thread = ThreadId::new();

    let base = git.checkpoint(&dir, thread).await.unwrap();
    assert_eq!(base.n, 0);
    write(&dir, "README.md", "new intro\nsecond line\n");
    write(&dir, "src/added.rs", "fn main() {}\n");
    let after = git.checkpoint(&dir, thread).await.unwrap();

    let diff = git.diff_scope(&dir, thread, &DiffScope::Turn(1)).await.unwrap();
    let mut changes: Vec<(String, ChangeKind, u32, u32)> =
        diff.changes().into_iter().map(|c| (c.path, c.kind, c.additions, c.deletions)).collect();
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        changes,
        vec![("README.md".into(), ChangeKind::Modified, 1, 1), ("src/added.rs".into(), ChangeKind::Added, 1, 0)],
        "ignored files and node_modules stay out"
    );

    // Restore to before the turn, then undo it.
    git.restore(&dir, thread, &base.tree).await.unwrap();
    assert_eq!(read(&dir, "README.md"), "old intro\nsecond line\n");
    assert!(!dir.join("src/added.rs").exists());
    assert_eq!(read(&dir, "secret.env"), "TOKEN=1\n", "ignored files are left alone");
    git.undo_restore(&dir, thread).await.unwrap();
    assert_eq!(read(&dir, "README.md"), "new intro\nsecond line\n");
    assert_eq!(git.checkpoints(&dir, thread).await.unwrap().last().unwrap().tree, after.tree);

    assert!(!dir.join(".git").exists(), "the folder is never made a repository");
    assert!(matches!(git.status(&dir).await, Err(GitError::NotARepo(_))), "status still needs a real repository");
    assert!(git.current_branch(&dir).await.is_err());

    // A second Git (a restart) finds the same shadow store and its checkpoints.
    let again = layer(&tmp);
    assert_eq!(again.checkpoints(&dir, thread).await.unwrap().len(), 2);
}

#[tokio::test]
async fn home_and_drive_roots_are_never_shadowed() {
    let tmp = tempfile::tempdir().unwrap();
    let git = layer(&tmp);
    let root = Path::new(if cfg!(windows) { "C:\\" } else { "/" });
    assert!(git.checkpoint(root, ThreadId::new()).await.is_err());
}
