//! Writes a demo data folder for visual checks and screenshots:
//! `cargo run -p orbit-core --example seed -- <data dir> [<project dir>]`
//! then start Orbit with `ORBIT_DATA=<data dir>`.

use orbit_core::store::Store;
use orbit_proto::*;

fn item(turn: u32, body: ItemBody) -> Item {
    Item { id: ItemId::new(), turn, status: ItemStatus::Done, body }
}

fn tool(kind: ToolKind, name: &str, title: &str, output: &str, files: Vec<FileChange>) -> ItemBody {
    ItemBody::Tool(ToolCall {
        call_id: ItemId::new().to_string(),
        kind,
        name: name.into(),
        title: title.into(),
        input: "{}".into(),
        output: output.into(),
        output_blob: None,
        exit_code: Some(0),
        files,
    })
}

/// An id created at `ms` (Unix), so seeded history can span days.
fn id_at(ms: u64) -> ItemId {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ItemId(((ms - 1_704_067_200_000) << 20) + n)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: seed <data dir> [<project dir>]");
    let project_dir = args.next().unwrap_or_else(|| std::env::current_dir().unwrap().display().to_string());
    let mut store = Store::open(&dir).expect("open store");
    let now = now_ms();

    let project = Project {
        id: ProjectId::new(),
        name: std::path::Path::new(&project_dir).file_name().unwrap().to_string_lossy().into_owned(),
        path: project_dir.clone(),
        order: 1,
        scripts: ProjectScripts::default(),
        base_branch: Some("main".into()),
    };
    let web = Project {
        id: ProjectId::new(),
        name: "storefront-web".into(),
        path: project_dir.clone(),
        order: 2,
        scripts: ProjectScripts::default(),
        base_branch: Some("main".into()),
    };
    store.commit(Event::ProjectUpserted(Box::new(project.clone()))).unwrap();
    store.commit(Event::ProjectUpserted(Box::new(web.clone()))).unwrap();

    let usage = Usage {
        input_tokens: 48_200,
        output_tokens: 6_900,
        cost_micros: 412_000,
        context_tokens: 96_000,
        context_window: 200_000,
        ..Default::default()
    };
    let mut thread = |title: &str, project: &Project, minutes_ago: u64, f: &dyn Fn(&mut Thread)| {
        let mut t = Thread {
            id: ThreadId::new(),
            project: Some(project.id),
            title: title.into(),
            created_ms: now - minutes_ago * 60_000 - 3_600_000,
            updated_ms: now - minutes_ago * 60_000,
            turns: 2,
            usage,
            workspace: Workspace { branch: Some("main".into()), ..Default::default() },
            ..Default::default()
        };
        f(&mut t);
        store.commit(Event::ThreadUpserted(Box::new(t.clone()))).unwrap();
        t
    };

    let main = thread("Make the parser stream tokens instead of buffering", &project, 1, &|t| {
        t.status = ThreadStatus::Running;
    });
    // The waiting thread asks two questions (one multi-select) and an approval.
    let questions = [
        (
            "Which browsers should the checkout tests cover?",
            vec![
                ("Chromium", "Chrome and Edge; most of the traffic"),
                ("Firefox", "Catches Gecko-only layout bugs"),
                ("WebKit", "Safari; slowest to run on Windows"),
            ],
            true,
        ),
        (
            "Run them against the staging API or a local mock?",
            vec![
                ("Staging API", "Real data; needs the VPN and can be flaky"),
                ("Local mock server", "Fast and deterministic (Recommended)"),
            ],
            false,
        ),
    ];
    let question_items: Vec<(ItemId, AgentRequest)> = questions
        .iter()
        .enumerate()
        .map(|(i, (q, options, multi))| {
            (
                ItemId::new(),
                AgentRequest {
                    id: RequestId::new(),
                    provider_ref: format!("q#{i}"),
                    kind: RequestKind::Question {
                        question: (*q).into(),
                        options: options.iter().map(|(label, description)| QuestionOption::new(*label, *description)).collect(),
                        multi_select: *multi,
                    },
                    resolution: None,
                },
            )
        })
        .collect();
    let pending: Vec<PendingRequest> = question_items
        .iter()
        .map(|(item, request)| PendingRequest { item: *item, request: request.clone(), created_ms: now })
        .collect();
    let waiting = thread("Add Playwright tests for checkout", &web, 3, &|t| {
        t.status = ThreadStatus::Waiting;
        t.pending = pending.clone();
    });
    let failed = thread("Fix flaky websocket reconnect test", &project, 9, &|t| {
        t.status = ThreadStatus::Failed { message: "The agent exited unexpectedly: out of memory".into() };
    });
    let limited = thread("Migrate auth to passkeys", &web, 14, &|t| {
        t.status = ThreadStatus::RateLimited { resets_ms: now + 47 * 60_000 };
    });
    thread("Explain the event-sourcing layer", &project, 22, &|t| t.unread = true);
    thread("Bump dependencies and fix breakages", &web, 40, &|t| t.pinned = true);
    thread("Profile cold start on Windows", &project, 75, &|_| {});
    thread("Write the release notes for 0.4", &project, 130, &|t| t.snoozed_until = Some(now + 3 * 3_600_000));
    for (i, title) in ["Rename the settings keys", "Spike: SQLite vs append log", "Dark mode contrast pass"].iter().enumerate() {
        thread(title, &project, 600 + i as u64 * 300, &|t| t.settled_ms = Some(now));
    }

    // A rich timeline on the main thread.
    let items = vec![
        item(1, ItemBody::User(UserMessage {
            text: "The tokenizer buffers the whole file before parsing. Make it stream tokens so the first diagnostics show up immediately, and keep the public API the same.".into(),
            ..Default::default()
        })),
        item(1, ItemBody::Reasoning { text: "The tokenizer owns a `String` and hands out slices. To stream, it needs an internal ring buffer and a `Read` source. The parser only calls `next_token`, so the public API can stay.".into() }),
        item(1, tool(ToolKind::Search, "Grep", "next_token", "src/tokenizer.rs:42\nsrc/parser.rs:118\nsrc/parser.rs:240", vec![])),
        item(1, tool(ToolKind::Read, "Read", "src/tokenizer.rs", "", vec![])),
        item(1, tool(ToolKind::Edit, "Edit", "src/tokenizer.rs", "", vec![FileChange { path: "src/tokenizer.rs".into(), kind: ChangeKind::Modified, additions: 84, deletions: 31 }])),
        item(1, tool(ToolKind::Command, "Bash", "cargo test -p parser", "running 48 tests\n................................................\ntest result: ok. 48 passed; 0 failed", vec![])),
        item(1, ItemBody::Assistant { text: "Done. The tokenizer now reads through a **64 KB ring buffer** and yields tokens as soon as they are complete:\n\n- `Tokenizer::new` accepts any `impl Read` (files and strings both work)\n- `next_token` is unchanged, so the parser didn't need edits\n- the first diagnostic on a 40 MB file now appears after **3 ms** instead of 410 ms\n\n```rust\npub fn new(source: impl Read + 'static) -> Self {\n    Self { buf: Ring::with_capacity(64 * 1024), source: Box::new(source), pos: 0 }\n}\n```\n\nAll 48 parser tests pass.".into() }),
        item(1, ItemBody::TurnEnd(TurnSummary {
            usage: Usage { input_tokens: 21_000, output_tokens: 3_100, cost_micros: 184_000, ..Default::default() },
            duration_ms: 154_000,
            files: vec![FileChange { path: "src/tokenizer.rs".into(), kind: ChangeKind::Modified, additions: 84, deletions: 31 }],
            ..Default::default()
        })),
        item(2, ItemBody::User(UserMessage { text: "Nice. Now add a benchmark and make sure memory stays flat on huge inputs.".into(), ..Default::default() })),
        item(2, ItemBody::Plan { steps: vec![
            PlanStep { text: "Add a criterion benchmark over a generated 200 MB file".into(), state: StepState::Done },
            PlanStep { text: "Track peak RSS in the benchmark harness".into(), state: StepState::Active },
            PlanStep { text: "Assert memory stays under 8 MB in CI".into(), state: StepState::Pending },
        ] }),
        item(2, tool(ToolKind::Edit, "Write", "benches/tokenize.rs", "", vec![FileChange { path: "benches/tokenize.rs".into(), kind: ChangeKind::Added, additions: 57, deletions: 0 }])),
        Item {
            id: ItemId::new(),
            turn: 2,
            status: ItemStatus::Streaming,
            body: tool(ToolKind::Command, "Bash", "cargo bench --bench tokenize", "Benchmarking tokenize/200MB: Warming up for 3.0000 s", vec![]),
        },
    ];
    // Turn 1 happened yesterday: re-stamp its items a day back.
    let yesterday = now - 26 * 3_600_000;
    let mut items: Vec<Item> = items
        .into_iter()
        .enumerate()
        .map(|(i, mut item)| {
            if item.turn == 1 {
                item.id = id_at(yesterday + i as u64 * 20_000);
            }
            item
        })
        .collect();
    let streaming = items.pop().unwrap();
    items.push(Item {
        id: ItemId::new(),
        turn: 2,
        status: ItemStatus::Streaming,
        body: ItemBody::Tool(ToolCall {
            call_id: "task-1".into(),
            kind: ToolKind::Task,
            name: "Task".into(),
            title: "Find every caller that relies on buffered input (Explore)".into(),
            input: r#"{"description":"Find every caller that relies on buffered input","prompt":"Search the workspace for code that assumes the tokenizer holds the whole file:\n- slices kept past the next token\n- calls to `source()`\nReport file and line.","subagent_type":"Explore"}"#.into(),
            output: "Grep: source()\nRead: src/diagnostics.rs\nRead: src/lsp/hover.rs\nGrep: &self.buf\n".into(),
            output_blob: None,
            exit_code: None,
            files: vec![],
        }),
    });
    items.push(streaming);
    for item in items {
        store.commit(Event::ItemUpserted { thread: main.id, item: Box::new(item) }).unwrap();
    }

    // The waiting thread: a turn that asks.
    let mut waiting_items = vec![
        item(1, ItemBody::User(UserMessage { text: "Add Playwright tests for the checkout flow: cart, address, payment and the confirmation page.".into(), ..Default::default() })),
        item(1, tool(ToolKind::Read, "Read", "tests/e2e/cart.spec.ts", "", vec![])),
        // A dev server the preview browser offers ("Open preview localhost:5173").
        item(1, tool(ToolKind::Command, "Bash", "npm run dev", "\n  VITE v6.0.3  ready in 412 ms\n\n  ➜  Local:   http://localhost:5173/\n  ➜  Network: use --host to expose\n", vec![])),
        item(1, ItemBody::Assistant { text: "Before I write the specs, two choices change how they run:".into() }),
    ];
    for (id, request) in &question_items {
        waiting_items.push(Item { id: *id, turn: 1, status: ItemStatus::Streaming, body: ItemBody::Request(request.clone()) });
    }
    for item in waiting_items {
        store.commit(Event::ItemUpserted { thread: waiting.id, item: Box::new(item) }).unwrap();
    }

    // The failed thread: the first attempt failed, a retry failed again.
    let mut log = String::new();
    for i in 0..400 {
        log.push_str(&format!("test ws::reconnect::case_{i} ... ok\n"));
    }
    log.push_str("test ws::reconnect::drops_after_idle ... FAILED\n\nfailures:\n    ws::reconnect::drops_after_idle: timed out after 30 s\n");
    let failed_items = vec![
        item(1, ItemBody::User(UserMessage { text: "The websocket reconnect test fails about one run in five. Find out why and fix it.".into(), ..Default::default() })),
        item(1, ItemBody::Reasoning { text: "Flaky timing: probably the idle timer races the reconnect.".into() }),
        item(1, ItemBody::Tool(ToolCall {
            call_id: "c1".into(),
            kind: ToolKind::Command,
            name: "Bash".into(),
            title: "cargo test -p net ws::reconnect -- --test-threads=1".into(),
            input: r#"{"command":"cargo test -p net ws::reconnect -- --test-threads=1","description":"Run the reconnect tests serially"}"#.into(),
            output: log,
            output_blob: None,
            exit_code: Some(101),
            files: vec![],
        })),
        item(1, ItemBody::TurnEnd(TurnSummary { duration_ms: 48_000, ..Default::default() })),
        item(1, ItemBody::Notice { level: NoticeLevel::Error, text: "The model is overloaded. Try again in a moment.".into() }),
        item(1, ItemBody::Reasoning { text: "Retrying: run the single failing case with logging.".into() }),
        item(1, tool(ToolKind::Command, "Bash", "RUST_LOG=trace cargo test -p net drops_after_idle", "thread 'main' has overflowed its stack", vec![])),
        item(1, tool(ToolKind::Edit, "Edit", "crates/net/src/ws.rs", "", vec![FileChange { path: "crates/net/src/ws.rs".into(), kind: ChangeKind::Modified, additions: 6, deletions: 2 }])),
        item(1, ItemBody::TurnEnd(TurnSummary { duration_ms: 95_000, ..Default::default() })),
        item(1, ItemBody::Notice { level: NoticeLevel::Error, text: "The agent exited unexpectedly: out of memory".into() }),
    ];
    for item in failed_items {
        store.commit(Event::ItemUpserted { thread: failed.id, item: Box::new(item) }).unwrap();
    }

    // The rate-limited thread.
    let limited_items = vec![
        item(1, ItemBody::User(UserMessage { text: "Replace password sign-in with passkeys, keeping password as a fallback for a release.".into(), ..Default::default() })),
        item(1, ItemBody::Assistant { text: "I'll start with the WebAuthn registration endpoint.".into() }),
        item(1, ItemBody::TurnEnd(TurnSummary { duration_ms: 12_000, ..Default::default() })),
    ];
    for item in limited_items {
        store.commit(Event::ItemUpserted { thread: limited.id, item: Box::new(item) }).unwrap();
    }

    // `SEED_FOCUS=main|questions|failed|limited` makes that thread the latest, for
    // `ORBIT_DEBUG_ROUTE=latest`.
    let focus = match std::env::var("SEED_FOCUS").as_deref() {
        Ok("main") => Some(main),
        Ok("questions") => Some(waiting),
        Ok("failed") => Some(failed),
        Ok("limited") => Some(limited),
        _ => None,
    };
    if let Some(mut t) = focus {
        t.updated_ms = now + 86_400_000;
        store.commit(Event::ThreadUpserted(Box::new(t))).unwrap();
    }
    store.sync().unwrap();
    println!("seeded {dir}");
}
