//! Agentic loop (SPEC-AGENT §4, SPEC-UX Part A): streams the model's
//! response, accumulates tool-call deltas by index, executes tool calls
//! sequentially with permission prompts, appends `role: "tool"` results, and
//! repeats until the model stops calling tools. A
//! user turn's Ctrl+C aborts just that turn (back to the chat prompt), never
//! the process.
//!
//! SPEC-UX Part A hardens this loop against real failures observed with
//! non-conforming models (e.g. `z-ai/glm-5.2`): a richer system prompt
//! (`system_prompt`), visible `finish_reason` handling, a lenient fallback
//! that recovers a tool call emitted as plain text (`lenient`), and
//! stderr-only progress feedback so a long turn is never silent.

pub mod lenient;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use crate::client::{ChatRequest, LlmClient, ToolCall, ToolCallFunction, Usage};
use crate::error::ApiError;
use crate::session::Session;
use crate::tools::{self, permission::Permissions, ToolExecCtx};
use crate::ui::AgentUi;
use crate::AppContext;

/// Cap on the number of project-snapshot entries embedded in the system
/// prompt (SPEC-UX A1).
const SNAPSHOT_CAP: usize = 150;

/// What a completed turn produced -- enough for callers to show a
/// `finish_reason` warning, offer `/continue`, or fill in `--json` output
/// (SPEC-UX A3).
#[derive(Debug, Default)]
pub struct TurnOutcome {
    /// The last round's `finish_reason` (`None` if the turn was cancelled
    /// with Ctrl+C before any round completed).
    pub finish_reason: Option<String>,
    /// The final assistant text produced this turn (empty if the turn ended
    /// on a tool-calls round, or was cancelled).
    pub last_text: String,
    /// How many model rounds the turn took.
    pub rounds: u32,
    /// How many tool calls were executed this turn.
    pub tools_executed: u32,
    /// Usage from the last round that reported it, if any.
    pub usage: Option<Usage>,
}

/// Converts days since the Unix epoch to a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days` algorithm). Used to embed today's date
/// in the system prompt without pulling in a date/time dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Today's date in `YYYY-MM-DD`, computed from the system clock without a
/// date/time dependency. Falls back to omitting the date on clock errors
/// (e.g. a system clock set before the Unix epoch).
fn today_utc_date() -> Option<String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// The shell used by `run_command` on this OS (SPEC-UX A1), matching
/// `tools::exec::run_command`.
fn shell_name() -> &'static str {
    tools::exec::shell_display_name()
}

