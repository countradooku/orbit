//! Fakes for the bot tests: a local HTTP server standing in for the
//! platforms' REST APIs, a local websocket server (sockudo-ws) for Slack's
//! Socket Mode and Discord's gateway, a scripted agent, and an engine to
//! run them against. Nothing here touches the network beyond loopback.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossfire::{BlockingTxTrait, mpsc};
use orbit_core::engine::Launcher;
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use sockudo_ws::server::WebSocketServer;
use sockudo_ws::{Config, Http1, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ── HTTP ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> sonic_rs::Value {
        sonic_rs::from_slice(&self.body).unwrap_or_default()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Resp {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    /// Held this long before answering (a long poll).
    pub delay: Duration,
}

impl Resp {
    pub fn json(body: impl Into<String>) -> Self {
        Self { status: 200, body: body.into(), headers: Vec::new(), delay: Duration::ZERO }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self { status, body: body.into(), headers: Vec::new(), delay: Duration::ZERO }
    }

    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

/// A loopback HTTP/1.1 server that records every request.
pub struct FakeHttp {
    pub url: String,
    pub log: Arc<Mutex<Vec<Req>>>,
}

impl FakeHttp {
    pub async fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> FakeHttp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let log: Arc<Mutex<Vec<Req>>> = Arc::default();
        let handler: Handler = Arc::new(handler);
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let handler = handler.clone();
                let log = log2.clone();
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut stream).await else { return };
                    log.lock().unwrap().push(req.clone());
                    let resp = handler(&req);
                    if !resp.delay.is_zero() {
                        tokio::time::sleep(resp.delay).await;
                    }
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        resp.status,
                        resp.body.len()
                    );
                    for (n, v) in &resp.headers {
                        head.push_str(&format!("{n}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(resp.body.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        FakeHttp { url, log }
    }

    pub fn requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }

    /// Requests whose path ends with `suffix`.
    pub fn calls(&self, suffix: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.ends_with(suffix)).collect()
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':').map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_owned())))
        .collect();
    let length: usize = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Req { method, path, headers, body })
}

// ── WebSocket ────────────────────────────────────────────────────────────

/// One accepted websocket connection, driven by the test.
pub struct WsConn {
    to_client: tokio::sync::mpsc::UnboundedSender<WsOut>,
    from_client: tokio::sync::mpsc::UnboundedReceiver<String>,
    pub path: String,
}

enum WsOut {
    Text(String),
    Close(u16, String),
}

impl WsConn {
    pub fn send(&self, text: impl Into<String>) {
        let _ = self.to_client.send(WsOut::Text(text.into()));
    }

    pub fn close(&self, code: u16, reason: &str) {
        let _ = self.to_client.send(WsOut::Close(code, reason.into()));
    }

    /// The next text frame from the client, waiting up to `secs`.
    pub async fn recv(&mut self, secs: u64) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(secs), self.from_client.recv()).await.ok().flatten()
    }

    /// The next frame whose JSON has `field == value` (skipping others).
    pub async fn recv_where(&mut self, secs: u64, pred: impl Fn(&sonic_rs::Value) -> bool) -> Option<sonic_rs::Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now()).as_secs().max(1);
            let text = self.recv(left).await?;
            let value: sonic_rs::Value = sonic_rs::from_str(&text).unwrap_or_default();
            if pred(&value) {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
        }
    }
}

pub struct FakeWs {
    pub url: String,
    conns: tokio::sync::mpsc::UnboundedReceiver<WsConn>,
}

impl FakeWs {
    pub async fn start() -> FakeWs {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, conns) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let server = WebSocketServer::<Http1>::new(Config::default());
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let server = server.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok((ws, handshake)) = server.accept(stream).await else { return };
                    let (mut reader, mut writer) = ws.split();
                    let (to_client, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<WsOut>();
                    let (incoming, from_client) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let _ = tx.send(WsConn { to_client, from_client, path: handshake.path.clone() });
                    tokio::spawn(async move {
                        while let Some(out) = outgoing.recv().await {
                            match out {
                                WsOut::Text(t) => {
                                    if writer.send(Message::Text(Bytes::from(t))).await.is_err() {
                                        return;
                                    }
                                }
                                WsOut::Close(code, reason) => {
                                    let _ = writer.close(code, &reason).await;
                                    return;
                                }
                            }
                        }
                    });
                    while let Some(Ok(msg)) = reader.next().await {
                        if let Message::Text(b) = msg {
                            let _ = incoming.send(String::from_utf8_lossy(&b).into_owned());
                        }
                    }
                });
            }
        });
        FakeWs { url, conns }
    }

    pub async fn accept(&mut self, secs: u64) -> Option<WsConn> {
        tokio::time::timeout(Duration::from_secs(secs), self.conns.recv()).await.ok().flatten()
    }
}

// ── Engine and agent ─────────────────────────────────────────────────────

