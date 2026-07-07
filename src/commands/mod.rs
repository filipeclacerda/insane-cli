pub mod ask;
pub mod chat;
pub mod config_cmd;
pub mod doctor;
pub mod explain;
pub mod fix;
pub mod models;
pub mod refactor;
pub mod review;
pub mod status;
pub mod test;

use std::time::Instant;

use futures_util::StreamExt;

use crate::cache::Cache;
use crate::client::{ChatMessage, ChatRequest, LlmClient};
use crate::error::ApiError;
use crate::output::{self, JsonResult};
use crate::AppContext;

/// Shared request/response/streaming/cache plumbing for the deterministic,
/// single-turn commands (`ask`, `explain`, `review`). `cacheable` gates
/// whether the on-disk cache (SPEC §8) is consulted/populated; it has no
/// effect while streaming, since streaming responses are never cached.
pub async fn run_chat(
    ctx: &AppContext,
    model: String,
    messages: Vec<ChatMessage>,
    cacheable: bool,
) -> Result<(), ApiError> {
    let req = ChatRequest {
        model: model.clone(),
        messages: messages.clone(),
        temperature: Some(ctx.cfg.temperature),
        top_p: None,
        max_tokens: Some(ctx.cfg.max_tokens),
        stream: ctx.cfg.stream,
        stream_options: None,
        tools: None,
        tool_choice: None,
    };
    let out = ctx.out;
    let start = Instant::now();

    if ctx.cfg.stream {
        let mut stream = ctx.client.chat_stream(req).await?;
        let mut full = String::new();
        let mut finish_reason: Option<String> = None;
        while let Some(item) = stream.next().await {
            let chunk = item?;
            output::print_stream_chunk(out, &chunk.delta);
            full.push_str(&chunk.delta);
            if chunk.finish_reason.is_some() {
                finish_reason = chunk.finish_reason;
            }
        }
        if !out.json {
            println!();
        } else {
            let metrics = ctx.limiter.metrics().await;
            let result = JsonResult {
                response: &full,
                model: &model,
                usage: &Default::default(),
                timing_ms: start.elapsed().as_millis(),
                rate_limiter: metrics,
                finish_reason: finish_reason.as_deref(),
            };
            output::print_result(out, &result);
        }
        return Ok(());
    }

    let cache = (cacheable && ctx.cfg.cache.enabled).then(|| Cache::from_config(&ctx.cfg));
    let cache_key = cache.as_ref().map(|_| {
        crate::cache::compute_key(
            &ctx.cfg.base_url,
            &model,
            &messages,
            ctx.cfg.temperature,
            ctx.cfg.max_tokens,
        )
    });

    if let (Some(cache), Some(key)) = (&cache, &cache_key) {
        if let Some(cached) = cache.get(key) {
            let metrics = ctx.limiter.metrics().await;
            let result = JsonResult {
                response: cached.content(),
                model: &model,
                usage: &cached.usage,
                timing_ms: start.elapsed().as_millis(),
                rate_limiter: metrics,
                finish_reason: None,
            };
            output::print_result(out, &result);
            return Ok(());
        }
    }

    let resp = ctx.client.chat(req).await?;
    if let (Some(cache), Some(key)) = (&cache, &cache_key) {
        cache.put(key, &resp);
    }
    let metrics = ctx.limiter.metrics().await;
    let result = JsonResult {
        response: resp.content(),
        model: &model,
        usage: &resp.usage,
        timing_ms: start.elapsed().as_millis(),
        rate_limiter: metrics,
        finish_reason: resp.finish_reason(),
    };
    output::print_result(out, &result);
    Ok(())
}

/// Runs a single non-streaming chat completion and returns just the text.
/// Used by `fix`/`refactor`/`test`, which always need the complete response
/// up front to extract a fenced code block, regardless of the global
/// `--stream` flag.
pub async fn fetch_full_response(
    ctx: &AppContext,
    model: String,
    messages: Vec<ChatMessage>,
) -> Result<String, ApiError> {
    let req = ChatRequest {
        model,
        messages,
        temperature: Some(ctx.cfg.temperature),
        top_p: None,
        max_tokens: Some(ctx.cfg.max_tokens),
        stream: false,
        stream_options: None,
        tools: None,
        tool_choice: None,
    };
    let resp = ctx.client.chat(req).await?;
    Ok(resp.content().to_string())
}