/// Builds the agent's system prompt (SPEC-UX A1): OS/shell, cwd, date,
/// model, a capped project snapshot, and explicit behavioral rules aimed at
/// the "announced an action but never called the tool" failure mode. `extra`
/// is `config [agent] system_prompt_extra`, appended verbatim at the end.
pub fn system_prompt(cwd: &Path, model: &str, extra_ignore: &[String], extra: &str) -> String {
    let os = std::env::consts::OS;
    let shell = shell_name();
    let shell_rules = if cfg!(windows) {
        "Shell syntax: write PowerShell commands only. Use $env:NAME for environment variables, \
PowerShell cmdlets where appropriate, and `;` between commands for Windows PowerShell compatibility. \
Do not emit Bash-only syntax such as `export`, `source`, or `$(...)`."
    } else {
        "Shell syntax: write Bash-compatible commands only. Do not emit PowerShell cmdlets or \
PowerShell-only syntax such as `$env:NAME`."
    };
    let date_line = today_utc_date()
        .map(|d| format!("Date: {d}\n"))
        .unwrap_or_default();
    let snapshot = tools::fs::snapshot_listing(cwd, extra_ignore, SNAPSHOT_CAP);

    let mut prompt = format!(
        "You are insane-cli, an autonomous coding agent running in a terminal.\n\
OS: {os} ({shell}) -- run_command runs via {shell}.\n\
Working directory: {cwd} -- every tool call is sandboxed to this directory.\n\
{date_line}\
Model: {model}\n\
{shell_rules}\n\
\n\
Project snapshot (cwd, capped at {SNAPSHOT_CAP} entries):\n{snapshot}\n\
\n\
Available tools: list_files, read_file, search_files, write_file, edit_file, run_command.\n\
\n\
Rules:\n\
- You are an agent. Keep working until the user's request is fully resolved. NEVER end your \
turn right after announcing an action -- announcing without calling the corresponding tool is a \
critical failure. If you say you will read/create/edit/run something, CALL THE TOOL in this same \
turn.\n\
- When asked to create or modify a file, actually create/modify it with write_file/edit_file -- \
do not print the would-be content as your answer unless the user asked to see it first.\n\
- Prefer edit_file for small changes; write_file for new files. Verify your work with \
run_command when a test/build command is available and relevant.\n\
- If a tool returns an error, adapt and try a different approach instead of giving up.\n\
- During tool-calling rounds, avoid narrating repeated exploration/status like \"vou ler mais \
arquivos\". Call the tools directly; save explanation for meaningful findings or the final answer.\n\
- Respond in the user's language; keep code and file contents in their original language.\n\
\n\
Prefer edit_file for small, targeted changes to existing files (old_string must be unique unless \
replace_all is set); use write_file for new files or full rewrites. Read a file with read_file \
before editing it. Writes and shell commands require the user's confirmation, which the \
surrounding UI handles -- just call the tool. Keep explanations brief and let tool calls do the \
work.",
        cwd = cwd.display(),
    );

    let extra = extra.trim();
    if !extra.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(extra);
    }
    prompt
}

const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Builds the rate-limiter wait notice (SPEC-UX A5), or `None` if the next
/// request won't have to wait. A pure function so both `PlainUi` and the
/// TUI can reuse it without duplicating the wording.
async fn rate_limit_wait_message(ctx: &AppContext) -> Option<String> {
    let metrics = ctx.limiter.metrics().await;
    if metrics.next_below_75pct_in_ms > 0 {
        let secs = metrics.next_below_75pct_in_ms.div_ceil(1000);
        let capacity = metrics
            .capacity
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unlimited".to_string());
        Some(format!(
            "rate limit: cooling down {secs}s until usage drops below 75% ({}/{capacity} used)",
            metrics.used
        ))
    } else {
        None
    }
}

pub(crate) fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

fn format_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        let rounded = (d.as_secs_f64() * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            format!("{}s", rounded as u64)
        } else {
            format!("{rounded:.1}s")
        }
    }
}

/// The short label after the tool name in a summary line: the file path for
/// filesystem tools, the (truncated) command for `run_command`, or empty.
fn tool_summary_label(name: &str, arguments: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let field = if name == "run_command" {
        "command"
    } else {
        "path"
    };
    match parsed.get(field).and_then(|v| v.as_str()) {
        Some(s) if name == "run_command" => format!("\"{}\"", tools::summarize_args(s)),
        Some(s) => s.to_string(),
        None => tools::summarize_args(arguments),
    }
}

/// Builds the post-tool-execution summary line (SPEC-UX A5), e.g.
/// `✓ read_file agent.rs (14.2 KB, 3ms)` or `✗ edit_file f.rs (user denied)`.
/// A pure function so it's directly unit-testable without capturing stderr.
pub(crate) fn tool_summary_line(
    name: &str,
    arguments: &str,
    result: &str,
    elapsed: Duration,
) -> String {
    let value: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let label = tool_summary_label(name, arguments);
    let marker = if ok { '\u{2713}' } else { '\u{2717}' };

    let detail = if ok {
        let output = value.get("output").and_then(|v| v.as_str()).unwrap_or("");
        if name == "run_command" {
            let exit = output
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("exit_code: "))
                .unwrap_or("?");
            format!("(exit {exit}, {})", format_duration(elapsed))
        } else {
            format!(
                "({}, {})",
                format_bytes(output.len()),
                format_duration(elapsed)
            )
        }
    } else {
        let error = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("error");
        if error.contains("denied") {
            "(user denied)".to_string()
        } else {
            format!("({})", tools::summarize_args(error))
        }
    };

    format!("{marker} {name} {label} {detail}")
}

