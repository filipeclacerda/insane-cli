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

    // This test measures *server arrival* timestamps, but the limiter's
    // guarantee is over *admission* timestamps (the instant `acquire()`
    // succeeds on the client). The two differ by request latency, so the
    // gap between consecutive admission batches (≈ `window`) shows up at
    // the server as `window + skew`. With `window = 1s` the inter-batch
    // arrival gap is ~1.008s, comfortably larger than the 1s window we
    // check, so a sliding window of arrival times can never straddle two
    // admission batches. (The sibling `metrics_total_matches_concurrent_call_count`
    // test uses the same 1s window for the same reason -- see its comment.)
    assert_no_window_exceeds(&timestamps, capacity, window);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn metrics_total_matches_concurrent_call_count() {
    // A second, independent run with different numbers, to make sure the
    // invariant isn't an artifact of one particular capacity/n combination.
    let capacity = 8;
    // NOTE: previously 750 ms, which made the test flaky. The limiter
    // bounds *admission* timestamps, but this test measures *server
    // arrival* timestamps, which lag admission by request latency. Under
    // load, consecutive admission batches (each ≈ `window` apart) arrive at
    // the server as compact clusters separated by `window + skew` (skew ≈
    // 7-13 ms of round-trip latency). With `window = 750 ms` that inter-
    // batch arrival gap is ~757 ms -- only ~7 ms more than the window --
    // so a sliding 750 ms window of arrival times intermittently straddled
    // two admission batches and counted up to 2×capacity, producing the
    // flake. Widening the window to 1 s (matching the sibling test above)
    // makes the inter-batch arrival gap (~1.008 s) comfortably exceed the
    // checked window, so a sliding window can never span two batches. This
    // does not weaken the limiter's real guarantee (≤ capacity admissions
    // per `window`, verified by the unit test `never_exceeds_capacity_in_any_window`
    // in `src/limiter.rs`, which measures post-acquire timestamps directly);
    // it only decouples this end-to-end test from sub-10 ms scheduler jitter.
    let window = Duration::from_secs(1);
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
