//! Reading Codex plan limits on a scripted fake `codex app-server`
//! (`tests/support/fake_codex.rs`): the handshake, the plan from
//! `account/read`, the windows from `account/rateLimits/read`, and the
//! states only the user can change.

#[path = "support/exe.rs"]
mod exe;

use std::path::PathBuf;

use orbit_core::provider::codex::read_limits;
use orbit_core::usage::ReadError;
use orbit_proto::*;
use sonic_rs::{Value, json};

struct Fake {
    _dir: tempfile::TempDir,
    exe: PathBuf,
}

fn fake(handlers: Value) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(if cfg!(windows) { "codex.exe" } else { "codex" });
    exe::install(env!("CARGO_BIN_EXE_orbit-fake-codex").as_ref(), &exe);
    let script = json!({"runs": [{"handlers": handlers, "steps": []}]});
    std::fs::write(dir.path().join("fake-codex.json"), sonic_rs::to_string(&script).unwrap()).unwrap();
    Fake { _dir: dir, exe }
}

fn handlers(account: Value) -> Value {
    let limits: Value = sonic_rs::from_str(include_str!("fixtures/usage/codex_rate_limits.json")).unwrap();
    json!({
        "initialize": {"userAgent": "codex_cli_rs/0.170.2 (Windows 11; x86_64) orbit"},
        "account/read": account,
        "account/rateLimits/read": limits
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

#[test]
fn a_signed_in_plan_reads_its_windows() {
    let fake = fake(handlers(json!({"account": {"type": "chatgpt", "email": "dev@example.com", "planType": "pro"}, "requiresOpenaiAuth": true})));
    let reading = runtime().block_on(read_limits(Some(fake.exe.clone()), vec![])).unwrap();
    assert_eq!(reading.plan.as_deref(), Some("ChatGPT Pro"), "the account's plan wins over the limits'");
    let windows: Vec<(&str, Option<f32>)> = reading.windows.iter().map(|w| (w.window.as_str(), w.utilization)).collect();
    assert_eq!(windows, [("5h", Some(0.42)), ("1w", Some(0.18)), ("GPT-5-Codex-Mini 5h", Some(0.03))]);
    assert_eq!(describe_window(&reading.windows[2].window).0, "5-hour · GPT-5-Codex-Mini");
}

#[test]
fn an_api_key_or_a_signed_out_codex_has_no_plan() {
    let key = fake(handlers(json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true})));
    let err: ReadError = runtime().block_on(read_limits(Some(key.exe.clone()), vec![])).unwrap_err();
    assert!(matches!(err.status, QuotaStatus::NoPlan(ref m) if m.contains("API key")), "{err:?}");
    assert!(!err.transient);
    let out = fake(handlers(json!({"account": null, "requiresOpenaiAuth": true})));
    let err = runtime().block_on(read_limits(Some(out.exe.clone()), vec![])).unwrap_err();
    assert!(matches!(err.status, QuotaStatus::NoPlan(ref m) if m.contains("codex login")), "{err:?}");
}

#[test]
fn a_missing_codex_says_so() {
    let err = runtime().block_on(read_limits(Some(PathBuf::from("/nowhere/codex-missing")), vec![])).unwrap_err();
    assert!(matches!(err.status, QuotaStatus::Unavailable(_)), "{err:?}");
}
