//! Workspaces end to end: project configuration (set, persisted, imported
//! from `orbit.toml`), run targets, live git status and reading files.

use std::path::Path;
use std::time::{Duration, Instant};

use orbit_proto::*;

use super::{Harness, git};

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

fn add_project(h: &Harness, dir: &Path) -> ProjectId {
    let Reply::Project(id) = h.call(Command::AddProject { path: dir.display().to_string() }) else { panic!() };
    id
}

fn workspace(h: &Harness, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
    match h.engine.call(Command::Workspace(command)).recv().unwrap() {
        Ok(Reply::Workspace(reply)) => Ok(reply),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e),
    }
}

#[test]
fn a_projects_configuration_is_kept_and_survives_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let config = ProjectConfig {
        icon: "🛰".into(),
        env: vec![EnvVar::new("API_URL", "http://localhost:9")],
        targets: vec![ProjectTarget { name: "Dev".into(), command: "npm run dev".into(), ..Default::default() }],
        ..Default::default()
    };
    let project = {
        let mut h = Harness::open(data.path().to_owned());
        let project = add_project(&h, folder.path());
        h.call(Command::Workspace(WorkspaceCommand::SetConfig { project, config: Box::new(config.clone()) }));
        let event = wait_event(&mut h, 5, "the configuration", |e| matches!(e, Event::ProjectConfigChanged { .. }));
        let Event::ProjectConfigChanged { config: sent, .. } = event else { unreachable!() };
        assert_eq!(*sent, config.clone().sanitized());
        h.engine.shutdown();
        project
    };
    let h = Harness::open(data.path().to_owned());
    let snapshot = h.engine.subscribe(0, 0).recv().unwrap().snapshot.unwrap();
    assert_eq!(snapshot.project_configs, vec![(project, config.sanitized())]);
    // Removing the project takes its configuration with it.
    h.call(Command::RemoveProject { id: project });
    let snapshot = h.engine.subscribe(0, 0).recv().unwrap().snapshot.unwrap();
    assert!(snapshot.project_configs.is_empty());
}

#[test]
fn a_project_added_with_an_orbit_toml_takes_its_configuration() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(
        folder.path().join("orbit.toml"),
        "icon = \"🧪\"\n[scripts]\nsetup = \"echo ready\"\n[[targets]]\nname = \"Tests\"\ncommand = \"cargo test\"\n",
    )
    .unwrap();
    let mut h = Harness::new();
    let project = add_project(&h, folder.path());
    let event = wait_event(&mut h, 10, "the imported configuration", |e| matches!(e, Event::ProjectConfigChanged { .. }));
    let Event::ProjectConfigChanged { project: id, config } = event else { unreachable!() };
    assert_eq!(id, project);
    assert_eq!(config.icon, "🧪");
    assert_eq!(config.targets[0].kind, TargetKind::Test);
    assert_eq!(config.imported_from, vec!["orbit.toml"]);
    let Ok(WorkspaceReply::Imported { project: imported, sources, .. }) = workspace(&h, WorkspaceCommand::Import { project })
    else {
        panic!()
    };
    assert_eq!(imported.scripts.setup.as_deref(), Some("echo ready"));
    assert_eq!(sources, vec!["orbit.toml"]);
}

#[test]
fn files_are_read_and_listed_inside_the_workspace_only() {
    let outer = tempfile::tempdir().unwrap();
    let folder = outer.path().join("app");
    std::fs::create_dir_all(folder.join("src")).unwrap();
    std::fs::write(folder.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(outer.path().join("secret.txt"), "no").unwrap();
    let h = Harness::new();
    let project = add_project(&h, &folder);
    let root = WorkspaceRoot::Project(project);
    let Ok(WorkspaceReply::File(file)) = workspace(&h, WorkspaceCommand::ReadFile { root, path: "src/main.rs".into(), max_bytes: 0 })
    else {
        panic!()
    };
    assert_eq!(file.body, FileBody::Text { text: "fn main() {}\n".into(), truncated: false });
    let refused = workspace(&h, WorkspaceCommand::ReadFile { root, path: "../secret.txt".into(), max_bytes: 0 }).unwrap_err();
    assert!(refused.contains("outside"), "{refused}");
    let refused = workspace(&h, WorkspaceCommand::ListDir { root, path: "src/../..".into() }).unwrap_err();
    assert!(refused.contains("outside"), "{refused}");
    let Ok(WorkspaceReply::Dir(entries)) = workspace(&h, WorkspaceCommand::ListDir { root, path: String::new() }) else { panic!() };
    assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["src"]);
}