/// Builds the end-of-turn metrics line (SPEC-UX A5), e.g.
/// `-- 2 tools | 1.9k tokens | 14s`. Tokens are only shown when
/// the provider reported `usage` on some round's stream (most NIM/OpenAI-
/// compatible streaming responses don't, by default); otherwise that segment
/// is omitted rather than approximated. A pure function so it's directly
/// unit-testable without capturing stderr.
pub(crate) fn turn_summary_line(
    _rounds: u32,
    tools_executed: u32,
    usage: Option<&Usage>,
    elapsed: Duration,
) -> String {
    let tool_word = if tools_executed == 1 { "tool" } else { "tools" };
    let mut parts = vec![format!("{tools_executed} {tool_word}")];
    if let Some(u) = usage {
        if u.total_tokens > 0 {
            parts.push(format!("{} tokens", format_token_count(u.total_tokens)));
        }
    }
    parts.push(format_duration(elapsed));
    format!("-- {}", parts.join(" | "))
}

pub(crate) fn format_token_count(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn normalized_status_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .trim_matches(&['.', '!', ':', ';'][..])
        .to_ascii_lowercase()
}

fn low_value_tool_preamble(text: &str) -> bool {
    let normalized = normalized_status_text(text);
    if normalized.is_empty() || normalized.chars().count() > 320 {
        return false;
    }

    let first_person_setup = [
        "vou ",
        "vou explorar",
        "vou ler",
        "vou verificar",
        "vou checar",
        "vou inspecionar",
        "vou buscar",
        "i will ",
        "i'll ",
        "let me ",
        "i’m going to ",
        "i'm going to ",
    ];
    let toolish_context = [
        "arquivo",
        "arquivos",
        "projeto",
        "código",
        "codigo",
        "explorar",
        "entender",
        "estado atual",
        "identificar",
        "read",
        "inspect",
        "check",
        "look at",
        "files",
        "project",
    ];

    first_person_setup
        .iter()
        .any(|needle| normalized.contains(needle))
        && toolish_context
            .iter()
            .any(|needle| normalized.contains(needle))
}

