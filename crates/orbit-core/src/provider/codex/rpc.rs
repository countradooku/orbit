//! JSON-RPC transport to one `codex app-server` process.
//!
//! Two small tasks own the pipes: the reader forwards raw stdout lines in
//! wire order, the writer drains an unbounded queue into stdin. The session
//! never awaits a response inline: it parses every line itself, so
//! responses, notifications and server requests are handled strictly in the
//! order Codex wrote them and nothing can deadlock on a pending request
//! (T3 Code's reader dispatched notifications inline and had to fork every
//! handler that issued a request to avoid hanging, §10.2–10.3).

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use super::super::process::{AgentProcess, JsonlReader, JsonlWriter};

/// Raw stdout lines buffered between the reader and the session. Bounded
/// so a stalled session applies backpressure to Codex instead of growing.
const LINE_BUFFER: usize = 512;

pub struct Connection {
    pub process: AgentProcess,
    out: mpsc::UnboundedSender<String>,
    next_id: u64,
}

#[derive(Serialize)]
struct Request<'a, P: Serialize> {
    id: u64,
    method: &'a str,
    params: &'a P,
}

#[derive(Serialize)]
struct Notification<'a> {
    method: &'a str,
}

impl Connection {
    /// Starts `codex app-server` and returns the connection and its stdout
    /// lines, in wire order.
    pub fn spawn(binary: &Path, cwd: &Path, env: &[(String, String)]) -> std::io::Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        Self::spawn_with(binary, &[], cwd, env)
    }

    /// Like [`Connection::spawn`], with `-c key=value` configuration
    /// overrides for this process only (a session's MCP servers).
    pub fn spawn_with(
        binary: &Path,
        overrides: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> std::io::Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        let mut args: Vec<&str> = overrides.iter().map(String::as_str).collect();
        args.push("app-server");
        let mut process = AgentProcess::spawn(binary, &args, cwd, env, &[])?;
        let stdout = process.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stdin = process.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;

        let (line_tx, lines) = mpsc::channel(LINE_BUFFER);
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
                        log::warn!("codex stdout: {err}");
                        return;
                    }
                }
            }
        });

        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut writer = JsonlWriter::new(stdin);
            while let Some(line) = out_rx.recv().await {
                if let Err(err) = writer.send_raw(&line).await {
                    log::debug!("codex stdin closed: {err}");
                    return;
                }
            }
        });

        Ok((Self { process, out, next_id: 1 }, lines))
    }

    /// Sends a request and returns its id.
    pub fn request<P: Serialize>(&mut self, method: &str, params: &P) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        match sonic_rs::to_string(&Request { id, method, params }) {
            Ok(line) => {
                let _ = self.out.send(line);
            }
            Err(err) => log::error!("codex: cannot encode {method}: {err}"),
        }
        id
    }

    pub fn notify(&self, method: &str) {
        if let Ok(line) = sonic_rs::to_string(&Notification { method }) {
            let _ = self.out.send(line);
        }
    }

    /// Answers a server request. `raw_id` is the request id exactly as Codex
    /// sent it (number or string), `result` serialized JSON.
    pub fn respond(&self, raw_id: &str, result: &str) {
        let _ = self.out.send(format!("{{\"id\":{raw_id},\"result\":{result}}}"));
    }

    pub fn respond_error(&self, raw_id: &str, code: i64, message: &str) {
        let message = sonic_rs::to_string(message).unwrap_or_else(|_| "\"error\"".into());
        let _ = self
            .out
            .send(format!("{{\"id\":{raw_id},\"error\":{{\"code\":{code},\"message\":{message}}}}}"));
    }

    /// Why the process ended: exit status and the tail of stderr. Waits
    /// briefly for the exit status after stdout closed.
    pub async fn exit_reason(&mut self) -> String {
        let status = tokio::time::timeout(Duration::from_millis(500), self.process.wait()).await;
        let mut reason = match status {
            Ok(Ok(status)) => format!("Codex app-server exited ({status})"),
            Ok(Err(err)) => format!("Codex app-server exited ({err})"),
            Err(_) => "Codex app-server closed its output".to_owned(),
        };
        let tail = self.process.stderr_tail();
        if !tail.is_empty() {
            let tail: String = last_chars(&tail, 600);
            reason.push_str(": ");
            reason.push_str(&tail);
        }
        reason
    }
}

fn last_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}
