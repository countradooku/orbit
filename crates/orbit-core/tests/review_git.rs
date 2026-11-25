//! Review-side git operations against real repositories: merging locally,
//! "since turn" diffs and undoing a restore.

use std::path::{Path, PathBuf};
use std::process::Command;

use orbit_core::git::*;
use orbit_proto::{DiffScope, ThreadId};

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// A repo on `main` with one commit.
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
    sh(&dir, &["commit", "-qm", "initial"]);
    (tmp, dir)
}

fn layer(tmp: &tempfile::TempDir) -> Git {
    Git::new(GitConfig::new(tmp.path().join("data")))
}

#[tokio::test]
async fn merging_a_branch_that_is_not_checked_out_writes_the_previewed_merge() {
    let (tmp, dir) = repo(&[("shared.txt", "one\ntwo\nthree\n"), ("main.txt", "main\n")]);
    let git = layer(&tmp);
    sh(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "shared.txt", "one\nTWO\nthree\n");
    sh(&dir, &["commit", "-qam", "feature work"]);
    // main moves on too, elsewhere in the file: a real (non fast-forward) merge.
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "main.txt", "main moved\n");
    sh(&dir, &["commit", "-qam", "main work"]);
    sh(&dir, &["checkout", "-q", "feature"]);
    // Uncommitted work is part of the preview and committed by the merge.
    write(&dir, "new.txt", "brand new\n");
    let main_before = sh(&dir, &["rev-parse", "main"]);

    let plan = git.merge_preview(&dir, "main").await.unwrap();
    assert_eq!((plan.source.as_str(), plan.target.as_str()), ("feature", "main"));
    assert_eq!(plan.uncommitted, 1);
    assert!(!plan.fast_forward);
    assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
    let mut paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["new.txt", "shared.txt"]);
    assert_eq!(sh(&dir, &["rev-parse", "main"]), main_before, "a preview changes nothing");
    assert!(sh(&dir, &["status", "--porcelain"]).contains("new.txt"), "a preview leaves uncommitted work alone");

    let (_, tip) = git.merge_into(&dir, "main", "Add new.txt").await.unwrap();
    assert_eq!(sh(&dir, &["rev-parse", "main"]), tip);
    assert_eq!(sh(&dir, &["show", "main:shared.txt"]), "one\nTWO\nthree");
    assert_eq!(sh(&dir, &["show", "main:new.txt"]), "brand new");
    assert_eq!(sh(&dir, &["show", "main:main.txt"]), "main moved");
    assert_eq!(sh(&dir, &["rev-list", "--parents", "-n1", "main"]).split(' ').count(), 3, "a merge commit");
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "", "the uncommitted work was committed");
    assert_eq!(sh(&dir, &["branch", "--show-current"]), "feature", "the checkout stays where it was");
}

#[tokio::test]
async fn a_conflicting_merge_is_refused_before_anything_changes() {
    let (tmp, dir) = repo(&[("f.txt", "a\n")]);
    let git = layer(&tmp);
    sh(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "f.txt", "feature\n");
    sh(&dir, &["commit", "-qam", "feature"]);
    sh(&dir, &["checkout", "-q", "main"]);
    write(&dir, "f.txt", "main\n");
    sh(&dir, &["commit", "-qam", "main"]);
    sh(&dir, &["checkout", "-q", "feature"]);
    write(&dir, "other.txt", "uncommitted\n");
    let main_before = sh(&dir, &["rev-parse", "main"]);
    let head_before = sh(&dir, &["rev-parse", "HEAD"]);

    let plan = git.merge_preview(&dir, "main").await.unwrap();
    assert_eq!(plan.conflicts, vec!["f.txt".to_owned()]);
    let err = git.merge_into(&dir, "main", "wip").await.unwrap_err().to_string();
    assert!(err.contains("conflict") && err.contains("f.txt"), "{err}");
    assert_eq!(sh(&dir, &["rev-parse", "main"]), main_before);
    assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), head_before, "nothing was committed");
    assert!(sh(&dir, &["status", "--porcelain"]).contains("other.txt"));
}

