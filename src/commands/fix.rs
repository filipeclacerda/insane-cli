//! `fix` command: asks the model for a corrected version of a whole file,
//! always shows a diff, and only writes with `--apply` (after confirmation,
//! atomically, with a `.insane-bak` backup). `--rollback` restores the
//! backup and exits without contacting the model.

use std::path::PathBuf;

use crate::client::ChatMessage;
use crate::context;
use crate::error::ApiError;
use crate::fileops;
use crate::secrets;
use crate::AppContext;

const SYSTEM_PROMPT: &str = "You are a senior software engineer fixing a bug or issue in the \
given file. Respond with the COMPLETE corrected file content inside a single fenced code block \
and nothing else outside the fence. Do not omit or abbreviate unchanged parts of the file.";

/// Extracts the contents of the first fenced code block in `response`, if
/// any (used by `fix`, `refactor`, and `test` to recover the model's file
/// output from its surrounding prose).
pub fn extract_fence(response: &str) -> Option<String> {
    let start = response.find("```")?;
    let after_open = &response[start + 3..];
    // Skip an optional language tag on the opening fence line.
    let body_start = after_open.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after_open[body_start..];
    let end = body.find("```")?;
    Some(body[..end].to_string())
}

pub async fn run(
    ctx: &AppContext,
    file: PathBuf,
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
    // Captured before `context::load`'s own (send-time) secret-scan
    // confirmation, and used only to decide whether `--yes` may skip the
    // separate write-time confirmation below (SPEC §6: never for writes
    // with detected secrets).
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
        ChatMessage::text("user", format!("Fix this file:\n\n{block}")),
    ];

    let response = crate::commands::fetch_full_response(ctx, model, messages).await?;

    let Some(fixed) = extract_fence(&response) else {
        println!("{response}");
        return Err(ApiError::permanent(
            "could not extract a fenced code block from the model response; nothing applied"
                .to_string(),
        ));
    };

    fileops::show_diff(&original, &fixed, &source);

    if !apply {
        return Ok(());
    }

    let should_write = if ctx.cli.yes && findings.is_empty() {
        true
    } else {
        fileops::confirm(&format!("Apply this fix to {source}?"))
    };

    if !should_write {
        return Err(ApiError::Usage {
            message: "aborted: fix not applied".to_string(),
        });
    }

    fileops::write_atomic(&file, &fixed)?;
    println!("Applied fix to {source} (backup at {source}.insane-bak)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fence_with_language_tag() {
        let response = "Here is the fix:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(extract_fence(response).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn extracts_fence_without_language_tag() {
        let response = "```\nplain content\n```";
        assert_eq!(extract_fence(response).unwrap(), "plain content\n");
    }

    #[test]
    fn returns_none_without_a_fence() {
        assert!(extract_fence("no fences here").is_none());
    }

    #[test]
    fn returns_none_with_unterminated_fence() {
        assert!(extract_fence("```rust\nfn main() {}").is_none());
    }
}
