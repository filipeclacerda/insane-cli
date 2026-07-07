use std::time::Instant;

use futures_util::StreamExt;
use serde::Serialize;

use crate::client::{ChatMessage, ChatRequest, LlmClient};
use crate::error::ApiError;
use crate::AppContext;

#[derive(Serialize)]
struct DoctorReport {
    provider: String,
    base_url: String,
    auth: String,
    models_reachable: bool,
    selected_model_found: bool,
    models_latency_ms: u128,
    deep_ok: Option<bool>,
    ttft_ms: Option<u128>,
    warning: Option<String>,
}

pub async fn run(ctx: &AppContext, deep: bool) -> Result<(), ApiError> {
    let started = Instant::now();
    let models = ctx.client.list_models().await?;
    let models_latency_ms = started.elapsed().as_millis();
    let selected_model_found = models.iter().any(|model| model.id == ctx.cfg.model);
    let warning = if ctx.cfg.provider_kind == crate::config::ProviderKind::Lmstudio
        && !ctx.cfg.base_url.contains("127.0.0.1")
        && !ctx.cfg.base_url.contains("localhost")
        && ctx.cfg.provider_auth != crate::config::AuthMode::Required
    {
        Some("LM Studio is not on loopback and authentication is not required".to_string())
    } else {
        None
    };

    let mut deep_ok = None;
    let mut ttft_ms = None;
    if deep {
        let started = Instant::now();
        let req = ChatRequest {
            model: ctx.cfg.model.clone(),
            messages: vec![ChatMessage::text("user", "Reply with OK.")],
            temperature: Some(0.0),
            top_p: None,
            max_tokens: Some(8),
            stream: true,
            stream_options: None,
            tools: Some(crate::tools::all_tool_defs().into_iter().take(1).collect()),
            tool_choice: Some("auto".to_string()),
        };
        let mut stream = ctx.client.chat_stream(req).await?;
        let mut received = false;
        while let Some(item) = stream.next().await {
            let chunk = item?;
            if !chunk.delta.is_empty()
                || !chunk.reasoning_delta.is_empty()
                || !chunk.tool_calls.is_empty()
            {
                ttft_ms = Some(started.elapsed().as_millis());
                received = true;
                break;
            }
        }
        deep_ok = Some(received);
    }

    let report = DoctorReport {
        provider: ctx.cfg.active_provider.clone(),
        base_url: ctx.cfg.base_url.clone(),
        auth: format!("{:?}", ctx.cfg.provider_auth).to_ascii_lowercase(),
        models_reachable: true,
        selected_model_found,
        models_latency_ms,
        deep_ok,
        ttft_ms,
        warning,
    };
    if ctx.out.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        println!("Provider: {} ({})", report.provider, report.base_url);
        println!("Models endpoint: ok ({}ms)", report.models_latency_ms);
        println!("Selected model present: {}", report.selected_model_found);
        if let Some(ok) = report.deep_ok {
            println!(
                "Streaming probe: {ok} (TTFT: {}ms)",
                report.ttft_ms.unwrap_or(0)
            );
        }
        if let Some(warning) = report.warning {
            println!("Warning: {warning}");
        }
    }
    Ok(())
}
