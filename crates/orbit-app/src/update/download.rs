//! HTTPS for the updater: the manifest, and a bundle streamed to disk with
//! progress and cancellation.
//!
//! Each call runs on its own short-lived thread with a single-threaded Tokio
//! runtime (the hyper and rustls stack the engine already links), so a
//! download of tens of megabytes never occupies the UI's executors. Plain
//! HTTP is refused except to loopback (the local test fixture). Redirects
//! are followed (release hosts redirect to their storage), at most five, and
//! never from HTTPS down to HTTP.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

const MAX_REDIRECTS: usize = 5;
const MAX_MANIFEST: usize = 1024 * 1024;
/// No byte for this long ends the download.
const STALL: Duration = Duration::from_secs(60);
const CONNECT: Duration = Duration::from_secs(30);

/// Shared between the download thread and whoever shows it.
#[derive(Default)]
pub struct Progress {
    downloaded: AtomicU64,
    total: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Bytes so far and the total, when known.
    pub fn read(&self) -> (u64, Option<u64>) {
        let total = self.total.load(Ordering::Relaxed);
        (self.downloaded.load(Ordering::Relaxed), (total > 0).then_some(total))
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Runs `work` on its own thread and runtime; the future resolves with its
/// result.
pub async fn on_thread<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .map_err(|e| format!("Couldn’t start the download: {e}"))?;
    rx.await.unwrap_or_else(|_| Err("The download stopped unexpectedly.".to_owned()))
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Couldn’t start the download: {e}"))
}

type HttpsClient = Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Empty<Bytes>>;

fn client() -> Result<HttpsClient, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(provider)
        .map_err(|e| format!("Couldn’t load the system’s certificates: {e}"))?
        .https_or_http()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

fn is_loopback(uri: &hyper::Uri) -> bool {
    matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1"))
}

/// Parses `url`, refusing plain HTTP to anything but this machine.
fn check_url(url: &str) -> Result<hyper::Uri, String> {
    let uri: hyper::Uri = url.parse().map_err(|_| format!("“{url}” is not a valid address."))?;
    match uri.scheme_str() {
        Some("https") => Ok(uri),
        Some("http") if is_loopback(&uri) => Ok(uri),
        _ => Err(format!("Updates are only downloaded over HTTPS (got {url}).")),
    }
}

/// GETs `url`, following redirects, and returns the final response.
async fn get(client: &HttpsClient, url: &str) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let mut uri = check_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let host = uri.host().unwrap_or_default().to_owned();
        let request = Request::get(uri.clone())
            .header("user-agent", concat!("Orbit/", env!("CARGO_PKG_VERSION")))
            .body(Empty::new())
            .map_err(|e| format!("Couldn’t build the request: {e}"))?;
        let response = tokio::time::timeout(CONNECT, client.request(request))
            .await
            .map_err(|_| format!("{host} didn’t answer within {} seconds.", CONNECT.as_secs()))?
            .map_err(|e| format!("Couldn’t reach {host}: {e}"))?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{host} redirected without saying where."))?;
            let next: hyper::Uri = if location.starts_with('/') {
                format!("{}://{}{location}", uri.scheme_str().unwrap_or("https"), uri.authority().map(|a| a.as_str()).unwrap_or(""))
                    .parse()
                    .map_err(|_| format!("{host} redirected to an invalid address."))?
            } else {
                location.parse().map_err(|_| format!("{host} redirected to an invalid address."))?
            };
            if uri.scheme_str() == Some("https") && next.scheme_str() != Some("https") {
                return Err(format!("{host} redirected away from HTTPS."));
            }
            uri = check_url(&next.to_string())?;
            continue;
        }
        if !status.is_success() {
            return Err(match status.as_u16() {
                404 => format!("{host} has no update information at this address (404)."),
                code => format!("{host} answered {code}."),
            });
        }
        return Ok(response);
    }
    Err("Too many redirects.".to_owned())
}

/// The manifest at `url` (blocking: call through [`on_thread`]).
pub fn fetch_text(url: &str) -> Result<String, String> {
    let rt = runtime()?;
    rt.block_on(async {
        let client = client()?;
        let response = get(&client, url).await?;
        let body = http_body_util::Limited::new(response.into_body(), MAX_MANIFEST)
            .collect()
            .await
            .map_err(|e| format!("The update information couldn’t be read: {e}"))?
            .to_bytes();
        String::from_utf8(body.to_vec()).map_err(|_| "The update information isn’t text.".to_owned())
    })
}

