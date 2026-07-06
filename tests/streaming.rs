//! Streaming (SSE) behavior against the mock NIM server: full completion,
//! an invalid chunk mid-stream (tolerated, stream continues), an abrupt
//! connection cutoff (clean transient error / natural end, never a panic),
//! a client-side timeout against a slow server, and a basic backpressure
//! sanity check (slow consumer doesn't blow up memory/time).

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{spawn_cutoff_server, EndpointMode, MockServer};
use futures_util::StreamExt;
use insane_cli::client::nim::NimClient;
use insane_cli::client::{ChatMessage, ChatRequest, LlmClient};
use insane_cli::limiter::RateLimiter;

fn make_client(base_url: &str, timeout: Duration) -> NimClient {
    let limiter = Arc::new(RateLimiter::new(100, Duration::from_secs(60)));
    NimClient::new(
        base_url.to_string(),
        "nvapi-test-fake-key-000".to_string(),
        timeout,
        limiter,
    )
    .unwrap()
}

fn stream_req() -> ChatRequest {
    ChatRequest {
        model: "mock-model".to_string(),
        messages: vec![ChatMessage::text("user", "hi")],
        temperature: None,
        top_p: None,
        max_tokens: None,
        stream: true,
        tools: None,
        tool_choice: None,
    }
}

#[tokio::test]
async fn full_sse_stream_completes_with_all_deltas() {
    let mode = EndpointMode::Sse {
        chunks: vec!["Hel".to_string(), "lo".to_string(), " world".to_string()],
        delay: Duration::from_millis(5),
        invalid_mid: false,
    };
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server.base_url, Duration::from_secs(10));

    let mut stream = client.chat_stream(stream_req()).await.unwrap();
    let mut full = String::new();
    while let Some(item) = stream.next().await {
        full.push_str(&item.unwrap().delta);
    }
    assert_eq!(full, "Hello world");
}

#[tokio::test]
async fn invalid_chunk_mid_stream_is_skipped_and_stream_continues() {
    let mode = EndpointMode::Sse {
        chunks: vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ],
        delay: Duration::from_millis(2),
        invalid_mid: true,
    };
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server.base_url, Duration::from_secs(10));

    let mut stream = client.chat_stream(stream_req()).await.unwrap();
    let mut full = String::new();
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => full.push_str(&chunk.delta),
            Err(_) => saw_error = true,
        }
    }
    // The malformed line is logged as a warning and silently skipped (per
    // src/client/sse.rs), not surfaced as a stream error.
    assert!(
        !saw_error,
        "an invalid mid-stream chunk must not be surfaced as an Err item"
    );
    assert_eq!(
        full, "abcd",
        "all valid deltas must still arrive despite the bad chunk"
    );
}

#[tokio::test]
async fn abrupt_connection_cutoff_does_not_panic_and_ends_cleanly() {
    let base_url =
        spawn_cutoff_server(vec!["partial-1".to_string(), "partial-2".to_string()]).await;
    let client = make_client(&base_url, Duration::from_secs(10));

    let mut stream = client.chat_stream(stream_req()).await.unwrap();
    let mut items = Vec::new();
    // Draining to completion must not panic even though the underlying
    // socket is closed mid-chunk; either a transient Err is surfaced or the
    // stream just ends after the bytes that did arrive.
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    // Reaching this line at all means draining the stream to completion did
    // not panic despite the socket closing mid-chunk. Either some Ok deltas
    // arrived before the cutoff, or the parser surfaced a transient error --
    // both are acceptable; a panic or an infinite hang are not.
    let _ = items;
}

#[tokio::test]
async fn client_side_timeout_on_slow_server() {
    let mode = EndpointMode::Slow {
        delay: Duration::from_secs(3),
    };
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    // Timeout much shorter than the server's artificial delay.
    let client = make_client(&server.base_url, Duration::from_millis(200));

    let req = ChatRequest {
        model: "mock-model".to_string(),
        messages: vec![ChatMessage::text("user", "hi")],
        temperature: None,
        top_p: None,
        max_tokens: None,
        stream: false,
        tools: None,
        tool_choice: None,
    };
    let start = std::time::Instant::now();
    let err = client.chat(req).await.expect_err("must time out, not hang");
    // reqwest timeouts surface as network errors -> classified Transient and
    // retried with backoff (MAX_ATTEMPTS=5), so total wall time includes
    // several backoff waits, not just the raw per-attempt timeout. What
    // matters here is that each attempt gives up at ~200ms rather than
    // waiting out the server's 3s artificial delay -- bound generously
    // above the worst-case backoff sum (well under the 5 * 3s = 15s it
    // would take if timeouts weren't actually firing).
    assert!(matches!(err, insane_cli::error::ApiError::Transient { .. }));
    assert!(
        start.elapsed() < Duration::from_secs(12),
        "attempts should each time out at ~200ms (plus backoff), not wait for the server's 3s \
         delay every time; elapsed {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn slow_consumer_backpressure_does_not_explode() {
    // 200 small chunks with no server-side delay; the consumer artificially
    // slows itself down. This approximates backpressure: the `Stream` is
    // pulled on demand (per src/client/sse.rs), so a slow consumer simply
    // takes longer -- it must not buffer unboundedly or panic.
    let chunks: Vec<String> = (0..200).map(|i| format!("c{i}")).collect();
    let mode = EndpointMode::Sse {
        chunks: chunks.clone(),
        delay: Duration::ZERO,
        invalid_mid: false,
    };
    let server = MockServer::start(mode, EndpointMode::Ok, true).await;
    let client = make_client(&server.base_url, Duration::from_secs(30));

    let mut stream = client.chat_stream(stream_req()).await.unwrap();
    let mut total_len = 0usize;
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        total_len += chunk.delta.len();
        count += 1;
        // Simulate a slow consumer (e.g. slow terminal writes).
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(count, 200);
    assert!(total_len > 0);
}
