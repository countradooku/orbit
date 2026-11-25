//! Writes a data folder with automations and their run history, for visual
//! checks and screenshots of the Automations page:
//! `cargo run -p orbit-core --example seed_automations -- <data dir> <project dir>`
//! then start Orbit with `ORBIT_DATA=<data dir> ORBIT_DEBUG_ROUTE=automations`.
//! The project folder should not be a git repository, so event triggers
//! find no forge and nothing starts by itself.

use orbit_core::automations::{recipes, schedule::Schedule};
use orbit_core::store::Store;
use orbit_proto::*;

const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: seed_automations <data dir> <project dir>");
    let project_dir = args.next().expect("usage: seed_automations <data dir> <project dir>");
    std::fs::create_dir_all(&project_dir).unwrap();
    let mut store = Store::open(&dir).expect("open store");
    let now = now_ms();
    let project = Project {
        id: ProjectId::new(),
        name: "storefront-web".into(),
        path: project_dir,
        order: 1,
        scripts: ProjectScripts::default(),
        base_branch: Some("main".into()),
    };
    store.commit(Event::ProjectUpserted(project.clone())).unwrap();
    let config = ThreadConfig { model: "claude-opus-4-5".into(), ..Default::default() };
    store.commit(Event::DefaultsChanged(config.clone())).unwrap();

    let thread = |store: &mut Store, title: &str, ago: u64| -> ThreadId {
        let t = Thread {
            id: ThreadId::new(),
            project: Some(project.id),
            title: title.into(),
            config: config.clone(),
            created_ms: now - ago,
            updated_ms: now - ago + 4 * MIN,
            turns: 1,
            usage: Usage { input_tokens: 31_000, output_tokens: 4_200, ..Default::default() },
            ..Default::default()
        };
        store.commit(Event::ThreadUpserted(t.clone())).unwrap();
        t.id
    };

    let automation = |store: &mut Store, recipe: &str, edit: &dyn Fn(&mut Automation)| -> Automation {
        let spec = recipes::find(recipe).unwrap().spec(Some(project.id), &config);
        let mut a = Automation {
            id: AutomationId::new(),
            spec,
            created_ms: now - 9 * 24 * HOUR,
            updated_ms: now - HOUR,
            ..Default::default()
        };
        edit(&mut a);
        a.state.next_ms = a
            .spec
            .triggers
            .iter()
            .map(|t| match Schedule::of(t) {
                Ok(Some(s)) => s.next_after(now).unwrap_or(0),
                _ => 0,
            })
            .collect();
        store.commit(Event::AutomationUpserted(Box::new(a.clone()))).unwrap();
        a
    };
    // Past occurrences of an automation's first schedule, oldest first.
    let past = |a: &Automation, n: usize| -> Vec<u64> {
        let schedule = Schedule::of(&a.spec.triggers[0]).unwrap().unwrap();
        let times: Vec<u64> = schedule.upcoming(now - 8 * 24 * HOUR, 40).into_iter().filter(|t| *t < now).collect();
        times[times.len().saturating_sub(n)..].to_vec()
    };
    let run = |store: &mut Store, a: &Automation, ago: u64, took: u64, status: RunStatus, reason: RunReason, thread: Option<ThreadId>, summary: &str| {
        let started = if matches!(status, RunStatus::Skipped(_)) { 0 } else { now - ago };
        let r = AutomationRun {
            id: RunId::new(),
            automation: a.id,
            reason,
            status: status.clone(),
            thread,
            due_ms: now - ago,
            started_ms: started,
            finished_ms: if status == RunStatus::Running { 0 } else { now - ago + took },
            tokens: if thread.is_some() { 18_000 + took / 40 } else { 0 },
            changed: thread.map(|_| !summary.contains("Nothing")),
            summary: summary.into(),
            ..Default::default()
        };
        store.commit(Event::AutomationRun(Box::new(r))).unwrap();
    };

    let deps = automation(&mut store, "nightly-deps", &|_| {});
    for (i, at) in past(&deps, 5).into_iter().enumerate() {
        let day = 5 - i as u64;
        let ago = now - at;
        if day == 3 {
            run(&mut store, &deps, ago, 0, RunStatus::Skipped(Skip::Missed { count: 1 }), RunReason::Schedule, None, "");
            continue;
        }
        let t = thread(&mut store, &format!("Nightly dependency updates · {}", ["Mon", "Tue", "Wed", "Thu", "Fri"][i]), ago);
        let summary = if day == 2 {
            "Nothing to update: every dependency is already on its newest patch and minor version."
        } else {
            "Updated 7 dependencies (serde 1.0.228, tokio 1.48, clap 4.6.2…). Tests pass. Left alone: axum 0.9 (major)."
        };
        run(&mut store, &deps, ago, 6 * MIN + day * 17_000, RunStatus::Succeeded, RunReason::Schedule, Some(t), summary);
    }

    let standup = automation(&mut store, "standup", &|_| {});
    let times = past(&standup, 2);
    let t = thread(&mut store, "Standup summary", now - times[1]);
    run(&mut store, &standup, now - times[0], 55_000, RunStatus::Succeeded, RunReason::Schedule, Some(t),
        "Done: checkout validation, flaky auth test fixed. In progress: search indexing. Blocked: nothing.");
    run(&mut store, &standup, now - times[1] - 7 * MIN, 48_000, RunStatus::Succeeded, RunReason::Late, Some(t),
        "Done: 3 PRs merged (#412 cart totals, #415 a11y labels, #418 deps). In progress: payments retry. Blocked: staging is down.");

    let ci = automation(&mut store, "fix-ci", &|_| {});
    let t1 = thread(&mut store, "#418 Fix the flaky cart total test", 50 * HOUR);
    run(&mut store, &ci, 50 * HOUR, 9 * MIN, RunStatus::Succeeded, RunReason::Event("Checks failed on #418".into()), Some(t1),
        "The cart total test compared floats exactly. Switched to cents, pushed the fix; checks are green.");
    let t2 = thread(&mut store, "#421 Payments retry", 40 * MIN);
    run(&mut store, &ci, 40 * MIN, 12 * MIN, RunStatus::TimedOut, RunReason::Event("Checks failed on #421".into()), Some(t2),
        "Reproduced the integration failure; it needs the sandbox credentials, which are not set in CI.");

    automation(&mut store, "triage-issues", &|a| a.spec.name = "Triage new bugs".into());

    let rebase = automation(&mut store, "keep-rebased", &|a| a.paused = true);
    run(&mut store, &rebase, 6 * 24 * HOUR, 0, RunStatus::Skipped(Skip::Dirty), RunReason::Schedule, None, "");

    store.sync().unwrap();
    println!("Seeded {} with 5 automations.", dir);
}
