//! In-process tests for the agentic loop (`src/agent.rs`, SPEC-AGENT §4/§6).
//!
//! These never spawn the compiled `insane`/`insane.exe` binary -- this
//! machine's Windows Smart App Control intermittently blocks spawning
//! freshly-built, unsigned binaries (`os error 4551`), which would make any
//! `assert_cmd`-based test here flaky for reasons that have nothing to do
//! with the code under test. Instead, every test drives the library
//! directly: builds a real `NimClient` pointed at the in-process mock
//! server (`tests/common/mod.rs`), a real `Session`/`Permissions`, and calls
//! `insane_cli::agent::run_turn` exactly as `commands::chat::run` does.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use common::{MockServer, ScriptedCall, ScriptedResponse};
use insane_cli::agent;
use insane_cli::cli::Cli;
use insane_cli::client::nim::NimClient;
use insane_cli::config::{CacheConfig, EffectiveConfig};
use insane_cli::limiter::RateLimiter;
use insane_cli::output::OutputOptions;
use insane_cli::session::Session;
use insane_cli::tools;
use insane_cli::tools::permission::Permissions;
use insane_cli::ui::test_support::FakeUi;
use insane_cli::ui::PlainUi;
use insane_cli::AppContext;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const FAKE_KEY: &str = "nvapi-test-fake-key-000";

/// Builds an `AppContext` wired to `base_url`, with a fresh `RateLimiter`
/// (defaults to generous capacity unless overridden by the caller).
fn test_ctx(base_url: &str, limiter: Arc<RateLimiter>) -> AppContext {
    let cli = Cli::parse_from(["insane", "status"]);
    let cfg = EffectiveConfig {
        active_provider: "test".to_string(),
        providers: std::collections::BTreeMap::new(),
        provider_kind: insane_cli::config::ProviderKind::OpenaiCompatible,
        provider_auth: insane_cli::config::AuthMode::Required,
        provider_api_key_env: "TEST_API_KEY".to_string(),
        model: "mock/agent-model".to_string(),
        base_url: base_url.to_string(),
        timeout_secs: 10,
        max_tokens: 256,
        temperature: 0.0,
        stream: true,
        cache: CacheConfig::default(),
        rate_limit_rpm: Some(1000),
        rate_limit_min_interval_ms: 0,
        ignore: vec![],
        max_context_bytes: 192 * 1024,
        agent_max_rounds: 20,
        agent_rate_cooldown_pct: 75,
        agent_compact_max_tokens: 1024,
        agent_temperature: 0.2,
        lenient_tool_calls: true,
        system_prompt_extra: String::new(),
        ui: "plain".to_string(),
        config_path: std::path::PathBuf::from("config.toml"),
    };
    let client = NimClient::new(
        base_url.to_string(),
        FAKE_KEY.to_string(),
        Duration::from_secs(10),
        limiter.clone(),
    )
    .expect("build NimClient");
    AppContext {
        cfg,
        client,
        limiter,
        out: OutputOptions {
            json: false,
            quiet: true,
        },
        cli,
    }
}

fn default_ctx(base_url: &str) -> AppContext {
    test_ctx(
        base_url,
        Arc::new(RateLimiter::new(1000, Duration::from_secs(60))),
    )
}

fn new_session() -> Session {
    let mut s = Session::new("mock/agent-model".to_string(), 192 * 1024);
    s.push_user("please read f.txt".to_string());
    s
}

// ---------------------------------------------------------------------
// Full turn: tool_calls round, then a final text round.
// ---------------------------------------------------------------------

#[tokio::test]
async fn full_turn_executes_tool_and_sends_role_tool_reply() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "hello from disk").unwrap();

    let args = json!({"path": "f.txt"}).to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_1", "read_file", &args)]),
        ScriptedResponse::Text("the file says hello".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let result = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await;
    assert!(result.is_ok(), "turn should complete: {result:?}");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one request per round");

    // Both requests advertise tools with tool_choice "auto".
    for req in &requests {
        assert!(req["tools"].is_array(), "request missing tools: {req}");
        assert_eq!(req["tool_choice"], "auto");
    }

    // The 2nd request's message history must contain the assistant's
    // tool_calls message and the matching `role: "tool"` reply.
    let messages = requests[1]["messages"].as_array().expect("messages array");
    let assistant_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("assistant tool_calls message present");
    assert_eq!(assistant_msg["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        assistant_msg["tool_calls"][0]["function"]["name"],
        "read_file"
    );

    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool reply message present");
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], true);
    assert!(tool_content["output"]
        .as_str()
        .unwrap()
        .contains("hello from disk"));
}

