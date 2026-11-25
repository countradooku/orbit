//! The local transport between the app and its background host: a named
//! pipe on Windows, a Unix domain socket elsewhere. Messages are the same
//! frames as on the WebSocket link, each prefixed by its length (`u32`
//! little endian).
//!
//! Why not loopback WebSockets: a named pipe's round trip is a fifth of a
//! loopback TCP one on Windows (4 µs against 19 µs at the median for a
//! small frame, `examples/transport_rtt.rs`), there is no port to allocate
//! or guard, and no HTTP upgrade or masking. The WebSocket endpoint stays
//! for the `orbit` CLI and remote clients.
//!
//! Addresses are written `pipe:\\.\pipe\orbit-…` or `unix:/path/to/sock`.
//! The name carries 64 random bits and the first frame must still carry
//! the host's token, as on every other transport. On Windows the pipe is
//! created as the first instance of its name (a squatter makes binding
//! fail rather than intercept clients) and refuses remote clients; on Unix
//! the socket is made readable by its owner only.

use std::io;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted, as on the WebSocket link.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

const PIPE: &str = "pipe:";
const UNIX: &str = "unix:";

/// Whether `url` names a local transport (`pipe:` or `unix:`).
pub fn is_local(url: &str) -> bool {
    url.starts_with(PIPE) || url.starts_with(UNIX)
}

/// A fresh, unguessable local address for a host on `data_dir`.
#[cfg(feature = "engine")]
pub fn new_address(data_dir: &std::path::Path) -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let tag: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    #[cfg(windows)]
    {
        let _ = data_dir;
        Ok(format!("{PIPE}\\\\.\\pipe\\orbit-host-{tag}"))
    }
    #[cfg(unix)]
    {
        // Socket paths are limited to about 104 bytes.
        let path = data_dir.join(format!("host-{tag}.sock"));
        let path = if path.as_os_str().len() < 100 {
            path
        } else {
            std::env::temp_dir().join(format!("orbit-host-{tag}.sock"))
        };
        Ok(format!("{UNIX}{}", path.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (data_dir, tag);
        Err(io::Error::new(io::ErrorKind::Unsupported, "no local transport on this platform"))
    }
}

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

/// Reads length-prefixed frames.
pub struct FrameReader {
    inner: BoxRead,
    buf: BytesMut,
}

/// Writes length-prefixed frames, header and body in one write.
pub struct FrameWriter {
    inner: BoxWrite,
    out: Vec<u8>,
}

impl FrameReader {
    pub fn new(inner: BoxRead) -> Self {
        Self { inner, buf: BytesMut::with_capacity(16 * 1024) }
    }

    /// The next frame; `None` when the other side closed. Cancel-safe:
    /// partial frames stay buffered.
    pub async fn next(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_le_bytes(self.buf[..4].try_into().expect("four bytes")) as usize;
                if len > MAX_FRAME {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a frame of {len} bytes is too large")));
                }
                if self.buf.len() >= 4 + len {
                    self.buf.advance(4);
                    return Ok(Some(self.buf.split_to(len).freeze()));
                }
                self.buf.reserve(4 + len - self.buf.len());
            }
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                return Ok(None);
            }
        }
    }
}

impl FrameWriter {
    pub fn new(inner: BoxWrite) -> Self {
        Self { inner, out: Vec::with_capacity(1024) }
    }

    pub async fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        if frame.len() > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
        }
        self.out.clear();
        self.out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        self.out.extend_from_slice(frame);
        self.inner.write_all(&self.out).await?;
        self.inner.flush().await
    }

    pub async fn close(&mut self) {
        let _ = self.inner.shutdown().await;
    }
}

/// Connects to a `pipe:` or `unix:` address.
pub async fn connect(url: &str) -> io::Result<(FrameReader, FrameWriter)> {
    #[cfg(windows)]
    if let Some(name) = url.strip_prefix(PIPE) {
        use tokio::net::windows::named_pipe::ClientOptions;
        // All instances busy: the server creates the next one right after
        // accepting, so retry briefly.
        let mut tries = 0;
        let client = loop {
            match ClientOptions::new().open(name) {
                Ok(client) => break client,
                Err(err) if err.raw_os_error() == Some(231) && tries < 50 => {
                    // ERROR_PIPE_BUSY
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(err) => return Err(err),
            }
        };
        let (r, w) = tokio::io::split(client);
        return Ok((FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w))));
    }
    #[cfg(unix)]
    if let Some(path) = url.strip_prefix(UNIX) {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (r, w) = stream.into_split();
        return Ok((FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w))));
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a local address: {url}")))
}

/// Accepts connections on a local address.
pub struct LocalListener {
    #[cfg(windows)]
    name: String,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    #[cfg(unix)]
    path: std::path::PathBuf,
}

impl LocalListener {
    /// Binds `url`. Call from inside a Tokio runtime.
    pub fn bind(url: &str) -> io::Result<Self> {
        #[cfg(windows)]
        if let Some(name) = url.strip_prefix(PIPE) {
            use tokio::net::windows::named_pipe::ServerOptions;
            let next = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(name)?;
            return Ok(Self { name: name.to_owned(), next });
        }
        #[cfg(unix)]
        if let Some(path) = url.strip_prefix(UNIX) {
            use std::os::unix::fs::PermissionsExt as _;
            let path = std::path::PathBuf::from(path);
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            return Ok(Self { listener, path });
        }
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a local address: {url}")))
    }

    /// The next client.
    pub async fn accept(&mut self) -> io::Result<(FrameReader, FrameWriter)> {
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            self.next.connect().await?;
            let fresh = ServerOptions::new().reject_remote_clients(true).create(&self.name)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            let (r, w) = tokio::io::split(connected);
            Ok((FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w))))
        }
        #[cfg(unix)]
        {
            let (stream, _) = self.listener.accept().await?;
            let (r, w) = stream.into_split();
            Ok((FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w))))
        }
    }
}

#[cfg(unix)]
impl Drop for LocalListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(all(test, feature = "engine"))]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_over_the_local_transport() {
        let dir = tempfile::tempdir().unwrap();
        let url = new_address(dir.path()).unwrap();
        assert!(is_local(&url));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut listener = LocalListener::bind(&url).unwrap();
            let server = tokio::spawn(async move {
                let (mut r, mut w) = listener.accept().await.unwrap();
                while let Some(frame) = r.next().await.unwrap() {
                    w.send(&frame).await.unwrap();
                }
            });
            let (mut r, mut w) = connect(&url).await.unwrap();
            let big = vec![7u8; 1 << 20];
            for frame in [&b"hello"[..], &[][..], &big[..]] {
                w.send(frame).await.unwrap();
                assert_eq!(r.next().await.unwrap().unwrap(), frame);
            }
            w.close().await;
            drop(w);
            drop(r);
            server.await.unwrap();
        });
    }

    #[test]
    fn a_second_host_cannot_take_a_live_address() {
        let dir = tempfile::tempdir().unwrap();
        let url = new_address(dir.path()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _first = LocalListener::bind(&url).unwrap();
            #[cfg(windows)]
            assert!(LocalListener::bind(&url).is_err(), "the first instance owns the pipe name");
        });
    }
}