#[tokio::test]
async fn merging_into_a_checked_out_branch_updates_that_checkout() {
    let (tmp, dir) = repo(&[("f.txt", "a\n")]);
    let git = layer(&tmp);
    let wt = tmp.path().join("wt");
    sh(&dir, &["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
    write(&wt, "g.txt", "from the worktree\n");
    sh(&wt, &["add", "-A"]);
    sh(&wt, &["commit", "-qm", "feature"]);

    let plan = git.merge_preview(&wt, "main").await.unwrap();
    assert!(plan.fast_forward);
    assert_eq!(plan.commits, 1);
    assert_eq!(plan.target_worktree.as_deref().map(|p| p.canonicalize().unwrap()), Some(dir.canonicalize().unwrap()));
    git.merge_into(&wt, "main", "unused").await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("g.txt")).unwrap(), "from the worktree\n", "main's checkout follows");
    assert_eq!(sh(&dir, &["rev-parse", "main"]), sh(&wt, &["rev-parse", "HEAD"]), "fast-forwarded");

    let again = git.merge_into(&wt, "main", "unused").await.unwrap_err().to_string();
    assert!(again.contains("nothing"), "{again}");
}

#[tokio::test]
async fn since_turn_diffs_reach_the_working_tree_and_restores_undo_both_ways() {
    let (tmp, dir) = repo(&[("a.txt", "a\n")]);
    let git = layer(&tmp);
    let t = ThreadId::new();
    git.checkpoint(&dir, t).await.unwrap(); // 0: baseline
    write(&dir, "turn1.txt", "one\n");
    git.checkpoint(&dir, t).await.unwrap(); // 1
    write(&dir, "turn2.txt", "two\n");
    git.checkpoint(&dir, t).await.unwrap(); // 2
    write(&dir, "later.txt", "by hand\n");

    let since = git.diff_scope(&dir, t, &DiffScope::SinceTurn(2)).await.unwrap();
    let mut paths: Vec<&str> = since.files.iter().map(|f| f.path()).collect();
    paths.sort();
    assert_eq!(paths, vec!["later.txt", "turn2.txt"], "turn 2 and everything after it");

    let before_two = git.checkpoint_tree(&dir, t, 1).await.unwrap();
    git.restore(&dir, t, &before_two).await.unwrap();
    assert!(!dir.join("turn2.txt").exists() && !dir.join("later.txt").exists());
    assert!(dir.join("turn1.txt").exists());
    git.undo_restore(&dir, t).await.unwrap();
    assert!(dir.join("turn2.txt").exists() && dir.join("later.txt").exists(), "undo brings it all back");
    git.undo_restore(&dir, t).await.unwrap();
    assert!(!dir.join("later.txt").exists(), "undoing the undo restores again");
}

#[tokio::test]
async fn a_reverted_hunk_or_file_comes_back_with_its_inverted_patch() {
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    let (tmp, dir) = repo(&[("f.txt", &lines), ("gone.txt", "bye\n")]);
    let git = layer(&tmp);
    let edited = lines.replace("line 2\n", "line two\n").replace("line 25\n", "line twenty-five\n");
    write(&dir, "f.txt", &edited);
    write(&dir, "new.txt", "fresh\n");
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    let head = git.head_tree(&dir).await.unwrap();
    let diff = git.diff(&dir, &head, None).await.unwrap();
    let f = diff.file("f.txt").unwrap();
    assert_eq!(f.hunks.len(), 2);

    // Reject the second hunk, then undo the rejection.
    let patch = orbit_proto::hunk_patch(f, 1);
    git.revert_patch(&dir, &patch).await.unwrap();
    let now = std::fs::read_to_string(dir.join("f.txt")).unwrap();
    assert!(now.contains("line two") && now.contains("line 25\n"));
    git.revert_patch(&dir, &orbit_proto::diff::invert_patch(&patch).unwrap()).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), edited);

    // Whole files: a new file and a deletion.
    for path in ["new.txt", "gone.txt"] {
        let file = diff.file(path).unwrap();
        let patch = orbit_proto::file_patch(file);
        git.revert_patch(&dir, &patch).await.unwrap();
        assert_eq!(dir.join(path).exists(), path == "gone.txt", "{path} reverted");
        git.revert_patch(&dir, &orbit_proto::diff::invert_patch(&patch).unwrap()).await.unwrap();
        assert_eq!(dir.join(path).exists(), path == "new.txt", "{path} back");
    }
}
