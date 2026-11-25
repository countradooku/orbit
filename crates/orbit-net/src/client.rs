//! The remote client: a connection to an `orbitd` host that survives drops.
//!
//! It runs on its own thread with a single-threaded Tokio runtime, so it
//! works from any UI framework. On disconnect it reconnects with backoff and
//! asks for the events after the last one it delivered; the UI sees a
//! `Status` update and, at worst, a `Reset`, never a blank state.
//!
//! The host is reached over WebSockets (`ws://`) or the local transport
//! (`pipe:`, `unix:`; see [`crate::transport`]). [`RemoteClient::connect_with`]
//! asks where to connect before every attempt, so a background host that
//! restarted on a new address (or had to be started again) is found.

use std::collections::HashMap;
use std::time::Duration;

use crossfire::oneshot::{RxOneshot, TxOneshot, oneshot};
use crossfire::{AsyncRx, BlockingTxTrait, MTx, mpsc};
use orbit_proto::link::ServerDecoder;
use orbit_proto::*;
use std::sync::{Arc, Mutex};

use crate::transport::{self, FrameReader, FrameWriter};
use crate::ws_lite::{self, Incoming};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{ConnectionStatus, PendingReply, Update, ready};

enum Outgoing {
    /// Boxed: a command can be large (settings) and the queue holds many.
    Call { command: Box<Command>, reply: TxOneshot<Result<Reply, String>> },
    Close,
}

/// Where to connect next: `(url, token)`, or why there is nowhere yet
/// (shown to the user while the client keeps trying).
pub type Resolver = Box<dyn FnMut() -> Result<(String, String), String> + Send>;

#[derive(Clone)]
pub struct RemoteClient {
    outgoing: MTx<mpsc::List<Outgoing>>,
    /// Single consumer: the first [`Backend::feed`](crate::Backend::feed)
    /// takes it.
    updates: Arc<Mutex<Option<AsyncRx<mpsc::List<Update>>>>>,
    /// Replies travel through the update stream (see [`Self::order_replies`]).
    ordered: Arc<AtomicBool>,
}

impl RemoteClient {
    /// Connects to `url` (`ws://host:port/`, `pipe:…` or `unix:…`) with
    /// `token`, in the background.
    pub fn connect(url: String, token: String) -> std::io::Result<Self> {
        Self::connect_with(Box::new(move || Ok((url.clone(), token.clone()))))
    }