/// Runs the agentic loop for a single user turn (SPEC-AGENT §4). Returns
/// `Ok(TurnOutcome)` when the turn ends normally (model stopped calling
/// tools, or the user cancelled with Ctrl+C); returns `Err` for API errors.
pub async fn run_turn(
    ctx: &AppContext,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &Path,
    _max_rounds: u32,
    ui: &dyn AgentUi,
) -> Result<TurnOutcome, ApiError> {
    let tool_defs = tools::all_tool_defs();
    let known_tools: Vec<&str> = tool_defs.iter().map(|d| d.function.name.as_str()).collect();
    let turn_start = Instant::now();
    let mut tools_executed: u32 = 0;
    let mut last_usage: Option<Usage> = None;
    let mut text_call_counter: usize = 0;

    let mut rounds = 0u32;
    loop {
        if let Some(msg) = rate_limit_wait_message(ctx).await {
            ui.warn(&msg);
        }
        ctx.limiter.wait_until_below_fraction(3, 4).await;

        let req = ChatRequest {
            model: session.model.clone(),
            messages: session.history.clone(),
            temperature: Some(ctx.cfg.agent_temperature),
            top_p: None,
            max_tokens: Some(ctx.cfg.max_tokens),
            stream: true,
            tools: Some(tool_defs.clone()),
            tool_choice: Some("auto".to_string()),
        };

        let round_outcome = run_round_with_spinner(ctx, req, ui).await;

        let Some(round_result) = round_outcome else {
            ui.end_of_stream();
            ui.warn("^C received, cancelling this turn");
            return Ok(TurnOutcome {
                finish_reason: None,
                last_text: String::new(),
                rounds,
                tools_executed,
                usage: last_usage,
            });
        };
        rounds += 1;

        let (mut text, mut tool_calls, finish_reason, usage) = round_result?;
        if usage.is_some() {
            last_usage = usage;
        }
        ui.end_of_stream();

        if let Some(fr) = &finish_reason {
            if fr != "stop" && fr != "tool_calls" {
                ui.warn(&format!(
                    "warning: response ended early (finish_reason={fr}) -- type /continue to \
resume"
                ));
            }
        }

        if tool_calls.is_empty()
            && ctx.cfg.lenient_tool_calls
            && finish_reason.as_deref() == Some("stop")
        {
            if let Some((prefix, call)) = lenient::detect(&text, &known_tools) {
                text_call_counter += 1;
                let id = format!("text_call_{text_call_counter}");
                let summary = tools::summarize_args(&call.arguments);
                ui.warn(&format!(
                    "\u{2192} (recovered from text) {}({summary})",
                    call.name
                ));
                tool_calls = vec![ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: call.name,
                        arguments: call.arguments,
                    },
                }];
                text = prefix;
            }
        }

        if tool_calls.is_empty() {
            if !text.is_empty() || finish_reason.is_some() {
                session.push_assistant(text.clone());
            }
            ui.turn_summary(
                rounds,
                tools_executed,
                last_usage.as_ref(),
                turn_start.elapsed(),
            );
            return Ok(TurnOutcome {
                finish_reason,
                last_text: text,
                rounds,
                tools_executed,
                usage: last_usage,
            });
        }

        let assistant_content = if low_value_tool_preamble(&text) {
            ui.discard_last_assistant_message();
            None
        } else if text.is_empty() {
            None
        } else {
            Some(text)
        };
        session.push_assistant_tool_calls(assistant_content, tool_calls.clone());

        for call in &tool_calls {
            if !call.id.starts_with("text_call_") {
                ui.tool_trace(&call.function.name, &call.function.arguments);
            }
            let mut ectx = ToolExecCtx {
                cwd: cwd.to_path_buf(),
                max_context_bytes: ctx.cfg.max_context_bytes,
                extra_ignore: &ctx.cfg.ignore,
                permissions,
            };
            let t0 = Instant::now();
            let result =
                tools::execute(&call.function.name, &call.function.arguments, &mut ectx).await;
            let elapsed = t0.elapsed();
            ui.tool_summary(
                &call.function.name,
                &call.function.arguments,
                &result,
                elapsed,
            );
            tools_executed += 1;
            session.push_tool_result(call.id.clone(), result);
        }
        // Loop continues: next round lets the model see the tool results.
    }
}

/// Runs one round while showing a "model thinking..." status via `ui`
/// (SPEC-UX A5/B3): a `tokio::time::interval` redraws the spinner frame
/// every 90ms until the round's `Notify` fires (first delta/tool-call-delta
/// arrived) or the round completes, whichever is first. Everything happens
/// in this one task -- no spawned spinner task, so `ui` never needs a
/// `'static` bound. Returns `None` if Ctrl+C arrived first (SPEC-AGENT §4:
/// aborts just this round, never the process).
async fn run_round_with_spinner(
    ctx: &AppContext,
    req: ChatRequest,
    ui: &dyn AgentUi,
) -> Option<Result<(String, Vec<ToolCall>, Option<String>, Option<Usage>), ApiError>> {
    let notify = Arc::new(tokio::sync::Notify::new());
    let round_call = stream_round(ctx, req, notify.clone(), ui);
    tokio::pin!(round_call);
    let notified = notify.notified();
    tokio::pin!(notified);

    let mut interval = tokio::time::interval(Duration::from_millis(90));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut spinner_on = true;
    let mut frame_i = 0usize;

    let outcome = loop {
        tokio::select! {
            res = &mut round_call => break Some(res),
            _ = tokio::signal::ctrl_c() => break None,
            _ = &mut notified, if spinner_on => {
                spinner_on = false;
                ui.clear_status();
            }
            _ = interval.tick(), if spinner_on => {
                let frame = SPINNER_FRAMES[frame_i % SPINNER_FRAMES.len()];
                frame_i += 1;
                ui.spinner_tick(&format!("{frame} model thinking..."));
            }
        }
    };
    if spinner_on {
        ui.clear_status();
    }
    outcome
}

