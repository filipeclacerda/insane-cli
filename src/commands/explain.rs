//! `explain` command: explains a piece of code (a file, `--lines` range, or
//! stdin via `-`).

use crate::client::ChatMessage;
use crate::context;
use crate::error::ApiError;
use crate::AppContext;

const SYSTEM_PROMPT: &str = "You are a senior software engineer. Explain the given code clearly \
and concisely: its purpose, control flow, notable edge cases, and any risks. Do not rewrite the \
code.";

pub async fn run(ctx: &AppContext, file: String, lines: Option<String>) -> Result<(), ApiError> {
    let range = lines.as_deref().map(context::parse_lines_arg).transpose()?;
    let loaded = context::load(
        &file,
        ctx.cfg.max_context_bytes,
        range,
        &ctx.cfg.ignore,
        ctx.out.quiet,
    )?;
    let block = context::format_block(&loaded.display_path, &loaded.content);

    let model = ctx
        .cli
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.model.clone());
    let messages = vec![
        ChatMessage::text("system", SYSTEM_PROMPT),
        ChatMessage::text("user", format!("Explain this code:\n\n{block}")),
    ];

    // Deterministic single-turn command: eligible for the on-disk cache
    // whenever `cache.enabled` is set (SPEC §8).
    crate::commands::run_chat(ctx, model, messages, true).await
}
