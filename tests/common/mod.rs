//! Shared mock NIM server for integration tests (`tests/rate_limit.rs`,
//! `tests/retry.rs`, `tests/streaming.rs`, `tests/cli_e2e.rs`, ...).
//!
//! This is the "mock_nim" helper called for in the phase-3 task description;
//! it lives at `tests/common/mod.rs` (rather than `tests/mock_nim.rs`) so it
//! is compiled as a *shared module* included by each integration-test binary
//! via `#[path = "common/mod.rs"] mod common;`, instead of being built (and
//! run) as its own separate test binary -- the standard Rust pattern for
//! code shared across `tests/*.rs` files.
//!
//! Built on `axum` (chosen over `wiremock`/`httpmock` because the SPEC's test
//! requirements -- configurable SSE chunk delays, an invalid chunk mid-stream,
//! abrupt connection cutoff, and a raw per-request timestamp log -- are all
//! much more directly expressed as a small real HTTP server than as a
//! declarative mocking DSL).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::stream;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// One tool call in a scripted response. `arg_fragments` lets a test model
/// how the streamed `arguments` string is split across several SSE deltas
/// (SPEC-AGENT §1/§6); a single-fragment vec means "arrives whole" and is
/// used verbatim for non-streaming responses too.
#[derive(Clone)]
pub struct ScriptedCall {
    pub id: String,
    pub name: String,
    pub arg_fragments: Vec<String>,
}

impl ScriptedCall {
    /// A tool call whose arguments arrive as a single, unfragmented string.
    pub fn new(id: &str, name: &str, arguments: &str) -> Self {
        ScriptedCall {
            id: id.to_string(),
            name: name.to_string(),
            arg_fragments: vec![arguments.to_string()],
        }
    }

