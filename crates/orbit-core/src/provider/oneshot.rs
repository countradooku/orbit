//! One-shot text generation: a single prompt, a single answer, no tools,
//! no session left behind. Used for thread titles, commit messages and pull
//! request text.
//!
//! * **Claude Code** runs `claude -p` with a small model (`haiku`), no
//!   tools, no slash commands, no MCP servers, no hooks and no session
//!   persistence, with the prompt on stdin (never on the command line).
//! * **Codex** runs `codex exec --ephemeral` read-only with low reasoning
//!   effort and reads the last message from a file.
//! * **Every other runtime** (ACP agents, OpenCode, Pi) runs one headless
//!   turn through its own adapter in a scratch folder at read-only access,
//!   and keeps the final text.
//!
//! Each run is bounded by a timeout and its process tree dies with it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossfire::mpsc;
use orbit_proto::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::process::AgentProcess;
use super::{AgentEvent, BlockKind, Launch, SessionHandle, SessionInput, SessionSpec, TurnOutcome};

/// A one-shot run may take this long.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// The Claude model for one-shot work: fast and cheap.
pub const CLAUDE_MODEL: &str = "haiku";

/// Generates text for `prompt` on the instance `launch` describes.
pub async fn generate(launch: Launch, prompt: String) -> Result<String, String> {
    let run = async {
        match launch.kind {
            ProviderKind::Claude => claude(&launch, &prompt).await,
            ProviderKind::Codex => codex(&launch, &prompt).await,
            _ => {
                let (tx, rx) = mpsc::unbounded_async();
                let kind = launch.kind;
                let spec = headless_spec(&launch);
                let handle = super::start(kind, spec, tx);
                via_session(handle, rx, prompt).await
            }
        }
    };
    tokio::time::timeout(TIMEOUT, run)
        .await
        .unwrap_or_else(|_| Err(format!("No answer within {} seconds.", TIMEOUT.as_secs())))
}

/// A throwaway session spec: scratch folder, read-only, no resume.
pub fn headless_spec(launch: &Launch) -> SessionSpec {
    let cwd = scratch_dir();
    SessionSpec {
        thread: ThreadId::new(),
        cwd,
        config: ThreadConfig {
            provider: launch.kind,
            instance: launch.instance.clone(),
            access: Access::Readonly,
            ..Default::default()
        },
        resume: None,
        resume_at: None,
        binary: launch.binary.clone(),
        args: launch.args.clone(),
        env: launch.env.clone(),
        extra_dirs: Vec::new(),
        guard: Default::default(),
        mcp: Vec::new(),
    }
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("orbit-oneshot");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Runs one turn on a started session and returns its final text. The
/// session is shut down afterwards.
pub async fn via_session(
    handle: SessionHandle,
    rx: crossfire::AsyncRx<mpsc::List<(ThreadId, AgentEvent)>>,
    prompt: String,
) -> Result<String, String> {
    let _ = handle.send(SessionInput::Prompt { uuid: uuid::Uuid::new_v4().to_string(), text: prompt, attachments: Vec::new(), steer: false });
    let mut blocks: Vec<(String, String)> = Vec::new();
    let result = loop {
        let Ok((_, event)) = rx.recv().await else { break Err("The agent stopped without answering.".to_owned()) };
        match event {
            AgentEvent::Delta { block, kind: BlockKind::Text, text } => match blocks.iter_mut().find(|(b, _)| *b == block) {
                Some((_, acc)) => acc.push_str(&text),
                None => blocks.push((block, text)),
            },
            AgentEvent::BlockDone { block, kind: BlockKind::Text, text } => match blocks.iter_mut().find(|(b, _)| *b == block) {
                Some((_, acc)) => *acc = text,
                None => blocks.push((block, text)),
            },
            // A one-shot run never asks: decline anything it requests.
            AgentEvent::Request { provider_ref, .. } => {
                let _ = handle.send(SessionInput::Resolve { provider_ref, decision: Decision::Deny { feedback: String::new() } });
            }
            AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, .. } => {
                // The answer is the last text block.
                break blocks.last().map(|(_, t)| t.trim().to_owned()).filter(|t| !t.is_empty()).ok_or_else(|| "The agent answered with nothing.".to_owned());
            }
            AgentEvent::TurnFinished { outcome: TurnOutcome::Failed { message }, .. } => break Err(message),
            AgentEvent::TurnFinished { .. } => break Err("The agent stopped before answering.".to_owned()),
            AgentEvent::Exited { error } => break Err(error.unwrap_or_else(|| "The agent exited before answering.".to_owned())),
            _ => {}
        }
    };
    let _ = handle.send(SessionInput::Shutdown);
    result
}

