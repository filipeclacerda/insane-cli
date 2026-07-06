//! `models` command: lists available models via `GET /models`.

use serde::Serialize;

use crate::client::LlmClient;
use crate::error::ApiError;
use crate::output;
use crate::AppContext;

#[derive(Serialize)]
struct ModelsJson {
    models: Vec<String>,
}

pub async fn run(ctx: &AppContext, _refresh: bool) -> Result<(), ApiError> {
    // NOTE: on-disk model-list caching (per SPEC §8) is deferred to phase 2
    // (cache.rs). `--refresh` is accepted now for CLI-surface completeness
    // and currently always fetches fresh.
    let models = ctx.client.list_models().await?;
    let ids: Vec<String> = models.into_iter().map(|m| m.id).collect();

    if ctx.out.json {
        let json = ModelsJson { models: ids };
        println!("{}", serde_json::to_string(&json).unwrap_or_default());
    } else {
        for id in &ids {
            println!("{id}");
        }
        if ids.is_empty() {
            output::log_info(ctx.out, "no models returned");
        }
    }
    Ok(())
}