/// Streams one round of the completion: sends text deltas incrementally
/// through `ui.stream_text` (SPEC-UX B0), accumulates tool-call deltas by
/// `index` (SPEC-AGENT §1/§6), and returns the accumulated text, finalized
/// tool calls, finish reason, and usage (if the provider reported one).
/// `notify` is woken the moment the first non-empty delta of any kind
/// arrives (SPEC-UX A5: clears the "model thinking..." status as soon as
/// something is happening).
async fn stream_round(
    ctx: &AppContext,
    req: ChatRequest,
    notify: Arc<tokio::sync::Notify>,
    ui: &dyn AgentUi,
) -> Result<(String, Vec<ToolCall>, Option<String>, Option<Usage>), ApiError> {
    let mut stream = ctx.client.chat_stream(req).await?;

    let mut text = String::new();
    // index -> (id, name, accumulated arguments)
    let mut partials: BTreeMap<usize, (Option<String>, Option<String>, String)> = BTreeMap::new();
    let mut finish_reason = None;
    let mut usage = None;
    let mut notified = false;

    while let Some(item) = stream.next().await {
        let chunk = item?;
        if !notified && (!chunk.delta.is_empty() || !chunk.tool_calls.is_empty()) {
            notified = true;
            notify.notify_waiters();
        }
        if !chunk.delta.is_empty() {
            ui.stream_text(&chunk.delta);
            text.push_str(&chunk.delta);
        }
        for d in chunk.tool_calls {
            let entry = partials
                .entry(d.index)
                .or_insert((None, None, String::new()));
            if d.id.is_some() {
                entry.0 = d.id;
            }
            if d.name.is_some() {
                entry.1 = d.name;
            }
            entry.2.push_str(&d.arguments);
        }
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason;
        }
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }

    let tool_calls: Vec<ToolCall> = partials
        .into_iter()
        .map(|(idx, (id, name, arguments))| ToolCall {
            id: id.unwrap_or_else(|| format!("call_{idx}")),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: name.unwrap_or_default(),
                arguments,
            },
        })
        .collect();

    Ok((text, tool_calls, finish_reason, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_mentions_cwd_os_shell_and_all_tools() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = system_prompt(dir.path(), "meta/llama-3.3-70b-instruct", &[], "");
        assert!(prompt.contains(&dir.path().display().to_string()));
        assert!(prompt.contains(std::env::consts::OS));
        assert!(prompt.contains(shell_name()));
        assert!(prompt.contains("meta/llama-3.3-70b-instruct"));
        for tool in [
            "list_files",
            "read_file",
            "search_files",
            "write_file",
            "edit_file",
            "run_command",
        ] {
            assert!(prompt.contains(tool), "missing {tool} in system prompt");
        }
    }

    #[test]
    fn system_prompt_includes_behavioral_rules_against_announcing_without_acting() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = system_prompt(dir.path(), "m", &[], "");
        assert!(prompt.contains("critical failure"));
        assert!(prompt.contains("NEVER end your turn"));
    }

    #[test]
    fn system_prompt_appends_extra_config_text() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = system_prompt(dir.path(), "m", &[], "always run cargo fmt first");
        assert!(prompt.contains("always run cargo fmt first"));
    }

    #[test]
    fn system_prompt_includes_project_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let prompt = system_prompt(dir.path(), "m", &[], "");
        assert!(prompt.contains("Cargo.toml"));
        assert!(prompt.contains("src/"));
    }

    #[test]
    fn system_prompt_caps_snapshot_and_shows_more_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(SNAPSHOT_CAP + 20) {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let prompt = system_prompt(dir.path(), "m", &[], "");
        assert!(prompt.contains("(+20 more)"));
    }

    #[test]
    fn civil_from_days_matches_known_epoch_date() {
        // 2026-07-04 00:00:00 UTC is 20638 days after the Unix epoch.
        let (y, m, d) = civil_from_days(20638);
        assert_eq!((y, m, d), (2026, 7, 4));
    }

    #[test]
    fn format_bytes_switches_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert!(format_bytes(2048).ends_with("KB"));
    }

    #[test]
    fn format_duration_switches_units() {
        assert_eq!(format_duration(Duration::from_millis(3)), "3ms");
        assert_eq!(format_duration(Duration::from_secs_f64(8.4)), "8.4s");
    }

    #[test]
    fn format_token_count_uses_k_suffix_above_1000() {
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(1900), "1.9k");
    }

    #[test]
    fn detects_low_value_tool_preamble() {
        assert!(low_value_tool_preamble(
            "Vou explorar mais alguns arquivos para entender melhor o estado atual."
        ));
        assert!(low_value_tool_preamble(
            "I will inspect the project files before making changes."
        ));
        assert!(!low_value_tool_preamble(
            "Encontrei o problema: o limiter espera corretamente, mas max_rounds estava encerrando a rodada."
        ));
    }

    // -- A5 smoke: pure summary-line formatting (SPEC-UX A5) ---------------

    #[test]
    fn tool_summary_line_reports_success_with_size_and_duration() {
        let result = serde_json::json!({"ok": true, "output": "x".repeat(14540)}).to_string();
        let line = tool_summary_line(
            "read_file",
            r#"{"path": "agent.rs"}"#,
            &result,
            Duration::from_millis(3),
        );
        assert!(line.starts_with('\u{2713}'), "line: {line}");
        assert!(line.contains("read_file"));
        assert!(line.contains("agent.rs"));
        assert!(line.contains("KB"));
        assert!(line.contains("3ms"));
    }

    #[test]
    fn tool_summary_line_reports_user_denial() {
        let result = serde_json::json!({"ok": false, "error": "user denied write"}).to_string();
        let line = tool_summary_line(
            "edit_file",
            r#"{"path": "f.rs"}"#,
            &result,
            Duration::from_millis(1),
        );
        assert!(line.starts_with('\u{2717}'), "line: {line}");
        assert!(line.contains("edit_file"));
        assert!(line.contains("user denied"));
    }

    #[test]
    fn tool_summary_line_reports_run_command_exit_code() {
        let result =
            serde_json::json!({"ok": true, "output": "exit_code: 0\nall good"}).to_string();
        let line = tool_summary_line(
            "run_command",
            r#"{"command": "cargo test"}"#,
            &result,
            Duration::from_secs_f64(8.4),
        );
        assert!(line.contains("run_command"));
        assert!(line.contains("cargo test"));
        assert!(line.contains("exit 0"));
        assert!(line.contains("8.4s"));
    }

    #[test]
    fn turn_summary_line_reports_rounds_tools_and_duration() {
        let line = turn_summary_line(3, 2, None, Duration::from_secs(14));
        assert_eq!(line, "-- 2 tools | 14s");
    }

    #[test]
    fn turn_summary_line_includes_tokens_when_usage_available() {
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 1800,
            total_tokens: 1900,
        };
        let line = turn_summary_line(3, 2, Some(&usage), Duration::from_secs(14));
        assert_eq!(line, "-- 2 tools | 1.9k tokens | 14s");
    }

    #[test]
    fn turn_summary_line_omits_tokens_when_usage_unavailable() {
        let line = turn_summary_line(1, 0, None, Duration::from_secs(2));
        assert!(!line.contains("tokens"));
    }
}