// ---------------------------------------------------------------------
// Fragmented streamed tool-call arguments.
// ---------------------------------------------------------------------

#[tokio::test]
async fn fragmented_stream_arguments_are_accumulated_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("frag.txt"), "fragmented content ok").unwrap();

    // The JSON arguments string `{"path":"frag.txt"}` split across 5
    // deltas -- none of which is valid JSON on its own.
    let fragments = ["{\"pa", "th\":\"fr", "ag", ".txt", "\"}"];
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::fragmented(
            "call_frag",
            "read_file",
            &fragments,
        )]),
        ScriptedResponse::Text("done".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let result = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await;
    assert!(result.is_ok(), "turn should complete: {result:?}");

    let requests = server.requests();
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], true);
    assert!(tool_content["output"]
        .as_str()
        .unwrap()
        .contains("fragmented content ok"));
}

// ---------------------------------------------------------------------
// max_rounds limits tool-calling loops; rate limiting governs pacing.
// ---------------------------------------------------------------------

#[tokio::test]
async fn low_max_rounds_interrupts_tool_calling_loop() {
    let dir = tempfile::tempdir().unwrap();
    const MAX_ROUNDS: u32 = 2;

    let scripted: Vec<ScriptedResponse> = (0..4u32)
        .map(|i| {
            ScriptedResponse::ToolCalls(vec![ScriptedCall::new(
                &format!("call_{i}"),
                "list_files",
                "{}",
            )])
        })
        .chain(std::iter::once(ScriptedResponse::Text("done".to_string())))
        .collect();
    let server = MockServer::scripted(scripted).await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let result = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        MAX_ROUNDS,
        &PlainUi::new(ctx.out),
    )
    .await;

    let outcome = result.expect("turn should stop cleanly at max_rounds");
    assert_eq!(outcome.finish_reason.as_deref(), Some("max_rounds"));
    assert_eq!(outcome.rounds, MAX_ROUNDS);
    assert_eq!(
        server.requests().len(),
        MAX_ROUNDS as usize,
        "the loop must stop before spending another model request"
    );
}

// ---------------------------------------------------------------------
// Every round goes through the rate limiter.
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_round_passes_through_the_rate_limiter() {
    let dir = tempfile::tempdir().unwrap();
    let args = json!({}).to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_1", "list_files", &args)]),
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_2", "list_files", &args)]),
        ScriptedResponse::Text("done".to_string()),
    ])
    .await;

    // Small capacity/short window: still large enough that this test isn't
    // slow, but proves the *same* limiter instance is actually being used
    // by counting acquires afterward.
    let limiter = Arc::new(RateLimiter::new(50, Duration::from_millis(200)));
    let ctx = test_ctx(&server.base_url, limiter.clone());
    let mut session = new_session();
    let mut permissions = Permissions::new();

    agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn completes");

    let metrics = limiter.metrics().await;
    assert_eq!(
        metrics.total_acquired as usize,
        server.requests().len(),
        "one limiter acquire per request the mock received"
    );
    assert_eq!(server.requests().len(), 3);
}

// ---------------------------------------------------------------------
// Model asks for a tool that doesn't exist: session must continue.
// ---------------------------------------------------------------------