/// A scripted agent. By prompt: `ask` raises a command approval (`cargo
/// build`), `push` a guarded one (`git push --force`), `pick` a question
/// with three options, `long` answers with 9 000 characters and a long
/// code block, `tool` runs an edit before answering; anything else answers
/// `echo: <text>`.
pub fn launcher() -> Launcher {
    Box::new(|_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let emit = |e: AgentEvent| {
                let _ = events.send((thread, e));
            };
            emit(AgentEvent::Ready { session_id: "fake".into(), model: None });
            let mut turn = 0;
            while let Ok(msg) = rx.recv().await {
                let SessionInput::Prompt { text, steer: false, .. } = msg else {
                    if matches!(msg, SessionInput::Shutdown) {
                        return;
                    }
                    continue;
                };
                turn += 1;
                let block = format!("t{turn}");
                emit(AgentEvent::TurnStarted { prompt: Some("p".into()) });
                let say = |text: String| emit(AgentEvent::BlockDone { block: block.clone(), kind: BlockKind::Text, text });
                let request = match text.as_str() {
                    "ask" => Some(RequestKind::Command { command: "cargo build".into(), cwd: ".".into(), reason: String::new() }),
                    "push" => Some(RequestKind::Command {
                        command: "git push --force origin main".into(),
                        cwd: ".".into(),
                        reason: String::new(),
                    }),
                    "pick" => Some(RequestKind::Question {
                        question: "Which colour?".into(),
                        options: vec![
                            QuestionOption::new("Red", "Warm"),
                            QuestionOption::new("Blue", "Calm"),
                            QuestionOption::from("Green"),
                        ],
                        multi_select: false,
                    }),
                    _ => None,
                };
                if let Some(kind) = request {
                    emit(AgentEvent::Request { provider_ref: format!("r{turn}"), kind });
                    loop {
                        match rx.recv().await {
                            Ok(SessionInput::Resolve { decision, .. }) => {
                                say(format!("decision: {decision:?}"));
                                break;
                            }
                            Ok(SessionInput::Shutdown) | Err(_) => return,
                            Ok(_) => {}
                        }
                    }
                } else if text == "long" {
                    let prose: String = (0..300).map(|i| format!("Line {i} of the answer, long enough to matter.\n")).collect();
                    let code: String = (0..60).map(|i| format!("let x{i} = {i};\n")).collect();
                    say(format!("{prose}\n```rust\n{code}```\nDone."));
                } else if text == "tool" {
                    let call = orbit_proto::ToolCall {
                        call_id: "c1".into(),
                        kind: ToolKind::Edit,
                        name: "Edit".into(),
                        title: "src/main.rs".into(),
                        input: "{}".into(),
                        output: String::new(),
                        output_blob: None,
                        exit_code: None,
                        files: Vec::new(),
                    };
                    emit(AgentEvent::ToolStarted(call));
                    tokio::time::sleep(Duration::from_millis(5500)).await;
                    emit(AgentEvent::ToolFinished { call_id: "c1".into(), output: "ok".into(), is_error: false, exit_code: Some(0), files: Vec::new() });
                    say("edited".into());
                } else {
                    say(format!("echo: {text}"));
                }
                emit(AgentEvent::TurnFinished {
                    outcome: orbit_core::provider::TurnOutcome::Completed,
                    usage: Usage::default(),
                    duration_ms: 1,
                });
            }
        });
        SessionHandle { input }
    })
}

pub struct World {
    pub engine: EngineHandle,
    pub dir: tempfile::TempDir,
    pub bots: Option<orbit_bots::Bots>,
}

impl World {
    pub fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let engine =
            EngineHandle::start_with(EngineOptions { data_dir: dir.path().to_owned(), probe_providers: false }, launcher()).unwrap();
        World { engine, dir, bots: None }
    }

    pub fn start_bots(&mut self, endpoints: orbit_bots::Endpoints) {
        self.bots = Some(orbit_bots::Bots::start_with(self.engine.clone(), self.dir.path(), endpoints).unwrap());
    }

    pub async fn call(&self, command: Command) -> Result<Reply, String> {
        self.engine.call(command).await.unwrap()
    }

    pub async fn bots(&self, command: BotCommand) -> Result<Reply, String> {
        self.call(Command::Bots(command)).await
    }

    pub async fn state(&self) -> BotsState {
        match self.bots(BotCommand::State).await {
            Ok(Reply::Bots(BotReply::State(s))) => s,
            other => panic!("{other:?}"),
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(bots) = self.bots.take() {
            bots.stop();
        }
        self.engine.shutdown();
    }
}

pub fn endpoints(telegram: &str, slack: &str, discord: &str) -> orbit_bots::Endpoints {
    orbit_bots::Endpoints {
        telegram: telegram.into(),
        slack: slack.into(),
        discord: discord.into(),
        backoff_min_ms: 50,
        backoff_max_ms: 200,
        pace_ms: Some(10),
    }
}

/// Polls `check` every 20 ms until it returns `Some`, for up to `secs`.
pub async fn eventually<T>(secs: u64, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
