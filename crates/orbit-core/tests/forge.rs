//! The forge through the engine, against a fake `gh` that answers from
//! fixtures and records every call. Nothing here touches the network: the
//! repository's GitHub remote is rewritten (`insteadOf`) to a local bare
//! repository, so pull request checkouts fetch real refs from disk.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use orbit_core::engine::Launcher;
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;

const FAKE: &str = env!("CARGO_BIN_EXE_orbit-fake-forge");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/forge");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Process::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Prompts the fake agent received, by thread.
type Prompts = Arc<Mutex<Vec<(ThreadId, String)>>>;

fn launcher(prompts: Prompts) -> Launcher {
    Box::new(move |_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let (thread, prompts) = (spec.thread, prompts.clone());
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: "s".into(), model: None });
            while let Ok(msg) = rx.recv().await {
                match msg {
                    SessionInput::Prompt { text, .. } => {
                        prompts.lock().unwrap().push((thread, text));
                        emit(AgentEvent::TurnStarted { prompt: None });
                        emit(AgentEvent::BlockDone { block: "b".into(), kind: BlockKind::Text, text: "on it".into() });
                        emit(AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, usage: Usage::default(), duration_ms: 1 });
                    }
                    SessionInput::Shutdown => return,
                    _ => {}
                }
            }
        });
        SessionHandle { input }
    })
}

struct World {
    _tmp: tempfile::TempDir,
    engine: EngineHandle,
    events: orbit_core::engine::EventRx,
    threads: std::collections::HashMap<ThreadId, Thread>,
    prompts: Prompts,
    bin: PathBuf,
    work: PathBuf,
    project: ProjectId,
}

const PR_VIEW: &str = r#"{"number":7,"title":"Make search fast","author":{"login":"alice"},"state":"OPEN","isDraft":false,
 "headRefName":"feature","baseRefName":"main","url":"https://github.com/acme/widgets/pull/7",
 "createdAt":"2026-11-20T10:00:00Z","updatedAt":"2026-11-21T10:00:00Z","reviewDecision":"APPROVED",
 "additions":10,"deletions":2,"labels":[],"isCrossRepository":false,"body":"Speeds up **search**.",
 "headRefOid":"HEADSHA","mergeable":"MERGEABLE","mergeStateStatus":"UNSTABLE","autoMergeRequest":null,"changedFiles":3,
 "reviews":[{"author":{"login":"bob"},"state":"APPROVED","body":"Nice","submittedAt":"2026-11-21T09:00:00Z"}],
 "comments":[{"author":{"login":"carol"},"body":"Ship it","createdAt":"2026-11-21T09:30:00Z"}],
 "statusCheckRollup":[
  {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","workflowName":"CI",
   "detailsUrl":"https://github.com/acme/widgets/actions/runs/1/job/21","startedAt":"2026-11-21T10:00:00Z","completedAt":"2026-11-21T10:01:00Z"},
  {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE","workflowName":"CI",
   "detailsUrl":"https://github.com/acme/widgets/actions/runs/1/job/22","startedAt":"2026-11-21T10:00:00Z","completedAt":"2026-11-21T10:03:00Z"}]}"#;

const PR_LIST: &str = r#"[{"number":7,"title":"Make search fast","author":{"login":"alice"},"state":"OPEN","isDraft":false,
 "headRefName":"feature","baseRefName":"main","url":"https://github.com/acme/widgets/pull/7","updatedAt":"2026-11-21T10:00:00Z",
 "statusCheckRollup":[{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE"}],
 "reviewDecision":"APPROVED","additions":10,"deletions":2,"labels":[{"name":"perf"}],"isCrossRepository":false},
 {"number":8,"title":"Docs","author":{"login":"bob"},"state":"OPEN","isDraft":true,"headRefName":"docs","baseRefName":"main",
 "url":"https://github.com/acme/widgets/pull/8","updatedAt":"2026-11-20T10:00:00Z","statusCheckRollup":[],"reviewDecision":"",
 "additions":1,"deletions":0,"labels":[],"isCrossRepository":true}]"#;

const LOG: &str = "test\tRun tests\t2026-11-21T10:02:00.0000000Z running 12 tests\n\
test\tRun tests\t2026-11-21T10:02:01.0000000Z thread 'search::finds_prefix' panicked: assertion failed\n\
test\tRun tests\t2026-11-21T10:02:02.0000000Z ##[error]Process completed with exit code 101.\n";

