//! insane-cli library crate.
//!
//! Phase 1/2 built this as a binary-only crate (`src/main.rs` with `mod`
//! declarations). Phase 3 needs integration tests (in `tests/`) to drive the
//! real `RateLimiter`, `ApiError` classification, config precedence, etc.
//! against a mock NIM server -- which is only possible if those types are
//! reachable from an external test crate, i.e. exported by a `lib` target.
//!
//! This file is a mechanical extraction of what used to live in
//! `src/main.rs`: the module declarations, `AppContext`, tracing setup, and
//! the `run`/`run_command` dispatch. `src/main.rs` is now a thin shim that
//! calls [`main_entry`]. Production behavior is unchanged -- this is purely a
//! testability seam (see `docs/REPORT.md` for the full justification).

pub mod agent;
pub mod cache;
pub mod cli;
pub mod client;
pub mod commands;
pub mod config;
pub mod context;
pub mod error;
pub mod fileops;
pub mod limiter;
pub mod output;
pub mod secrets;
pub mod session;
pub mod session_store;
pub mod tools;
pub mod tui;
pub mod ui;

use std::sync::Arc;

use clap::Parser;

use cli::{Cli, Command};
use client::nim::NimClient;
use error::ApiError;
use limiter::RateLimiter;
use output::OutputOptions;

/// Shared context threaded through every command handler.
#[derive(Clone)]
pub struct AppContext {
    pub cfg: config::EffectiveConfig,
    pub client: NimClient,
    pub limiter: Arc<RateLimiter>,
    pub out: OutputOptions,
    pub cli: Cli,
}

impl AppContext {
    pub fn switched_provider(&self, name: &str) -> Result<Self, ApiError> {
        let cfg = self.cfg.activated(name)?;
        build_context(cfg, self.out, self.cli.clone())
    }
}

fn build_context(
    cfg: config::EffectiveConfig,
    out: OutputOptions,
    cli: Cli,
) -> Result<AppContext, ApiError> {
    let api_key = config::resolve_provider_api_key(&cfg)?;
    let limiter = Arc::new(RateLimiter::with_policy(
        cfg.rate_limit_rpm.map(|rpm| rpm as usize),
        std::time::Duration::from_secs(60),
        std::time::Duration::from_millis(cfg.rate_limit_min_interval_ms),
    ));
    let client = NimClient::new_with_auth(
        cfg.base_url.clone(),
        api_key,
        std::time::Duration::from_secs(cfg.timeout_secs),
        limiter.clone(),
    )?;
    Ok(AppContext {
        cfg,
        client,
        limiter,
        out,
        cli,
    })
}

fn init_tracing(cli: &Cli) {
    use tracing_subscriber::EnvFilter;

    // `INSANE_LOG` drives structured filtering; `--verbose` raises the
    // default level when the env var is not set; `--quiet` lowers it.
    let default_level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        }
    };
    let filter =
        EnvFilter::try_from_env("INSANE_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if cli.json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Entry point invoked by `src/main.rs`. Parses argv, initializes tracing,
/// builds the async runtime, dispatches, and returns the process exit code.
pub fn main_entry() -> i32 {
    let cli = Cli::parse();
    init_tracing(&cli);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            return 1;
        }
    };

    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> i32 {
    let out = OutputOptions {
        json: cli.json,
        quiet: cli.quiet,
    };

    let result = tokio::select! {
        res = run_command(&cli, out) => res,
        _ = tokio::signal::ctrl_c() => {
            output::log_info(out, "^C received, cancelling...");
            Err(ApiError::Cancelled)
        }
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            if !matches!(e, ApiError::Cancelled) {
                output::log_error(&e.to_string());
            }
            e.exit_code()
        }
    }
}

async fn run_command(cli: &Cli, out: OutputOptions) -> Result<(), ApiError> {
    let cfg = config::load(cli)?;
    let command = cli.resolved_command();

    // `config` subcommands that don't touch the network can run without an
    // API key or an HTTP client at all.
    if let Command::Config { action } = &command {
        return commands::config_cmd::run(action, &cfg);
    }

    // Validate usage-level requirements before touching the keyring/network
    // (SPEC §9: don't touch network/keyring unless necessary; also keeps
    // exit code 2 vs 3 precedence intuitive for the user).
    if let Command::Ask { prompt: None, .. } = &command {
        return Err(ApiError::Usage {
            message: "missing prompt (pass an argument or `-` for stdin)".to_string(),
        });
    }

    // `--rollback` is a purely local filesystem operation; short-circuit
    // before resolving an API key or building an HTTP client (SPEC §9).
    if let Command::Fix {
        rollback: true,
        file,
        ..
    } = &command
    {
        fileops::rollback(file)?;
        if !out.quiet {
            eprintln!("Restored {} from backup", file.display());
        }
        return Ok(());
    }
    if let Command::Refactor {
        rollback: true,
        file,
        ..
    } = &command
    {
        fileops::rollback(file)?;
        if !out.quiet {
            eprintln!("Restored {} from backup", file.display());
        }
        return Ok(());
    }

    let ctx = build_context(cfg, out, cli.clone())?;

    match &command {
        Command::Ask {
            prompt,
            files,
            cache,
            tools,
        } => commands::ask::run(&ctx, prompt.clone(), files, *cache, *tools).await,
        Command::Chat { continue_last } => {
            commands::chat::run(&ctx, !cli.no_tools, *continue_last).await
        }
        Command::Explain { file, lines } => {
            commands::explain::run(&ctx, file.clone(), lines.clone()).await
        }
        Command::Review { files, diff } => commands::review::run(&ctx, files, *diff).await,
        Command::Fix {
            file,
            apply,
            rollback,
        } => commands::fix::run(&ctx, file.clone(), *apply, *rollback).await,
        Command::Refactor {
            file,
            goal,
            apply,
            rollback,
        } => commands::refactor::run(&ctx, file.clone(), goal.clone(), *apply, *rollback).await,
        Command::Test { file, output } => {
            commands::test::run(&ctx, file.clone(), output.clone()).await
        }
        Command::Config { .. } => unreachable!("handled above"),
        Command::Models { refresh } => commands::models::run(&ctx, *refresh).await,
        Command::Status => commands::status::run(&ctx).await,
        Command::Doctor { deep } => commands::doctor::run(&ctx, *deep).await,
    }
}
