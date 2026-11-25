//! Newline-delimited JSON over a child's stdio, shared by the ACP and Pi
//! adapters (Codex keeps its own, which omits the `jsonrpc` field).
//!
//! Two small tasks own the pipes: the reader forwards raw stdout lines in
//! wire order, the writer drains an unbounded queue into stdin. A session
//! never awaits a response inline; it parses every line itself, so
//! responses, notifications and agent requests are handled strictly in the
//! order the agent wrote them and nothing can deadlock on a pending request.

use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc;

use super::process::{AgentProcess, JsonlReader, JsonlWriter};

/// Raw stdout lines buffered between the reader and the session. Bounded
/// so a stalled session applies backpressure to the agent.
const LINE_BUFFER: usize = 512;

pub struct Pipe {
    pub process: AgentProcess,
    out: mpsc::UnboundedSender<String>,
    next_id: u64,
    /// Adds `"jsonrpc":"2.0"` to every message (JSON-RPC 2.0 peers).
    jsonrpc: bool,
    /// Shown in exit reasons ("Gemini CLI exited …").
    name: String,
}

impl Pipe {
    /// Starts `binary args…` and returns the pipe and its stdout lines.
    pub fn spawn(
        name: &str,
        binary: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        jsonrpc: bool,
    ) -> std::io::Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        let mut process = AgentProcess::spawn(binary, args, cwd, env, super::claude::HOST_ENV)?;
        let stdout = process.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stdin = process.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;

        let (line_tx, lines) = mpsc::channel(LINE_BUFFER);
        let label = name.to_owned();
        let exit = process.exit_watch();
        tokio::spawn(async move {
            let mut reader = JsonlReader::new(stdout).until_exit(exit);
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if line_tx.send(line.to_vec()).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(err) => {
                        log::warn!("{label} stdout: {err}");
                        return;
                    }
                }
            }
        });

        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let label = name.to_owned();
        tokio::spawn(async move {
            let mut writer = JsonlWriter::new(stdin);
            while let Some(line) = out_rx.recv().await {
                if let Err(err) = writer.send_raw(&line).await {
                    log::debug!("{label} stdin closed: {err}");
                    return;
                }
            }
        });
        Ok((Self { process, out, next_id: 1, jsonrpc, name: name.to_owned() }, lines))
    }

    fn prefix(&self) -> &'static str {
        if self.jsonrpc { "{\"jsonrpc\":\"2.0\"," } else { "{" }
    }

    /// Sends a JSON-RPC request with serialized `params`; returns its id.
    pub fn request(&mut self, method: &str, params: &sonic_rs::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = format!(
            "{}\"id\":{id},\"method\":{},\"params\":{}}}",
            self.prefix(),
            json_string(method),
            sonic_rs::to_string(params).unwrap_or_else(|_| "{}".into())
        );
        let _ = self.out.send(line);
        id
    }

    pub fn notify(&self, method: &str, params: &sonic_rs::Value) {
        let line = format!(
            "{}\"method\":{},\"params\":{}}}",
            self.prefix(),
            json_string(method),
            sonic_rs::to_string(params).unwrap_or_else(|_| "{}".into())
        );
        let _ = self.out.send(line);
    }

    /// Answers an agent request. `raw_id` is the id exactly as the agent sent
    /// it (number or string), `result` serialized JSON.
    pub fn respond(&self, raw_id: &str, result: &str) {
        let _ = self.out.send(format!("{}\"id\":{raw_id},\"result\":{result}}}", self.prefix()));
    }

    pub fn respond_error(&self, raw_id: &str, code: i64, message: &str) {
        let _ = self.out.send(format!(
            "{}\"id\":{raw_id},\"error\":{{\"code\":{code},\"message\":{}}}}}",
            self.prefix(),
            json_string(message)
        ));
    }

    /// Writes one raw JSON line (protocols that are not JSON-RPC).
    pub fn send_raw(&self, line: String) {
        let _ = self.out.send(line);
    }

    /// A fresh id for protocols that number their own commands.
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Why the process ended: exit status and the tail of stderr. Waits
    /// briefly for the exit status after stdout closed.
    pub async fn exit_reason(&mut self) -> String {
        let status = tokio::time::timeout(Duration::from_millis(500), self.process.wait()).await;
        let mut reason = match status {
            Ok(Ok(status)) => format!("{} exited ({status})", self.name),
            Ok(Err(err)) => format!("{} exited ({err})", self.name),
            Err(_) => format!("{} closed its output", self.name),
        };
        let tail = self.process.stderr_tail();
        if !tail.is_empty() {
            let count = tail.chars().count();
            let tail: String = tail.chars().skip(count.saturating_sub(600)).collect();
            reason.push_str(": ");
            reason.push_str(&tail);
        }
        reason
    }
}

pub fn json_string(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Resolves an instance's binary: a path is used as is, a bare name is
/// looked up on PATH (and the usual install folders).
pub fn resolve_command(explicit: Option<&Path>, default_name: &str, extra: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    match explicit {
        Some(path) if path.components().count() > 1 || path.is_absolute() => {
            super::process::resolve_binary(default_name, Some(path), extra)
        }
        Some(name) => super::process::resolve_binary(&name.to_string_lossy(), None, extra),
        None => super::process::resolve_binary(default_name, None, extra),
    }
}

/// The usual places npm, bun, pipx and installers put agent CLIs.
pub fn common_install_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(std::path::PathBuf::from(appdata).join("npm"));
    }
    if let Some(local) = dirs::data_local_dir() {
        dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
    }
    if let Some(home) = dirs::home_dir() {
        for dir in [".local/bin", ".bun/bin", ".npm-global/bin", ".cargo/bin", ".opencode/bin"] {
            dirs.push(home.join(dir));
        }
    }
    dirs
}