impl World {
    /// A repository on "github.com/acme/widgets" (really a local bare repo)
    /// with `main` and a `feature` branch whose PR is #7, and a fake `gh`
    /// answering `rules`.
    fn new(rules: &str) -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let bare = root.join("remote.git");
        let work = root.join("widgets");
        std::fs::create_dir_all(&work).unwrap();
        git(root, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "T"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("README.md"), "widgets\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", "https://github.com/acme/widgets.git"]);
        let bare_url = format!("file:///{}", bare.display().to_string().replace('\\', "/").trim_start_matches('/'));
        git(&work, &["config", &format!("url.{bare_url}.insteadOf"), "https://github.com/acme/widgets.git"]);
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(work.join("search.rs"), "fn search() {}\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "Make search fast"]);
        git(&work, &["push", "-q", "origin", "feature", "feature:refs/pull/7/head"]);
        git(&work, &["checkout", "-q", "main"]);
        git(&work, &["branch", "-q", "-D", "feature"]);

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join(if cfg!(windows) { "gh.exe" } else { "gh" });
        std::fs::copy(FAKE, &gh).unwrap();
        std::fs::write(bin.join("fake-forge.json"), rules).unwrap();
        std::fs::write(bin.join("pr_view_7.json"), PR_VIEW).unwrap();
        std::fs::write(bin.join("pr_list.json"), PR_LIST).unwrap();
        std::fs::write(bin.join("log_22.txt"), LOG).unwrap();
        std::fs::copy(Path::new(FIXTURES).join("gh_issue_list.json"), bin.join("issue_list.json")).unwrap();

        let prompts: Prompts = Arc::default();
        let data = root.join("data");
        let engine = EngineHandle::start_with(EngineOptions { data_dir: data, probe_providers: false }, launcher(prompts.clone())).unwrap();
        let sub = engine.subscribe(0, 0).recv().unwrap();
        let mut settings = *sub.snapshot.unwrap().settings;
        settings.advanced.gh_binary = gh.display().to_string();
        settings.workspace.worktree_root = root.join("worktrees").display().to_string();
        call(&engine, Command::UpdateSettings { settings: Box::new(settings) });
        let Reply::Project(project) = call(&engine, Command::AddProject { path: work.display().to_string() }) else { panic!() };
        World { _tmp: tmp, engine, events: sub.events, threads: Default::default(), prompts, bin, work, project }
    }

    fn forge(&self, command: ForgeCommand) -> Result<Reply, String> {
        self.engine.call(Command::Forge(command)).recv().unwrap()
    }

    /// Command lines the fake `gh` saw.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.bin.join("fake-forge.calls"))
            .unwrap_or_default()
            .lines()
            .map(|l| sonic_rs::from_str(l).unwrap())
            .collect()
    }

    fn count(&self, words: &[&str]) -> usize {
        self.calls().iter().filter(|c| words.iter().all(|w| c.iter().any(|a| a == w))).count()
    }

    fn wait(&mut self, what: &str, done: impl Fn(&World) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            match self.events.try_recv() {
                Ok(env) => {
                    if let Event::ThreadUpserted(t) = env.event {
                        self.threads.insert(t.id, t);
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

fn call(engine: &EngineHandle, command: Command) -> Reply {
    engine.call(command).recv().unwrap().unwrap()
}

const RULES: &str = r#"{"rules":[
  {"args":["auth","status"],"stdout":"github.com\n  ✓ Logged in to github.com account octo (keyring)\n  - Active account: true\n"},
  {"args":["pr","view","7"],"stdout_file":"pr_view_7.json"},
  {"args":["pr","view","9"],"stderr":"GraphQL: Could not resolve to a PullRequest with the number of 9. (repository.pullRequest)","exit":1},
  {"args":["pr","list","--head","feature"],"stdout":"[]"},
  {"args":["pr","list"],"stdout_file":"pr_list.json"},
  {"args":["repo","view"],"stdout":"{\"mergeCommitAllowed\":false,\"squashMergeAllowed\":true,\"rebaseMergeAllowed\":true}"},
  {"args":["pr","merge","7"],"stdout":""},
  {"args":["api","DELETE"],"stdout":""},
  {"args":["run","view","--job","22","--log-failed"],"stdout_file":"log_22.txt"},
  {"args":["issue","list"],"stdout_file":"issue_list.json"}
]}"#;

#[test]
fn info_lists_and_details_come_from_gh_and_are_cached() {
    let w = World::new(RULES);
    let Ok(Reply::Forge(ForgeReply::Info(info))) = w.forge(ForgeCommand::Info { project: w.project }) else { panic!() };
    let info = *info;
    let repo = info.repo.expect("a GitHub repo");
    assert_eq!((repo.kind, repo.path.as_str()), (ForgeKind::GitHub, "acme/widgets"));
    assert_eq!(info.auth, ForgeAuth::Ready { account: "octo".into() });
    assert_eq!(info.branch.as_deref(), Some("main"));

    let list = |refresh| match w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh }) {
        Ok(Reply::Forge(ForgeReply::PullRequests(list))) => list,
        other => panic!("{other:?}"),
    };
    let first = list(false);
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.items[0].checks.overall(), Some(CheckState::Failed));
    assert_eq!(first.items[1].state, PrState::Draft);
    assert!(first.stale.is_none());
    // Served from the cache, even when a refresh is asked for seconds later.
    list(false);
    list(true);
    assert_eq!(w.count(&["pr", "list"]), 1, "{:?}", w.calls());
    let args = w.calls().into_iter().find(|c| c.contains(&"list".to_owned())).unwrap();
    assert!(args.windows(2).any(|p| p == ["--repo", "acme/widgets"]), "{args:?}");

    let Ok(Reply::Forge(ForgeReply::PullRequest(detail))) =
        w.forge(ForgeCommand::PullRequest { project: w.project, number: 7, refresh: false })
    else {
        panic!()
    };
    assert_eq!(detail.mergeable, Mergeability::Unstable);
    assert_eq!(detail.merge_methods, vec![MergeMethod::Squash, MergeMethod::Rebase]);
    assert_eq!(detail.checks[0].name, "test");
    assert_eq!(detail.checks[0].state, CheckState::Failed);
    assert_eq!(detail.checks[0].finished_ms - detail.checks[0].started_ms, 180_000);
    assert_eq!(detail.reviews[0].state, ReviewState::Approved);
    assert_eq!(detail.comments[0].body, "Ship it");

    let Ok(Reply::Forge(ForgeReply::Issues(issues))) =
        w.forge(ForgeCommand::Issues { project: Some(w.project), tracker: TrackerKind::GitHub, query: String::new(), refresh: false })
    else {
        panic!()
    };
    assert_eq!(issues.items.len(), 2);
}