    /// A tool call whose `arguments` string is split across several deltas
    /// when streamed (id/name are only sent on the first fragment).
    pub fn fragmented(id: &str, name: &str, fragments: &[&str]) -> Self {
        ScriptedCall {
            id: id.to_string(),
            name: name.to_string(),
            arg_fragments: fragments.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn full_arguments(&self) -> String {
        self.arg_fragments.concat()
    }
}

/// One scripted `/chat/completions` response, consumed in order from the
/// `Scripted` queue. Rendered as plain JSON or SSE depending on the
/// request's own `stream` field, so the same script works whether the
/// caller streams or not.
#[derive(Clone)]
pub enum ScriptedResponse {
    /// A plain assistant text reply, `finish_reason: "stop"`.
    Text(String),
    /// An assistant reply that calls one or more tools,
    /// `finish_reason: "tool_calls"`, `content: null`.
    ToolCalls(Vec<ScriptedCall>),
    /// A plain assistant text reply with an arbitrary `finish_reason` (e.g.
    /// `"length"`) -- used to exercise SPEC-UX A3 (finish_reason warning /
    /// `/continue`).
    TextWithFinish(String, String),
}

/// Every request's arrival timestamp, regardless of route or outcome.
/// `rate_limit.rs` uses this (captured server-side, not client-side) to
/// prove no sliding window of `window` ever contains more than `capacity`
/// requests.
#[derive(Clone, Default)]
pub struct RequestLog(pub Arc<Mutex<Vec<Instant>>>);

impl RequestLog {
    pub fn snapshot(&self) -> Vec<Instant> {
        let mut v = self.0.lock().unwrap().clone();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn record(&self) {
        self.0.lock().unwrap().push(Instant::now());
    }
}

/// How the mock's `/chat/completions` and `/models` routes should behave for
/// the lifetime of one `MockServer`.
#[derive(Clone)]
pub enum EndpointMode {
    /// Always succeeds. Chat responds according to the request's own
    /// `stream` field (plain JSON or a small canned SSE completion).
    Ok,
    /// Always returns `status`, with an optional `Retry-After` header.
    AlwaysStatus {
        status: u16,
        retry_after_secs: Option<u64>,
    },
    /// Returns `status` (with optional `Retry-After`) for the first N
    /// requests (tracked by the shared counter), then `Ok` behavior after.
    FailNTimes {
        status: u16,
        retry_after_secs: Option<u64>,
        remaining: Arc<Mutex<u32>>,
    },
    /// Always streams SSE built from `chunks`, waiting `delay` between each,
    /// optionally injecting one invalid-JSON `data:` line halfway through.
    Sse {
        chunks: Vec<String>,
        delay: Duration,
        invalid_mid: bool,
    },
    /// Sleeps `delay` before responding `Ok` (used to trigger client-side
    /// timeouts).
    Slow { delay: Duration },
    /// Returns `status` with a body that echoes back whatever
    /// `Authorization` header value the request carried. Used by
    /// `fileops_secrets.rs` to prove that a secret genuinely present in a
    /// raw upstream error message still gets redacted before it reaches the
    /// user (rather than merely observing that redaction never had
    /// anything to strip).
    EchoAuthInError { status: u16 },
    /// A fixed queue of scripted responses, consumed one per request (in
    /// order). Once the queue is empty, falls back to a plain "exhausted"
    /// text reply rather than hanging -- tests that need an exact request
    /// count (e.g. `max_rounds`) simply script exactly that many entries.
    Scripted(Arc<Mutex<VecDeque<ScriptedResponse>>>),
}

impl EndpointMode {
    pub fn fail_n_times(status: u16, n: u32, retry_after_secs: Option<u64>) -> Self {
        EndpointMode::FailNTimes {
            status,
            retry_after_secs,
            remaining: Arc::new(Mutex::new(n)),
        }
    }

    /// Builds a `Scripted` mode from a fixed, ordered list of responses.
    pub fn scripted(responses: Vec<ScriptedResponse>) -> Self {
        EndpointMode::Scripted(Arc::new(Mutex::new(VecDeque::from(responses))))
    }
}

#[derive(Clone)]
struct MockState {
    log: RequestLog,
    chat_mode: Arc<Mutex<EndpointMode>>,
    models_mode: Arc<Mutex<EndpointMode>>,
    require_auth: bool,
    /// Every `/chat/completions` request body received, in arrival order --
    /// lets tests inspect exactly what was sent (messages, tools,
    /// tool_choice, tool_call_id, ...) regardless of `chat_mode`.
    requests: Arc<Mutex<Vec<Value>>>,
}

fn has_auth(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .map(|v| !v.as_bytes().is_empty())
        .unwrap_or(false)
}

fn status_response(status: u16, retry_after_secs: Option<u64>, body: Value) -> Response {
    let mut resp = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(body),
    )
        .into_response();
    if let Some(secs) = retry_after_secs {
        resp.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            secs.to_string().parse().unwrap(),
        );
    }
    resp
}

fn ok_chat_json(model: &str) -> Value {
    json!({
        "id": "mock-chat-1",
        "choices": [{
            "message": {"role": "assistant", "content": format!("mock response for {model}")},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    })
}

fn sse_body(chunks: Vec<String>, delay: Duration, invalid_mid: bool) -> Body {
    let mut lines: Vec<String> = Vec::new();
    let mid = chunks.len() / 2;
    for (i, c) in chunks.into_iter().enumerate() {
        if invalid_mid && i == mid {
            lines.push("data: {this is not valid json\n\n".to_string());
        }
        let payload = json!({"choices": [{"delta": {"content": c}, "finish_reason": null}]});
        lines.push(format!("data: {payload}\n\n"));
    }
    lines.push("data: [DONE]\n\n".to_string());

    let s = stream::unfold(lines.into_iter(), move |mut it| async move {
        match it.next() {
            Some(line) => {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Some((Ok::<Bytes, Infallible>(Bytes::from(line)), it))
            }
            None => None,
        }
    });
    Body::from_stream(s)
}

/// Non-streaming JSON body for a scripted `Text` response.
fn scripted_text_json(text: &str) -> Value {
    scripted_text_json_with_finish(text, "stop")
}

fn scripted_text_json_with_finish(text: &str, finish_reason: &str) -> Value {
    json!({
        "id": "mock-scripted-1",
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": finish_reason
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    })
}

/// Non-streaming JSON body for a scripted `ToolCalls` response: full
/// (unfragmented) `arguments` per call, `content: null`.
fn scripted_tool_calls_json(calls: &[ScriptedCall]) -> Value {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.full_arguments()}
            })
        })
        .collect();
    json!({
        "id": "mock-scripted-1",
        "choices": [{
            "message": {"role": "assistant", "content": null, "tool_calls": tool_calls},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    })
}

/// SSE body for a scripted `Text` response: one content delta, then a
/// `finish_reason: "stop"` delta, then `[DONE]`.
fn scripted_text_sse(text: &str) -> Body {
    scripted_text_sse_with_finish(text, "stop")
}

fn scripted_text_sse_with_finish(text: &str, finish_reason: &str) -> Body {
    let mut lines = Vec::new();
    lines.push(format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]})
    ));
    lines.push(format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": finish_reason}]})
    ));
    lines.push("data: [DONE]\n\n".to_string());
    lines_to_body(lines)
}

