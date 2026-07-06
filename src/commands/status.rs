//! `status` command: API healthcheck (`GET /models`), rate-limiter metrics,
//! and the effective configuration (never including the API key).

use serde::Serialize;

use crate::client::LlmClient;
use crate::error::ApiError;
use crate::limiter::RateLimiterMetrics;
use crate::AppContext;

#[derive(Serialize)]
struct StatusReport<'a> {
    api_reachable: bool,
    error: Option<String>,
    rate_limiter: RateLimiterMetrics,
    config: &'a crate::config::EffectiveConfig,
}

pub async fn run(ctx: &AppContext) -> Result<(), ApiError> {
    let health = ctx.client.list_models().await;
    let (api_reachable, error) = match &health {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let metrics = ctx.limiter.metrics().await;

    if ctx.out.json {
        let report = StatusReport {
            api_reachable,
            error,
            rate_limiter: metrics,
            config: &ctx.cfg,
        };
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        println!("API reachable: {api_reachable}");
        if let Some(e) = &error {
            println!("  error: {e}");
        }
        println!(
            "Rate limiter: used={} remaining={} next_request_in_ms={} interval_ms={} total_acquired={}",
            metrics.used,
            metrics.remaining.map(|v| v.to_string()).unwrap_or_else(|| "unlimited".into()),
            metrics.next_request_in_ms,
            metrics.min_interval_ms,
            metrics.total_acquired
        );
        println!("Config:");
        println!("  provider: {}", ctx.cfg.active_provider);
        println!("  provider_kind: {:?}", ctx.cfg.provider_kind);
        println!("  model: {}", ctx.cfg.model);
        println!("  base_url: {}", ctx.cfg.base_url);
        println!("  timeout_secs: {}", ctx.cfg.timeout_secs);
        println!("  max_tokens: {}", ctx.cfg.max_tokens);
        println!("  temperature: {}", ctx.cfg.temperature);
        println!("  stream: {}", ctx.cfg.stream);
        println!(
            "  rate_limit.rpm: {}",
            ctx.cfg
                .rate_limit_rpm
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unlimited".into())
        );
        println!(
            "  rate_limit.min_interval_ms: {}",
            ctx.cfg.rate_limit_min_interval_ms
        );
        println!("  cache.enabled: {}", ctx.cfg.cache.enabled);
        println!("  config_path: {}", ctx.cfg.config_path.display());
    }

    // `status` reports the failure inline rather than returning an error, so
    // the report is still emitted even when the API is unreachable.
    Ok(())
}
