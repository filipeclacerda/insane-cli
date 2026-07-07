//! Command-line surface (clap derive). Phase-2 commands (`explain`, `review`,
//! `fix`, `refactor`, `test`) are declared here so the CLI is complete, but
//! their handlers return a clean "not implemented in this phase" error.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "insane",
    version,
    about = "insane-cli - multi-provider programming assistant CLI"
)]
pub struct Cli {
    /// Provider profile to use, overriding active_provider.
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Model to use, overrides config/env.
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Emit a single JSON object on stdout instead of plain text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Force streaming output.
    #[arg(long, global = true, conflicts_with = "no_stream")]
    pub stream: bool,

    /// Disable streaming output.
    #[arg(long, global = true)]
    pub no_stream: bool,

    /// Request timeout in seconds.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,

    /// Suppress non-essential stderr output.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Increase log verbosity on stderr.
    #[arg(long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Path to an alternate config.toml.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Disable on-disk response caching for this invocation.
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Auto-confirm prompts where it is safe to do so (never for writes with
    /// detected secrets).
    #[arg(long, global = true)]
    pub yes: bool,

    /// Disable tool calling. Only meaningful when no subcommand is given
    /// (which resolves to `chat`) or when running `chat` itself.
    #[arg(long, global = true)]
    pub no_tools: bool,

    /// Force the line-mode chat REPL instead of the fullscreen TUI (SPEC-UX
    /// Part B), even when stdin/stdout are both a TTY. Always in effect for
    /// pipes/CI regardless of this flag.
    #[arg(long, global = true)]
    pub plain: bool,

    /// Resume the most recently saved chat session for the active provider
    /// when no subcommand is given (i.e. `insane` resolves to `chat`).
    /// Equivalent to `insane chat --continue`. Aliases: `--resume`,
    /// `--continue-last`. Ignored when an explicit subcommand is given
    /// (use the subcommand's own `--continue` in that case).
    #[arg(
        long = "continue",
        alias = "resume",
        visible_alias = "continue-last",
        global = false
    )]
    pub continue_last: bool,

    /// With no subcommand, `insane` resolves to `chat` (SPEC-UX A6).
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// The subcommand to run: the one the user gave, or `chat` (honoring the
    /// top-level `--no-tools` flag and `--continue`) when none was given
    /// (SPEC-UX A6).
    pub fn resolved_command(&self) -> Command {
        self.command.clone().unwrap_or(Command::Chat {
            continue_last: self.continue_last,
        })
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Ask a single question. Use `-` to read the prompt from stdin.
    Ask {
        /// The prompt, or `-` to read from stdin.
        prompt: Option<String>,
        /// Additional file(s) to include as context.
        #[arg(short = 'f', long = "file")]
        files: Vec<PathBuf>,
        /// Cache this request's response on disk (deterministic commands only).
        #[arg(long)]
        cache: bool,
        /// Run one non-interactive round of the agentic tool-calling loop
        /// (SPEC-AGENT). With non-TTY stdin, every write/shell tool call is
        /// automatically refused.
        #[arg(long)]
        tools: bool,
    },
    /// Start an interactive chat session (/exit, /clear, /model <m>, /tools, /cwd,
    /// /continue). Tool calling is enabled by default; pass the top-level
    /// `--no-tools` for the old plain chat. This is also what `insane` runs
    /// when no subcommand is given (SPEC-UX A6).
    Chat {
        /// Resume the most recently saved chat session for the active
        /// provider, restoring its model and message history. The session
        /// is saved automatically when the chat exits normally.
        #[arg(long = "continue", alias = "resume", visible_alias = "continue-last")]
        continue_last: bool,
    },
    /// Explain a piece of code.
    Explain {
        /// File to explain, or `-` for stdin.
        file: String,
        /// Restrict to a line range, e.g. `10:42`.
        #[arg(long, value_name = "A:B")]
        lines: Option<String>,
    },
    /// Review file(s), or a diff.
    Review {
        /// Files to review.
        files: Vec<PathBuf>,
        /// Read a `git diff` (or stdin) instead of whole files.
        #[arg(long)]
        diff: bool,
    },
    /// Propose a fix for a file.
    Fix {
        file: PathBuf,
        /// Apply the fix (with confirmation, atomic write + `.insane-bak` backup).
        #[arg(long)]
        apply: bool,
        /// Restore the most recent `.insane-bak` backup for this file.
        #[arg(long)]
        rollback: bool,
    },
    /// Refactor a file toward a stated goal.
    Refactor {
        file: PathBuf,
        #[arg(long)]
        goal: String,
        /// Apply the refactor (with confirmation, atomic write + `.insane-bak` backup).
        #[arg(long)]
        apply: bool,
        /// Restore the most recent `.insane-bak` backup for this file.
        #[arg(long)]
        rollback: bool,
    },
    /// Generate tests for a file.
    Test {
        file: PathBuf,
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
    },
    /// Manage configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List available models.
    Models {
        /// Bypass any cached model list and refetch.
        #[arg(long)]
        refresh: bool,
    },
    /// Check API connectivity, rate-limiter metrics, and effective config.
    Status,
    /// Diagnose provider configuration, connectivity, streaming and tools.
    Doctor {
        /// Send a minimal chat request in addition to the connectivity checks.
        #[arg(long)]
        deep: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Print a single config value.
    Get { key: String },
    /// Set a single config value in the file.
    Set { key: String, value: String },
    /// Print the full effective configuration (never includes the API key).
    List,
    /// Print the path to the config file.
    Path,
    /// Read an API key from stdin (no echo) and store it in the OS keyring.
    SetKey {
        /// Provider profile whose credential should be stored.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Remove the API key from the OS keyring.
    UnsetKey {
        /// Provider profile whose credential should be removed.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Convert a legacy flat config into provider profiles.
    Migrate,
    /// Remove every entry from the on-disk response cache.
    CacheClear,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_resolves_to_chat() {
        let cli = Cli::parse_from(["insane"]);
        assert!(cli.command.is_none());
        assert!(matches!(cli.resolved_command(), Command::Chat { .. }));
        assert!(!cli.no_tools);
    }

    #[test]
    fn no_tools_flag_works_without_a_subcommand() {
        let cli = Cli::parse_from(["insane", "--no-tools"]);
        assert!(cli.command.is_none());
        assert!(matches!(cli.resolved_command(), Command::Chat { .. }));
        assert!(cli.no_tools);
    }

    #[test]
    fn explicit_chat_subcommand_still_works() {
        let cli = Cli::parse_from(["insane", "chat"]);
        assert!(matches!(cli.resolved_command(), Command::Chat { .. }));
    }

    #[test]
    fn explicit_subcommand_is_not_overridden() {
        let cli = Cli::parse_from(["insane", "status"]);
        assert!(matches!(cli.resolved_command(), Command::Status));
    }

    #[test]
    fn no_tools_combines_with_explicit_chat_subcommand() {
        let cli = Cli::parse_from(["insane", "--no-tools", "chat"]);
        assert!(cli.no_tools);
        assert!(matches!(cli.resolved_command(), Command::Chat { .. }));
    }

    #[test]
    fn chat_continue_flag_parses() {
        let cli = Cli::parse_from(["insane", "chat", "--continue"]);
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn chat_resume_alias_parses() {
        let cli = Cli::parse_from(["insane", "chat", "--resume"]);
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn top_level_continue_resolves_to_chat_with_continue_last() {
        let cli = Cli::parse_from(["insane", "--continue"]);
        assert!(cli.command.is_none());
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn top_level_resume_alias_resolves_to_chat_with_continue_last() {
        let cli = Cli::parse_from(["insane", "--resume"]);
        assert!(cli.command.is_none());
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn top_level_continue_last_alias_resolves_to_chat_with_continue_last() {
        let cli = Cli::parse_from(["insane", "--continue-last"]);
        assert!(cli.command.is_none());
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn top_level_continue_without_flag_is_false() {
        let cli = Cli::parse_from(["insane"]);
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(!continue_last),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn top_level_continue_ignored_for_explicit_non_chat_subcommand() {
        // `--continue` at the root only affects the implicit `chat` fallback;
        // an explicit `status` subcommand still wins.
        let cli = Cli::parse_from(["insane", "--continue", "status"]);
        assert!(matches!(cli.resolved_command(), Command::Status));
    }

    #[test]
    fn explicit_chat_continue_still_works_alongside_top_level_flag() {
        // When `chat` is given explicitly, its own `--continue` is what
        // matters; the top-level flag is simply unused in that path.
        let cli = Cli::parse_from(["insane", "chat", "--continue"]);
        match cli.resolved_command() {
            Command::Chat { continue_last } => assert!(continue_last),
            _ => panic!("expected Chat"),
        }
    }
}
