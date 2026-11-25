//! A small WebSocket client transport (RFC 6455) for [`crate::RemoteClient`].
//!
//! The handshake is sockudo-ws's; framing is done here. sockudo-ws's stream
//! type calibrates `quanta`'s TSC clock on first use, which blocks for up
//! to 200 ms on some machines: invisible in a long-lived app, but most of a
//! CLI call's run time. A client needs little of what that stream offers
//! (binary messages, ping replies, close), so it is done directly.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Largest message accepted, as on the server.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// What the reader hands back.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Incoming {
    Binary(Bytes),
    Ping(Bytes),
    Close,
}

pub(crate) struct Reader {
    stream: OwnedReadHalf,
    buf: BytesMut,
    /// A fragmented message being assembled.
    partial: Option<BytesMut>,
}

pub(crate) struct Writer {
    stream: OwnedWriteHalf,
    out: Vec<u8>,
}

/// Connects to `ws://host:port/path` and completes the upgrade.
pub(crate) async fn connect(url: &str, protocol: &str) -> Result<(Reader, Writer), String> {
    let rest = url.strip_prefix("ws://").ok_or_else(|| format!("unsupported URL {url} (use ws://)"))?;
    let (authority, path) = match rest.find('/') {
        Some(ix) => (&rest[..ix], &rest[ix..]),
        None => (rest, "/"),
    };
    let stream = match authority.parse::<std::net::SocketAddr>() {
        // An IP needs no resolver round trip.
        Ok(addr) => TcpStream::connect(addr).await,
        Err(_) => TcpStream::connect(authority).await,
    }
    .map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut stream = stream;
    let handshake = sockudo_ws::handshake::client_handshake(&mut stream, authority, path, Some(protocol))
        .await
        .map_err(|e| e.to_string())?;
    let (read, write) = stream.into_split();
    let mut buf = BytesMut::with_capacity(16 * 1024);
    if let Some(leftover) = handshake.leftover {
        buf.extend_from_slice(&leftover);
    }
    Ok((Reader { stream: read, buf, partial: None }, Writer { stream: write, out: Vec::with_capacity(1024) }))
}

impl Reader {
    /// The next message. Cancel-safe: partial frames stay buffered.
    pub(crate) async fn next(&mut self) -> Result<Incoming, String> {
        loop {
            if let Some(incoming) = self.parse()? {
                return Ok(incoming);
            }
            let n = self.stream.read_buf(&mut self.buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(Incoming::Close);
            }
        }
    }

    /// Takes one complete message out of the buffer, if there is one.
    fn parse(&mut self) -> Result<Option<Incoming>, String> {
        loop {
            let Some((fin, opcode, payload)) = take_frame(&mut self.buf)? else { return Ok(None) };
            match opcode {
                0x2 | 0x1 if fin && self.partial.is_none() => return Ok(Some(Incoming::Binary(payload))),
                0x2 | 0x1 => {
                    if self.partial.is_some() {
                        return Err("a new message started inside a fragmented one".into());
                    }
                    self.partial = Some(BytesMut::from(&payload[..]));
                }
                0x0 => {
                    let partial = self.partial.as_mut().ok_or("a continuation frame without a message")?;
                    if partial.len() + payload.len() > MAX_MESSAGE {
                        return Err("message too large".into());
                    }
                    partial.extend_from_slice(&payload);
                    if fin {
                        return Ok(Some(Incoming::Binary(self.partial.take().unwrap().freeze())));
                    }
                }
                0x8 => return Ok(Some(Incoming::Close)),
                0x9 => return Ok(Some(Incoming::Ping(payload))),
                0xA => {}
                other => return Err(format!("unknown frame opcode {other}")),
            }
        }
    }
}

