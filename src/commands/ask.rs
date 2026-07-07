//! `ask` command: a single question/answer round-trip, with optional file
//! context and streaming.

use std::io::Read as _;
use std::path::PathBuf;

use std::time::Instant;

use crate::agent;
use crate::client::ChatMessage;
use crate::context;
use crate::error::ApiError;
use crate::output::{self, JsonResult};
use crate::session::Session;
use crate::tools::permission::Permissions;
use crate::ui::PlainUi;
use crate::AppContext;

/// Reads the prompt from an argument, `-` (stdin), or errors if missing.
fn read_prompt(prompt: Option<String>) -> Result<String, ApiError> {
    match prompt.as_deref() {
        Some("-") => read_stdin(),
        Some(p) => Ok(p.to_string()),
        None => Err(ApiError::Usage {
            message: "missing prompt (pass an argument or `-` for stdin)".to_string(),
        }),
    }
}

fn read_stdin() -> Result<String, ApiError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| ApiError::permanent(format!("failed to read stdin: {e}")))?;
    Ok(buf)
}

pub async fn run(
    ctx: &AppContext,
    prompt: Option<String>,
    files: &[PathBuf],
    cache: bool,
    tools: bool,
) -> Result<(), ApiError> {
    let prompt_text = read_prompt(prompt)?;

    let mut content = String::new();
    for f in files {
        let source = f.to_string_lossy().into_owned();
        // Selective read + denylist/.gitignore filtering
        // confirmation before any file content is included (SPEC §7).
        let loaded = context::load(
            &source,
            ctx.cfg.max_context_bytes,
            None,
            &ctx.cfg.ignore,
            ctx.out.quiet,
        )?;
        content.push_str(&context::format_block(
            &loaded.display_path,
            &loaded.content,
        ));
    }
    content.push_str(&prompt_text);

    let model = ctx
        .cli
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.model.clone());
    if tools {
        // The agentic loop is a single non-interactive turn here (SPEC-AGENT
        // §5): with non-TTY stdin (the common case for `ask`), every
        // write_file/edit_file/run_command confirmation is automatically
        // refused -- read-only exploration still works.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut session = Session::new(model.clone(), ctx.cfg.max_context_bytes);
        session.push_system(agent::system_prompt(
            &cwd,
            &model,
            &ctx.cfg.ignore,
            &ctx.cfg.system_prompt_extra,
        ));
        session.push_user(content);
        let ui = PlainUi::new(ctx.out);
        let mut permissions = Permissions::with_ui(Box::new(PlainUi::new(ctx.out)));
        let start = Instant::now();
        let outcome = agent::run_turn(
            ctx,
            &mut session,
            &mut permissions,
            &cwd,
            ctx.cfg.agent_max_rounds,
            &ui,
        )
        .await?;
        if ctx.out.json {
            let metrics = ctx.limiter.metrics().await;
            let result = JsonResult {
                response: &outcome.last_text,
                model: &model,
                usage: &outcome.usage.unwrap_or_default(),
                timing_ms: start.elapsed().as_millis(),
                rate_limiter: metrics,
                finish_reason: outcome.finish_reason.as_deref(),
            };
            output::print_result(ctx.out, &result);
        }
        return Ok(());
    }

    let messages = vec![ChatMessage::text("user", content)];

    crate::commands::run_chat(ctx, model, messages, cache).await
}