#[tokio::test]
async fn nonexistent_tool_returns_error_result_and_session_continues() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_bad", "not_a_real_tool", "{}")]),
        ScriptedResponse::Text("recovered".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let result = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await;
    assert!(
        result.is_ok(),
        "unknown tool must not abort the turn: {result:?}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], false);
    assert!(tool_content["error"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));
}

// ---------------------------------------------------------------------
// Non-TTY stdin: write_file / edit_file / run_command are auto-refused,
// and the filesystem is left untouched.
// ---------------------------------------------------------------------

#[tokio::test]
async fn write_file_is_refused_on_non_tty_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let args = json!({"path": "new.txt", "content": "should never land"}).to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_w", "write_file", &args)]),
        ScriptedResponse::Text("ok".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let ui = FakeUi::deny();
    let mut permissions = Permissions::with_ui(Box::new(FakeUi::deny()));

    agent::run_turn(&ctx, &mut session, &mut permissions, dir.path(), 20, &ui)
        .await
        .expect("turn completes even after a denied write");

    assert!(!dir.path().join("new.txt").exists());

    let requests = server.requests();
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], false);
    assert!(tool_content["error"].as_str().unwrap().contains("denied"));
}

#[tokio::test]
async fn plan_tool_defs_do_not_allow_file_writes_even_if_model_requests_them() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("plan-should-not-write.txt");
    let args = json!({"path": "plan-should-not-write.txt", "content": "nope"}).to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_w", "write_file", &args)]),
        ScriptedResponse::Text("Plano sem editar.".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let ui = FakeUi::accept();
    let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));

    agent::run_turn_with_tool_defs(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &ui,
        tools::plan_tool_defs(),
    )
    .await
    .expect("turn completes after disallowed plan-mode write request");

    assert!(!target.exists(), "PLAN mode must not execute write_file");
    let requests = server.requests();
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], false);
    assert!(tool_content["error"]
        .as_str()
        .unwrap()
        .contains("not available in this mode"));
}

#[tokio::test]
async fn edit_file_is_refused_on_non_tty_and_leaves_file_intact() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "original content").unwrap();
    let args = json!({
        "path": "existing.txt",
        "old_string": "original",
        "new_string": "hacked"
    })
    .to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_e", "edit_file", &args)]),
        ScriptedResponse::Text("ok".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let ui = FakeUi::deny();
    let mut permissions = Permissions::with_ui(Box::new(FakeUi::deny()));

    agent::run_turn(&ctx, &mut session, &mut permissions, dir.path(), 20, &ui)
        .await
        .expect("turn completes even after a denied edit");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
        "original content"
    );
}

// ---------------------------------------------------------------------
// SPEC-UX A2: agent temperature and max_tokens are actually sent.
// ---------------------------------------------------------------------

#[tokio::test]
async fn agent_requests_send_configured_temperature_and_max_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::scripted(vec![ScriptedResponse::Text("done".to_string())]).await;

    let mut ctx = default_ctx(&server.base_url);
    ctx.cfg.agent_temperature = 0.33;
    ctx.cfg.max_tokens = 4096;
    let mut session = new_session();
    let mut permissions = Permissions::new();

    agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn completes");

    let requests = server.requests();
    assert_eq!(requests[0]["temperature"], 0.33);
    assert_eq!(requests[0]["max_tokens"], 4096);
}

// ---------------------------------------------------------------------
// SPEC-UX A3: finish_reason surfaced on the outcome; a non-stop/tool_calls
// reason (e.g. "length") is reported.
// ---------------------------------------------------------------------

#[tokio::test]
async fn finish_reason_is_reported_on_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::scripted(vec![ScriptedResponse::TextWithFinish(
        "the response got cut off mid".to_string(),
        "length".to_string(),
    )])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let outcome = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn completes even on finish_reason=length");

    assert_eq!(outcome.finish_reason.as_deref(), Some("length"));
    assert_eq!(outcome.last_text, "the response got cut off mid");
}

#[tokio::test]
async fn finish_reason_stop_is_reported_normally() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::scripted(vec![ScriptedResponse::Text("all done".to_string())]).await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let outcome = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn completes");

    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
}

// ---------------------------------------------------------------------
// SPEC-UX A4: a tool call emitted as text (not structured tool_calls) is
// recovered and executed -- the real-world GLM-style failure mode.
// ---------------------------------------------------------------------

