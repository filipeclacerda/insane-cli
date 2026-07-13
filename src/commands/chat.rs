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

use std::{io::IsTerminal, time::Instant};

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent;
use crate::client::{ChatMessage, ChatRequest, LlmClient, Usage};
use crate::error::ApiError;
use crate::output;
use crate::session::{self, Command as ReplCommand, Session, CONTINUE_MESSAGE};
use crate::session_store;
use crate::tools::{self, permission::Permissions};
use crate::ui::{AgentUi, PlainInput, PlainUi};
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

pub(crate) fn initial_model(ctx: &AppContext) -> String {
    ctx.cli
        .model
        .clone()
        .or_else(|| std::env::var("INSANE_MODEL").ok())
        .or_else(|| session_store::last_model(&ctx.cfg.active_provider))
        .unwrap_or_else(|| ctx.cfg.model.clone())
}

pub(crate) fn resume_choice(provider: &str, choice: Option<usize>) -> Option<usize> {
    let sessions = session_store::list(provider);
    match choice {
        Some(n @ 1..=3) if n <= sessions.len() => Some(n - 1),
        Some(_) => None,
        None if sessions.len() == 1 => Some(0),
        None => None,
    }
}

pub(crate) fn format_session_options(provider: &str) -> Vec<String> {
    let sessions = session_store::list(provider);
    if sessions.is_empty() {
        return vec!["no saved session to resume for this provider".to_string()];
    }
    let mut lines = vec!["saved sessions:".to_string()];
    for summary in sessions {
        lines.push(format!(
            "  {}. {} messages, model {} -- {}",
            summary.index + 1,
            summary.messages,
            summary.model,
            summary.preview
        ));
    }
    lines.push("use /resume 1, /resume 2, or /resume 3".to_string());
    lines
}

