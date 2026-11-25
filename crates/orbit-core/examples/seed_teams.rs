//! Writes a data folder and real git repositories for checking agent teams
//! and races by eye:
//!
//! `cargo run -p orbit-core --example seed_teams -- <data dir> <work dir> [--light]`
//!
//! `<work dir>/shop` is a project on `main` with a `test` run script. Team
//! "Add user accounts" has an architect in its own worktree and eight
//! workers in every state (working, waiting on a dependency, a free slot and
//! a teammate's files, done, merged, failed), one of them a sub-worker, two
//! of them editing the same file for real, and a notes board. A second team
//! has finished. Race "Apply discounts before tax" has three finished
//! candidates with different real changes to `src/cart.rs`, test results and
//! a judge's verdict; a second race still runs.
//!
//! Nothing here starts an agent: the threads are idle with no mail waiting,
//! so the engine only reads them. Start Orbit with `ORBIT_DATA=<data dir>`,
//! `ORBIT_DEBUG_DEMO=teams` (to draw the working workers as working) and
//! `ORBIT_DEBUG_ROUTE=teams|team|compare|galaxy`.

use std::path::{Path, PathBuf};
use std::process::Command;

use orbit_core::store::Store;
use orbit_core::teams::TeamFiles;
use orbit_proto::*;

fn sh(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).status().expect("git runs").success();
    assert!(ok, "git {args:?}");
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn item(turn: u32, body: ItemBody) -> Item {
    Item { id: ItemId::new(), turn, status: ItemStatus::Done, body }
}

fn user(text: &str) -> ItemBody {
    ItemBody::User(UserMessage { text: text.into(), attachments: vec![], steer: false })
}

fn reply(text: &str) -> ItemBody {
    ItemBody::Assistant { text: text.into() }
}

const CART: &str = "//! The shopping cart.

pub struct Line {
    pub price_cents: u64,
    pub quantity: u32,
}

pub struct Cart {
    pub lines: Vec<Line>,
    pub tax_rate: f64,
}

impl Cart {
    pub fn subtotal(&self) -> u64 {
        self.lines.iter().map(|l| l.price_cents * l.quantity as u64).sum()
    }

    pub fn total(&self) -> u64 {
        let subtotal = self.subtotal();
        subtotal + (subtotal as f64 * self.tax_rate).round() as u64
    }
}
";

const USERS: &str = "//! Users.

pub struct User {
    pub id: u64,
    pub name: String,
}

