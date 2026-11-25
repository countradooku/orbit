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
    store.commit(Event::ProjectUpserted(project.clone())).unwrap();
    store.commit(Event::ProjectUpserted(web.clone())).unwrap();

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
        store.commit(Event::ThreadUpserted(t.clone())).unwrap();
        t
    };

    let main = thread("Make the parser stream tokens instead of buffering", &project, 1, &|t| {
        t.status = ThreadStatus::Running;
    });
    let waiting = thread("Add Playwright tests for checkout", &web, 3, &|t| {
        t.status = ThreadStatus::Waiting;
        t.pending = vec![PendingRequest {
            item: ItemId::new(),
            created_ms: now,
            request: AgentRequest {
                id: RequestId::new(),
                provider_ref: "r1".into(),
                kind: RequestKind::Command {
                    command: "pnpm exec playwright install --with-deps chromium".into(),
                    cwd: "storefront-web".into(),
                    reason: "Install the browser Playwright needs".into(),
                },
                resolution: None,
            },
        }];
    });
    thread("Fix flaky websocket reconnect test", &project, 9, &|t| {
        t.status = ThreadStatus::Failed { message: "The agent exited unexpectedly: out of memory".into() };
    });
    thread("Migrate auth to passkeys", &web, 14, &|t| {
        t.status = ThreadStatus::RateLimited { resets_ms: now + 47 * 60_000 };
    });
    thread("Explain the event-sourcing layer", &project, 22, &|t| t.unread = true);
    thread("Bump dependencies and fix breakages", &web, 40, &|t| t.pinned = true);
    thread("Profile cold start on Windows", &project, 75, &|_| {});
    thread("Write the release notes for 0.4", &project, 130, &|t| t.snoozed_until = Some(now + 3 * 3_600_000));
    for (i, title) in ["Rename the settings keys", "Spike: SQLite vs append log", "Dark mode contrast pass"].iter().enumerate() {
        thread(title, &project, 600 + i as u64 * 300, &|t| t.settled_ms = Some(now));
    }
    let _ = waiting;

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
    for item in items {
        store.commit(Event::ItemUpserted { thread: main.id, item }).unwrap();
    }
    store.sync().unwrap();
    println!("seeded {dir}");
}
