//! Adds usage data to a data folder made by the `seed` example, for visual
//! checks of quota rings, the Usage page and the limit banner's offer:
//! `cargo run -p orbit-core --example seed_usage -- <data dir>`.
//!
//! * A second Claude account, "Claude (work)", with a placeholder token in
//!   its own environment (never a real sign-in), so it counts as signed in.
//! * Six weeks of turns in the usage ledger across the seeded threads, three
//!   accounts and several models.
//! * Last readings and an hour of history for each account's windows, so
//!   rings, burn rates and projections show; reading limits from the agents
//!   is turned off, so nothing is asked of a real provider.

use std::path::Path;

use orbit_core::store::Store;
use orbit_core::usage::burn::Sample;
use orbit_core::usage::ledger::Ledger;
use orbit_core::usage::saved::{self, Saved, SavedAccount};
use orbit_proto::*;

const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const DAY: u64 = 24 * HOUR;

/// A small deterministic generator, so every seeded folder looks the same.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn window(id: &str, used: f32, resets_ms: u64) -> QuotaWindow {
    QuotaWindow { window: id.into(), utilization: Some(used), resets_ms, rejected: false }
}

/// Readings every five minutes over the last `minutes`, rising from `from` to `to`.
fn rising(now: u64, minutes: u64, from: f32, to: f32, resets_ms: u64) -> Vec<Sample> {
    let steps = minutes / 5;
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps.max(1) as f32;
            let used = ((from + (to - from) * t) * 100.0).round() / 100.0;
            Sample { at_ms: now - (steps - i) * 5 * MIN, used, resets_ms }
        })
        .collect()
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: seed_usage <data dir>");
    let dir = Path::new(&dir);
    let now = now_ms();
    let mut store = Store::open(dir).expect("open store (is Orbit running on it?)");

    // A second Claude account.
    let mut settings = store.settings().clone();
    if !settings.agents.instances.iter().any(|i| i.id == "claude-work") {
        settings.agents.instances.push(ProviderInstance {
            id: "claude-work".into(),
            kind: ProviderKind::Claude,
            name: "Claude (work)".into(),
            enabled: true,
            config_dir: dir.join("accounts").join("claude-work").display().to_string(),
            env: vec![EnvVar::new("CLAUDE_CODE_OAUTH_TOKEN", "seed-placeholder-not-a-token")],
            ..Default::default()
        });
        store.commit(Event::SettingsChanged(Box::new(settings))).unwrap();
    }

    // The ledger: turns over six weeks, busier on weekdays and lately.
    let threads: Vec<(ThreadId, Option<ProjectId>)> = store.threads().map(|t| (t.id, t.project)).collect();
    assert!(!threads.is_empty(), "run the `seed` example on this folder first");
    let accounts = [
        (ProviderKind::Claude, "claude", "opus"),
        (ProviderKind::Claude, "claude", "sonnet"),
        (ProviderKind::Claude, "claude-work", "opus"),
        (ProviderKind::Codex, "codex", "gpt-5-codex"),
    ];
    let mut rng = Rng(0x5eed_u64);
    let mut rows = Vec::new();
    for day in (0..42u64).rev() {
        let busy = 2 + (42 - day) / 6 + rng.below(4);
        for _ in 0..busy {
            let (kind, instance, model) = accounts[rng.below(accounts.len() as u64) as usize];
            let (thread, project) = threads[rng.below(threads.len() as u64) as usize];
            let input = 8_000 + rng.below(90_000);
            let cached = input * (3 + rng.below(5)) / 2;
            let output = 600 + rng.below(9_000);
            // Dollars per million tokens are micro-dollars per token.
            let cost = if kind == ProviderKind::Claude { input * 3 + output * 15 + cached * 3 / 10 } else { 0 };
            let ended_ms = now - day * DAY - rng.below(10 * HOUR);
            rows.push(TurnUsage {
                ended_ms,
                thread,
                project,
                kind,
                instance: instance.into(),
                model: model.into(),
                usage: Usage { input_tokens: input, output_tokens: output, cache_read_tokens: cached, cost_micros: cost, ..Default::default() },
            });
        }
    }
    let count = rows.len();
    drop(store);
    let (mut ledger, fresh) = Ledger::open(dir);
    if fresh {
        ledger.backfilled(rows, now + 1).expect("write the ledger");
    } else {
        for row in rows {
            ledger.append(row);
        }
    }

    // Last readings and their history.
    let five = now + 2 * HOUR + 10 * MIN;
    let week = now + 4 * DAY + 7 * HOUR;
    let saved = Saved {
        settings: UsageSettings { poll: false, ..Default::default() },
        accounts: vec![
            SavedAccount {
                instance: "claude".into(),
                plan: Some("Max 20x".into()),
                windows: vec![window("five_hour", 0.62, five), window("seven_day", 0.31, week)],
                updated_ms: now - 2 * MIN,
                source: QuotaSource::Provider,
                history: vec![
                    ("five_hour".into(), rising(now - 2 * MIN, 50, 0.41, 0.62, five)),
                    ("seven_day".into(), rising(now - 2 * MIN, 50, 0.29, 0.31, week)),
                ],
            },
            SavedAccount {
                instance: "claude-work".into(),
                plan: Some("Max 5x".into()),
                windows: vec![window("five_hour", 0.12, now + 4 * HOUR), window("seven_day", 0.44, now + 2 * DAY)],
                updated_ms: now - 6 * MIN,
                source: QuotaSource::Provider,
                history: vec![("five_hour".into(), rising(now - 6 * MIN, 40, 0.12, 0.12, now + 4 * HOUR))],
            },
            SavedAccount {
                instance: "codex".into(),
                plan: Some("ChatGPT Plus".into()),
                windows: vec![window("5h", 0.86, now + 75 * MIN), window("1w", 0.18, now + 6 * DAY)],
                updated_ms: now - MIN,
                source: QuotaSource::Session,
                history: vec![("5h".into(), rising(now - MIN, 45, 0.63, 0.86, now + 75 * MIN))],
            },
        ],
    };
    saved::save(dir, &saved).expect("write usage.bin");
    println!("Seeded {count} turns, three accounts and their windows in {}", dir.display());
}