/// Streams `url` into `dest` (blocking: call through [`on_thread`]). The
/// file is written as `dest` + `.part` and renamed when complete, so a
/// partial download is never mistaken for a whole one.
pub fn download(url: &str, dest: &Path, expected: Option<u64>, progress: &Progress) -> Result<(), String> {
    let part = dest.with_extension("part");
    let result = runtime()?.block_on(async {
        let client = client()?;
        let response = get(&client, url).await?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .or(expected)
            .unwrap_or(0);
        progress.total.store(total, Ordering::Relaxed);
        progress.downloaded.store(0, Ordering::Relaxed);
        let mut file = std::fs::File::create(&part).map_err(|e| format!("Couldn’t save the download: {e}"))?;
        let mut body = response.into_body();
        let mut written = 0u64;
        loop {
            if progress.is_cancelled() {
                return Err("Cancelled.".to_owned());
            }
            let frame = match tokio::time::timeout(STALL, body.frame()).await {
                Err(_) => return Err(format!("The download stalled for {} seconds.", STALL.as_secs())),
                Ok(None) => break,
                Ok(Some(frame)) => frame.map_err(|e| format!("The download was interrupted: {e}"))?,
            };
            if let Ok(data) = frame.into_data() {
                file.write_all(&data).map_err(|e| format!("Couldn’t save the download: {e}"))?;
                written += data.len() as u64;
                progress.downloaded.store(written, Ordering::Relaxed);
            }
        }
        file.sync_all().map_err(|e| format!("Couldn’t save the download: {e}"))?;
        if total > 0 && written != total {
            return Err(format!("The download ended early ({written} of {total} bytes)."));
        }
        Ok(())
    });
    match result {
        Ok(()) => std::fs::rename(&part, dest).map_err(|e| format!("Couldn’t save the download: {e}")),
        Err(err) => {
            let _ = std::fs::remove_file(&part);
            Err(err)
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! A loopback HTTP server serving fixed files, for the updater's tests.

    use std::collections::HashMap;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct FixtureServer {
        pub base: String,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl FixtureServer {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
            let served = files.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let served = served.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                                break;
                            }
                        }
                        let body = served.lock().unwrap().get(&path).cloned();
                        let _ = match (path.as_str(), body) {
                            ("/moved", _) => stream.write_all(b"HTTP/1.1 302 Found\r\nlocation: /bundle.zip\r\ncontent-length: 0\r\n\r\n"),
                            (_, Some(body)) => stream
                                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len()).as_bytes())
                                .and_then(|_| stream.write_all(&body)),
                            (_, None) => stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n"),
                        };
                    });
                }
            });
            Self { base, files }
        }

        pub fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
            self.files.lock().unwrap().insert(path.to_owned(), body.into());
        }

        pub fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FixtureServer;
    use super::*;

    #[test]
    fn downloads_follow_redirects_and_report_progress() {
        let server = FixtureServer::start();
        let body = vec![42u8; 300_000];
        server.put("/bundle.zip", body.clone());
        server.put("/stable.json", "{\"version\":\"1.0.0\"}");
        assert_eq!(fetch_text(&server.url("/stable.json")).unwrap(), "{\"version\":\"1.0.0\"}");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bundle.zip");
        let progress = Progress::new();
        download(&server.url("/moved"), &dest, None, &progress).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(progress.read(), (300_000, Some(300_000)));
        assert!(!dest.with_extension("part").exists());
    }

    #[test]
    fn failures_leave_nothing_behind() {
        let server = FixtureServer::start();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bundle.zip");
        let err = download(&server.url("/missing.zip"), &dest, None, &Progress::new()).unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert!(!dest.exists() && !dest.with_extension("part").exists());
        // Cancelled before it starts.
        server.put("/bundle.zip", vec![1u8; 10]);
        let progress = Progress::new();
        progress.cancel();
        assert_eq!(download(&server.url("/bundle.zip"), &dest, None, &progress).unwrap_err(), "Cancelled.");
        assert!(!dest.exists());
    }

    #[test]
    fn plain_http_is_only_for_this_machine() {
        assert!(check_url("http://example.com/x.json").is_err());
        assert!(check_url("ftp://example.com/x.json").is_err());
        assert!(check_url("http://127.0.0.1:9/x.json").is_ok());
        assert!(check_url("https://example.com/x.json").is_ok());
    }
}
