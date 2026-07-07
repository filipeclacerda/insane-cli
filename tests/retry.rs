//! Retry/backoff behavior against the mock NIM server (SPEC §4/§10):
//! `Retry-After` respected on 429, transient 5xx recovered, exponential
//! backoff with jitter between attempts, no retry on permanent 4xx, and
//! attempt exhaustion classified correctly.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{EndpointMode, MockServer};
use insane_cli::client::nim::NimClient;
use insane_cli::client::{ChatMessage, ChatRequest, LlmClient};
use insane_cli::error::ApiError;
use insane_cli::limiter::RateLimiter;

fn make_client(server: &MockServer, timeout: Duration) -> NimClient {
    let limiter = Arc::new(RateLimiter::new(100, Duration::from_secs(60)));
    NimClient::new(
        server.base_url.clone(),
        "nvapi-test-fake-key-000".to_string(),
        timeout,
        limiter,
    )
    .unwrap()
}

fn chat_req() -> ChatRequest {
    ChatRequest {
        model: "mock-model".to_string(),
        messages: vec![ChatMessage::text("user", "hi")],
        temperature: None,
        top_p: None,
        max_tokens: None,
        stream: false,
        stream_options: None,
        tools: None,
        tool_choice: None,
    }
}

#[tokio::test]
async fn respects_retry_after_on_429() {
    let mode = EndpointMode::fail_n_times(429, 1, Some(1)); // 1 second Retry-After
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server, Duration::from_secs(10));

    let start = Instant::now();
    let resp = client
        .chat(chat_req())
        .await
        .expect("should eventually succeed");
    let elapsed = start.elapsed();

    assert!(!resp.content().is_empty());
    assert!(
        elapsed >= Duration::from_millis(950),
        "client must wait at least ~Retry-After (1s) before retrying, waited {elapsed:?}"
    );
    assert_eq!(server.request_count(), 2, "one failed attempt + one retry");
}

#[tokio::test]
async fn recovers_from_transient_500() {
    let mode = EndpointMode::fail_n_times(500, 2, None);
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server, Duration::from_secs(10));

    let resp = client
        .chat(chat_req())
        .await
        .expect("should recover after 2 failures");
    assert!(!resp.content().is_empty());
    assert_eq!(server.request_count(), 3, "2 failed attempts + 1 success");
}

#[tokio::test]
async fn backoff_grows_between_attempts_with_jitter() {
    // 3 failures forces 3 backoff waits before the 4th (successful) attempt.
    // Base backoff is 500ms*2^0=500ms (attempt 0), 500ms*2^1=1s (attempt 1),
    // 500ms*2^2=2s (attempt 2) each ±50% jitter -- so total wait is bounded
    // well below a naive "no growth" assumption, and well above a single
    // fixed-delay retry.
    let mode = EndpointMode::fail_n_times(500, 3, None);
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server, Duration::from_secs(15));

    let start = Instant::now();
    client
        .chat(chat_req())
        .await
        .expect("should recover after 3 failures");
    let elapsed = start.elapsed();

    // Minimum possible total wait across 3 backoffs, if jitter always landed
    // at the low end (0.5x): 250ms + 500ms + 1000ms = 1750ms.
    assert!(
        elapsed >= Duration::from_millis(1700),
        "backoff across 3 failed attempts should accumulate meaningfully, got {elapsed:?}"
    );
    assert_eq!(server.request_count(), 4);
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    for status in [400u16, 401, 404] {
        let mode = EndpointMode::AlwaysStatus {
            status,
            retry_after_secs: None,
        };
        let server = MockServer::start(mode, EndpointMode::Ok, true).await;
        let client = make_client(&server, Duration::from_secs(5));

        let err = client
            .chat(chat_req())
            .await
            .expect_err("should not succeed");
        match status {
            401 => assert!(matches!(err, ApiError::Auth { .. })),
            400 | 404 => assert!(matches!(err, ApiError::Permanent { .. })),
            _ => unreachable!(),
        }
        assert_eq!(
            server.request_count(),
            1,
            "status {status} must not be retried (exactly one request expected)"
        );
    }
}

#[tokio::test]
async fn attempt_exhaustion_yields_classified_error() {
    // Always fails (500) -- MAX_ATTEMPTS (5) attempts should all be spent
    // and a classified Transient error returned.
    let mode = EndpointMode::AlwaysStatus {
        status: 500,
        retry_after_secs: None,
    };
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server, Duration::from_secs(60));

    let err = client
        .chat(chat_req())
        .await
        .expect_err("should exhaust attempts and fail");
    assert!(matches!(err, ApiError::Transient { .. }), "got {err:?}");
    assert_eq!(
        server.request_count(),
        5,
        "MAX_ATTEMPTS (5) attempts should have been made before giving up"
    );
}

#[tokio::test]
async fn missing_auth_header_is_rejected_with_401() {
    // Direct check (bypassing the CLI client, which always sends a Bearer
    // token) that the mock's auth gate actually works: a bare reqwest call
    // without an Authorization header must get 401.
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/chat/completions", server.base_url))
        .json(&serde_json::json!({"model": "m", "messages": [], "stream": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}
