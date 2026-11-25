//! Claude plan limits from Claude Code's own sign-in.
//!
//! Claude Code keeps its OAuth credentials in `<config dir>/.credentials.json`
//! (`~/.claude` unless `CLAUDE_CONFIG_DIR` points elsewhere; that is how a
//! second account works), or in the macOS keychain. Orbit reads the access
//! token and asks `GET https://api.anthropic.com/api/oauth/usage`, the
//! endpoint behind Claude Code's `/usage`: one small request, no model call,
//! nothing spent. It never refreshes the token (that would rotate the
//! refresh token under the CLI's feet); an expired one waits until Claude
//! Code next runs and renews it.
//!
//! The response names each window (`five_hour`, `seven_day`,
//! `seven_day_opus`, …) with `utilization` in percent and an RFC 3339
//! `resets_at`. Inactive windows are `null`. The ids match the ones running
//! sessions report in their `rate_limit_event`s, so both sources update the
//! same windows.

use std::path::{Path, PathBuf};

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _, Value};

use super::{ReadError, Reading};

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
const SIGN_IN: &str = "Run `claude` in a terminal and sign in with /login.";

/// What Claude Code's sign-in says about the account.
#[derive(Clone, Debug, PartialEq)]
pub struct Credentials {
    pub token: String,
    /// When the access token expires (Unix ms); 0 when unknown.
    pub expires_ms: u64,
    pub plan: Option<String>,
}

/// Reads `.credentials.json` (the `claudeAiOauth` entry).
pub fn parse_credentials(json: &str) -> Option<Credentials> {
    let value: Value = sonic_rs::from_str(json).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let token = oauth.get("accessToken")?.as_str()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    Some(Credentials {
        token,
        expires_ms: oauth.get("expiresAt").and_then(|v| v.as_f64()).map(|v| v as u64).unwrap_or(0),
        plan: plan_name(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            oauth.get("rateLimitTier").and_then(|v| v.as_str()),
        ),
    })
}

/// `max` + `default_claude_max_20x` → `Max 20x`; `pro` → `Pro`.
pub fn plan_name(subscription: Option<&str>, tier: Option<&str>) -> Option<String> {
    if let Some(tier) = tier {
        for (marker, name) in [("max_20x", "Max 20x"), ("max_5x", "Max 5x")] {
            if tier.contains(marker) {
                return Some(name.to_owned());
            }
        }
    }
    subscription.filter(|s| !s.is_empty() && *s != "null").map(humanize_id)
}

/// The configuration folder of an instance's environment.
pub fn config_dir(env: &[(String, String)]) -> Option<PathBuf> {
    match env.iter().rev().find(|(k, _)| k == "CLAUDE_CONFIG_DIR") {
        Some((_, dir)) if !dir.trim().is_empty() => Some(PathBuf::from(dir.trim())),
        _ => dirs::home_dir().map(|h| h.join(".claude")),
    }
}

fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.trim().is_empty())
}

/// The sign-in an instance with `env` runs under. Blocking (file and
/// keychain reads): call off the engine thread.
pub fn credentials(env: &[(String, String)], now_ms: u64) -> Result<Credentials, ReadError> {
    if let Some(token) = env_value(env, "CLAUDE_CODE_OAUTH_TOKEN") {
        return Ok(Credentials { token: token.trim().to_owned(), expires_ms: 0, plan: None });
    }
    let process_env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let uses = |name: &str| env_value(env, name).is_some() || process_env(name).is_some();
    let dir = config_dir(env);
    let stored = dir.as_deref().and_then(stored_credentials);
    let found = match stored {
        Some(found) => found,
        None if uses("ANTHROPIC_API_KEY") || uses("CLAUDE_CODE_USE_BEDROCK") || uses("CLAUDE_CODE_USE_VERTEX") => {
            return Err(ReadError::no_plan("Uses an API key or a cloud provider: no plan limits."));
        }
        None => return Err(ReadError::no_plan(format!("Not signed in to a Claude plan. {SIGN_IN}"))),
    };
    if found.expires_ms > 0 && found.expires_ms <= now_ms {
        // Claude Code renews it the next time it runs; until then, wait.
        return Err(ReadError {
            status: QuotaStatus::Unavailable("The sign-in expired. Claude Code renews it the next time it runs.".into()),
            retry_after: None,
            transient: true,
        });
    }
    Ok(found)
}