/// SSE body for a scripted `ToolCalls` response: each call's `arg_fragments`
/// become one delta apiece (id/name only on the first fragment of that
/// call's index), then a `finish_reason: "tool_calls"` delta, then `[DONE]`.
fn scripted_tool_calls_sse(calls: &[ScriptedCall]) -> Body {
    let mut lines = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        for (frag_i, fragment) in call.arg_fragments.iter().enumerate() {
            let mut tc = json!({"index": index, "function": {"arguments": fragment}});
            if frag_i == 0 {
                tc["id"] = json!(call.id);
                tc["function"]["name"] = json!(call.name);
            }
            lines.push(format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"tool_calls": [tc]}}]})
            ));
        }
    }
    lines.push(format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
    ));
    lines.push("data: [DONE]\n\n".to_string());
    lines_to_body(lines)
}

fn lines_to_body(lines: Vec<String>) -> Body {
    let s = stream::unfold(lines.into_iter(), |mut it| async move {
        it.next()
            .map(|line| (Ok::<Bytes, Infallible>(Bytes::from(line)), it))
    });
    Body::from_stream(s)
}

fn sse_response(body: Body) -> Response {
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "text/event-stream".parse().unwrap(),
    );
    resp
}

async fn chat_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.log.record();
    state.requests.lock().unwrap().push(body.clone());

    if state.require_auth && !has_auth(&headers) {
        return status_response(401, None, json!({"error": "missing Authorization header"}));
    }

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mode = state.chat_mode.lock().unwrap().clone();
    match mode {
        EndpointMode::Ok => {
            if wants_stream {
                let chunks = vec!["Hel".to_string(), "lo".to_string(), "!".to_string()];
                let mut resp = Response::new(sse_body(chunks, Duration::ZERO, false));
                resp.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    "text/event-stream".parse().unwrap(),
                );
                resp
            } else {
                Json(ok_chat_json(&model)).into_response()
            }
        }
        EndpointMode::AlwaysStatus {
            status,
            retry_after_secs,
        } => status_response(status, retry_after_secs, json!({"error": "mock failure"})),
        EndpointMode::FailNTimes {
            status,
            retry_after_secs,
            remaining,
        } => {
            let mut left = remaining.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                status_response(status, retry_after_secs, json!({"error": "mock failure"}))
            } else {
                Json(ok_chat_json(&model)).into_response()
            }
        }
        EndpointMode::Sse {
            chunks,
            delay,
            invalid_mid,
        } => {
            let mut resp = Response::new(sse_body(chunks, delay, invalid_mid));
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream".parse().unwrap(),
            );
            resp
        }
        EndpointMode::Slow { delay } => {
            tokio::time::sleep(delay).await;
            Json(ok_chat_json(&model)).into_response()
        }
        EndpointMode::EchoAuthInError { status } => {
            let auth = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            status_response(
                status,
                None,
                json!({"error": format!("upstream rejected credential: {auth}")}),
            )
        }
        EndpointMode::Scripted(queue) => {
            let next = queue.lock().unwrap().pop_front();
            let scripted = next
                .unwrap_or_else(|| ScriptedResponse::Text("scripted queue exhausted".to_string()));
            match scripted {
                ScriptedResponse::Text(text) => {
                    if wants_stream {
                        sse_response(scripted_text_sse(&text))
                    } else {
                        Json(scripted_text_json(&text)).into_response()
                    }
                }
                ScriptedResponse::ToolCalls(calls) => {
                    if wants_stream {
                        sse_response(scripted_tool_calls_sse(&calls))
                    } else {
                        Json(scripted_tool_calls_json(&calls)).into_response()
                    }
                }
                ScriptedResponse::TextWithFinish(text, finish_reason) => {
                    if wants_stream {
                        sse_response(scripted_text_sse_with_finish(&text, &finish_reason))
                    } else {
                        Json(scripted_text_json_with_finish(&text, &finish_reason)).into_response()
                    }
                }
            }
        }
    }
}

