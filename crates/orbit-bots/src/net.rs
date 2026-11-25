//! HTTP and WebSocket plumbing shared by the transports.
//!
//! HTTP goes through the engine's own client (`orbit_core::forge::http`:
//! hyper and rustls, plain HTTP only to loopback). WebSockets are
//! sockudo-ws over a TCP stream, wrapped in rustls for `wss://`.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use orbit_core::forge::http;
use sockudo_ws::client::WebSocketClient;
use sockudo_ws::{Config, Http1, Message, SplitReader, SplitWriter, Stream};
use sonic_rs::Value;
use tokio::net::TcpStream;

pub type WsReader = SplitReader<Stream<Http1>>;
pub type WsWriter = SplitWriter<Stream<Http1>>;

pub struct Response {
    pub status: u16,
    /// Seconds, from a `Retry-After` header.
    pub retry_after: Option<u64>,
    pub body: Bytes,
}

impl Response {
    pub fn json(&self) -> Option<Value> {
        sonic_rs::from_slice(&self.body).ok()
    }
}

/// A failed request: offline, refused, or an error status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetError(pub String);

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub async fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Response, NetError> {
    let r = http::send_with(method, url, headers, body, timeout).await.map_err(|e| NetError(e.to_string()))?;
    Ok(Response { status: r.status, retry_after: r.retry_after, body: r.body })
}

/// A `multipart/form-data` body of text fields and one file. Returns the
/// content type (with its boundary) and the body.
pub fn multipart(fields: &[(&str, &str)], file_field: &str, file_name: &str, file: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("orbit{:016x}", orbit_proto::now_ms().wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut body = Vec::with_capacity(file.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    let safe_name = file_name.replace(['"', '\r', '\n'], "_");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{safe_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// `application/x-www-form-urlencoded` of `pairs`.
pub fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&percent(k));
        out.push('=');
        out.push_str(&percent(v));
    }
    out.into_bytes()
}

fn percent(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, NetError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let loaded = rustls_native_certs::load_native_certs();
            for cert in loaded.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err("Couldn’t load the system’s certificates.".to_owned());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(NetError)
}

/// Connects to a `ws://` (loopback only) or `wss://` URL and splits the
/// socket. sockudo-ws answers pings and keeps the connection alive.
pub async fn ws_connect(url: &str) -> Result<(WsReader, WsWriter), NetError> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        (false, rest)
    } else {
        return Err(NetError(format!("Not a websocket address: {url}")));
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(ix) if rest.as_bytes()[ix] == b'/' => (&rest[..ix], rest[ix..].to_owned()),
        Some(ix) => (&rest[..ix], format!("/{}", &rest[ix..])),
        None => (rest, "/".to_owned()),
    };
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h).trim_matches(['[', ']']);
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    if !secure && !loopback {
        return Err(NetError(format!("Use a wss:// address (got {url}).")));
    }
    let port_given = authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !authority.ends_with(']');
    let target = if port_given { authority.to_owned() } else { format!("{host}:{}", if secure { 443 } else { 80 }) };
    let tcp = tokio::time::timeout(Duration::from_secs(20), TcpStream::connect(&target))
        .await
        .map_err(|_| NetError(format!("{host} didn’t answer within 20 seconds.")))?
        .map_err(|e| NetError(format!("Couldn’t reach {host}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let config = Config::builder().max_payload_length(16 * 1024 * 1024).ping_interval(30).pong_timeout(15).build();
    let client = WebSocketClient::<Http1>::new(config);
    let ws = if secure {
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| NetError(format!("“{host}” is not a valid host name.")))?;
        let tls = tokio_rustls::TlsConnector::from(tls_config()?)
            .connect(name, tcp)
            .await
            .map_err(|e| NetError(format!("Secure connection to {host} failed: {e}")))?;
        client.connect(tls, authority, &path, None).await
    } else {
        client.connect(tcp, authority, &path, None).await
    };
    let (ws, _) = ws.map_err(|e| NetError(format!("Websocket upgrade with {host} failed: {e}")))?;
    Ok(ws.split())
}

/// The next text message, skipping pings and pongs. `None` when the
/// socket closed; `Some(Err)` carries the close reason or error.
pub async fn ws_next(reader: &mut WsReader) -> Option<Result<String, String>> {
    loop {
        match reader.next().await? {
            Ok(Message::Text(bytes)) | Ok(Message::Binary(bytes)) => {
                return Some(Ok(String::from_utf8_lossy(&bytes).into_owned()));
            }
            Ok(Message::Close(reason)) => {
                return Some(Err(match reason {
                    Some(r) => format!("closed ({}) {}", r.code, r.reason),
                    None => "closed".to_owned(),
                }));
            }
            Ok(_) => continue,
            Err(e) => return Some(Err(e.to_string())),
        }
    }
}

/// The close code in an error from [`ws_next`], if it carried one.
pub fn close_code(error: &str) -> Option<u16> {
    error.strip_prefix("closed (")?.split(')').next()?.parse().ok()
}

pub async fn ws_send(writer: &mut WsWriter, text: String) -> Result<(), String> {
    writer.send(Message::text(text)).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_and_multiparts_are_well_formed() {
        assert_eq!(form(&[("a b", "x&y=z"), ("n", "1")]), b"a%20b=x%26y%3Dz&n=1");
        let (ty, body) = multipart(&[("chat_id", "42")], "document", "a\"b.rs", b"fn x() {}");
        let boundary = ty.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"chat_id\"\r\n\r\n42\r\n"));
        assert!(text.contains("filename=\"a_b.rs\""));
        assert!(text.ends_with(&format!("fn x() {{}}\r\n--{boundary}--\r\n")));
        assert_eq!(close_code("closed (4004) Authentication failed."), Some(4004));
        assert_eq!(close_code("reset"), None);
    }
}
