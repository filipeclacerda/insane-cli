//! Central proof of the rate-limiting requirement (SPEC §4/§10): using the
//! *real* `RateLimiter` (not a stand-in) with a reduced window, fire many
//! concurrent requests at the mock NIM server and verify -- from the
//! server's own timestamp log -- that no sliding window ever observed more
//! requests than the configured capacity.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{assert_no_window_exceeds, EndpointMode, MockServer};
use insane_cli::client::nim::NimClient;
use insane_cli::client::LlmClient;
use insane_cli::limiter::RateLimiter;

async fn fire_concurrent_requests(
    capacity: usize,
    window: Duration,
    n: usize,
) -> (Vec<std::time::Instant>, u64) {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let limiter = Arc::new(RateLimiter::new(capacity, window));
    let client = Arc::new(
        NimClient::new(
            server.base_url.clone(),
            "nvapi-test-fake-key-000".to_string(),
            Duration::from_secs(10),
            limiter.clone(),
        )
        .unwrap(),
    );

    let mut handles = Vec::new();
    for _ in 0..n {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            client
                .list_models()
                .await
                .expect("mock request should succeed");
        }));
    }
    for h in handles {
        h.await.expect("task panicked");
    }

    let metrics = limiter.metrics().await;
    let timestamps = server.log.snapshot();
    (timestamps, metrics.total_acquired)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_never_exceed_capacity_in_any_window() {
    let capacity = 5;
    let window = Duration::from_secs(1);
    let n = 50;

    let (timestamps, total_acquired) = fire_concurrent_requests(capacity, window, n).await;

    assert_eq!(
        timestamps.len(),
        n,
        "server should have observed exactly {n} requests"
    );
    assert_eq!(
        total_acquired, n as u64,
        "limiter's total_acquired must match the number of requests that actually went out \
         (proves every concurrent caller passed through the same shared limiter)"
    );

    assert_no_window_exceeds(&timestamps, capacity, window);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn metrics_total_matches_concurrent_call_count() {
    // A second, independent run with different numbers, to make sure the
    // invariant isn't an artifact of one particular capacity/n combination.
    let capacity = 8;
    let window = Duration::from_millis(750);
    let n = 33;

    let (timestamps, total_acquired) = fire_concurrent_requests(capacity, window, n).await;

    assert_eq!(total_acquired, n as u64);
    assert_eq!(timestamps.len(), n);
    assert_no_window_exceeds(&timestamps, capacity, window);
}

/// Heavier load test, not run by default (`cargo test -- --ignored` or
/// `cargo test -- --include-ignored` to include it). Same proof, larger N.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "heavy load test; run explicitly with `cargo test -- --ignored`"]
async fn heavy_load_never_exceeds_capacity() {
    let capacity = 20;
    let window = Duration::from_secs(2);
    let n = 500;

    let (timestamps, total_acquired) = fire_concurrent_requests(capacity, window, n).await;

    assert_eq!(total_acquired, n as u64);
    assert_eq!(timestamps.len(), n);
    assert_no_window_exceeds(&timestamps, capacity, window);
}