#[test]
fn merging_pins_the_head_and_never_touches_the_local_branch() {
    let w = World::new(RULES);
    let merge = ForgeCommand::Merge {
        project: w.project,
        number: 7,
        method: MergeMethod::Squash,
        auto: false,
        delete_branch: true,
        head_sha: "HEADSHA".into(),
    };
    assert_eq!(w.forge(merge), Ok(Reply::Done));
    let calls = w.calls();
    let merge = calls.iter().find(|c| c.starts_with(&["pr".into(), "merge".into()])).unwrap();
    assert!(merge.contains(&"--squash".to_owned()));
    assert!(merge.windows(2).any(|p| p == ["--match-head-commit", "HEADSHA"]), "{merge:?}");
    assert!(!merge.contains(&"--delete-branch".to_owned()), "gh would switch the local checkout");
    let delete = calls.iter().find(|c| c.contains(&"DELETE".to_owned())).expect("the remote branch is deleted");
    assert!(delete.contains(&"repos/acme/widgets/git/refs/heads/feature".to_owned()), "{delete:?}");

    let auto = ForgeCommand::Merge { project: w.project, number: 7, method: MergeMethod::Rebase, auto: true, delete_branch: false, head_sha: String::new() };
    assert_eq!(w.forge(auto), Ok(Reply::Done));
    assert!(w.calls().iter().any(|c| c.contains(&"--auto".to_owned()) && c.contains(&"--rebase".to_owned())));
    // A merge drops the cached list.
    w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh: false }).unwrap();
    w.forge(ForgeCommand::Merge { project: w.project, number: 7, method: MergeMethod::Squash, auto: false, delete_branch: false, head_sha: String::new() })
        .unwrap();
    w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh: false }).unwrap();
    assert_eq!(w.count(&["pr", "list"]) - w.count(&["--head"]), 2);
}

