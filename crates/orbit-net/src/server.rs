//! The `orbitd` server: the Orbit protocol over WebSockets and over the
//! local transport ([`crate::transport`]).
//!
//! One binary frame per message, wincode payloads (see `orbit_proto::link`),
//! WebSocket subprotocol `orbit.v1`. The first frame must be a `Hello`
//! carrying the host's token, or a JSON control request (see
//! [`crate::host`]); anything else closes the connection. Events are
//! coalesced into frames of up to [`MAX_BATCH`](crate::MAX_BATCH) envelopes,
//! so streaming costs one frame per burst instead of one per token.
//!
//! The host's own commands (`Command::Host`: attach a window, hold, restart)
//! are answered here from a [`HostControl`]; the rest go to the engine.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use crossfire::mpsc;
use orbit_core::EngineHandle;
use orbit_proto::link::ServerEncoder;
use orbit_proto::*;
use sockudo_ws::server::WebSocketServer;
use sockudo_ws::{Config, Http1, Message};
use tokio::net::TcpListener;

use crate::host::{ControlError, ControlRequest, HostControl, HostStatus, is_control};
use crate::transport::{FrameReader, FrameWriter, LocalListener};

pub struct ServerOptions {
    pub listen: SocketAddr,
    /// Clients must present this token in `Hello`.
    pub token: String,
}

/// Accepts connections until the listener fails.
pub async fn serve(engine: EngineHandle, options: ServerOptions) -> std::io::Result<()> {
    let listener = TcpListener::bind(options.listen).await?;
    serve_on(engine, listener, options.token).await
}

pub async fn serve_on(engine: EngineHandle, listener: TcpListener, token: String) -> std::io::Result<()> {
    serve_ws(engine, listener, token, None).await
}

/// WebSockets on `listener`; host commands answered from `control`.
pub async fn serve_ws(
    engine: EngineHandle,
    listener: TcpListener,
    token: String,
    control: Option<Arc<HostControl>>,
) -> std::io::Result<()> {
    let config = Config::builder()
        .max_payload_length(64 * 1024 * 1024)
        .ping_interval(20)
        .pong_timeout(10)
        .build();
    let server = WebSocketServer::<Http1>::new(config)
        .protocols([WS_SUBPROTOCOL])
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    log::info!("orbitd listening on {}", listener.local_addr()?);
    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let server = server.clone();
        let engine = engine.clone();
        let token = token.clone();
        let control = control.clone();
        tokio::spawn(async move {
            match server.accept(stream).await {
                Ok((ws, _)) => {
                    let (reader, writer) = ws.split();
                    if let Err(err) = connection(WsRx(reader), WsTx(writer), engine, &token, control).await {
                        log::info!("{peer}: {err}");
                    }
                }
                Err(err) => log::info!("{peer}: handshake failed: {err}"),
            }
        });
    }
}

/// The local transport (named pipe or Unix socket) on `listener`.
pub async fn serve_local(
    engine: EngineHandle,
    mut listener: LocalListener,
    token: String,
    control: Option<Arc<HostControl>>,
) -> std::io::Result<()> {
    loop {
        let (reader, writer) = listener.accept().await?;
        let engine = engine.clone();
        let token = token.clone();
        let control = control.clone();
        tokio::spawn(async move {
            if let Err(err) = connection(LocalRx(reader), LocalTx(writer), engine, &token, control).await {
                log::info!("local client: {err}");
            }
        });
    }
}

// ── Transports ───────────────────────────────────────────────────────────

/// The receiving half of a connection: binary frames until it closes.
trait LinkRx: Send + 'static {
    /// `None` when the peer closed.
    fn recv(&mut self) -> impl Future<Output = Option<Result<Bytes, String>>> + Send;
}

trait LinkTx: Send + 'static {
    fn send(&mut self, frame: Vec<u8>) -> impl Future<Output = Result<(), String>> + Send;
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

struct WsRx<S>(sockudo_ws::SplitReader<S>);
struct WsTx<S>(sockudo_ws::SplitWriter<S>);

impl<S> LinkRx for WsRx<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn recv(&mut self) -> Option<Result<Bytes, String>> {
        loop {
            match self.0.next().await {
                Some(Ok(Message::Binary(data))) => return Some(Ok(data)),
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Some(Err(e.to_string())),
            }
        }
    }
}