/// One frame from the front of `buf`: (fin, opcode, unmasked payload).
fn take_frame(buf: &mut BytesMut) -> Result<Option<(bool, u8, Bytes)>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let masked = b1 & 0x80 != 0;
    let (len, header) = match b1 & 0x7f {
        126 if buf.len() >= 4 => (u16::from_be_bytes([buf[2], buf[3]]) as u64, 4),
        127 if buf.len() >= 10 => (u64::from_be_bytes(buf[2..10].try_into().unwrap()), 10),
        126 | 127 => return Ok(None),
        n => (n as u64, 2),
    };
    if len > MAX_MESSAGE as u64 {
        return Err("message too large".into());
    }
    let mask_len = if masked { 4 } else { 0 };
    let total = header + mask_len + len as usize;
    if buf.len() < total {
        buf.reserve(total - buf.len());
        return Ok(None);
    }
    let mask: Option<[u8; 4]> = masked.then(|| buf[header..header + 4].try_into().unwrap());
    buf.advance(header + mask_len);
    let mut payload = buf.split_to(len as usize);
    if let Some(mask) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok(Some((b0 & 0x80 != 0, b0 & 0x0f, payload.freeze())))
}

impl Writer {
    pub(crate) async fn binary(&mut self, payload: &[u8]) -> Result<(), String> {
        self.frame(0x2, payload).await
    }

    pub(crate) async fn pong(&mut self, payload: &[u8]) -> Result<(), String> {
        self.frame(0xA, payload).await
    }

    pub(crate) async fn close(&mut self) {
        let _ = self.frame(0x8, &1000u16.to_be_bytes()).await;
        let _ = self.stream.shutdown().await;
    }

    /// Writes one final, masked frame (clients must mask, RFC 6455 §5.3).
    async fn frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        encode_frame(&mut self.out, opcode, payload, mask_key());
        let result = self.stream.write_all(&self.out).await.map_err(|e| e.to_string());
        self.out.clear();
        if self.out.capacity() > 1024 * 1024 {
            self.out.shrink_to(1024);
        }
        result
    }
}

fn encode_frame(out: &mut Vec<u8>, opcode: u8, payload: &[u8], mask: [u8; 4]) {
    out.put_u8(0x80 | opcode);
    match payload.len() {
        n if n < 126 => out.put_u8(0x80 | n as u8),
        n if n <= u16::MAX as usize => {
            out.put_u8(0x80 | 126);
            out.put_u16(n as u16);
        }
        n => {
            out.put_u8(0x80 | 127);
            out.put_u64(n as u64);
        }
    }
    out.extend_from_slice(&mask);
    let start = out.len();
    out.extend_from_slice(payload);
    for (i, byte) in out[start..].iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
}

/// A fresh masking key from the OS-seeded hasher.
fn mask_key() -> [u8; 4] {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0);
    (hasher.finish() as u32).to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(if fin { 0x80 } else { 0 }) | opcode];
        match payload.len() {
            n if n < 126 => out.push(n as u8),
            n if n <= 65535 => {
                out.push(126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn frames_round_trip_masked_and_in_every_length_class() {
        for len in [0usize, 5, 125, 126, 65_535, 65_536, 200_000] {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut out = Vec::new();
            encode_frame(&mut out, 0x2, &payload, [1, 2, 3, 4]);
            let mut buf = BytesMut::from(&out[..]);
            let (fin, opcode, back) = take_frame(&mut buf).unwrap().unwrap();
            assert!(fin && opcode == 2 && back == payload, "{len}");
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn partial_frames_wait_and_fragments_are_joined() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            let (read, _write) = client.into_split();
            let mut reader = Reader { stream: read, buf: BytesMut::new(), partial: None };
            let mut bytes = server_frame(false, 2, b"hel");
            bytes.extend(server_frame(true, 9, b"p"));
            bytes.extend(server_frame(true, 0, b"lo"));
            bytes.extend(server_frame(true, 2, &vec![7u8; 70_000]));
            bytes.extend(server_frame(true, 8, &[3, 232]));
            // Dribble it in two pieces cut inside a header.
            server.write_all(&bytes[..4]).await.unwrap();
            let rest = bytes[4..].to_vec();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                server.write_all(&rest).await.unwrap();
                server
            });
            assert_eq!(reader.next().await.unwrap(), Incoming::Ping(Bytes::from_static(b"p")));
            assert_eq!(reader.next().await.unwrap(), Incoming::Binary(Bytes::from_static(b"hello")));
            assert_eq!(reader.next().await.unwrap(), Incoming::Binary(Bytes::from(vec![7u8; 70_000])));
            assert_eq!(reader.next().await.unwrap(), Incoming::Close);
        });
    }
}
