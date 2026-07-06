//! `test` command: generates tests for a file. Without `-o`, prints to
//! stdout. With `-o`, shows the generated content (or a diff against an
//! existing output file) and asks for confirmation before writing
//! atomically.

use std::path::PathBuf;

use crate::client::ChatMessage;
use crate::commands::fix::extract_fence;
use crate::context;
use crate::error::ApiError;
use crate::fileops;
use crate::AppContext;

const SYSTEM_PROMPT: &str = "You are a senior software engineer writing tests for the given \
file, using the idiomatic test framework and conventions for its language. Respond with ONLY \
the test code inside a single fenced code block and nothing else outside the fence.";

pub async fn run(ctx: &AppContext, file: PathBuf, output: Option<PathBuf>) -> Result<(), ApiError> {
    let source = file.to_string_lossy().into_owned();
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
        ChatMessage::text("user", format!("Write tests for this file:\n\n{block}")),
    ];

    let response = crate::commands::fetch_full_response(ctx, model, messages).await?;
    let generated = extract_fence(&response).unwrap_or(response);

    let Some(out_path) = output else {
        println!("{generated}");
        return Ok(());
    };

    let out_str = out_path.to_string_lossy().into_owned();
    if out_path.exists() {
        let existing = std::fs::read_to_string(&out_path).map_err(|e| {
            ApiError::permanent(format!("failed to read {}: {e}", out_path.display()))
        })?;
        fileops::show_diff(&existing, &generated, &out_str);
    } else {
        println!("{generated}");
    }

    if !fileops::confirm(&format!("Write generated tests to {out_str}?")) {
        return Err(ApiError::Usage {
            message: "aborted: tests not written".to_string(),
        });
    }

    fileops::write_atomic(&out_path, &generated)?;
    println!("Wrote tests to {out_str}");
    Ok(())
}
