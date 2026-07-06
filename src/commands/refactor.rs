//! `refactor` command: same apply/diff/backup/rollback semantics as `fix`,
//! but requires an explicit `--goal` and preserves behavior unless the goal
//! says otherwise.

use std::path::PathBuf;

use crate::client::ChatMessage;
use crate::commands::fix::extract_fence;
use crate::context;
use crate::error::ApiError;
use crate::fileops;
use crate::secrets;
use crate::AppContext;

const SYSTEM_PROMPT: &str = "You are a senior software engineer refactoring code toward a \
stated goal, while preserving external behavior unless the goal explicitly asks otherwise. \
Respond with the COMPLETE refactored file content inside a single fenced code block and nothing \
else outside the fence.";

pub async fn run(
    ctx: &AppContext,
    file: PathBuf,
    goal: String,
    apply: bool,
    rollback: bool,
) -> Result<(), ApiError> {
    if rollback {
        fileops::rollback(&file)?;
        if !ctx.out.quiet {
            eprintln!("Restored {} from backup", file.display());
        }
        return Ok(());
    }

    let source = file.to_string_lossy().into_owned();
    let original = std::fs::read_to_string(&file)
        .map_err(|e| ApiError::permanent(format!("failed to read {}: {e}", file.display())))?;
    let findings = secrets::scan(&original);

    let loaded = context::load(
        &source,
        ctx.cfg.max_context_bytes,
        None,
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
        ChatMessage::text("user", format!("Refactor goal: {goal}\n\n{block}")),
    ];

    let response = crate::commands::fetch_full_response(ctx, model, messages).await?;

    let Some(refactored) = extract_fence(&response) else {
        println!("{response}");
        return Err(ApiError::permanent(
            "could not extract a fenced code block from the model response; nothing applied"
                .to_string(),
        ));
    };

    fileops::show_diff(&original, &refactored, &source);

    if !apply {
        return Ok(());
    }

    let should_write = if ctx.cli.yes && findings.is_empty() {
        true
    } else {
        fileops::confirm(&format!("Apply this refactor to {source}?"))
    };

    if !should_write {
        return Err(ApiError::Usage {
            message: "aborted: refactor not applied".to_string(),
        });
    }

    fileops::write_atomic(&file, &refactored)?;
    println!("Applied refactor to {source} (backup at {source}.insane-bak)");
    Ok(())
}
