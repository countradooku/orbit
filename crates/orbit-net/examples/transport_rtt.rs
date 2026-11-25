//! Raw round trip of a small frame over the local transports a host can
//! use: loopback TCP (what the WebSocket link runs on) and the platform's
//! local socket (a named pipe on Windows, a Unix socket elsewhere).
//!
//! ```text
//! cargo run -p orbit-net --release --example transport_rtt
//! ```
//!
//! Each side is a single-threaded Tokio runtime on its own thread, as in the
//! host and the client. A frame is a 4-byte length and 32 bytes of payload.

use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const ROUNDS: usize = 20_000;

async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut s: S) {
    let mut buf = [0u8; 36];
    while s.read_exact(&mut buf).await.is_ok() {
        if s.write_all(&buf).await.is_err() {
            return;
        }
    }
}

async fn ping<S: AsyncRead + AsyncWrite + Unpin>(mut s: S) -> Vec<Duration> {
    let mut buf = [7u8; 36];
    let mut samples = Vec::with_capacity(ROUNDS);
    for i in 0..ROUNDS + 1000 {
        let start = Instant::now();
        s.write_all(&buf).await.unwrap();
        s.read_exact(&mut buf).await.unwrap();
        if i >= 1000 {
            samples.push(start.elapsed());
        }
    }
    samples.sort();
    samples
}

fn report(name: &str, samples: &[Duration]) {
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e6;
    println!("{name:<24} p50 {:>6.1} µs   p99 {:>6.1} µs", p(0.5), p(0.99));
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn tcp() -> Vec<Duration> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        rt().block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap()).unwrap();
            let (s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            echo(s).await;
        })
    });
    let addr = rx.recv().unwrap();
    rt().block_on(async move {
        let s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.set_nodelay(true).unwrap();
        ping(s).await
    })
}

#[cfg(windows)]
fn local() -> Vec<Duration> {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    let name = format!(r"\\.\pipe\orbit-rtt-{}", std::process::id());
    let server_name = name.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        rt().block_on(async move {
            let server = ServerOptions::new().first_pipe_instance(true).create(&server_name).unwrap();
            tx.send(()).unwrap();
            server.connect().await.unwrap();
            echo(server).await;
        })
    });
    rx.recv().unwrap();
    rt().block_on(async move { ping(ClientOptions::new().open(&name).unwrap()).await })
}

#[cfg(unix)]
fn local() -> Vec<Duration> {
    let dir = std::env::temp_dir().join(format!("orbit-rtt-{}", std::process::id()));
    let _ = std::fs::remove_file(&dir);
    let path = dir.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        rt().block_on(async move {
            let l = tokio::net::UnixListener::bind(&path).unwrap();
            tx.send(()).unwrap();
            let (s, _) = l.accept().await.unwrap();
            echo(s).await;
        })
    });
    rx.recv().unwrap();
    let samples = rt().block_on(async move { ping(tokio::net::UnixStream::connect(&dir).await.unwrap()).await });
    samples
}

fn main() {
    report("loopback TCP", &tcp());
    report(if cfg!(windows) { "named pipe" } else { "unix socket" }, &local());
}
