//! A small HTTPS client for tracker and chat APIs, on the caller's Tokio
//! runtime.
//!
//! One shared connection pool; HTTP/1.1 over rustls with the operating
//! system's trust store. Plain HTTP is refused except to loopback, so a
//! token never leaves the machine unencrypted.

use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::Request;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use super::ForgeError;

/// Responses larger than this are refused (a tracker page is far smaller).
const MAX_BODY: usize = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

type HttpsClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct Response {
    pub status: u16,
    /// Seconds from a `Retry-After` header.
    pub retry_after: Option<u64>,
    pub body: Bytes,
}

fn client() -> Result<&'static HttpsClient, ForgeError> {
    static CLIENT: OnceLock<Result<HttpsClient, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
            let https = hyper_rustls::HttpsConnectorBuilder::new()
                .with_provider_and_native_roots(provider)
                .map_err(|e| format!("Couldn’t load the system’s certificates: {e}"))?
                .https_or_http()
                .enable_http1()
                .build();
            Ok(Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(https))
        })
        .as_ref()
        .map_err(|e| ForgeError::Failed(e.clone()))
}

/// Sends one request. `headers` may hold secrets; they are never logged.
pub async fn send(method: &str, url: &str, headers: &[(&str, String)], body: Option<Vec<u8>>) -> Result<Response, ForgeError> {
    send_with(method, url, headers, body, TIMEOUT).await
}

/// [`send`] with its own time limit, for long polls.
pub async fn send_with(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Response, ForgeError> {
    let uri: hyper::Uri = url.parse().map_err(|_| ForgeError::Failed(format!("“{url}” is not a valid address.")))?;
    let loopback = matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1"));
    if uri.scheme_str() != Some("https") && !loopback {
        return Err(ForgeError::Failed(format!("Use an https:// address (got {url}).")));
    }
    let mut request = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("user-agent")) {
        request = request.header("user-agent", concat!("Orbit/", env!("CARGO_PKG_VERSION")));
    }
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let request = request
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|e| ForgeError::Failed(format!("Couldn’t build the request: {e}")))?;
    let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url).to_owned();
    let work = async {
        let response = client()?.request(request).await.map_err(|e| ForgeError::Offline(format!("Couldn’t reach {host}: {e}")))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let body = Limited::new(response.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|e| ForgeError::Offline(format!("{host} closed the connection: {e}")))?
            .to_bytes();
        Ok(Response { status, retry_after, body })
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| ForgeError::Offline(format!("{host} didn’t answer within {} seconds.", timeout.as_secs())))?
}

/// Standard base64 (for HTTP Basic authentication).
pub(crate) fn base64(input: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(input)
}
