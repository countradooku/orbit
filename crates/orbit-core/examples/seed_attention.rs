//! Adds threads that wait on the user in every way the Inbox handles to a
//! data folder made by the `seed` example, for visual checks of the deck,
//! the sidebar and the palette:
//! `cargo run -p orbit-core --example seed_attention -- <data dir>`.

use orbit_core::store::Store;
use orbit_proto::*;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: seed_attention <data dir>");
    let mut store = Store::open(&dir).expect("open store");
    let now = now_ms();
    let projects = store.projects().to_vec();
    let project = projects.first().map(|p| p.id);
    let web = projects.get(1).map(|p| p.id).or(project);
    let request = |kind: RequestKind, minutes_ago: u64| PendingRequest {
        item: ItemId::new(),
        request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
        created_ms: now - minutes_ago * 60_000,
    };
    let mut add = |title: &str, project: Option<ProjectId>, minutes_ago: u64, pending: Vec<PendingRequest>| {
        let thread = Thread {
            id: ThreadId::new(),
            project,
            title: title.into(),
            status: ThreadStatus::Waiting,
            created_ms: now - minutes_ago * 60_000 - 1_800_000,
            updated_ms: now - minutes_ago * 60_000,
            turns: 1,
            pending,
            workspace: Workspace { branch: Some(format!("orbit/{}", title.to_lowercase().replace(' ', "-"))), ..Default::default() },
            ..Default::default()
        };
        store.commit(Event::ThreadUpserted(Box::new(thread))).unwrap();
    };
    add(
        "Run the migration against a scratch database",
        project,
        2,
        vec![request(
            RequestKind::Command {
                command: "cargo run -p migrate -- --database postgres://localhost/scratch --apply".into(),
                cwd: "~/code/orbit".into(),
                reason: "Apply the new schema to a throwaway database to check the migration".into(),
            },
            2,
        )],
    );
    add(
        "Clean up stale build artifacts",
        project,
        5,
        vec![
            request(
                RequestKind::Command { command: "cargo clean -p orbit-bench".into(), cwd: "~/code/orbit".into(), reason: String::new() },
                5,
            ),
            request(RequestKind::Command { command: "git gc --prune=now".into(), cwd: "~/code/orbit".into(), reason: String::new() }, 4),
        ],
    );
    add(
        "Tighten the checkout form validation",
        web,
        7,
        vec![request(
            RequestKind::FileChange {
                path: "src/checkout/validate.ts".into(),
                diff: "--- a/src/checkout/validate.ts\n+++ b/src/checkout/validate.ts\n@@ -12,7 +12,9 @@\n-  if (!email) return false;\n+  if (!email || !EMAIL.test(email)) {\n+    return { ok: false, field: \"email\" };\n+  }\n-  return true;\n+  return { ok: true };\n".into(),
            },
            7,
        )],
    );
    add(
        "Choose a cache for the tokenizer",
        project,
        11,
        vec![request(
            RequestKind::Question {
                question: "Which cache should the tokenizer use for repeated inputs?".into(),
                options: vec!["No cache".into(), "LRU with 1,024 entries (Recommended)".into(), "Unbounded hash map".into()],
                multi_select: false,
            },
            11,
        )],
    );
    add(
        "Plan the settings page split",
        web,
        16,
        vec![request(
            RequestKind::Plan {
                plan: "1. Move each settings page into its own module.\n2. Keep one SettingsModel global.\n3. Add a search index over item titles and keywords.".into(),
            },
            16,
        )],
    );
    println!("added attention threads to {dir}");
}
