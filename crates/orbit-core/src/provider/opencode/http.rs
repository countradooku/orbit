//! A minimal HTTP/1.1 client for the local `opencode serve`: plain
//! requests and one server-sent-events stream, over loopback TCP. Enough for
//! a server Orbit itself started on 127.0.0.1, without pulling an HTTP stack
//! into the engine.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A plain request may take this long end to end.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Response bodies larger than this are refused.
const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Client {
    /// `127.0.0.1:port`
    pub addr: String,
    /// The `Authorization` header value, if the server wants one.
    pub auth: Option<String>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl Client {
    fn head(&self, method: &str, path: &str, extra: &str) -> String {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: orbit\r\n", self.addr);
        if let Some(auth) = &self.auth {
            head.push_str(&format!("Authorization: {auth}\r\n"));
        }
        head.push_str(extra);
        head
    }

    /// Sends one request (`Connection: close`) and reads the whole response.
    pub async fn request(&self, method: &str, path: &str, body: Option<&str>) -> std::io::Result<Response> {
        let run = async {
            let mut stream = TcpStream::connect(&self.addr).await?;
            stream.set_nodelay(true)?;
            let body = body.unwrap_or("");
            let extra = format!(
                "Accept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut request = self.head(method, path, &extra).into_bytes();
            request.extend_from_slice(body.as_bytes());
            stream.write_all(&request).await?;
            let mut reader = BufReader::new(stream);
            let (status, headers) = read_head(&mut reader).await?;
            let body = if header(&headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
                let mut out = Vec::new();
                while let Some(chunk) = read_chunk(&mut reader).await? {
                    out.extend_from_slice(&chunk);
                    if out.len() > MAX_BODY {
                        return Err(std::io::Error::other("response too large"));
                    }
                }
                out
            } else if let Some(len) = header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok()) {
                if len > MAX_BODY {
                    return Err(std::io::Error::other("response too large"));
                }
                let mut out = vec![0; len];
                reader.read_exact(&mut out).await?;
                out
            } else {
                let mut out = Vec::new();
                reader.take(MAX_BODY as u64).read_to_end(&mut out).await?;
                out
            };
            Ok(Response { status, body: String::from_utf8_lossy(&body).into_owned() })
        };
        tokio::time::timeout(REQUEST_TIMEOUT, run)
            .await
            .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "OpenCode didn't answer")))
    }

    /// Opens a server-sent-events stream. Each event's `data` arrives on the
    /// returned channel; the channel closes when the stream ends.
    pub async fn events(&self, path: &str) -> std::io::Result<mpsc::Receiver<String>> {
        let mut stream = TcpStream::connect(&self.addr).await?;
        stream.set_nodelay(true)?;
        let head = self.head("GET", path, "Accept: text/event-stream\r\nCache-Control: no-cache\r\n\r\n");
        stream.write_all(head.as_bytes()).await?;
        let mut reader = BufReader::new(stream);
        let (status, headers) = tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut reader))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no event stream")))?;
        if !(200..300).contains(&status) {
            return Err(std::io::Error::other(format!("event stream refused ({status})")));
        }
        let chunked = header(&headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(async move {
            let mut parser = SseParser::default();
            loop {
                let bytes = if chunked {
                    match read_chunk(&mut reader).await {
                        Ok(Some(chunk)) => chunk,
                        _ => return,
                    }
                } else {
                    let mut buf = vec![0; 16 * 1024];
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            buf.truncate(n);
                            buf
                        }
                    }
                };
                for data in parser.feed(&bytes) {
                    if tx.send(data).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}

async fn read_head<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> std::io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line {line:?}")))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok((status, headers))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// One chunk of a chunked body; `None` after the last.
async fn read_chunk<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
        .map_err(|_| std::io::Error::other("bad chunk size"))?;
    if size > MAX_BODY {
        return Err(std::io::Error::other("chunk too large"));
    }
    let mut chunk = vec![0; size];
    reader.read_exact(&mut chunk).await?;
    let mut crlf = [0; 2];
    reader.read_exact(&mut crlf).await?;
    Ok((size > 0).then_some(chunk))
}

/// Splits a byte stream into server-sent events and returns each event's
/// `data` (lines joined with `\n`).
#[derive(Default)]
pub struct SseParser {
    pending: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data).join("\n"));
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_across_reads() {
        let mut parser = SseParser::default();
        assert!(parser.feed(b"data: {\"a\"").is_empty());
        let events = parser.feed(b":1}\r\n\r\nevent: x\ndata: one\ndata: two\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_owned(), "one\ntwo".to_owned()]);
    }

    #[tokio::test]
    async fn requests_read_chunked_and_sized_bodies() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            for reply in [
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
                "HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let _ = socket.read(&mut buf).await;
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let client = Client { addr, auth: Some("Basic x".into()) };
        let first = client.request("GET", "/a", None).await.unwrap();
        assert_eq!((first.status, first.body.as_str()), (200, "ok"));
        let second = client.request("POST", "/b", Some("{}")).await.unwrap();
        assert_eq!((second.status, second.body.as_str()), (201, "abcde"));
    }
}
