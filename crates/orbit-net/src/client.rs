//! The remote client: a connection to an `orbitd` host that survives drops.
//!
//! It runs on its own thread with a single-threaded Tokio runtime, so it
//! works from any UI framework. On disconnect it reconnects with backoff and
//! asks for the events after the last one it delivered; the UI sees a
//! `Status` update and, at worst, a `Reset`, never a blank state.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use crossfire::oneshot::{RxOneshot, TxOneshot, oneshot};
use crossfire::{AsyncRx, BlockingTxTrait, MTx, mpsc};
use orbit_proto::*;
use std::sync::{Arc, Mutex};

use sockudo_ws::client::WebSocketClient;
use sockudo_ws::{Config, Http1, Message};

use crate::{ConnectionStatus, Update, ready};

enum Outgoing {
    Call { command: Command, reply: TxOneshot<Result<Reply, String>> },
    Close,
}

#[derive(Clone)]
pub struct RemoteClient {
    outgoing: MTx<mpsc::List<Outgoing>>,
    /// Single consumer: the first [`Backend::feed`](crate::Backend::feed)
    /// takes it.
    updates: Arc<Mutex<Option<AsyncRx<mpsc::List<Update>>>>>,
}

impl RemoteClient {
    /// Connects to `url` (`ws://host:port/`) with `token`, in the background.
    pub fn connect(url: String, token: String) -> std::io::Result<Self> {
        let (outgoing, out_rx) = mpsc::unbounded_async::<Outgoing>();
        let (update_tx, updates) = mpsc::unbounded_async::<Update>();
        std::thread::Builder::new().name("orbit-remote".into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            runtime.block_on(run(url, token, out_rx, update_tx));
        })?;
        Ok(Self { outgoing, updates: Arc::new(Mutex::new(Some(updates))) })
    }

    pub fn call(&self, command: Command) -> RxOneshot<Result<Reply, String>> {
        let (reply, rx) = oneshot();
        match self.outgoing.send(Outgoing::Call { command, reply }) {
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

async fn run(
    url: String,
    token: String,
    outgoing: AsyncRx<mpsc::List<Outgoing>>,
    updates: MTx<mpsc::List<Update>>,
) {
    let mut epoch = 0u64;
    let mut since = 0u64;
    let mut backoff = Duration::from_millis(250);
    let mut pending: HashMap<u32, TxOneshot<Result<Reply, String>>> = HashMap::new();
    let mut next_id = 1u32;
    loop {
        let _ = updates.send(Update::Status(ConnectionStatus::Connecting));
        let config = Config::builder().max_payload_length(64 * 1024 * 1024).build();
        let client = WebSocketClient::<Http1>::new(config);
        let reason = match client.connect_to_url(&url, Some(WS_SUBPROTOCOL)).await {
            Err(e) => format!("Couldn’t reach the host: {e}"),
            Ok((ws, _)) => {
                backoff = Duration::from_millis(250);
                let (mut reader, mut writer) = ws.split();
                let hello = ClientMsg::Hello { version: PROTOCOL_VERSION, token: token.clone(), epoch, since };
                if let Err(e) = writer.send(Message::Binary(Bytes::from(encode(&hello)))).await {
                    format!("The host closed the connection: {e}")
                } else {
                    let mut welcomed = false;
                    let mut codec = Codec::new();
                    let reason = loop {
                        tokio::select! {
                            msg = reader.next() => {
                                let data = match msg {
                                    Some(Ok(Message::Binary(data))) => data,
                                    Some(Ok(Message::Close(_))) | None => break "The host closed the connection.".to_string(),
                                    Some(Ok(_)) => continue,
                                    Some(Err(e)) => break format!("Connection lost: {e}"),
                                };
                                match codec.decode::<ServerMsg>(&data) {
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
                                            reply.send(result);
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
                                    let msg = ClientMsg::Call { id, command };
                                    match writer.send(Message::Binary(Bytes::from(encode(&msg)))).await {
                                        Ok(()) => { pending.insert(id, reply); }
                                        Err(e) => {
                                            reply.send(Err(format!("Connection lost: {e}")));
                                            break format!("Connection lost: {e}");
                                        }
                                    }
                                }
                                Ok(Outgoing::Close) | Err(_) => {
                                    let _ = writer.close(1000, "bye").await;
                                    return;
                                }
                            },
                        }
                    };
                    reason
                }
            }
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