#[test]
fn failures_say_what_happened_and_how_to_fix_it() {
    let w = World::new(RULES);
    let err = w.forge(ForgeCommand::PullRequest { project: w.project, number: 9, refresh: false }).unwrap_err();
    assert!(err.contains("Could not resolve to a PullRequest"), "{err}");

    let signed_out = r#"{"rules":[
      {"args":["auth","status"],"stderr":"You are not logged into any GitHub hosts. To log in, run: gh auth login","exit":1},
      {"args":["pr","list"],"stderr":"To get started with GitHub CLI, please run:  gh auth login","exit":4}
    ]}"#;
    let w = World::new(signed_out);
    let Ok(Reply::Forge(ForgeReply::Info(info))) = w.forge(ForgeCommand::Info { project: w.project }) else { panic!() };
    let info = *info;
    assert_eq!(info.auth, ForgeAuth::SignedOut { program: "gh".into(), fix: "gh auth login --hostname github.com".into() });
    let err = w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh: false }).unwrap_err();
    assert!(err.contains("gh auth login --hostname github.com"), "{err}");

    let limited = r#"{"rules":[{"args":["pr","list"],"stderr":"GraphQL: API rate limit exceeded for user ID 1.","exit":1}]}"#;
    let w = World::new(limited);
    let err = w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh: false }).unwrap_err();
    assert!(err.contains("rate limit"), "{err}");
    // While limited, Orbit doesn't ask again.
    let _ = w.forge(ForgeCommand::PullRequests { project: w.project, filter: PrFilter::Open, refresh: true });
    assert_eq!(w.count(&["pr", "list"]), 1);
}

#[test]
fn checkout_opens_the_branch_in_a_worktree_and_fix_checks_sends_the_logs() {
    let mut w = World::new(RULES);
    let config = ThreadConfig::default();
    let Ok(Reply::Thread(thread)) = w.forge(ForgeCommand::Checkout { project: w.project, number: 7, config: config.clone() }) else {
        panic!()
    };
    w.wait("the worktree", |w| w.threads.get(&thread).is_some_and(|t| t.workspace.worktree.is_some()));
    let t = w.threads[&thread].clone();
    assert_eq!(t.workspace.branch.as_deref(), Some("feature"));
    assert_eq!(t.title, "#7 Make search fast");
    let tree = PathBuf::from(t.workspace.worktree.unwrap());
    assert!(tree.join("search.rs").exists(), "the PR's files are checked out");
    assert_eq!(git(&tree, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature");
    // Checking out again reuses the thread (T3 Code #42402).
    assert_eq!(w.forge(ForgeCommand::Checkout { project: w.project, number: 7, config: config.clone() }), Ok(Reply::Thread(thread)));

    let fix = ForgeCommand::FixChecks { project: w.project, number: 7, thread: None, config };
    assert_eq!(w.forge(fix), Ok(Reply::Thread(thread)), "the thread on the branch gets the fix");
    w.wait("the fix prompt", |w| !w.prompts.lock().unwrap().is_empty());
    let (to, text) = w.prompts.lock().unwrap()[0].clone();
    assert_eq!(to, thread);
    assert!(text.contains("pull request #7"), "{text}");
    assert!(text.contains("Failing: CI / test"), "{text}");
    assert!(text.contains("panicked: assertion failed"), "{text}");
    assert!(text.contains("error: Process completed with exit code 101."), "{text}");
    assert!(!text.contains("2026-11-21T10:02"), "timestamps are dropped");
    let _ = &w.work;
}

#[test]
fn push_sets_the_upstream_on_the_first_push_then_follows_it() {
    let w = World::new(RULES);
    let Reply::Thread(thread) =
        call(&w.engine, Command::CreateThread { project: Some(w.project), config: ThreadConfig::default(), worktree: false })
    else {
        panic!()
    };
    git(&w.work, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(w.work.join("topic.rs"), "fn topic() {}\n").unwrap();
    git(&w.work, &["add", "-A"]);
    git(&w.work, &["commit", "-q", "-m", "Topic"]);
    let bare = w.work.parent().unwrap().join("remote.git");

    // First push: no upstream yet, so it pushes to origin and sets one.
    assert_eq!(call(&w.engine, Command::Push { thread }), Reply::Text("origin/topic".into()));
    assert_eq!(git(&w.work, &["rev-parse", "--abbrev-ref", "@{upstream}"]).trim(), "origin/topic");
    let head = git(&w.work, &["rev-parse", "HEAD"]);
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/topic"]), head);

    // Later pushes follow the upstream.
    std::fs::write(w.work.join("topic.rs"), "fn topic() { /* more */ }\n").unwrap();
    git(&w.work, &["commit", "-q", "-am", "More topic"]);
    assert_eq!(call(&w.engine, Command::Push { thread }), Reply::Text("origin/topic".into()));
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/topic"]), git(&w.work, &["rev-parse", "HEAD"]));

    // Nothing to push to: the error says so.
    git(&w.work, &["checkout", "-q", "--detach"]);
    assert!(w.engine.call(Command::Push { thread }).recv().unwrap().is_err());
}
