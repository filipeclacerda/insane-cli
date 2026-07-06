//! `review` command: reviews whole files, or a diff (`git diff` in the cwd,
//! or stdin when `-` is passed alongside `--diff`).

use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command as ProcCommand;

use crate::client::ChatMessage;
use crate::context;
use crate::error::ApiError;
use crate::AppContext;

const SYSTEM_PROMPT: &str = "You are a meticulous senior code reviewer. Point out correctness \
bugs, security issues, and unclear or unmaintainable code. Be concise and specific, citing line \
numbers where possible. Do not rewrite the whole file.";

fn read_stdin() -> Result<String, ApiError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| ApiError::permanent(format!("failed to read stdin: {e}")))?;
    Ok(buf)
}

fn git_diff() -> Result<String, ApiError> {
    let output = ProcCommand::new("git")
        .args(["diff"])
        .output()
        .map_err(|e| ApiError::permanent(format!("failed to run `git diff`: {e}")))?;
    if !output.status.success() {
        return Err(ApiError::permanent(format!(
            "`git diff` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn run(ctx: &AppContext, files: &[PathBuf], diff: bool) -> Result<(), ApiError> {
    let body = if diff {
        if files.iter().any(|f| f.as_os_str() == "-") {
            read_stdin()?
        } else {
            git_diff()?
        }
    } else {
        if files.is_empty() {
            return Err(ApiError::Usage {
                message: "review requires at least one file, or --diff".to_string(),
            });
        }
        let mut content = String::new();
        for f in files {
            let source = f.to_string_lossy().into_owned();
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
        content
    };

    let model = ctx
        .cli
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.model.clone());
    let user_content = if diff {
        format!("Review this diff:\n\n```diff\n{body}\n```\n")
    } else {
        format!("Review this code:\n\n{body}")
    };
    let messages = vec![
        ChatMessage::text("system", SYSTEM_PROMPT),
        ChatMessage::text("user", user_content),
    ];

    crate::commands::run_chat(ctx, model, messages, true).await
}