#[test]
fn project_rows_get_live_git_status_without_polling() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let mut h = Harness::new();
    let project = add_project(&h, repo.path());
    let root = WorkspaceRoot::Project(project);
    let event = wait_event(&mut h, 10, "the first status", |e| matches!(e, Event::GitStatus { root: r, .. } if *r == root));
    let Event::GitStatus { status, .. } = event else { unreachable!() };
    assert!(status.repo && status.is_clean());
    assert_eq!(status.branch.as_deref(), Some("main"));

    std::fs::write(repo.path().join("new.txt"), "a\nb\n").unwrap();
    let event = wait_event(&mut h, 10, "the change", |e| {
        matches!(e, Event::GitStatus { root: r, status } if *r == root && status.changed == 1)
    });
    let Event::GitStatus { status, .. } = event else { unreachable!() };
    assert_eq!(status.additions, 2);
    let Ok(WorkspaceReply::Status(now)) = workspace(&h, WorkspaceCommand::Status { root }) else { panic!() };
    assert_eq!(now.changed, 1);

    // Archived projects are not watched.
    let config = ProjectConfig { archived: true, ..Default::default() };
    h.call(Command::Workspace(WorkspaceCommand::SetConfig { project, config: Box::new(config) }));
    std::fs::write(repo.path().join("more.txt"), "c\n").unwrap();
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        match h.events.try_recv() {
            Ok(env) => assert!(!matches!(env.event, Event::GitStatus { root: r, .. } if r == root), "archived projects are not watched"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[test]
fn run_targets_start_report_their_exit_and_stop() {
    let folder = tempfile::tempdir().unwrap();
    let mut h = Harness::new();
    let project = add_project(&h, folder.path());
    let quick = "exit 3";
    let slow = if cfg!(windows) { "ping -n 30 127.0.0.1" } else { "sleep 30" };
    let config = ProjectConfig {
        env: vec![EnvVar::new("ORBIT_TEST_VAR", "from-the-project")],
        targets: vec![
            ProjectTarget { name: "Quick".into(), command: quick.into(), kind: TargetKind::Test, url: String::new() },
            ProjectTarget { name: "Server".into(), command: slow.into(), kind: TargetKind::Dev, url: "http://localhost:$PORT".into() },
        ],
        ..Default::default()
    };
    h.call(Command::Workspace(WorkspaceCommand::SetConfig { project, config: Box::new(config) }));
    let Reply::Thread(thread) = h.call(Command::CreateThread { project: Some(project), config: ThreadConfig::default(), worktree: false })
    else {
        panic!()
    };

    let Ok(WorkspaceReply::Run(run)) = workspace(&h, WorkspaceCommand::Run { thread, target: "Quick".into(), cols: 80, rows: 24 }) else {
        panic!()
    };
    assert_eq!(run.status, TargetStatus::Running);
    let term = run.term.unwrap();
    // ConPTY waits for a cursor-position reply; the engine answers it.
    let event = wait_event(&mut h, 20, "the target to exit", |e| {
        matches!(e, Event::RunChanged(r) if r.target == "Quick" && !r.status.is_running())
    });
    let Event::RunChanged(run) = event else { unreachable!() };
    assert_eq!(run.status, TargetStatus::Exited { code: Some(3) });
    assert!(run.status.is_failure());
    let Reply::TerminalTail { info, .. } = h.call(Command::ReadTerminal { term, max_bytes: 0 }) else { panic!() };
    assert_eq!(info.command, quick);

    // Without a worktree there is no port, so no address either.
    let Ok(WorkspaceReply::Run(run)) = workspace(&h, WorkspaceCommand::Run { thread, target: "Server".into(), cols: 80, rows: 24 }) else {
        panic!()
    };
    assert_eq!(run.url, None);
    let first = run.term;
    // Running again restarts it.
    let Ok(WorkspaceReply::Run(again)) = workspace(&h, WorkspaceCommand::Run { thread, target: "Server".into(), cols: 80, rows: 24 })
    else {
        panic!()
    };
    assert_ne!(again.term, first);
    let snapshot = h.engine.subscribe(0, 0).recv().unwrap().snapshot.unwrap();
    assert!(snapshot.runs.iter().any(|r| r.target == "Server" && r.status.is_running()));
    h.call(Command::Workspace(WorkspaceCommand::Stop { thread, target: "Server".into() }));
    wait_event(&mut h, 5, "the stop", |e| matches!(e, Event::RunChanged(r) if r.target == "Server" && r.status == TargetStatus::Stopped));
    assert!(workspace(&h, WorkspaceCommand::Stop { thread, target: "Server".into() }).unwrap_err().contains("not running"));
    assert!(workspace(&h, WorkspaceCommand::Run { thread, target: "Nope".into(), cols: 0, rows: 0 }).unwrap_err().contains("no target"));
}
