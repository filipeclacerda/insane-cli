//! `chat` command: interactive REPL over async stdin, streaming responses
//! chunk by chunk. Supports `/exit`, `/clear`, `/model <m>`, `/tools`,
//! `/cwd`. Tool calling (SPEC-AGENT) is enabled by default; `--no-tools`
//! restores the plain streaming chat.
//!
//! SPEC-UX Part B: in an interactive terminal (stdin *and* stdout both a
//! TTY), `insane`/`insane chat` opens the fullscreen TUI (`crate::tui`)
//! instead of this line-mode REPL. `--plain`, `config ui = "plain"`, or a
//! non-TTY stdin/stdout keep this line-mode path -- the one pipes/CI/tests
//! always get.

use std::io::IsTerminal;

use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent;
use crate::client::{ChatRequest, LlmClient};
use crate::error::ApiError;
use crate::output;
use crate::session::{self, Command as ReplCommand, Session, CONTINUE_MESSAGE};
use crate::session_store;
use crate::tools::{self, permission::Permissions};
use crate::ui::{AgentUi, PlainUi};
use crate::AppContext;

/// Whether this invocation should use the fullscreen TUI (SPEC-UX B1):
/// stdin *and* stdout are both a terminal, `--plain` wasn't passed, and
/// `config ui` isn't `"plain"`.
pub fn use_tui(ctx: &AppContext) -> bool {
    !ctx.cli.plain
        && ctx.cfg.ui != "plain"
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

/// Runs one agentic turn, logging any error and returning the turn's
/// `finish_reason` (SPEC-UX A3) so the caller can offer `/continue`.
async fn run_agent_turn(
    ctx: &AppContext,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    ui: &dyn AgentUi,
) -> Option<String> {
    match agent::run_turn(ctx, session, permissions, cwd, ctx.cfg.agent_max_rounds, ui).await {
        Ok(outcome) => outcome.finish_reason,
        Err(e) => {
            output::log_error(&e.to_string());
            None
        }
    }
}

pub async fn run(
    ctx: &AppContext,
    tools_enabled: bool,
    continue_last: bool,
) -> Result<(), ApiError> {
    if use_tui(ctx) {
        return crate::tui::run(ctx, tools_enabled, continue_last).await;
    }
    run_plain(ctx, tools_enabled, continue_last).await
}

async fn run_plain(
    ctx: &AppContext,
    tools_enabled: bool,
    continue_last: bool,
) -> Result<(), ApiError> {
    let model = ctx
        .cli
        .model
        .clone()
        .unwrap_or_else(|| ctx.cfg.model.clone());
    let mut session = Session::new(model.clone(), ctx.cfg.max_context_bytes);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let ui = PlainUi::new(ctx.out);
    let mut permissions = Permissions::with_ui(Box::new(PlainUi::new(ctx.out)));
    // Tracks whether the last turn ended on a non-`stop`/`tool_calls`
    // `finish_reason` (e.g. `length`), so `/continue` has something to do
    // (SPEC-UX A3).
    let mut last_finish_reason: Option<String> = None;

    if tools_enabled {
        session.push_system(agent::system_prompt(
            &cwd,
            &session.model,
            &ctx.cfg.ignore,
            &ctx.cfg.system_prompt_extra,
        ));
    }

    // Resume the most recently saved session for this provider, if asked.
    let resumed = if continue_last {
        if let Some(loaded) = session_store::load(&ctx.cfg.active_provider) {
            session.model = loaded.model.clone();
            // Re-push the system prompt first so the restored history sits
            // after it (the saved history excludes the system message).
            if tools_enabled {
                session.history.clear();
                session.push_system(agent::system_prompt(
                    &cwd,
                    &session.model,
                    &ctx.cfg.ignore,
                    &ctx.cfg.system_prompt_extra,
                ));
            } else {
                session.history.clear();
            }
            for m in loaded.messages {
                session.history.push(m);
            }
            session.trim();
            output::log_info(
                ctx.out,
                &format!(
                    "resumed session ({} messages, model {}) -- /exit to quit, /clear to reset",
                    session.history.len(),
                    session.model
                ),
            );
            true
        } else {
            output::log_info(
                ctx.out,
                "no saved session to resume for this provider; starting a fresh chat",
            );
            false
        }
    } else {
        false
    };
    let _ = resumed;

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    if tools_enabled {
        output::log_info(
            ctx.out,
            "insane-cli chat (tools enabled) -- /exit, /clear, /model <name>, /tools, /cwd, \
/continue",
        );
    } else {
        output::log_info(
            ctx.out,
            "insane-cli chat -- /exit to quit, /clear to reset, /model <name> to switch",
        );
    }

    loop {
        if !ctx.out.quiet {
            eprint!("> ");
            use std::io::Write as _;
            let _ = std::io::stderr().flush();
        }

        let line = match lines
            .next_line()
            .await
            .map_err(|e| ApiError::permanent(format!("failed to read stdin: {e}")))?
        {
            Some(l) => l,
            None => break, // EOF (e.g. piped input exhausted)
        };

        if line.trim().is_empty() {
            continue;
        }

        if let Some(cmd) = session::parse_command(&line) {
            match cmd {
                ReplCommand::Exit => break,
                ReplCommand::Clear => {
                    session.clear();
                    // Also drop the saved session so a later `--continue`
                    // doesn't resurrect a conversation the user wiped.
                    let _ = session_store::clear(&ctx.cfg.active_provider);
                    output::log_info(ctx.out, "history cleared");
                }
                ReplCommand::SetModel(m) => {
                    if m.is_empty() {
                        output::log_info(ctx.out, &format!("current model: {}", session.model));
                    } else {
                        session.model = m.clone();
                        if tools_enabled {
                            session.push_system(agent::system_prompt(
                                &cwd,
                                &session.model,
                                &ctx.cfg.ignore,
                                &ctx.cfg.system_prompt_extra,
                            ));
                        }
                        output::log_info(ctx.out, &format!("model set to {m}"));
                    }
                }
                ReplCommand::Models => match ctx.client.list_models().await {
                    Ok(models) => {
                        for model in models {
                            let marker = if model.id == session.model { "*" } else { " " };
                            output::log_info(ctx.out, &format!("{marker} {}", model.id));
                        }
                    }
                    Err(err) => output::log_error(&format!("could not list models: {err}")),
                },
                ReplCommand::Providers => {
                    for name in ctx.cfg.providers.keys() {
                        let marker = if name == &ctx.cfg.active_provider {
                            "*"
                        } else {
                            " "
                        };
                        output::log_info(ctx.out, &format!("{marker} {name}"));
                    }
                }
                ReplCommand::SetProvider(name) => {
                    output::log_info(
                        ctx.out,
                        &format!(
                            "restart with `--provider {}` to switch providers in plain mode",
                            if name.is_empty() { "<name>" } else { &name }
                        ),
                    );
                }
                ReplCommand::SetMode(_) => {
                    output::log_info(ctx.out, "/mode is available in the fullscreen TUI");
                }
                ReplCommand::Cwd => {
                    output::log_info(ctx.out, &cwd.display().to_string());
                }
                ReplCommand::Help => {
                    output::log_info(ctx.out, session::HELP_COMMANDS);
                }
                ReplCommand::Continue => {
                    if !tools_enabled {
                        output::log_info(ctx.out, "/continue only applies to tool-calling turns");
                    } else if last_finish_reason.as_deref() == Some("stop")
                        || last_finish_reason.as_deref() == Some("tool_calls")
                        || last_finish_reason.is_none()
                    {
                        output::log_info(
                            ctx.out,
                            "nothing to continue -- the last turn finished normally",
                        );
                    } else {
                        session.push_user(CONTINUE_MESSAGE.to_string());
                        last_finish_reason =
                            run_agent_turn(ctx, &mut session, &mut permissions, &cwd, &ui).await;
                    }
                }
                ReplCommand::Resume => {
                    if let Some(loaded) = session_store::load(&ctx.cfg.active_provider) {
                        session.model = loaded.model.clone();
                        if tools_enabled {
                            session.history.clear();
                            session.push_system(agent::system_prompt(
                                &cwd,
                                &session.model,
                                &ctx.cfg.ignore,
                                &ctx.cfg.system_prompt_extra,
                            ));
                        } else {
                            session.history.clear();
                        }
                        for m in loaded.messages {
                            session.history.push(m);
                        }
                        session.trim();
                        last_finish_reason = None;
                        output::log_info(
                            ctx.out,
                            &format!(
                                "resumed session ({} messages, model {})",
                                session.history.len(),
                                session.model
                            ),
                        );
                    } else {
                        output::log_info(ctx.out, "no saved session to resume for this provider");
                    }
                }
                ReplCommand::Tools => {
                    if !tools_enabled {
                        output::log_info(ctx.out, "tools are disabled (--no-tools)");
                    } else {
                        let always = permissions.always_allowed_tools();
                        for def in tools::all_tool_defs() {
                            let status = if always.contains(&def.function.name.as_str()) {
                                "always-allowed"
                            } else {
                                "asks each time"
                            };
                            output::log_info(
                                ctx.out,
                                &format!("  {} ({status})", def.function.name),
                            );
                        }
                        output::log_info(
                            ctx.out,
                            &format!(
                                "  run_command: {} exact command(s) always-allowed",
                                permissions.always_allowed_command_count()
                            ),
                        );
                    }
                }
            }
            continue;
        }

        session.push_user(line);
        tracing::debug!(
            "session history ~{} tokens after trim",
            session.approx_tokens()
        );

        if tools_enabled {
            last_finish_reason =
                run_agent_turn(ctx, &mut session, &mut permissions, &cwd, &ui).await;
            continue;
        }

        let req = ChatRequest {
            model: session.model.clone(),
            messages: session.history.clone(),
            temperature: Some(ctx.cfg.temperature),
            top_p: None,
            max_tokens: Some(ctx.cfg.max_tokens),
            stream: true,
            tools: None,
            tool_choice: None,
        };

        let mut stream = ctx.client.chat_stream(req).await?;
        let mut full = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    output::print_stream_chunk(ctx.out, &chunk.delta);
                    full.push_str(&chunk.delta);
                }
                Err(e) => {
                    output::log_error(&e.to_string());
                    break;
                }
            }
        }
        println!();
        if !full.is_empty() {
            session.push_assistant(full);
        }
    }

    // Persist the session so `insane chat --continue` (or `/resume`) can
    // pick it back up. Best-effort: failures are logged inside `save`.
    session_store::save(&ctx.cfg.active_provider, &session.model, &session.history);
    Ok(())
}