/// Runs `binary args…` with `stdin` as input and returns stdout.
async fn run_cli(binary: &Path, args: &[String], env: &[(String, String)], stdin: &str) -> Result<(String, String), String> {
    let mut process = AgentProcess::spawn(binary, args, &scratch_dir(), env, super::claude::HOST_ENV)
        .map_err(|e| format!("Couldn't start {}: {e}", binary.display()))?;
    let mut input = process.stdin.take().ok_or("no stdin")?;
    let mut output = process.stdout.take().ok_or("no stdout")?;
    let text = stdin.to_owned();
    let writer = tokio::spawn(async move {
        let _ = input.write_all(text.as_bytes()).await;
        let _ = input.shutdown().await;
    });
    let mut out = String::new();
    output.read_to_string(&mut out).await.map_err(|e| e.to_string())?;
    let _ = writer.await;
    let status = process.wait().await.map_err(|e| e.to_string())?;
    let tail = process.stderr_tail();
    if !status.success() && out.trim().is_empty() {
        return Err(if tail.is_empty() { format!("exited with {status}") } else { tail });
    }
    Ok((out, tail))
}

async fn claude(launch: &Launch, prompt: &str) -> Result<String, String> {
    let binary = super::claude::resolve(launch.binary.as_deref()).ok_or("Claude Code is not installed.")?;
    let args: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--model",
        CLAUDE_MODEL,
        "--tools",
        "",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--permission-mode",
        "dontAsk",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let mut env = launch.env.clone();
    env.push(("CLAUDE_CODE_ENTRYPOINT".into(), "orbit".into()));
    env.push(("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into()));
    let (out, tail) = run_cli(&binary, &args, &env, prompt).await?;
    parse_claude(&out).map_err(|e| if tail.is_empty() { e } else { format!("{e}: {tail}") })
}

/// The `result` of `claude -p --output-format json`.
pub fn parse_claude(out: &str) -> Result<String, String> {
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    // The envelope is the last JSON line (older versions print a list).
    let line = out.lines().rev().find(|l| l.trim_start().starts_with(['{', '['])).ok_or("Claude Code printed no result")?;
    let value: sonic_rs::Value = sonic_rs::from_str(line).map_err(|e| format!("Unreadable result: {e}"))?;
    let result = if value.is_array() {
        value.as_array().and_then(|a| a.iter().rev().find(|v| v.get("type").and_then(|t| t.as_str()) == Some("result")).cloned())
    } else {
        Some(value)
    }
    .ok_or("Claude Code printed no result")?;
    let text = result.get("result").and_then(|r| r.as_str()).unwrap_or("").to_owned();
    if result.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
        return Err(if text.is_empty() { "Claude Code reported an error".into() } else { text });
    }
    if text.trim().is_empty() {
        return Err("Claude Code answered with nothing".into());
    }
    Ok(text.trim().to_owned())
}

async fn codex(launch: &Launch, prompt: &str) -> Result<String, String> {
    let binary = super::process::resolve_binary("codex", launch.binary.as_deref(), &[]).ok_or("Codex is not installed.")?;
    let out_file = scratch_dir().join(format!("codex-{}.txt", uuid::Uuid::new_v4().simple()));
    let args: Vec<String> = vec![
        "exec".into(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "-s".into(),
        "read-only".into(),
        "-c".into(),
        "model_reasoning_effort=\"low\"".into(),
        "-c".into(),
        "features.hooks=false".into(),
        "--output-last-message".into(),
        out_file.display().to_string(),
        "-".into(),
    ];
    let result = run_cli(&binary, &args, &launch.env, prompt).await;
    let text = std::fs::read_to_string(&out_file).unwrap_or_default();
    let _ = std::fs::remove_file(&out_file);
    let (stdout, _) = result?;
    let text = if text.trim().is_empty() { stdout } else { text };
    let text = text.trim();
    if text.is_empty() { Err("Codex answered with nothing".into()) } else { Ok(text.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_results_are_read_from_the_envelope() {
        let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"Fix the login bug\n"}"#;
        assert_eq!(parse_claude(out).unwrap(), "Fix the login bug");
        let err = r#"{"type":"result","is_error":true,"result":"Not logged in"}"#;
        assert_eq!(parse_claude(err).unwrap_err(), "Not logged in");
        let list = r#"[{"type":"system"},{"type":"result","is_error":false,"result":"ok"}]"#;
        assert_eq!(parse_claude(list).unwrap(), "ok");
    }

    #[tokio::test]
    async fn a_headless_session_returns_its_last_text() {
        let (input, inputs) = mpsc::bounded_blocking_async::<SessionInput>(8);
        let (tx, rx) = mpsc::unbounded_async();
        tokio::spawn(async move {
            use crossfire::BlockingTxTrait;
            let thread = ThreadId::new();
            while let Ok(msg) = inputs.recv().await {
                if let SessionInput::Prompt { .. } = msg {
                    let _ = tx.send((thread, AgentEvent::Delta { block: "a".into(), kind: BlockKind::Text, text: "Title".into() }));
                    let _ = tx.send((thread, AgentEvent::Delta { block: "a".into(), kind: BlockKind::Text, text: " here".into() }));
                    let _ = tx.send((thread, AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, usage: Usage::default(), duration_ms: 1 }));
                }
            }
        });
        let text = via_session(SessionHandle { input }, rx, "name it".into()).await.unwrap();
        assert_eq!(text, "Title here");
    }
}
