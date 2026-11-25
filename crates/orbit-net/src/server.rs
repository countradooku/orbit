//! The `orbitd` WebSocket server.
//!
//! One binary frame per message, `bitcode` payloads, subprotocol `orbit.v1`.
//! The first frame must be a `Hello` carrying the host's token; anything else
//! closes the connection. Events are coalesced into frames of up to
//! [`MAX_BATCH`](crate::MAX_BATCH) envelopes, so streaming costs one frame per
//! burst instead of one per token.

use std::net::SocketAddr;

use bytes::Bytes;
use crossfire::mpsc;
use orbit_core::EngineHandle;
use orbit_proto::*;
use sockudo_ws::server::WebSocketServer;
use sockudo_ws::{Config, Http1, Message};
use tokio::net::TcpListener;

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
        tokio::spawn(async move {
            match server.accept(stream).await {
                Ok((ws, _)) => {
                    if let Err(err) = connection(ws, engine, &token).await {
                        log::info!("{peer}: {err}");
                    }
                }
                Err(err) => log::info!("{peer}: handshake failed: {err}"),
            }
        });
    }
}

fn frame(msg: &ServerMsg) -> Message {
    Message::Binary(Bytes::from(encode(msg)))
}

/// Frames with a reused codec, for the hot event path.
fn frame_with(codec: &mut Codec, msg: &ServerMsg) -> Message {
    Message::Binary(Bytes::copy_from_slice(codec.encode(msg)))
}

/// Constant-time comparison, so the token cannot be guessed byte by byte
/// from response timing.
fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn connection<S>(ws: sockudo_ws::WebSocketStream<S>, engine: EngineHandle, token: &str) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = ws.split();

    // Handshake.
    let hello = loop {
        match reader.next().await {
            Some(Ok(Message::Binary(data))) => break decode::<ClientMsg>(&data).map_err(|e| e.to_string())?,
            Some(Ok(Message::Close(_))) | None => return Ok(()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.to_string()),
        }
    };
    let ClientMsg::Hello { version, token: presented, epoch, since } = hello else {
        return Err("first message was not Hello".into());
    };
    if version != PROTOCOL_VERSION {
        let reason = format!("This host speaks protocol {PROTOCOL_VERSION}; update Orbit to connect.");
        let _ = writer.send(frame(&ServerMsg::Goodbye { reason })).await;
        return Err("protocol mismatch".into());
    }
    if !same_token(&presented, token) {
        let _ = writer.send(frame(&ServerMsg::Goodbye { reason: "The access token is wrong.".into() })).await;
        return Err("bad token".into());
    }
    let sub = engine.subscribe(epoch, since).await.map_err(|_| "engine stopped")?;
    writer
        .send(frame(&ServerMsg::Welcome { version: PROTOCOL_VERSION, snapshot: sub.snapshot }))
        .await
        .map_err(|e| e.to_string())?;
    if !sub.replay.is_empty() {
        for chunk in sub.replay.chunks(crate::MAX_BATCH) {
            writer.send(frame(&ServerMsg::Events(chunk.to_vec()))).await.map_err(|e| e.to_string())?;
        }
    }

    // Replies are produced by spawned tasks and written by the writer loop.
    let (reply_tx, reply_rx) = mpsc::unbounded_async::<ServerMsg>();
    let events = sub.events;
    let mut codec = Codec::new();
    let reader_task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            while let Some(msg) = reader.next().await {
                let data = match msg {
                    Ok(Message::Binary(data)) => data,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let Ok(ClientMsg::Call { id, command }) = decode::<ClientMsg>(&data) else { continue };
                let reply = engine.call(command);
                let tx = reply_tx.clone();
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
                    if let Err(e) = writer.send(frame(&msg)).await {
                        break Err(e.to_string());
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
                    if let Err(e) = writer.send(frame_with(&mut codec, &ServerMsg::Events(batch))).await {
                        break Err(e.to_string());
                    }
                }
                Err(_) => {
                    // The engine dropped this subscriber because it fell
                    // behind; the client reconnects and replays from its seq.
                    let _ = writer.send(frame(&ServerMsg::Goodbye { reason: "resync".into() })).await;
                    break Ok(());
                }
            },
        }
        if reader_task.is_finished() {
            break Ok(());
        }
    };
    reader_task.abort();
    let _ = writer.close(1000, "bye").await;
    result
}