fn stored_credentials(dir: &Path) -> Option<Credentials> {
    #[cfg(target_os = "macos")]
    if let Some(found) = keychain(dir) {
        return Some(found);
    }
    let text = std::fs::read_to_string(dir.join(".credentials.json")).ok()?;
    parse_credentials(&text)
}

/// The macOS keychain item Claude Code writes: `Claude Code-credentials`
/// for the default folder, with a short hash of the folder for others.
#[cfg(target_os = "macos")]
fn keychain(dir: &Path) -> Option<Credentials> {
    let default = dirs::home_dir().map(|h| h.join(".claude"));
    let service = if default.as_deref() == Some(dir) {
        "Claude Code-credentials".to_owned()
    } else {
        let digest = ring::digest::digest(&ring::digest::SHA256, dir.to_string_lossy().as_bytes());
        let hex: String = digest.as_ref().iter().take(4).map(|b| format!("{b:02x}")).collect();
        format!("Claude Code-credentials-{hex}")
    };
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then(|| parse_credentials(String::from_utf8_lossy(&output.stdout).trim())).flatten()
}

/// The windows of a `/api/oauth/usage` response.
pub fn parse_usage(json: &str) -> Result<Vec<QuotaWindow>, String> {
    let value: Value = sonic_rs::from_str(json).map_err(|e| format!("Claude's usage answer was unreadable: {e}"))?;
    let object = value.as_object().ok_or("Claude's usage answer was not an object.")?;
    let mut windows = Vec::new();
    for (key, entry) in object.iter() {
        let named = key == "five_hour" || key == "seven_day" || key.starts_with("seven_day_");
        if !named || entry.is_null() || key == "seven_day_breakdown" {
            continue;
        }
        let Some(percent) = entry.get("utilization").and_then(|v| v.as_f64()) else { continue };
        let used = (percent / 100.0).clamp(0.0, 1.0) as f32;
        let resets_ms = entry.get("resets_at").and_then(|v| v.as_str()).map(timestamp_ms).unwrap_or(0);
        let locked = entry.get("locked_reason").is_some_and(|v| !v.is_null());
        windows.push(QuotaWindow { window: key.to_owned(), utilization: Some(used), resets_ms, rejected: locked || used >= 1.0 });
    }
    // Model-scoped weekly limits that are in force, when not named above.
    if let Some(limits) = value.get("limits").and_then(|v| v.as_array()) {
        for limit in limits.iter() {
            let active = limit.get("is_active").and_then(|v| v.as_bool()).unwrap_or(false);
            let model = limit.get("scope").and_then(|s| s.get("model")).and_then(|m| m.get("display_name")).and_then(|v| v.as_str());
            let (Some(model), true) = (model, active) else { continue };
            if limit.get("group").and_then(|v| v.as_str()) != Some("weekly") {
                continue;
            }
            let id = format!("seven_day_{}", model.to_lowercase().replace(' ', "_"));
            if windows.iter().any(|w| w.window == id) {
                continue;
            }
            let Some(percent) = limit.get("percent").and_then(|v| v.as_f64()) else { continue };
            let used = (percent / 100.0).clamp(0.0, 1.0) as f32;
            let resets_ms = limit.get("resets_at").and_then(|v| v.as_str()).map(timestamp_ms).unwrap_or(0);
            windows.push(QuotaWindow { window: id, utilization: Some(used), resets_ms, rejected: used >= 1.0 });
        }
    }
    // Shortest first: the five-hour window, then the weekly ones.
    windows.sort_by_key(|w| (describe_window(&w.window).1 == 0, describe_window(&w.window).1, w.window.clone()));
    Ok(windows)
}

fn timestamp_ms(text: &str) -> u64 {
    text.parse::<jiff::Timestamp>().map(|t| t.as_millisecond().max(0) as u64).unwrap_or(0)
}