#[tokio::test]
async fn tool_call_emitted_as_text_is_recovered_and_executed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("agent.rs"), "fn main() {}").unwrap();

    // First round: the model announces the action in prose, then emits the
    // tool call as JSON text instead of using structured tool_calls
    // (finish_reason "stop", no tool_calls array at all).
    let text_call = "Vou ler o arquivo para analisar.\n\n\
        {\"name\": \"read_file\", \"arguments\": {\"path\": \"agent.rs\"}}";
    let server = MockServer::scripted(vec![
        ScriptedResponse::Text(text_call.to_string()),
        ScriptedResponse::Text("O arquivo contem fn main.".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let outcome = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn should recover the text tool call and complete");

    // Two requests: the round with the recovered call, then the round after
    // the tool executed.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("recovered tool call must have produced a role:tool reply");
    let tool_content: serde_json::Value =
        serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_content["ok"], true);
    assert!(tool_content["output"].as_str().unwrap().contains("fn main"));

    // The synthetic id is a text_call_* id, and it matches the assistant
    // message's tool_calls entry it replies to.
    assert!(tool_msg["tool_call_id"]
        .as_str()
        .unwrap()
        .starts_with("text_call_"));
    let assistant_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("assistant message with recovered tool_calls");
    let visible_text = assistant_msg["content"]
        .as_str()
        .expect("visible text should be preserved beside recovered tool call");
    assert!(visible_text.contains("Vou ler o arquivo"));
    assert!(!visible_text.contains("\"name\""));
    assert!(!visible_text.contains("read_file"));
    assert_eq!(
        assistant_msg["tool_calls"][0]["function"]["name"],
        "read_file"
    );

    assert_eq!(outcome.last_text, "O arquivo contem fn main.");
}

#[tokio::test]
async fn lenient_tool_calls_disabled_leaves_text_as_a_plain_answer() {
    let dir = tempfile::tempdir().unwrap();
    let text_call = r#"{"name": "read_file", "arguments": {"path": "agent.rs"}}"#;
    let server = MockServer::scripted(vec![ScriptedResponse::Text(text_call.to_string())]).await;

    let mut ctx = default_ctx(&server.base_url);
    ctx.cfg.lenient_tool_calls = false;
    let mut session = new_session();
    let mut permissions = Permissions::new();

    let outcome = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &PlainUi::new(ctx.out),
    )
    .await
    .expect("turn completes");

    // With lenient detection disabled, the JSON text is left as the plain
    // final answer -- no second request was made to execute anything.
    assert_eq!(server.requests().len(), 1);
    assert_eq!(outcome.last_text, text_call);
}

#[tokio::test]
async fn run_command_is_refused_on_non_tty_and_never_executes() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker.txt");
    let command = if cfg!(windows) {
        format!(
            "New-Item -Path '{}' -ItemType File | Out-Null",
            marker.display()
        )
    } else {
        format!("touch '{}'", marker.display())
    };
    let args = json!({"command": command}).to_string();
    let server = MockServer::scripted(vec![
        ScriptedResponse::ToolCalls(vec![ScriptedCall::new("call_c", "run_command", &args)]),
        ScriptedResponse::Text("ok".to_string()),
    ])
    .await;

    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let ui = FakeUi::deny();
    let mut permissions = Permissions::with_ui(Box::new(FakeUi::deny()));

    agent::run_turn(&ctx, &mut session, &mut permissions, dir.path(), 20, &ui)
        .await
        .expect("turn completes even after a denied command");

    assert!(
        !marker.exists(),
        "command must never have been executed after being denied"
    );
}

#[tokio::test]
async fn compact_session_replaces_history_and_preserves_system_prompt() {
    let server = MockServer::scripted(vec![ScriptedResponse::Text(
        "Objetivo: ajustar o agente. Arquivos: src/agent.rs. Próximo passo: testar.".to_string(),
    )])
    .await;
    let ctx = default_ctx(&server.base_url);
    let mut session = Session::new("mock/agent-model".to_string(), 192 * 1024);
    session.push_system("system prompt atual".to_string());
    session.push_user("preciso ajustar max_rounds".to_string());
    session.push_assistant("vou ler os arquivos".to_string());
    session.push_user("também compacte a conversa".to_string());
    session.push_assistant("ok".to_string());
    session.push_user("mais contexto para passar do mínimo".to_string());

    let stats = insane_cli::commands::chat::compact_session(&ctx, &mut session)
        .await
        .expect("compact succeeds")
        .expect("conversation should be compacted");

    assert_eq!(stats.original_messages, 5);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[0].role, "system");
    assert_eq!(
        session.history[0].content.as_deref(),
        Some("system prompt atual")
    );
    assert_eq!(session.history[1].role, "user");
    assert!(session.history[1]
        .content
        .as_deref()
        .unwrap()
        .contains("[compacted conversation summary]"));
    assert!(session.history[1]
        .content
        .as_deref()
        .unwrap()
        .contains("Objetivo"));

    let request = &server.requests()[0];
    assert_eq!(request["stream"], false);
    assert!(request.get("tools").is_none());
    assert_eq!(request["max_tokens"], 1024);
}