async fn models_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.log.record();

    if state.require_auth && !has_auth(&headers) {
        return status_response(401, None, json!({"error": "missing Authorization header"}));
    }

    let mode = state.models_mode.lock().unwrap().clone();
    match mode {
        EndpointMode::Ok => Json(json!({
            "data": [{"id": "meta/llama-3.3-70b-instruct"}, {"id": "mock/other-model"}]
        }))
        .into_response(),
        EndpointMode::AlwaysStatus {
            status,
            retry_after_secs,
        } => status_response(status, retry_after_secs, json!({"error": "mock failure"})),
        EndpointMode::FailNTimes {
            status,
            retry_after_secs,
            remaining,
        } => {
            let mut left = remaining.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                status_response(status, retry_after_secs, json!({"error": "mock failure"}))
            } else {
                Json(json!({"data": [{"id": "mock/other-model"}]})).into_response()
            }
        }
        EndpointMode::Slow { delay } => {
            tokio::time::sleep(delay).await;
            Json(json!({"data": []})).into_response()
        }
        EndpointMode::Sse { .. }
        | EndpointMode::EchoAuthInError { .. }
        | EndpointMode::Scripted(_) => Json(json!({"data": []})).into_response(),
    }
}

/// A running mock NIM server bound to an ephemeral localhost port. Dropping
/// this stops accepting new connections (the background task is aborted).
pub struct MockServer {
    pub base_url: String,
    pub log: RequestLog,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    /// Starts a server with independent chat/models behavior. `require_auth`
    /// controls whether a missing `Authorization` header gets a 401.
    pub async fn start(
        chat_mode: EndpointMode,
        models_mode: EndpointMode,
        require_auth: bool,
    ) -> Self {
        let log = RequestLog::default();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            log: log.clone(),
            chat_mode: Arc::new(Mutex::new(chat_mode)),
            models_mode: Arc::new(Mutex::new(models_mode)),
            require_auth,
            requests: requests.clone(),
        };

