//! `AgentUi` abstraction (SPEC-UX Part B, B0/B3): everything the agentic
//! loop and the tools show to the user -- confirmations, tool traces,
//! streamed text, warnings, round status, and the end-of-turn summary --
//! goes through this trait instead of touching stdout/stderr/stdin
//! directly. `PlainUi` reproduces the exact stderr/stdin behavior the
//! line-mode chat always had (SPEC-UX Part A); the TUI (`src/tui/`)
//! implements the same trait by pushing into shared render state and
//! blocking (properly, via a `oneshot`) on a modal for confirmations.
//!
//! `confirm` is `async` so a TUI confirmation can `.await` a `oneshot`
//! response from the render loop without blocking the whole task -- the
//! surrounding `tokio::select!` keeps polling the UI/render branches while a
//! confirm is pending. `PlainUi::confirm` does a normal blocking stdin read
//! inside that `async fn`; that's fine because line mode never has a
//! concurrent render loop to starve (identical to today's behavior).

use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::client::Usage;
use crate::output::OutputOptions;

/// The user's answer to a `y`/`n`/`a` confirmation prompt (SPEC-AGENT §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Yes,
    No,
    /// "Always allow this tool (or this exact command) for the rest of the
    /// session."
    Always,
}

/// What is shown to the user for one confirmation (SPEC-UX B3): a
/// diff (write/edit), a shell command (`run_command`), or neither (a
/// secret-detected read/search).
#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    /// Tool name; scopes the "always" answer (except `run_command`, which
    /// only remembers the exact command string).
    pub tool: String,
    /// The one-line question/summary shown above the diff/command.
    pub prompt: String,
    /// Additional safe, pre-rendered context (for example redacted secret
    /// findings) that belongs inside the confirmation modal.
    pub details: Option<String>,
    /// A pre-rendered unified diff (write_file/edit_file), if any.
    pub diff: Option<String>,
    /// The shell command being confirmed (`run_command`), if any.
    pub command: Option<String>,
}

/// Progress/feedback surface for one agentic turn (SPEC-UX A5/B3). Every
/// method must return promptly (no unbounded blocking besides `confirm`,
/// which legitimately waits on the user).
#[async_trait::async_trait]
pub trait AgentUi: Send + Sync {
    /// Blocks (cooperatively) until the user answers a confirmation.
    async fn confirm(&self, req: ConfirmRequest) -> Decision;
    /// A tool is about to run: `→ name(args)`.
    fn tool_trace(&self, name: &str, arguments: &str);
    /// A tool finished: `✓/✗ name label (detail)`.
    fn tool_summary(&self, name: &str, arguments: &str, result: &str, elapsed: Duration);
    /// A non-fatal warning (`finish_reason` surprise, rate-limit wait).
    fn warn(&self, msg: &str);
    /// One chunk of the assistant's streamed text.
    fn stream_text(&self, chunk: &str);
    /// One chunk of provider-supplied reasoning/thinking text.
    fn stream_thinking(&self, _chunk: &str) {}
    /// Removes the just-streamed assistant text for a tool-calling round when
    /// it was only a low-value preamble ("vou ler alguns arquivos...").
    /// Plain terminals cannot reliably erase already-printed output, so the
    /// default implementation is intentionally a no-op.
    fn discard_last_assistant_message(&self) {}
    /// Replaces the just-streamed assistant text after a text-encoded tool
    /// call was recovered. TUI can cleanly remove the JSON/tool-call block;
    /// plain terminals cannot erase already-printed output, so default no-op.
    fn replace_last_assistant_message(&self, _text: &str) {}
    /// The assistant's streamed text for this round has ended.
    fn end_of_stream(&self);
    /// Redraws the "waiting for the model" status line/spinner frame.
    fn spinner_tick(&self, line: &str);
    /// Clears whatever `spinner_tick` last drew.
    fn clear_status(&self);
    /// The whole turn ended: rounds/tools/tokens/duration.
    fn turn_summary(
        &self,
        rounds: u32,
        tools_executed: u32,
        usage: Option<&Usage>,
        elapsed: Duration,
    );
}

// ---------------------------------------------------------------------
// PlainUi: byte-for-byte the pre-TUI stderr/stdin behavior.
// ---------------------------------------------------------------------

/// Reads one `y`/`n`/`a` line from stdin, refusing (never destructive) if
/// stdin isn't a terminal or fails to read.
fn ask_yna(prompt: &str) -> Decision {
    eprint!("{prompt} [y/n/a] ");
    let _ = std::io::stderr().flush();

    if !std::io::stdin().is_terminal() {
        return Decision::No;
    }

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Decision::No;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Decision::Yes,
        "a" | "always" => Decision::Always,
        _ => Decision::No,
    }
}

