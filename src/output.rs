//! Output formatting: model responses go to stdout, logs to stderr.
//! `--json` mode emits a single JSON object; `--quiet` suppresses
//! non-essential stderr chatter.

use std::io::Write;

use serde::Serialize;

use crate::client::Usage;
use crate::limiter::RateLimiterMetrics;

#[derive(Debug, Clone, Copy)]
pub struct OutputOptions {
    pub json: bool,
    pub quiet: bool,
}

#[derive(Debug, Serialize)]
pub struct JsonResult<'a> {
    pub response: &'a str,
    pub model: &'a str,
    pub usage: &'a Usage,
    pub timing_ms: u128,
    pub rate_limiter: RateLimiterMetrics,
    /// The completion's `finish_reason` (SPEC-UX A3), when known. `None` for
    /// paths that don't track it (e.g. a cached response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<&'a str>,
}

/// Prints a single, complete result (non-streaming path).
pub fn print_result(opts: OutputOptions, result: &JsonResult<'_>) {
    if opts.json {
        match serde_json::to_string(result) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialize JSON output: {e}"),
        }
    } else {
        println!("{}", result.response);
    }
}

/// Writes one streamed chunk of model output to stdout immediately (no
/// buffering), unless JSON mode is active (JSON mode accumulates and prints
/// once at the end via `print_result`).
pub fn print_stream_chunk(opts: OutputOptions, chunk: &str) {
    if opts.json {
        return;
    }
    print!("{chunk}");
    let _ = std::io::stdout().flush();
}

/// Logs an informational message to stderr, suppressed by `--quiet`.
pub fn log_info(opts: OutputOptions, msg: &str) {
    if !opts.quiet {
        eprintln!("{msg}");
    }
}

/// Prints the insane-cli ASCII banner to stderr, occupying the top of the
/// terminal. Suppressed by `--quiet` and `--json` (the latter must keep
/// stdout/stderr free of any non-JSON text). Only called from the interactive
/// `chat` entry point, never from one-shot commands (`ask`, `status`, ...).
pub fn print_banner(opts: OutputOptions) {
    if opts.quiet || opts.json {
        return;
    }
    // FIGlet "slant" style, trimmed to keep it compact yet recognizable.
    let banner = "\
  ███                                                  
 ░░░                                                   
 ████  ████████    █████   ██████   ████████    ██████ 
░░███ ░░███░░███  ███░░   ░░░░░███ ░░███░░███  ███░░███
 ░███  ░███ ░███ ░░█████   ███████  ░███ ░███ ░███████ 
 ░███  ░███ ░███  ░░░░███ ███░░███  ░███ ░███ ░███░░░  
 █████ ████ █████ ██████ ░░████████ ████ █████░░██████ 
░░░░░ ░░░░ ░░░░░ ░░░░░░   ░░░░░░░░ ░░░░ ░░░░░  ░░░░░░  
                                                       
                                                       
   







";
    eprintln!("{banner}");
}

/// Logs an error message to stderr (never suppressed by `--quiet`). Runs
/// both the NVIDIA-key redactor (`error::redact`) and the general secret
/// redactor (`secrets::redact`) so stray tokens/passwords in error text
/// never reach the terminal.
pub fn log_error(msg: &str) {
    eprintln!(
        "error: {}",
        crate::secrets::redact(&crate::error::redact(msg))
    );
}