pub fn find(id: u64) -> Option<User> {
    (id == 1).then(|| User { id, name: \"Ada\".into() })
}
";

fn usage(input: u64, output: u64, cost_micros: u64) -> Usage {
    Usage { input_tokens: input, output_tokens: output, cost_micros, context_tokens: input / 2, context_window: 200_000, ..Default::default() }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().expect("usage: seed_teams <data dir> <work dir>"));
    let work = PathBuf::from(args.next().expect("usage: seed_teams <data dir> <work dir>"));
    let light = args.any(|a| a == "--light");
    let repo = work.join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["config", "user.name", "Orbit Demo"]);
    sh(&repo, &["config", "user.email", "demo@example.com"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, "src/cart.rs", CART);
    write(&repo, "src/api/users.rs", USERS);
    write(&repo, "src/api/session.rs", "//! Sessions.\n\npub struct Session {\n    pub user: u64,\n}\n");
    write(&repo, "src/lib.rs", "pub mod api;\npub mod cart;\n");
    write(&repo, "src/api/mod.rs", "pub mod session;\npub mod users;\n");
    write(&repo, "web/index.html", "<h1>Shop</h1>\n");
    write(&repo, "README.md", "# shop\n\nA small shop.\n");
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "Initial shop"]);

    let wt = work.join("wt");
    let add_worktree = |name: &str, branch: &str, base: &str| -> PathBuf {
        let path = wt.join(name);
        sh(&repo, &["worktree", "add", "-q", "-b", branch, path.to_str().unwrap(), base]);
        path
    };

    let mut store = Store::open(&data).expect("open store");
    let now = now_ms();
    let min = 60_000;
    let project = Project {
        id: ProjectId::new(),
        name: "shop".into(),
        path: repo.display().to_string(),
        order: 1,
        scripts: ProjectScripts { run: vec![RunScript { name: "test".into(), command: "cargo test".into() }], ..Default::default() },
        base_branch: Some("main".into()),
    };
    store.commit(Event::ProjectUpserted(Box::new(project.clone()))).unwrap();
    if light {
        let mut settings = store.settings().clone();
        settings.appearance.mode = ColorMode::Light;
        store.commit(Event::SettingsChanged(Box::new(settings))).unwrap();
    }
    let opus = ThreadConfig { provider: ProviderKind::Claude, model: "opus".into(), access: Access::AutoEdit, ..Default::default() };
    let sonnet = ThreadConfig { model: "sonnet".into(), ..opus.clone() };
    let codex = ThreadConfig { provider: ProviderKind::Codex, model: "gpt-5-codex".into(), access: Access::AutoEdit, ..Default::default() };
    let mut thread = |title: &str, config: &ThreadConfig, parent: Option<ThreadId>, wt: Option<(&Path, &str)>, turns: u32, status: ThreadStatus, u: Usage, ago: u64, items: Vec<Item>| {
        let t = Thread {
            id: ThreadId::new(),
            project: Some(project.id),
            title: title.into(),
            config: config.clone(),
            parent,
            turns,
            status,
            usage: u,
            created_ms: now - ago - 20 * min,
            updated_ms: now - ago,
            workspace: wt.map(|(p, b)| Workspace { branch: Some(b.into()), worktree: Some(p.display().to_string()), port_base: None }).unwrap_or_default(),
            ..Default::default()
        };
        let id = t.id;
        store.commit(Event::ThreadUpserted(Box::new(t))).unwrap();
        for item in items {
            store.commit(Event::ItemUpserted { thread: id, item: Box::new(item) }).unwrap();
        }
        id
    };

    // ── Team 1: in flight ────────────────────────────────────────────────
    let arch_wt = add_worktree("accounts", "orbit/accounts", "main");
    write(&arch_wt, "src/api/accounts.rs", "//! Accounts: shared types the workers build on.\n\npub struct Account {\n    pub user: u64,\n    pub email: String,\n}\n");
    sh(&arch_wt, &["add", "-A"]);
    sh(&arch_wt, &["commit", "-qm", "Account types for the team"]);
    let architect = thread(
        "Add user accounts across the API and web app",
        &opus,
        None,
        Some((&arch_wt, "orbit/accounts")),
        2,
        ThreadStatus::Idle,
        usage(182_000, 21_400, 1_840_000),
        2 * min,
        vec![
            item(1, user("Add user accounts: sign-up, sessions, password reset, and an audit log. Split it across a team.")),
            item(1, reply("Plan: the account types are committed on `orbit/accounts`. I spawned eight workers: the users API, the sign-up page (with form validation as its sub-task), password-reset mail, session middleware, rate limiting, end-to-end tests, an audit log and docs. I'll merge each as it reports.")),
            item(2, user("<orbit-team-mail count=\"1\">\n<mail id=\"1\" from=\"w1\" kind=\"done\">\nw1 finished “Users API”.\n</mail>\n</orbit-team-mail>")),
            item(2, reply("Merged w1. The sign-up page can use `GET /users/:id` now; I posted its shape to the notes.")),
        ],
    );
    let worker_wt = |name: &str| add_worktree(name, &format!("orbit/{name}"), "orbit/accounts");
    let (signup_wt, mail_wt, rate_wt, form_wt) = (worker_wt("signup"), worker_wt("reset-mail"), worker_wt("rate-limit"), worker_wt("form-validation"));
    // Real changes: sign-up and rate limiting both touch the users API.
    write(&signup_wt, "web/signup.html", "<form id=\"signup\">\n  <input name=\"email\">\n  <input name=\"password\" type=\"password\">\n</form>\n");
    write(&signup_wt, "src/api/users.rs", &USERS.replace("    pub name: String,\n", "    pub name: String,\n    pub email: String,\n"));
    write(&rate_wt, "src/api/users.rs", &USERS.replace("pub fn find(", "#[rate_limited(per_minute = 60)]\npub fn find("));
    write(&rate_wt, "src/api/limits.rs", "//! Rate limits.\n\npub const PER_MINUTE: u32 = 60;\n");
    write(&mail_wt, "src/mail/reset.rs", "//! Password reset mail.\n\npub fn reset_link(token: &str) -> String {\n    format!(\"https://shop.example/reset/{token}\")\n}\n");
    write(&form_wt, "web/validate.js", "export function validEmail(s) {\n  return /.+@.+/.test(s);\n}\n");

    let brief = |title: &str, text: &str| vec![item(1, user(&format!("# {title}\n\n{text}")))];
    let w_users = thread("Users API", &sonnet, Some(architect), None, 1, ThreadStatus::Idle, usage(64_000, 7_100, 420_000), 18 * min, {
        let mut v = brief("Users API (w1)", "Add GET /users/:id returning {id, name, email}.");
        v.push(item(1, reply("Added `GET /users/:id` with email, and tests.")));
        v
    });
    let w_signup = thread("Sign-up page", &sonnet, Some(architect), Some((&signup_wt, "orbit/signup")), 0, ThreadStatus::Idle, usage(41_000, 3_900, 260_000), min / 2, brief("Sign-up page (w2)", "Build web/signup.html posting to /users."));
    let w_mail = thread("Password reset mail", &codex, Some(architect), Some((&mail_wt, "orbit/reset-mail")), 0, ThreadStatus::Idle, usage(28_000, 2_200, 0), min, brief("Password reset mail (w3)", "Send a reset link by mail."));
    let w_rate = thread("Rate limiting", &sonnet, Some(architect), Some((&rate_wt, "orbit/rate-limit")), 0, ThreadStatus::Idle, usage(19_000, 1_500, 120_000), min / 3, brief("Rate limiting (w5)", "Limit the users API to 60 requests a minute."));
    let w_form = thread("Form validation", &sonnet, Some(w_signup), Some((&form_wt, "orbit/form-validation")), 0, ThreadStatus::Idle, usage(9_000, 800, 60_000), min / 4, brief("Form validation (w6)", "Validate the sign-up form."));
    let w_audit = thread(
        "Audit log",
        &codex,
        Some(architect),
        None,
        1,
        ThreadStatus::Failed { message: "The model is overloaded.".into() },
        usage(12_000, 400, 0),
        6 * min,
        {
            let mut v = brief("Audit log (w7)", "Record sign-ins and password changes.");
            v.push(item(1, ItemBody::Notice { level: NoticeLevel::Error, text: "The model is overloaded.".into() }));
            v
        },
    );

    let worker = |key: &str, title: &str, parent: ThreadId, thread: Option<ThreadId>, config: &ThreadConfig, state: WorkerState, started: Option<u64>, finished: Option<u64>, scope: &[&str], deps: &[&str], summary: &str| Worker {
        key: key.into(),
        title: title.into(),
        brief: format!("{title}."),
        parent,
        thread,
        config: config.clone(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        scope: scope.iter().map(|s| s.to_string()).collect(),
        state,
        summary: summary.into(),
        merge: None,
        created_ms: now - 21 * min,
        started_ms: started,
        finished_ms: finished,
    };
    let team = Team {
        id: TeamId::new(),
        architect,
        project: Some(project.id),
        limits: TeamLimits { max_workers: 4, ..Default::default() },
        worker_config: Some(sonnet.clone()),
        workers: vec![
            worker("w1", "Users API", architect, Some(w_users), &sonnet, WorkerState::Merged, Some(now - 20 * min), Some(now - 18 * min), &["src/api/users.rs"], &[], "Added `GET /users/:id` with email, and tests."),
            worker("w2", "Sign-up page", architect, Some(w_signup), &sonnet, WorkerState::Working, Some(now - 17 * min), None, &["web"], &["w1"], ""),
            worker("w3", "Password reset mail", architect, Some(w_mail), &codex, WorkerState::Working, Some(now - 16 * min), None, &["src/mail"], &[], ""),
            worker("w4", "Session middleware", architect, None, &sonnet, WorkerState::Waiting(WaitReason::FileScope { path: "src/api/session.rs".into(), with: "w5".into() }), None, None, &["src/api/session.rs"], &[], ""),
            worker("w5", "Rate limiting", architect, Some(w_rate), &sonnet, WorkerState::Working, Some(now - 9 * min), None, &["src/api"], &[], ""),
            worker("w6", "Form validation", w_signup, Some(w_form), &sonnet, WorkerState::Working, Some(now - 4 * min), None, &["web/validate.js"], &[], ""),
            worker("w7", "Audit log", architect, Some(w_audit), &codex, WorkerState::Failed { message: "The model is overloaded.".into() }, Some(now - 12 * min), Some(now - 6 * min), &["src/audit"], &[], ""),
            worker("w8", "End-to-end tests", architect, None, &sonnet, WorkerState::Waiting(WaitReason::Dependency { on: vec!["w2".into(), "w3".into()] }), None, None, &["tests"], &["w2", "w3"], ""),
            worker("w9", "Docs", architect, None, &sonnet, WorkerState::Waiting(WaitReason::NoSlot { running: 4, limit: 4, global: false }), None, None, &[], &[], ""),
        ],
        notes: vec![
            Note { id: 1, author: architect, author_label: "architect".into(), text: "Account types live in src/api/accounts.rs; build on them, don’t copy them.".into(), at_ms: now - 20 * min },
            Note { id: 2, author: w_users, author_label: "w1".into(), text: "GET /users/:id returns {id, name, email}; 404 when unknown.".into(), at_ms: now - 18 * min },
            Note { id: 3, author: w_signup, author_label: "w2".into(), text: "The sign-up form posts JSON {email, password} to POST /users.".into(), at_ms: now - 7 * min },
            Note { id: 4, author: architect, author_label: "architect".into(), text: "Passwords: argon2id, never logged. Sessions expire after 30 days.".into(), at_ms: now - 3 * min },
        ],
        mail: vec![Mail {
            id: 1,
            from: Some(w_users),
            from_label: "w1".into(),
            to: architect,
            kind: MailKind::Done,
            text: "w1 finished “Users API”.".into(),
            urgent: false,
            sent_ms: now - 18 * min,
            delivered_ms: Some(now - 18 * min),
        }],
        breaker: None,
        overlaps: vec![FileOverlap { path: "src/api/users.rs".into(), threads: vec![w_signup.min(w_rate), w_signup.max(w_rate)] }],
        created_ms: now - 21 * min,
    };

    // ── Team 2: finished ─────────────────────────────────────────────────
    let arch2 = thread(
        "Move the catalog to the async database driver",
        &opus,
        None,
        None,
        3,
        ThreadStatus::Idle,
        usage(96_000, 11_000, 980_000),
        3 * 60 * min,
        vec![item(1, user("Move the catalog to the async driver.")), item(1, reply("Done: three workers, all merged."))],
    );
    let mut done_worker = |key: &str, title: &str| {
        let id = thread(title, &sonnet, Some(arch2), None, 1, ThreadStatus::Idle, usage(20_000, 2_000, 150_000), 3 * 60 * min, vec![item(1, user(title)), item(1, reply("Done."))]);
        worker(key, title, arch2, Some(id), &sonnet, WorkerState::Merged, Some(now - 4 * 60 * min), Some(now - 3 * 60 * min - 10 * min), &[], &[], "Done.")
    };
    let team2 = Team {
        id: TeamId::new(),
        architect: arch2,
        project: Some(project.id),
        workers: vec![done_worker("w1", "Connection pool"), done_worker("w2", "Catalog queries"), done_worker("w3", "Migrations")],
        created_ms: now - 4 * 60 * min,
        ..Default::default()
    };

    // ── Race 1: settled, judged ──────────────────────────────────────────
    let prompt = "Apply discounts before tax in Cart::total";
    let race_id = RaceId::new();
    let variants = [
        ("A", &opus, CART.replace("        let subtotal = self.subtotal();\n", "        let subtotal = self.subtotal().saturating_sub(self.discount_cents);\n").replace("    pub tax_rate: f64,\n", "    pub tax_rate: f64,\n    pub discount_cents: u64,\n"), TestState::Passed { ms: 14_200 }, 252_000, usage(58_000, 6_400, 540_000)),
        ("B", &codex, CART.replace("        let subtotal = self.subtotal();\n", "        let subtotal = self.discounted_subtotal();\n").replace("    pub fn total(&self) -> u64 {", "    /// The subtotal after the discount, never below zero.\n    pub fn discounted_subtotal(&self) -> u64 {\n        self.subtotal().saturating_sub(self.discount_cents)\n    }\n\n    pub fn total(&self) -> u64 {").replace("    pub tax_rate: f64,\n", "    pub tax_rate: f64,\n    pub discount_cents: u64,\n"), TestState::Passed { ms: 13_800 }, 198_000, usage(41_000, 5_100, 0)),
        ("C", &sonnet, CART.replace("        subtotal + (subtotal as f64 * self.tax_rate).round() as u64\n", "        let taxed = subtotal + (subtotal as f64 * self.tax_rate).round() as u64;\n        taxed - self.discount_cents\n").replace("    pub tax_rate: f64,\n", "    pub tax_rate: f64,\n    pub discount_cents: u64,\n"), TestState::Failed { code: Some(101), ms: 9_400, tail: "test cart::discount_applies_before_tax ... FAILED\nassertion `left == right` failed\n  left: 1070\n right: 1065".into() }, 141_000, usage(33_000, 3_800, 210_000)),
    ];
    let mut candidates = Vec::new();
    let mut winner = ThreadId(0);
    for (label, config, cart, test, ms, u) in variants {
        let path = add_worktree(&format!("race-{}", label.to_lowercase()), &format!("orbit/race-{}", label.to_lowercase()), "main");
        write(&path, "src/cart.rs", &cart);
        let lines = |s: &str| s.lines().count() as u32;
        let added = lines(&cart).saturating_sub(lines(CART)) + 1;
        let id = thread(
            &format!("{prompt} ({label})"),
            config,
            None,
            Some((&path, &format!("orbit/race-{}", label.to_lowercase()))),
            1,
            ThreadStatus::Idle,
            u,
            30 * min,
            vec![item(1, user(prompt)), item(1, reply(&format!("Candidate {label}: the discount now applies before tax.")))],
        );
        if label == "B" {
            winner = id;
        }
        candidates.push(Candidate {
            thread: id,
            label: label.into(),
            config: config.clone(),
            finished: Some(CandidateEnd::Completed),
            duration_ms: ms,
            usage: u,
            files: vec![FileChange { path: "src/cart.rs".into(), kind: ChangeKind::Modified, additions: added, deletions: 1 }],
            test,
        });
    }
    let race = Race {
        id: race_id,
        project: project.id,
        prompt: prompt.into(),
        base: "main".into(),
        test_command: Some("cargo test".into()),
        candidates,
        judge: JudgeState::Verdict {
            winner,
            reason: "B and A both pass; B keeps the discount in one named method, which reads better and is reused by the receipt.".into(),
        },
        state: RaceState::Running,
        created_ms: now - 35 * min,
    };

    // ── Race 2: still running ────────────────────────────────────────────
    let prompt2 = "Cache product images on disk";
    let mut running = Vec::new();
    for (label, config) in [("A", &opus), ("B", &codex)] {
        let id = thread(&format!("{prompt2} ({label})"), config, None, None, 0, ThreadStatus::Idle, usage(8_000, 700, 40_000), min, vec![item(1, user(prompt2))]);
        running.push(Candidate { thread: id, label: label.into(), config: config.clone(), test: TestState::Pending, ..Default::default() });
    }
    let race2 = Race {
        id: RaceId::new(),
        project: project.id,
        prompt: prompt2.into(),
        base: "main".into(),
        test_command: Some("cargo test".into()),
        candidates: running,
        state: RaceState::Running,
        created_ms: now - 2 * min,
        ..Default::default()
    };

    store.sync().unwrap();
    let files = TeamFiles::new(&data);
    files.save_team(&team).unwrap();
    files.save_team(&team2).unwrap();
    files.save_race(&race).unwrap();
    files.save_race(&race2).unwrap();
    println!("seeded {} and {}", data.display(), work.display());
}