#[tokio::test]
async fn compact_session_skips_small_conversation_without_network() {
    let server = MockServer::scripted(vec![ScriptedResponse::Text(
        "should not be requested".to_string(),
    )])
    .await;
    let ctx = default_ctx(&server.base_url);
    let mut session = Session::new("mock/agent-model".to_string(), 192 * 1024);
    session.push_system("system prompt atual".to_string());
    session.push_user("oi".to_string());

    let stats = insane_cli::commands::chat::compact_session(&ctx, &mut session)
        .await
        .expect("small compact check succeeds");

    assert!(stats.is_none());
    assert!(server.requests().is_empty());
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[1].content.as_deref(), Some("oi"));
}

#[tokio::test]
async fn cancellation_during_tool_stops_turn_before_next_round_and_replies_to_every_call() {
    let dir = tempfile::tempdir().unwrap();
    let calls = vec![
        ScriptedCall::new(
            "call_slow",
            "run_command",
            &json!({"command": "sleep 10", "timeout_secs": 30}).to_string(),
        ),
        ScriptedCall::new("call_pending", "list_files", "{}"),
    ];
    let server = MockServer::scripted(vec![ScriptedResponse::ToolCalls(calls)]).await;
    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();
    permissions.set_policy(insane_cli::tools::permission::ApprovalPolicy::Auto);
    let cancellation = CancellationToken::new();
    let cancel_soon = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_soon.cancel();
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        agent::run_turn_with_cancel(
            &ctx,
            &mut session,
            &mut permissions,
            dir.path(),
            20,
            &FakeUi::deny(),
            cancellation,
        ),
    )
    .await
    .expect("cancelled turn returns promptly")
    .expect("turn succeeds structurally");

    assert_eq!(outcome.finish_reason.as_deref(), Some("cancelled"));
    assert_eq!(server.requests().len(), 1, "must not start another round");
    let replies: Vec<_> = session
        .history
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(replies.len(), 2, "every assistant tool call has a reply");
    for reply in replies {
        let value: serde_json::Value =
            serde_json::from_str(reply.content.as_deref().unwrap()).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("cancelled"));
    }
}

#[tokio::test]
async fn legacy_run_turn_uses_a_fresh_uncancelled_token() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::scripted(vec![ScriptedResponse::Text("done".to_string())]).await;
    let ctx = default_ctx(&server.base_url);
    let mut session = new_session();
    let mut permissions = Permissions::new();
    let outcome = agent::run_turn(
        &ctx,
        &mut session,
        &mut permissions,
        dir.path(),
        20,
        &FakeUi::deny(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn cancellation_during_rate_cooldown_returns_without_requesting_model() {
    let server =
        MockServer::scripted(vec![ScriptedResponse::Text("must not run".to_string())]).await;
    let limiter = Arc::new(RateLimiter::new(1, Duration::from_secs(10)));
    limiter.acquire().await;
    let ctx = test_ctx(&server.base_url, limiter);
    let mut session = new_session();
    let mut permissions = Permissions::new();
    let cancellation = CancellationToken::new();
    let cancel_soon = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_soon.cancel();
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        agent::run_turn_with_cancel(
            &ctx,
            &mut session,
            &mut permissions,
            std::path::Path::new("."),
            20,
            &FakeUi::deny(),
            cancellation,
        ),
    )
    .await
    .expect("cooldown wait is cancellable")
    .unwrap();
    assert_eq!(outcome.finish_reason.as_deref(), Some("cancelled"));
    assert!(server.requests().is_empty());
}
