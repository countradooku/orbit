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