/// Prints a unified diff to stderr, colored (add green / del red) when
/// stderr is a terminal.
pub fn print_diff_colored(diff: &str) {
    let is_tty = std::io::stderr().is_terminal();
    for line in diff.lines() {
        if is_tty
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || line.starts_with("@@")
                || line.starts_with("+++"))
        {
            eprintln!("\x1b[32m{line}\x1b[0m");
        } else if is_tty
            && ((line.starts_with('-') && !line.starts_with("---")) || line.starts_with("---"))
        {
            eprintln!("\x1b[31m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

/// Line-mode `AgentUi`: stderr for logs/traces/confirmations, stdout for the
/// model's streamed text (unless `--json`), stdin for confirmations. This is
/// exactly what the agentic loop did before the TUI existed.
pub struct PlainUi {
    pub out: OutputOptions,
    total_tokens: AtomicU64,
}

impl PlainUi {
    pub fn new(out: OutputOptions) -> Self {
        PlainUi {
            out,
            total_tokens: AtomicU64::new(0),
        }
    }

    fn feedback_enabled(&self) -> bool {
        !self.out.quiet && std::io::stderr().is_terminal()
    }

    pub fn reset_token_total(&self) {
        self.total_tokens.store(0, Ordering::Relaxed);
    }

    fn add_usage(&self, usage: Option<&Usage>) -> u64 {
        match usage {
            Some(usage) if usage.total_tokens > 0 => {
                self.total_tokens
                    .fetch_add(usage.total_tokens as u64, Ordering::Relaxed)
                    + usage.total_tokens as u64
            }
            _ => self.total_tokens.load(Ordering::Relaxed),
        }
    }
}

impl Default for PlainUi {
    fn default() -> Self {
        PlainUi::new(OutputOptions {
            json: false,
            quiet: false,
        })
    }
}

#[async_trait::async_trait]
impl AgentUi for PlainUi {
    async fn confirm(&self, req: ConfirmRequest) -> Decision {
        if let Some(details) = &req.details {
            eprintln!("{details}");
        }
        if let Some(diff) = &req.diff {
            print_diff_colored(diff);
        }
        ask_yna(&req.prompt)
    }

    fn tool_trace(&self, name: &str, arguments: &str) {
        let summary = crate::tools::tool_call_label(name, arguments);
        let line = format!("\u{25c7} {name}  {summary}");
        if std::io::stderr().is_terminal() {
            eprintln!("\x1b[36m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }

    fn tool_summary(&self, name: &str, arguments: &str, result: &str, elapsed: Duration) {
        if !self.feedback_enabled() {
            return;
        }
        eprintln!(
            "{}",
            crate::agent::tool_summary_line(name, arguments, result, elapsed)
        );
    }

    fn warn(&self, msg: &str) {
        if self.out.quiet {
            return;
        }
        eprintln!("{msg}");
    }

    fn stream_text(&self, chunk: &str) {
        crate::output::print_stream_chunk(self.out, chunk);
    }

    fn stream_thinking(&self, _chunk: &str) {}

    fn end_of_stream(&self) {
        if self.out.json {
            return;
        }
        println!();
    }

    fn spinner_tick(&self, line: &str) {
        if !self.feedback_enabled() {
            return;
        }
        eprint!("\r{line}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    fn clear_status(&self) {
        if !self.feedback_enabled() {
            return;
        }
        eprint!("\r{}\r", " ".repeat(48));
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    fn turn_summary(
        &self,
        rounds: u32,
        tools_executed: u32,
        usage: Option<&Usage>,
        elapsed: Duration,
    ) {
        let total = self.add_usage(usage);
        if !self.feedback_enabled() {
            return;
        }
        eprintln!(
            "{}",
            crate::agent::turn_summary_line_with_total(
                rounds,
                tools_executed,
                usage,
                elapsed,
                total
            )
        );
    }
}

// ---------------------------------------------------------------------
// FakeUi: a non-blocking `AgentUi` for tests. Every confirmation returns
// a fixed `Decision` (default `No`), so tests never touch real stdin and
// never block. Enabled for unit tests (`cfg(test)`) and, behind the
// `test-utils` feature, for integration tests in tests/.
// ---------------------------------------------------------------------

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support {
    use super::{AgentUi, ConfirmRequest, Decision, Duration, Usage};

    /// A test double for [`AgentUi`] that answers every confirmation with a
    /// fixed [`Decision`] (default [`Decision::No`]) and otherwise no-ops.
    /// Use it via `Permissions::with_ui(Box::new(FakeUi::deny()))` so tests
    /// never block on a real stdin.
    pub struct FakeUi {
        pub answer: Decision,
    }

    impl FakeUi {
        pub fn deny() -> Self {
            FakeUi {
                answer: Decision::No,
            }
        }
        pub fn accept() -> Self {
            FakeUi {
                answer: Decision::Yes,
            }
        }
        pub fn always() -> Self {
            FakeUi {
                answer: Decision::Always,
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentUi for FakeUi {
        async fn confirm(&self, _req: ConfirmRequest) -> Decision {
            self.answer
        }
        fn tool_trace(&self, _name: &str, _arguments: &str) {}
        fn tool_summary(&self, _name: &str, _arguments: &str, _result: &str, _elapsed: Duration) {}
        fn warn(&self, _msg: &str) {}
        fn stream_text(&self, _chunk: &str) {}
        fn end_of_stream(&self) {}
        fn spinner_tick(&self, _line: &str) {}
        fn clear_status(&self) {}
        fn turn_summary(
            &self,
            _rounds: u32,
            _tools_executed: u32,
            _usage: Option<&Usage>,
            _elapsed: Duration,
        ) {
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_diff_colored_does_not_panic_on_empty_diff() {
        print_diff_colored("");
    }
}