pub(crate) fn restore_loaded_session(
    session: &mut Session,
    loaded: session_store::LoadedSession,
    tools_enabled: bool,
    cwd: &std::path::Path,
    ctx: &AppContext,
) {
    session.model = loaded.model.clone();
    if tools_enabled {
        session.history.clear();
        session.push_system(agent::system_prompt(
            cwd,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStats {
    pub original_messages: usize,
    pub summary_chars: usize,
}

const COMPACT_MIN_MESSAGES: usize = 4;

fn non_system_messages(session: &Session) -> Vec<ChatMessage> {
    session
        .history
        .iter()
        .filter(|m| m.role != "system")
        .cloned()
        .collect()
}

fn compact_needed(session: &Session, compact_max_tokens: u32) -> bool {
    let messages = non_system_messages(session);
    if messages.len() > COMPACT_MIN_MESSAGES {
        return true;
    }
    let approx_bytes: usize = messages
        .iter()
        .map(|m| {
            m.content.as_deref().map(str::len).unwrap_or(0)
                + m.tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| c.function.name.len() + c.function.arguments.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum();
    approx_bytes > (compact_max_tokens as usize).saturating_mul(4)
}

fn truncate_for_compact(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n...[truncated for compaction]");
    out
}

fn format_history_for_compaction(session: &Session) -> String {
    const PER_MESSAGE_CHARS: usize = 4_000;
    let mut out = String::new();
    for message in session.history.iter().filter(|m| m.role != "system") {
        out.push_str("\n--- ");
        out.push_str(&message.role);
        out.push_str(" ---\n");
        if let Some(content) = &message.content {
            out.push_str(&truncate_for_compact(content, PER_MESSAGE_CHARS));
            out.push('\n');
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str("tool_call: ");
                out.push_str(&call.function.name);
                out.push(' ');
                out.push_str(&crate::tools::summarize_args(&call.function.arguments));
                out.push('\n');
            }
        }
    }
    out
}

fn compact_prompt(session: &Session) -> Vec<ChatMessage> {
    let transcript = format_history_for_compaction(session);
    vec![
        ChatMessage::text(
            "system",
            "You compact coding-agent conversations. Produce a concise, useful state summary for \
future turns. Preserve the user's goal, important decisions, files read or changed, commands and \
results that matter, errors, constraints, and next steps. Do not include filler.",
        ),
        ChatMessage::text(
            "user",
            format!(
                "Compact this conversation into a short working-context summary. Keep it actionable \
for a coding agent and omit irrelevant chatter.\n\nConversation:\n{transcript}"
            ),
        ),
    ]
}

pub async fn compact_session(
    ctx: &AppContext,
    session: &mut Session,
) -> Result<Option<CompactStats>, ApiError> {
    compact_session_with_cancel(ctx, session, &CancellationToken::new()).await
}

pub async fn compact_session_with_cancel(
    ctx: &AppContext,
    session: &mut Session,
    cancellation: &CancellationToken,
) -> Result<Option<CompactStats>, ApiError> {
    if !compact_needed(session, ctx.cfg.agent_compact_max_tokens) {
        return Ok(None);
    }

    let original_messages = non_system_messages(session).len();
    let req = ChatRequest {
        model: session.model.clone(),
        messages: compact_prompt(session),
        temperature: Some(ctx.cfg.agent_temperature),
        top_p: None,
        max_tokens: Some(ctx.cfg.agent_compact_max_tokens),
        stream: false,
        stream_options: None,
        tools: None,
        tool_choice: None,
    };
    let response = tokio::select! {
        response = ctx.client.chat(req) => response?,
        _ = cancellation.cancelled() => return Err(ApiError::Cancelled),
    };
    let summary = response.content().trim();
    if summary.is_empty() {
        return Err(ApiError::permanent(
            "compact response was empty; conversation was left unchanged",
        ));
    }

    let system = session
        .history
        .first()
        .filter(|m| m.role == "system")
        .cloned();
    session.history.clear();
    if let Some(system) = system {
        session.history.push(system);
    }
    let compacted = format!(
        "[compacted conversation summary]\n{}",
        truncate_for_compact(summary, (ctx.cfg.agent_compact_max_tokens as usize) * 8)
    );
    let summary_chars = compacted.len();
    session.history.push(ChatMessage::text("user", compacted));
    session.trim();

    Ok(Some(CompactStats {
        original_messages,
        summary_chars,
    }))
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
    let cancellation = CancellationToken::new();
    let turn = agent::run_turn_with_cancel(
        ctx,
        session,
        permissions,
        cwd,
        ctx.cfg.agent_max_rounds,
        ui,
        cancellation.clone(),
    );
    tokio::pin!(turn);
    let outcome = tokio::select! {
        outcome = &mut turn => outcome,
        _ = tokio::signal::ctrl_c() => {
            cancellation.cancel();
            // Exactly one signal listener is installed for this plain turn.
            // Await the same future so no reader/task is orphaned.
            turn.await
        }
    };
    match outcome {
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
    // Show the ASCII banner at the top of the terminal for the interactive
    // chat entry point. Printed to stderr (so stdout stays clean for model
    // output) and before TUI raw mode is entered (the inline viewport would
    // otherwise scroll it away). Suppressed by `--quiet`/`--json`.
    output::print_banner(ctx.out);
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
    let model = initial_model(ctx);
    let mut session = Session::new(model.clone(), ctx.cfg.max_context_bytes);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let input = PlainInput::stdin();
    let ui = PlainUi::with_input(ctx.out, input.clone());
    let mut permissions =
        Permissions::with_ui(Box::new(PlainUi::with_input(ctx.out, input.clone())));
    // Tracks whether the last turn ended on a non-`stop`/`tool_calls`
    // `finish_reason` (e.g. `length`), so `/continue` has something to do
    // (SPEC-UX A3).
    let mut last_finish_reason: Option<String> = None;
    let mut conversation_tokens_total: u64 = 0;

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
            restore_loaded_session(&mut session, loaded, tools_enabled, &cwd, ctx);
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

    if tools_enabled {
        output::log_info(
            ctx.out,
            "insane-cli chat (tools enabled) -- /exit, /clear, /model <name>, /tools, /cwd, \
/continue, /compact",
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

        let line = match tokio::select! {
            line = input.next_line() => line.map_err(|e| ApiError::permanent(format!("failed to read stdin: {e}")))?,
            _ = tokio::signal::ctrl_c() => {
                return Err(ApiError::Cancelled);
            }
        } {
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
                    ui.reset_token_total();
                    conversation_tokens_total = 0;
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
                        session_store::save_model(&ctx.cfg.active_provider, &session.model);
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
                ReplCommand::Compact => {
                    output::log_info(ctx.out, "compacting conversation...");
                    match compact_session(ctx, &mut session).await {
                        Ok(Some(stats)) => {
                            ui.reset_token_total();
                            conversation_tokens_total = 0;
                            last_finish_reason = None;
                            output::log_info(
                                ctx.out,
                                &format!(
                                    "compacted {} messages into ~{} chars",
                                    stats.original_messages, stats.summary_chars
                                ),
                            );
                            session_store::save(
                                &ctx.cfg.active_provider,
                                &session.model,
                                &session.history,
                            );
                        }
                        Ok(None) => {
                            output::log_info(ctx.out, "nothing to compact");
                        }
                        Err(err) => {
                            output::log_error(&format!("could not compact conversation: {err}"));
                        }
                    }
                }
                ReplCommand::Resume(choice) => {
                    if let Some(index) = resume_choice(&ctx.cfg.active_provider, choice) {
                        if let Some(loaded) =
                            session_store::load_at(&ctx.cfg.active_provider, index)
                        {
                            restore_loaded_session(&mut session, loaded, tools_enabled, &cwd, ctx);
                            ui.reset_token_total();
                            conversation_tokens_total = 0;
                            last_finish_reason = None;
                            output::log_info(
                                ctx.out,
                                &format!(
                                    "resumed session ({} messages, model {})",
                                    session.history.len(),
                                    session.model
                                ),
                            );
                        }
                    } else {
                        last_finish_reason = None;
                        for line in format_session_options(&ctx.cfg.active_provider) {
                            output::log_info(ctx.out, &line);
                        }
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
                ReplCommand::Copy => {
                    use clipboard::{ClipboardContext, ClipboardProvider};
                    if let Some(last_assistant) =
                        session.history.iter().rev().find(|m| m.role == "assistant")
                    {
                        if let Some(content) = &last_assistant.content {
                            match ClipboardProvider::new()
                                .and_then(|mut ctx_clip: ClipboardContext| ctx_clip.set_contents(content.clone()))
                            {
                                Ok(()) => output::log_info(ctx.out, "Última mensagem do assistente copiada para a área de transferência."),
                                Err(err) => output::log_info(ctx.out, &format!("Não foi possível copiar para a área de transferência: {err}")),
                            }
                        } else {
                            output::log_info(
                                ctx.out,
                                "Última mensagem do assistente não tem conteúdo.",
                            );
                        }
                    } else {
                        output::log_info(
                            ctx.out,
                            "Nenhuma mensagem do assistente encontrada para copiar.",
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

        let turn_start = Instant::now();
        let req = ChatRequest {
            model: session.model.clone(),
            messages: session.history.clone(),
            temperature: Some(ctx.cfg.temperature),
            top_p: None,
            max_tokens: Some(ctx.cfg.max_tokens),
            stream: true,
            stream_options: None,
            tools: None,
            tool_choice: None,
        };

        let mut stream = ctx.client.chat_stream(req).await?;
        let mut full = String::new();
        let mut turn_usage: Option<Usage> = None;
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = tokio::signal::ctrl_c() => {
                    output::log_info(ctx.out, "^C received, cancelling this response");
                    break;
                }
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(chunk) => {
                    output::print_stream_chunk(ctx.out, &chunk.delta);
                    full.push_str(&chunk.delta);
                    if let Some(usage) = chunk.usage {
                        turn_usage = Some(usage);
                    }
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
        if let Some(usage) = &turn_usage {
            conversation_tokens_total =
                conversation_tokens_total.saturating_add(usage.total_tokens as u64);
            if !ctx.out.quiet && std::io::stderr().is_terminal() {
                eprintln!(
                    "{}",
                    crate::agent::turn_summary_line_with_total(
                        1,
                        0,
                        Some(usage),
                        turn_start.elapsed(),
                        conversation_tokens_total
                    )
                );
            }
        }
    }

    // Persist the session so `insane chat --continue` (or `/resume`) can
    // pick it back up. Best-effort: failures are logged inside `save`.
    session_store::save(&ctx.cfg.active_provider, &session.model, &session.history);
    Ok(())
}
