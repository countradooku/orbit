//! One-shot runs: a single prompt to an agent CLI, non-interactively, for
//! small text tasks (commit messages, pull request descriptions).
//!
//! The run has no tools, no MCP servers, no hooks and no saved session, so
//! it is quick, cannot change files, and never triggers the user's notify
//! hooks (T3 Code #41799, #40529). It uses the same binary and environment
//! as the thread's sessions, so it signs in the same way.

use std::path::{Path, PathBuf};
use std::time::Duration;

use orbit_proto::ProviderKind;
use sonic_rs::JsonValueTrait as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::process::{AgentProcess, resolve_binary};

pub const TIMEOUT: Duration = Duration::from_secs(120);

/// What to run.
#[derive(Clone, Debug)]
pub struct OneShot {
    pub kind: ProviderKind,
    /// The resolved CLI, or `None` to look it up on PATH.
    pub binary: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Provider model slug; empty for the provider's default.
    pub model: String,
}

/// Runs `prompt` and returns the agent's reply text.
pub async fn run(spec: OneShot, prompt: &str) -> Result<String, String> {
    let name = match spec.kind {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
    };
    let program = match &spec.binary {
        Some(path) if path.is_file() => path.clone(),
        other => resolve_binary(name, other.as_deref(), &[])
            .ok_or_else(|| format!("{} isn’t installed, so it can’t write this text.", spec.kind.label()))?,
    };
    let work = async {
        match spec.kind {
            ProviderKind::Claude => claude(&program, &spec, prompt).await,
            ProviderKind::Codex => codex(&program, &spec, prompt).await,
        }
    };
    tokio::time::timeout(TIMEOUT, work)
        .await
        .map_err(|_| format!("{} didn’t answer within {} seconds.", spec.kind.label(), TIMEOUT.as_secs()))?
}

async fn claude(program: &Path, spec: &OneShot, prompt: &str) -> Result<String, String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--permission-mode",
        "default",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !spec.model.is_empty() && spec.model != "default" {
        args.extend(["--model".into(), spec.model.clone()]);
    }
    let mut env = vec![
        ("CLAUDE_CODE_ENTRYPOINT".to_owned(), "orbit".to_owned()),
        ("ENABLE_CLAUDEAI_MCP_SERVERS".to_owned(), "false".to_owned()),
        ("CLAUDE_CODE_AUTO_CONNECT_IDE".to_owned(), "0".to_owned()),
    ];
    env.extend(spec.env.iter().cloned());
    let out = exchange(program, &args, &spec.cwd, &env, prompt).await?;
    parse_claude(&out.0).ok_or_else(|| failure("Claude Code", &out))
}

/// The `result` of `claude -p --output-format json`, or `None` on an error
/// result or unreadable output.
pub fn parse_claude(stdout: &str) -> Option<String> {
    // Some builds print a JSON array of events; the result is the last.
    let line = stdout.lines().rev().find(|l| l.trim_start().starts_with(['{', '[']))?;
    let v: sonic_rs::Value = sonic_rs::from_str(line).ok()?;
    let result = if v.is_array() {
        use sonic_rs::JsonContainerTrait as _;
        v.as_array()?.iter().rev().find(|e| e.get("type").and_then(|t| t.as_str()) == Some("result"))?.clone()
    } else {
        v
    };
    if result.get("is_error").and_then(|e| e.as_bool()) == Some(true) {
        return None;
    }
    result.get("result").and_then(|r| r.as_str()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

async fn codex(program: &Path, spec: &OneShot, prompt: &str) -> Result<String, String> {
    let last = std::env::temp_dir().join(format!("orbit-one-shot-{}.txt", uuid::Uuid::new_v4().simple()));
    let mut args: Vec<String> = ["exec", "--skip-git-repo-check", "--sandbox", "read-only", "--color", "never", "--output-last-message"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(last.display().to_string());
    if !spec.model.is_empty() {
        args.extend(["--model".into(), spec.model.clone()]);
    }
    args.push("-".into());
    let out = exchange(program, &args, &spec.cwd, &spec.env, prompt).await;
    let written = std::fs::read_to_string(&last).ok();
    let _ = std::fs::remove_file(&last);
    let out = out?;
    written.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).ok_or_else(|| failure("Codex", &out))
}

/// stdout, stderr tail and success.
type Exchanged = (String, String, bool);

async fn exchange(program: &Path, args: &[String], cwd: &Path, env: &[(String, String)], prompt: &str) -> Result<Exchanged, String> {
    // Variables that make a CLI think it runs inside another agent session.
    const NESTED: &[&str] = &["CLAUDECODE", "CLAUDE_CODE_SSE_PORT", "CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED"];
    let mut process =
        AgentProcess::spawn(program, args, cwd, env, NESTED).map_err(|e| format!("Couldn’t start {}: {e}", program.display()))?;
    let (Some(mut stdin), Some(mut stdout)) = (process.stdin.take(), process.stdout.take()) else {
        return Err("Couldn’t talk to the agent.".into());
    };
    let feed = async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    };
    let read = async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    };
    let ((), bytes) = tokio::join!(feed, read);
    let status = process.wait().await.map_err(|e| e.to_string())?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), process.stderr_tail(), status.success()))
}

fn failure(label: &str, out: &Exchanged) -> String {
    let (stdout, stderr, _) = out;
    let detail = if !stderr.trim().is_empty() { stderr.trim() } else { stdout.trim() };
    let detail: String = detail.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    if detail.is_empty() { format!("{label} returned no text.") } else { format!("{label} couldn’t write the text: {detail}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_results_are_read() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2100,"result":"Fix the cache key\n","session_id":"x"}"#;
        assert_eq!(parse_claude(ok).as_deref(), Some("Fix the cache key"));
        let err = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Not logged in"}"#;
        assert_eq!(parse_claude(err), None);
        let array = r#"[{"type":"system"},{"type":"result","is_error":false,"result":"Title"}]"#;
        assert_eq!(parse_claude(array).as_deref(), Some("Title"));
        assert_eq!(parse_claude("Warning: x\n"), None);
    }
}