        let app = Router::new()
            .route("/chat/completions", post(chat_handler))
            .route("/models", get(models_handler))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("mock server crashed");
        });

        // Give the acceptor loop a moment to be ready (rarely needed on
        // localhost, but keeps the very first request from racing startup).
        tokio::time::sleep(Duration::from_millis(10)).await;

        MockServer {
            base_url: format!("http://{addr}"),
            log,
            requests,
            handle,
        }
    }

    /// Convenience constructor: both routes succeed, auth required.
    pub async fn ok() -> Self {
        Self::start(EndpointMode::Ok, EndpointMode::Ok, true).await
    }

    /// Convenience constructor: `/chat/completions` serves a fixed, ordered
    /// script of responses (SPEC-AGENT §6); `/models` always succeeds; auth
    /// required.
    pub async fn scripted(responses: Vec<ScriptedResponse>) -> Self {
        Self::start(EndpointMode::scripted(responses), EndpointMode::Ok, true).await
    }

    pub fn request_count(&self) -> usize {
        self.log.len()
    }

    /// Every `/chat/completions` request body received so far, in arrival
    /// order (SPEC-AGENT §6: inspect what the agent loop actually sent).
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// Runs an `assert_cmd::Command`, returning `Some(Assert)` on a normal
/// spawn. This machine's Windows Smart App Control policy intermittently
/// blocks spawning a freshly-built, unsigned test/`insane` binary with `os
/// error 4551` -- a purely environmental quirk unrelated to any behavior
/// under test (see `docs/REPORT.md`). When that happens, this prints a skip
/// notice and returns `None` instead of letting the panic propagate; callers
/// should `return` early in that case. Any other panic (a genuine assertion
/// failure, or a spawn error for an unrelated reason) still propagates
/// normally -- this only swallows the specific App-Control block.
pub fn assert_or_skip(mut cmd: assert_cmd::Command) -> Option<assert_cmd::assert::Assert> {
    install_panic_capture_hook();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || cmd.assert())) {
        Ok(assert) => Some(assert),
        Err(payload) => {
            // Recent rustc versions box format-args panics lazily, so the
            // caught payload often no longer downcasts to &str/String; the
            // hook-captured text is the reliable source for the message.
            let mut msg = panic_payload_to_string(&payload);
            if let Some(captured) = take_captured_panic_message() {
                msg = captured;
            }
            if is_smart_app_control_block(&msg) {
                eprintln!("SKIP: blocked by Windows Smart App Control (os error 4551)");
                None
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

static PANIC_CAPTURE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, String>>,
> = std::sync::OnceLock::new();

fn install_panic_capture_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let map = PANIC_CAPTURE.get_or_init(Default::default);
            if let Ok(mut map) = map.lock() {
                map.insert(std::thread::current().id(), info.to_string());
            }
            prev(info);
        }));
    });
}

fn take_captured_panic_message() -> Option<String> {
    PANIC_CAPTURE
        .get()?
        .lock()
        .ok()?
        .remove(&std::thread::current().id())
}

fn is_smart_app_control_block(msg: &str) -> bool {
    msg.contains("4551") || msg.contains("Controle de Aplicativo") || msg.contains("App Control")
}

fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Asserts that no window of length `window` in `timestamps` (already
/// sorted) contains more than `capacity` entries. Panics with a descriptive
/// message on violation, otherwise returns quietly.
pub fn assert_no_window_exceeds(timestamps: &[Instant], capacity: usize, window: Duration) {
    for &t in timestamps {
        let count = timestamps
            .iter()
            .filter(|&&x| x >= t && x < t + window)
            .count();
        assert!(
            count <= capacity,
            "window starting at {:?} contained {} requests (capacity {})",
            t,
            count,
            capacity
        );
    }
}

/// Spawns a bare-bones raw TCP server that speaks just enough HTTP/1.1 to
/// send a chunked `text/event-stream` response, writes `chunks` as
/// individually-framed chunks, and then closes the socket *without* sending
/// the terminating `0\r\n\r\n` chunk -- modeling a genuine abrupt connection
/// cutoff mid-stream (as opposed to `EndpointMode::Sse`, which always ends
/// the stream cleanly). This deliberately bypasses axum: framework-level
/// response builders always finish the body correctly, so a raw socket is
/// the only way to reproduce a truncated chunked transfer.
pub async fn spawn_cutoff_server(chunks: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cutoff server");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        if let Ok((mut socket, _)) = listener.accept().await {
            // Drain (and discard) the request until the blank line that ends
            // the headers; we don't need to parse it.
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            let header = "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Transfer-Encoding: chunked\r\n\
                 \r\n";
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            for c in chunks {
                let line = format!("data: {c}\n\n");
                let framed = format!("{:x}\r\n{}\r\n", line.len(), line);
                if socket.write_all(framed.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
            // Deliberately NOT sending the terminating "0\r\n\r\n" chunk --
            // just drop the socket to simulate a mid-stream disconnect.
            let _ = socket.shutdown().await;
        }
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    format!("http://{addr}")
}
