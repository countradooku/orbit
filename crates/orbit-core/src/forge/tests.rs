//! The cache, request coalescing and rate-limit handling, without a host.

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

fn prs(n: u64) -> Cached {
    Cached::Prs(vec![PullRequest { number: n, ..Default::default() }])
}

fn number(fresh: &Fresh<Cached>) -> u64 {
    match &fresh.value {
        Cached::Prs(p) => p[0].number,
        _ => panic!("wrong kind"),
    }
}

fn key() -> Key {
    Key::Prs(PathBuf::from("/repo"), PrFilter::Open)
}

#[tokio::test]
async fn fresh_results_are_served_from_the_cache() {
    let forge = Forge::default();
    let calls = AtomicU32::new(0);
    let fetch = || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(prs(1))
    };
    forge.cached(key(), LIST_TTL, false, "github.com", fetch()).await.unwrap();
    forge.cached(key(), LIST_TTL, false, "github.com", fetch()).await.unwrap();
    // A refresh inside the minimum age is also served from the cache.
    forge.cached(key(), LIST_TTL, true, "github.com", fetch()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // An expired entry is refetched.
    forge.cached(key(), Duration::ZERO, false, "github.com", fetch()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_requests_share_one_fetch() {
    let forge = Forge::default();
    let calls = Arc::new(AtomicU32::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (forge, calls) = (forge.clone(), calls.clone());
        tasks.push(tokio::spawn(async move {
            let fetch = async {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(prs(5))
            };
            number(&forge.cached(key(), LIST_TTL, false, "github.com", fetch).await.unwrap())
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 5);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_rate_limit_serves_the_last_result_and_stops_asking() {
    let forge = Forge::default();
    forge.cached(key(), LIST_TTL, false, "github.com", async { Ok(prs(1)) }).await.unwrap();
    let limited = async { Err(ForgeError::RateLimited { host: "github.com".into(), reset_ms: 0 }) };
    let fresh = forge.cached(key(), Duration::ZERO, false, "github.com", limited).await.unwrap();
    assert_eq!(number(&fresh), 1);
    let note = fresh.stale.expect("a stale note");
    assert!(note.contains("rate limit"), "{note}");
    // While limited, nothing is fetched at all.
    let calls = AtomicU32::new(0);
    let fetch = async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(prs(2))
    };
    let fresh = forge.cached(key(), Duration::ZERO, true, "github.com", fetch).await.unwrap();
    assert_eq!((number(&fresh), calls.load(Ordering::SeqCst)), (1, 0));
    // Another host is unaffected, and an uncached key reports the limit.
    let other = Key::Prs(PathBuf::from("/other"), PrFilter::Open);
    let err = forge.cached(other, LIST_TTL, false, "github.com", async { Ok(prs(3)) }).await.err().unwrap();
    assert!(matches!(err, ForgeError::RateLimited { reset_ms, .. } if reset_ms > now_ms()));
    let gl = Key::Prs(PathBuf::from("/gl"), PrFilter::Open);
    assert!(forge.cached(gl, LIST_TTL, false, "gitlab.com", async { Ok(prs(4)) }).await.is_ok());
}

#[tokio::test]
async fn failures_that_are_not_transient_are_reported() {
    let forge = Forge::default();
    forge.cached(key(), LIST_TTL, false, "github.com", async { Ok(prs(1)) }).await.unwrap();
    let signed_out = async { Err(ForgeError::SignedOut { program: "gh".into(), fix: "gh auth login".into() }) };
    let err = forge.cached(key(), Duration::ZERO, false, "github.com", signed_out).await.err().unwrap();
    assert!(err.to_string().contains("gh auth login"));
}

#[tokio::test]
async fn invalidation_drops_one_repository() {
    let forge = Forge::default();
    let other = Key::Pr(PathBuf::from("/other"), 1);
    forge.cached(key(), LIST_TTL, false, "", async { Ok(prs(1)) }).await.unwrap();
    forge.cached(other.clone(), LIST_TTL, false, "", async { Ok(prs(9)) }).await.unwrap();
    forge.invalidate(Path::new("/repo"));
    assert!(forge.peek(&key()).is_none());
    assert!(forge.peek(&other).is_some());
}

#[tokio::test]
async fn join_all_keeps_input_order() {
    let slow = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        1
    };
    let fast = async { 2 };
    let fast2 = async { 3 };
    assert_eq!(join_all([Box::pin(slow) as Pin<Box<dyn Future<Output = i32> + Send>>, Box::pin(fast), Box::pin(fast2)]).await, vec![1, 2, 3]);
}

/// Serves one HTTP response on loopback and returns the request it got.
async fn serve_once(status: u16, body: String) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        // Headers, then the body by Content-Length.
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let response = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{addr}"), task)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/forge/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[tokio::test]
async fn linear_is_asked_with_the_key_over_http() {
    let (url, server) = serve_once(200, fixture("linear_assigned.json")).await;
    let linear = linear::Linear { api_key: "lin_api_KEY".into(), endpoint: format!("{url}/graphql") };
    let issues = linear.issues("").await.unwrap();
    assert_eq!(issues.len(), 2);
    let request = server.await.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("post /graphql"), "{request}");
    assert!(request.contains("authorization: lin_api_key"), "personal keys go without Bearer");
    assert!(request.contains("assignedissues"));

    let (url, server) = serve_once(400, fixture("linear_auth_error.json")).await;
    let linear = linear::Linear { api_key: "bad".into(), endpoint: format!("{url}/graphql") };
    let err = linear.issues("").await.err().unwrap();
    assert!(matches!(err, ForgeError::TrackerAuth { .. }), "{err:?}");
    server.await.unwrap();
}

#[tokio::test]
async fn jira_cloud_uses_basic_auth_and_the_jql_search() {
    let (url, server) = serve_once(200, fixture("jira_search.json")).await;
    let jira = jira::Jira { site: url, email: "a@acme.com".into(), token: "TOKEN".into() };
    let issues = jira.issues("").await.unwrap();
    assert_eq!(issues[0].key, "PROJ-7");
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /rest/api/3/search/jql?jql=assignee%20%3D%20currentUser%28%29"), "{request}");
    let basic = format!("Basic {}", http::base64("a@acme.com:TOKEN"));
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: {}", basic.to_ascii_lowercase())));

    let (url, server) = serve_once(401, "{}".into()).await;
    let server_dc = jira::Jira { site: url, email: String::new(), token: "PAT".into() };
    let err = server_dc.issue("OPS-1").await.err().unwrap();
    assert!(err.to_string().contains("personal access token"), "{err}");
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /rest/api/2/issue/OPS-1"), "{request}");
    assert!(request.contains("Bearer PAT"));
}

#[tokio::test]
async fn plain_http_is_refused_off_loopback() {
    let err = http::send("GET", "http://example.com/x", &[], None).await.err().unwrap();
    assert!(err.to_string().contains("https://"), "{err}");
}

/// Live: TLS with the system's roots against the real Linear endpoint,
/// with no key (Linear answers "authentication required"). Run with
/// `--ignored` on a machine with network access.
#[tokio::test]
#[ignore]
async fn live_tls_reaches_linear() {
    let linear = linear::Linear::new("not-a-key".into());
    let err = linear.issues("").await.err().unwrap();
    assert!(matches!(err, ForgeError::TrackerAuth { .. }), "{err:?}");
}
