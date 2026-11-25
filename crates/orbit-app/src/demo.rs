//! Debug builds only: `ORBIT_DEBUG_DEMO=attention` gives the threads made by
//! the `seed_attention` example the requests they were seeded with. The
//! engine closes requests on start (no agent can receive the answer), so
//! without this the Inbox, peeks and notifications cannot be checked
//! visually against a seeded data folder.

use std::collections::HashMap;

use orbit_proto::*;

fn request(kind: RequestKind, minutes_ago: u64) -> PendingRequest {
    PendingRequest {
        item: ItemId::new(),
        request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
        created_ms: now_ms().saturating_sub(minutes_ago * 60_000),
    }
}

fn command(command: &str, reason: &str, minutes_ago: u64) -> PendingRequest {
    request(RequestKind::Command { command: command.into(), cwd: "~/code/orbit".into(), reason: reason.into() }, minutes_ago)
}

/// Requests by thread title.
fn requests(title: &str) -> Vec<PendingRequest> {
    match title {
        "Run the migration against a scratch database" => vec![command(
            "cargo run -p migrate -- --database postgres://localhost/scratch --apply",
            "Apply the new schema to a throwaway database to check the migration",
            2,
        )],
        "Clean up stale build artifacts" => {
            vec![command("cargo clean -p orbit-bench", "", 5), command("git gc --prune=now", "", 4)]
        }
        "Tighten the checkout form validation" => vec![request(
            RequestKind::FileChange {
                path: "src/checkout/validate.ts".into(),
                diff: "--- a/src/checkout/validate.ts\n+++ b/src/checkout/validate.ts\n@@ -12,7 +12,9 @@\n-  if (!email) return false;\n+  if (!email || !EMAIL.test(email)) {\n+    return { ok: false, field: \"email\" };\n+  }\n-  return true;\n+  return { ok: true };\n".into(),
            },
            7,
        )],
        "Choose a cache for the tokenizer" => vec![request(
            RequestKind::Question {
                question: "Which cache should the tokenizer use for repeated inputs?".into(),
                options: vec!["No cache".into(), "LRU with 1,024 entries (Recommended)".into(), "Unbounded hash map".into()],
                multi_select: false,
            },
            11,
        )],
        "Add Playwright tests for checkout" => vec![request(
            RequestKind::Question {
                question: "Which browsers should the checkout tests cover?".into(),
                options: vec!["Chromium".into(), "Firefox".into(), "WebKit".into()],
                multi_select: true,
            },
            3,
        )],
        "Plan the settings page split" => vec![request(
            RequestKind::Plan {
                plan: "1. Move each settings page into its own module.\n2. Keep one SettingsModel global.\n3. Add a search index over item titles and keywords.".into(),
            },
            16,
        )],
        _ => Vec::new(),
    }
}

/// Gives the seeded threads their requests back.
pub fn apply(threads: &mut HashMap<ThreadId, Thread>) {
    for thread in threads.values_mut() {
        let pending = requests(&thread.title);
        if !pending.is_empty() && thread.pending.is_empty() {
            thread.pending = pending;
            thread.status = ThreadStatus::Waiting;
        }
    }
}