impl<S> LinkTx for WsTx<S>
where
    S: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), String> {
        self.0.send(Message::Binary(Bytes::from(frame))).await.map_err(|e| e.to_string())
    }

    async fn close(&mut self) {
        let _ = self.0.close(1000, "bye").await;
    }
}

struct LocalRx(FrameReader);
struct LocalTx(FrameWriter);

impl LinkRx for LocalRx {
    async fn recv(&mut self) -> Option<Result<Bytes, String>> {
        self.0.next().await.map_err(|e| e.to_string()).transpose()
    }
}

impl LinkTx for LocalTx {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), String> {
        self.0.send(&frame).await.map_err(|e| e.to_string())
    }

    async fn close(&mut self) {
        self.0.close().await;
    }
}

// ── A connection ─────────────────────────────────────────────────────────

/// Constant-time comparison, so the token cannot be guessed byte by byte
/// from response timing.
fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn connection(
    mut reader: impl LinkRx,
    mut writer: impl LinkTx,
    engine: EngineHandle,
    token: &str,
    control: Option<Arc<HostControl>>,
) -> Result<(), String> {
    // Frames for this link: hot messages skip the big enums (see `link`).
    let mut link = ServerEncoder::new();

    // Handshake.
    let first = match reader.recv().await {
        Some(Ok(data)) => data,
        Some(Err(e)) => return Err(e),
        None => return Ok(()),
    };
    if is_control(&first) {
        let reply = control_request(&first, &engine, token, control.as_ref()).await;
        let _ = writer.send(reply).await;
        writer.close().await;
        return Ok(());
    }
    let hello = decode::<ClientMsg>(&first).map_err(|e| e.to_string())?;
    let ClientMsg::Hello { version, token: presented, epoch, since } = hello else {
        return Err("first message was not Hello".into());
    };
    if version != PROTOCOL_VERSION {
        let reason = format!("This host speaks protocol {PROTOCOL_VERSION}; update Orbit to connect.");
        let _ = writer.send(link.message(&ServerMsg::Goodbye { reason })).await;
        return Err("protocol mismatch".into());
    }
    if !same_token(&presented, token) {
        let _ = writer.send(link.message(&ServerMsg::Goodbye { reason: "The access token is wrong.".into() })).await;
        return Err("bad token".into());
    }
    let sub = engine.subscribe(epoch, since).await.map_err(|_| "engine stopped")?;
    writer.send(link.message(&ServerMsg::Welcome { version: PROTOCOL_VERSION, snapshot: sub.snapshot.map(Box::new) })).await?;
    if !sub.replay.is_empty() {
        for chunk in sub.replay.chunks(crate::MAX_BATCH) {
            for bytes in link.events(chunk.to_vec()) {
                writer.send(bytes).await?;
            }
        }
    }

    // Replies are produced by spawned tasks and written by the writer loop.
    let (reply_tx, reply_rx) = mpsc::unbounded_async::<ServerMsg>();
    let events = sub.events;
    let reader_task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            // The reader decodes calls with its own reused buffers.
            let mut calls = ServerEncoder::new();
            // An attached window, released when the connection ends.
            let mut window = None;
            while let Some(Ok(data)) = reader.recv().await {
                let Ok(ClientMsg::Call { id, command }) = calls.client(&data) else { continue };
                let tx = reply_tx.clone();
                if let (Some(control), Command::Host(host)) = (&control, &command) {
                    let answered = match host {
                        HostCommand::AttachUi => {
                            window.get_or_insert_with(|| control.attach_window());
                            Some(Ok(Reply::Done))
                        }
                        HostCommand::Hold { ms } => {
                            control.hold(*ms);
                            Some(Ok(Reply::Done))
                        }
                        HostCommand::Restart { when } => {
                            control.request_restart(*when);
                            Some(Ok(Reply::Done))
                        }
                        HostCommand::Activity => {
                            let control = control.clone();
                            let reply = engine.call(command);
                            tokio::spawn(async move {
                                let result = match reply.await {
                                    Ok(Ok(Reply::Host(HostReply::Activity(activity)))) => {
                                        Ok(Reply::Host(HostReply::Activity(fill_host(activity, &control))))
                                    }
                                    Ok(other) => other,
                                    Err(_) => Err("The host is shutting down.".into()),
                                };
                                let _ = crossfire::BlockingTxTrait::send(&tx, ServerMsg::Reply { id, result });
                            });
                            continue;
                        }
                        _ => None,
                    };
                    if let Some(result) = answered {
                        let _ = crossfire::BlockingTxTrait::send(&tx, ServerMsg::Reply { id, result });
                        continue;
                    }
                }
                let reply = engine.call(command);
                tokio::spawn(async move {
                    let result = reply.await.unwrap_or_else(|_| Err("The host is shutting down.".into()));
                    let _ = crossfire::BlockingTxTrait::send(&tx, ServerMsg::Reply { id, result });
                });
            }
        })
    };

    let result = loop {
        tokio::select! {
            reply = reply_rx.recv() => match reply {
                Ok(msg) => {
                    // The engine emits a command's events before it replies,
                    // so they are queued by now: send them first. A client
                    // that reads its state after a reply then sees what the
                    // command did (the two queues would otherwise race).
                    let mut frames = Vec::new();
                    let mut batch = Vec::new();
                    while let Ok(env) = events.try_recv() {
                        batch.push(env);
                        if batch.len() == crate::MAX_BATCH {
                            frames.extend(link.events(std::mem::take(&mut batch)));
                        }
                    }
                    if !batch.is_empty() {
                        frames.extend(link.events(batch));
                    }
                    frames.push(link.message(&msg));
                    let mut failed = None;
                    for bytes in frames {
                        if let Err(e) = writer.send(bytes).await {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break Err(e);
                    }
                }
                Err(_) => break Ok(()),
            },
            env = events.recv() => match env {
                Ok(first) => {
                    let mut batch = Vec::with_capacity(32);
                    batch.push(first);
                    while batch.len() < crate::MAX_BATCH {
                        match events.try_recv() {
                            Ok(env) => batch.push(env),
                            Err(_) => break,
                        }
                    }
                    let mut failed = None;
                    for bytes in link.events(batch) {
                        if let Err(e) = writer.send(bytes).await {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break Err(e);
                    }
                }
                Err(_) => {
                    // The engine dropped this subscriber because it fell
                    // behind; the client reconnects and replays from its seq.
                    let _ = writer.send(link.message(&ServerMsg::Goodbye { reason: "resync".into() })).await;
                    break Ok(());
                }
            },
        }
        if reader_task.is_finished() {
            break Ok(());
        }
    };
    reader_task.abort();
    writer.close().await;
    result
}

/// The engine's activity with the host process's own facts.
fn fill_host(mut activity: HostActivity, control: &HostControl) -> HostActivity {
    activity.pid = std::process::id();
    activity.version = env!("CARGO_PKG_VERSION").to_owned();
    activity.protocol = PROTOCOL_VERSION;
    activity.started_ms = control.started_ms();
    activity.windows = control.windows();
    activity.restart_pending = control.restart().is_some();
    activity
}

/// Answers a JSON control request (see [`crate::host`]).
async fn control_request(frame: &[u8], engine: &EngineHandle, token: &str, control: Option<&Arc<HostControl>>) -> Vec<u8> {
    let error = |message: &str| serde_json::to_vec(&ControlError { error: message.to_owned() }).unwrap_or_default();
    let Ok(request) = serde_json::from_slice::<ControlRequest>(frame) else { return error("bad request") };
    if !same_token(&request.token, token) {
        return error("The access token is wrong.");
    }
    let Some(control) = control else { return error("This engine runs inside the app: there is no host to control.") };
    match request.op.as_str() {
        "status" => {}
        "restart" => control.request_restart(request.restart_when()),
        _ => return error("unknown request"),
    }
    let activity = match engine.call(Command::Host(HostCommand::Activity)).await {
        Ok(Ok(Reply::Host(HostReply::Activity(activity)))) => activity,
        _ => HostActivity::default(),
    };
    serde_json::to_vec(&HostStatus::from_activity(&fill_host(activity, control))).unwrap_or_default()
}