/// Reads an instance's limits. `env` is the instance's environment.
pub async fn read(env: Vec<(String, String)>) -> Result<Reading, ReadError> {
    let now = now_ms();
    let creds = tokio::task::spawn_blocking(move || credentials(&env, now))
        .await
        .map_err(|e| ReadError::transient(e.to_string()))??;
    let headers = [
        ("authorization", format!("Bearer {}", creds.token)),
        ("anthropic-beta", BETA.to_owned()),
        ("accept", "application/json".to_owned()),
    ];
    let response = crate::forge::http::send("GET", USAGE_URL, &headers, None).await.map_err(|e| ReadError::transient(e.to_string()))?;
    match response.status {
        200 => {
            let text = String::from_utf8_lossy(&response.body);
            let windows = parse_usage(&text).map_err(ReadError::transient)?;
            Ok(Reading { windows, plan: creds.plan })
        }
        401 | 403 => Err(ReadError {
            status: QuotaStatus::Unavailable(format!("Claude didn’t accept the sign-in. {SIGN_IN}")),
            retry_after: None,
            transient: true,
        }),
        429 => Err(ReadError {
            retry_after: response.retry_after,
            ..ReadError::transient("Claude asked Orbit to check less often; it tries again later.")
        }),
        status => Err(ReadError::transient(format!("Claude’s usage service answered {status}."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("../../tests/fixtures/usage/claude_oauth_usage.json");
    const BUSY: &str = include_str!("../../tests/fixtures/usage/claude_oauth_usage_busy.json");
    const CREDENTIALS: &str = include_str!("../../tests/fixtures/usage/claude_credentials.json");

    #[test]
    fn a_recorded_answer_reads_as_windows() {
        // Recorded from the endpoint: no five-hour window open yet, weekly
        // at 31 %, and a model-scoped weekly limit that is not in force.
        let windows = parse_usage(LIVE).unwrap();
        assert_eq!(windows.len(), 1, "{windows:?}");
        let weekly = &windows[0];
        assert_eq!(weekly.window, "seven_day");
        assert_eq!(weekly.utilization, Some(0.31));
        assert_eq!(weekly.resets_ms, 1_796_119_200_489);
        assert!(!weekly.rejected);
    }

    #[test]
    fn open_windows_scoped_limits_and_a_full_window() {
        let windows = parse_usage(BUSY).unwrap();
        let ids: Vec<&str> = windows.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day", "seven_day_fable", "seven_day_opus"]);
        assert_eq!(windows[0].utilization, Some(1.0));
        assert!(windows[0].rejected, "a full window refuses requests");
        assert_eq!(windows[2].utilization, Some(0.12), "an active scoped limit from `limits`");
        assert!(parse_usage("[]").is_err());
        assert!(parse_usage("{").is_err());
    }

    #[test]
    fn credentials_name_the_plan_and_never_need_more_than_the_token() {
        let creds = parse_credentials(CREDENTIALS).unwrap();
        assert_eq!(creds.token, "sk-ant-oat01-fixture");
        assert_eq!(creds.expires_ms, 1_796_000_000_000);
        assert_eq!(creds.plan.as_deref(), Some("Max 20x"));
        assert_eq!(plan_name(Some("pro"), None).as_deref(), Some("Pro"));
        assert_eq!(plan_name(Some("max"), Some("default_claude_max_5x")).as_deref(), Some("Max 5x"));
        assert_eq!(plan_name(None, None), None);
        assert_eq!(parse_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#), None);
        assert_eq!(parse_credentials("{}"), None);
    }

    #[test]
    fn the_sign_in_comes_from_the_instances_folder() {
        let dir = tempfile::tempdir().unwrap();
        let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), dir.path().display().to_string())];
        assert_eq!(config_dir(&env).as_deref(), Some(dir.path()));
        let err = credentials(&env, 0).unwrap_err();
        assert!(matches!(err.status, QuotaStatus::NoPlan(_)) || matches!(err.status, QuotaStatus::Unavailable(_)));
        std::fs::write(dir.path().join(".credentials.json"), CREDENTIALS).unwrap();
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(credentials(&env, 0).unwrap().plan.as_deref(), Some("Max 20x"));
            let expired = credentials(&env, 1_796_000_000_001).unwrap_err();
            assert!(expired.transient);
        }
        // A long-lived token in the instance's environment wins.
        let mut with_token = env.clone();
        with_token.push(("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into()));
        assert_eq!(credentials(&with_token, 0).unwrap().token, "tok");
    }
}
