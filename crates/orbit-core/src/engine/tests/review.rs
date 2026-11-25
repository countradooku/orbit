//! The review engine end to end: live workspace refresh, conflict warnings
//! across worktrees, "since turn" diffs, undoing a restore and local merges.

use std::path::Path;
use std::time::{Duration, Instant};

use orbit_proto::*;

use super::{Harness, git};

/// Applies events until one satisfies `want`, or fails after `secs`.
fn wait_event(h: &mut Harness, secs: u64, what: &str, mut want: impl FnMut(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        match h.events.try_recv() {
            Ok(env) => {
                if want(&env.event) {
                    return env.event;
                }
                h.apply(env.event);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn project_thread(h: &mut Harness, dir: &Path, worktree: bool) -> (ProjectId, ThreadId) {
    let Reply::Project(project) = h.call(Command::AddProject { path: dir.display().to_string() }) else { panic!() };
    let Reply::Thread(id) = h.call(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree })
    else {
        panic!()
    };
    (project, id)
}

fn has_file(files: &[FileChange], path: &str) -> bool {
    files.iter().any(|f| f.path == path)
}

#[test]
fn open_threads_get_live_workspace_changes_and_closed_ones_do_not() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let mut h = Harness::new();
    let (_, id) = project_thread(&mut h, repo.path(), false);
    h.call(Command::OpenThread { id });
    wait_event(&mut h, 10, "the first status", |e| matches!(e, Event::WorkspaceChanged { thread, .. } if *thread == id));

    std::fs::write(repo.path().join("live.txt"), "one\ntwo\n").unwrap();
    let event = wait_event(&mut h, 10, "the edit to show up", |e| {
        matches!(e, Event::WorkspaceChanged { thread, files } if *thread == id && has_file(files, "live.txt"))
    });
    let Event::WorkspaceChanged { files, .. } = event else { unreachable!() };
    let live = files.iter().find(|f| f.path == "live.txt").unwrap();
    assert_eq!((live.kind, live.additions), (ChangeKind::Added, 2));

    h.call(Command::CloseThread { id });
    std::fs::write(repo.path().join("after-close.txt"), "x\n").unwrap();
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if let Ok(env) = h.events.try_recv() {
            assert!(
                !matches!(&env.event, Event::WorkspaceChanged { files, .. } if has_file(files, "after-close.txt")),
                "a closed thread is no longer watched"
            );
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn since_turn_diffs_and_restores_can_be_undone() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let mut h = Harness::new();
    let (_, id) = project_thread(&mut h, repo.path(), false);
    h.send(id, "write");
    h.wait("turn", |h| h.thread(id).turns == 1 && h.thread(id).status == ThreadStatus::Idle);
    h.wait("checkpoint", |h| h.items.values().any(|i| matches!(&i.body, ItemBody::TurnEnd(s) if s.checkpoint.is_some())));
    std::fs::write(repo.path().join("by-hand.txt"), "edited after the turn\n").unwrap();

    let Reply::Diff(patch) = h.call(Command::Diff { thread: id, scope: DiffScope::SinceTurn(1) }) else { panic!() };
    let files = diff::parse(&patch);
    let mut paths: Vec<&str> = files.iter().map(|f| f.path()).collect();
    paths.sort();
    assert_eq!(paths, vec!["by-hand.txt", "written-by-agent.txt"], "the turn and everything after it");

    h.call(Command::RestoreCheckpoint { thread: id, turn: 1 });
    assert!(!repo.path().join("written-by-agent.txt").exists() && !repo.path().join("by-hand.txt").exists());
    h.call(Command::UndoRestore { thread: id });
    assert_eq!(std::fs::read_to_string(repo.path().join("by-hand.txt")).unwrap(), "edited after the turn\n");
    assert!(repo.path().join("written-by-agent.txt").exists());

    let other = h.new_thread();
    let err = h.engine.call(Command::UndoRestore { thread: other }).recv().unwrap().unwrap_err();
    assert!(err.contains("project"), "{err}");
}

#[test]
fn a_thread_merges_into_its_base_branch_locally() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    git(repo.path(), &["checkout", "-qb", "feature"]);
    let mut h = Harness::new();
    let (_, id) = project_thread(&mut h, repo.path(), false);
    std::fs::write(repo.path().join("feature.txt"), "done\n").unwrap();

    let Reply::Merge(preview) =
        h.call(Command::MergeLocal { thread: id, into: "main".into(), message: String::new(), dry_run: true })
    else {
        panic!()
    };
    assert_eq!((preview.source.as_str(), preview.target.as_str()), ("feature", "main"));
    assert_eq!((preview.uncommitted, preview.merged.as_deref()), (1, None));
    assert!(preview.fast_forward && preview.conflicts.is_empty());
    assert!(has_file(&preview.files, "feature.txt"));

    let Reply::Merge(done) =
        h.call(Command::MergeLocal { thread: id, into: "main".into(), message: "Add the feature".into(), dry_run: false })
    else {
        panic!()
    };
    let tip = done.merged.expect("merged");
    let out = std::process::Command::new("git").args(["rev-parse", "main"]).current_dir(repo.path()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), tip);
    let log = std::process::Command::new("git").args(["log", "-1", "--format=%s", "main"]).current_dir(repo.path()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "Add the feature");
}

#[test]
fn overlapping_worktrees_raise_a_conflict_warning() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let mut h = Harness::new();
    let (project, a) = project_thread(&mut h, repo.path(), true);
    let Reply::Thread(b) = h.call(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree: true })
    else {
        panic!()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while h.thread(a).workspace.worktree.is_none() || h.thread(b).workspace.worktree.is_none() {
        assert!(Instant::now() < deadline, "worktrees were not created");
        match h.events.try_recv() {
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    h.call(Command::OpenThread { id: a });
    h.call(Command::OpenThread { id: b });
    for id in [a, b] {
        let path = h.thread(id).workspace.worktree.clone().unwrap();
        std::fs::write(Path::new(&path).join("README.md"), format!("changed by {id}\n")).unwrap();
    }
    let event = wait_event(&mut h, 20, "a conflict warning", |e| matches!(e, Event::Conflicts(list) if !list.is_empty()));
    let Event::Conflicts(list) = event else { unreachable!() };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].path, "README.md");
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(list[0].threads, both);
}