    /// Connects wherever `resolve` says, asking again before every attempt.
    pub fn connect_with(resolve: Resolver) -> std::io::Result<Self> {
        let (outgoing, out_rx) = mpsc::unbounded_async::<Outgoing>();
        let (update_tx, updates) = mpsc::unbounded_async::<Update>();
        let ordered = Arc::new(AtomicBool::new(false));
        let run_ordered = ordered.clone();
        std::thread::Builder::new().name("orbit-remote".into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            runtime.block_on(run(resolve, out_rx, update_tx, run_ordered));
        })?;
        Ok(Self { outgoing, updates: Arc::new(Mutex::new(Some(updates))), ordered })
    }

    /// Delivers each reply through the update stream, after the events the
    /// host sent before it, as [`Update::Reply`]; the consumer resolves it
    /// once those events are applied ([`ClientState::apply`] does). A caller
    /// that reads its state after a reply then sees what its command did.
    /// Only for a consumer that applies every update: otherwise calls never
    /// resolve.
    ///
    /// [`ClientState::apply`]: crate::state::ClientState::apply
    pub fn order_replies(&self, on: bool) {
        self.ordered.store(on, Ordering::Release);
    }

    pub fn call(&self, command: Command) -> RxOneshot<Result<Reply, String>> {
        let (reply, rx) = oneshot();
        match self.outgoing.send(Outgoing::Call { command: Box::new(command), reply }) {
            Ok(()) => rx,
            Err(_) => ready(Err("The connection to the host is closed.".into())),
        }
    }

    /// The update stream. Returns `None` after the first call: a remote
    /// connection has exactly one consumer.
    pub fn updates(&self) -> Option<AsyncRx<mpsc::List<Update>>> {
        self.updates.lock().unwrap().take()
    }

    pub fn close(&self) {
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

/// One connection's halves, whichever the transport.
enum Reader {
    Ws(ws_lite::Reader),
    Local(FrameReader),
}

enum Writer {
    Ws(ws_lite::Writer),
    Local(FrameWriter),
}

impl Reader {
    /// The next message. Cancel-safe.
    async fn next(&mut self) -> Result<Incoming, String> {
        match self {
            Reader::Ws(r) => r.next().await,
            Reader::Local(r) => match r.next().await {
                Ok(Some(frame)) => Ok(Incoming::Binary(frame)),
                Ok(None) => Ok(Incoming::Close),
                Err(e) => Err(e.to_string()),
            },
        }
    }
}

impl Writer {
    async fn binary(&mut self, payload: &[u8]) -> Result<(), String> {
        match self {
            Writer::Ws(w) => w.binary(payload).await,
            Writer::Local(w) => w.send(payload).await.map_err(|e| e.to_string()),
        }
    }

    async fn pong(&mut self, payload: &[u8]) -> Result<(), String> {
        match self {
            Writer::Ws(w) => w.pong(payload).await,
            Writer::Local(_) => Ok(()),
        }
    }

    async fn close(&mut self) {
        match self {
            Writer::Ws(w) => w.close().await,
            Writer::Local(w) => w.close().await,
        }
    }
}

async fn open(url: &str) -> Result<(Reader, Writer), String> {
    if transport::is_local(url) {
        let (r, w) = transport::connect(url).await.map_err(|e| e.to_string())?;
        Ok((Reader::Local(r), Writer::Local(w)))
    } else {
        let (r, w) = ws_lite::connect(url, WS_SUBPROTOCOL).await?;
        Ok((Reader::Ws(r), Writer::Ws(w)))
    }
}

/// Sends one frame and returns the first frame of the answer (a control
/// request, see [`crate::host::control`]).
pub(crate) async fn exchange_once(url: &str, frame: &[u8]) -> std::io::Result<Vec<u8>> {
    let (mut reader, mut writer) = open(url).await.map_err(std::io::Error::other)?;
    writer.binary(frame).await.map_err(std::io::Error::other)?;
    loop {
        match reader.next().await.map_err(std::io::Error::other)? {
            Incoming::Binary(data) => {
                writer.close().await;
                return Ok(data.to_vec());
            }
            Incoming::Ping(data) => writer.pong(&data).await.map_err(std::io::Error::other)?,
            Incoming::Close => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the host closed the connection")),
        }
    }
}

async fn run(
    mut resolve: Resolver,
    outgoing: AsyncRx<mpsc::List<Outgoing>>,
    updates: MTx<mpsc::List<Update>>,
    ordered: Arc<AtomicBool>,
) {
    let mut epoch = 0u64;
    let mut since = 0u64;
    let mut backoff = Duration::from_millis(250);
    let mut pending: HashMap<u32, TxOneshot<Result<Reply, String>>> = HashMap::new();
    let mut next_id = 1u32;
    loop {
        let _ = updates.send(Update::Status(ConnectionStatus::Connecting));
        let target = resolve();
        let reason = match target {
            Err(why) => why,
            Ok((url, token)) => match open(&url).await {
                Err(e) => format!("Couldn’t reach the host: {e}"),
                Ok((mut reader, mut writer)) => {
                    backoff = Duration::from_millis(250);
                    // This link's frames: hot messages skip the big enums.
                    let mut link = ServerDecoder::new();
                    let hello = ClientMsg::Hello { version: PROTOCOL_VERSION, token, epoch, since };
                    if let Err(e) = writer.binary(&encode(&hello)).await {
                        format!("The host closed the connection: {e}")
                    } else {
                        let mut welcomed = false;
                        loop {
                            tokio::select! {
                                msg = reader.next() => {
                                    let data = match msg {
                                        Ok(Incoming::Binary(data)) => data,
                                        Ok(Incoming::Ping(data)) => {
                                            if let Err(e) = writer.pong(&data).await {
                                                break format!("Connection lost: {e}");
                                            }
                                            continue;
                                        }
                                        Ok(Incoming::Close) => break "The host closed the connection.".to_string(),
                                        Err(e) => break format!("Connection lost: {e}"),
                                    };
                                    match link.message(&data) {
                                        Ok(ServerMsg::Welcome { snapshot, .. }) => {
                                            welcomed = true;
                                            if let Some(snapshot) = snapshot {
                                                epoch = snapshot.epoch;
                                                since = snapshot.seq;
                                                let _ = updates.send(Update::Reset(snapshot));
                                            }
                                            let _ = updates.send(Update::Status(ConnectionStatus::Connected));
                                        }
                                        Ok(ServerMsg::Events(envs)) => {
                                            if let Some(seq) = envs.iter().rev().find(|e| e.seq != 0).map(|e| e.seq) {
                                                since = seq;
                                            }
                                            let _ = updates.send(Update::Events(envs));
                                        }
                                        Ok(ServerMsg::Reply { id, result }) => {
                                            if let Some(reply) = pending.remove(&id) {
                                                if ordered.load(Ordering::Acquire) {
                                                    let _ = updates.send(Update::Reply(PendingReply::new(reply, result)));
                                                } else {
                                                    reply.send(result);
                                                }
                                            }
                                        }
                                        Ok(ServerMsg::Goodbye { reason }) => break reason,
                                        Err(e) => log::warn!("bad frame from host: {e}"),
                                    }
                                }
                                out = outgoing.recv() => match out {
                                    Ok(Outgoing::Call { command, reply }) => {
                                        if !welcomed {
                                            reply.send(Err("Still connecting to the host.".into()));
                                            continue;
                                        }
                                        let id = next_id;
                                        next_id = next_id.wrapping_add(1).max(1);
                                        let msg = ClientMsg::Call { id, command: *command };
                                        match writer.binary(&link.client(&msg)).await {
                                            Ok(()) => { pending.insert(id, reply); }
                                            Err(e) => {
                                                reply.send(Err(format!("Connection lost: {e}")));
                                                break format!("Connection lost: {e}");
                                            }
                                        }
                                    }
                                    Ok(Outgoing::Close) | Err(_) => {
                                        writer.close().await;
                                        return;
                                    }
                                },
                            }
                        }
                    }
                }
            },
        };
        for (_, reply) in pending.drain() {
            reply.send(Err("The connection to the host was lost before it answered.".into()));
        }
        // A resync request from the host is not an error the user needs to see.
        if reason != "resync" {
            let _ = updates.send(Update::Status(ConnectionStatus::Disconnected { reason }));
            // Wait before reconnecting, but keep answering calls meanwhile.
            let deadline = tokio::time::sleep(backoff);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    out = outgoing.recv() => match out {
                        Ok(Outgoing::Call { reply, .. }) => reply.send(Err("Not connected to the host.".into())),
                        Ok(Outgoing::Close) | Err(_) => return,
                    },
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}
