//! Inline TUI (SPEC-UX Part B): raw mode via `crossterm`, rendered with
//! ratatui's inline viewport. Entered by `insane`/`insane chat`
//! when stdin *and* stdout are both a TTY and `--plain`/`config ui = "plain"`
//! weren't requested (`crate::commands::chat::use_tui`).
//!
//! Architecture (SPEC-UX B2/B3): one agentic turn's future
//! (`agent::run_turn`) is polled directly inside this module's own
//! `tokio::select!` loop, alongside the `crossterm` event stream and a
//! ~30fps render tick -- no `tokio::spawn` needed, since `TuiUi::confirm`
//! is a real `async fn` that `.await`s a `oneshot` receiver instead of
//! blocking a thread, so the surrounding `select!` keeps making progress
//! (polling input/redraw) while a confirmation is pending. Terminal state
//! is always restored: a `Drop` guard covers normal/error exits, and a
//! panic hook (installed for the lifetime of this function) restores it
//! before the (redacted) panic message prints -- necessary because this
//! crate builds with `panic = "abort"`, so `Drop` alone would never run on
//! a panic.

pub mod app;
pub mod format;
pub mod theme;
pub mod ui_impl;
pub mod view;

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tokio_util::sync::CancellationToken;

use crate::agent::{self, TurnOutcome};
use crate::client::LlmClient;
use crate::error::ApiError;
use crate::session::{self, Command as ReplCommand, Session, CONTINUE_MESSAGE, HELP_COMMANDS};
use crate::tools::{
    self,
    permission::{ApprovalPolicy, Permissions},
};
use crate::ui::Decision;
use crate::AppContext;

use app::{AppState, InteractionMode, MsgBlock, PendingSessionPicker};
use ui_impl::TuiUi;

type Term = Terminal<CrosstermBackend<io::Stdout>>;
const INLINE_INITIAL_HEIGHT: u16 = 6;

static LOG_SINK: OnceLock<Mutex<Option<Arc<Mutex<AppState>>>>> = OnceLock::new();

fn log_sink() -> &'static Mutex<Option<Arc<Mutex<AppState>>>> {
    LOG_SINK.get_or_init(|| Mutex::new(None))
}

struct TuiLogSinkGuard;

impl TuiLogSinkGuard {
    fn install(state: Arc<Mutex<AppState>>) -> Self {
        *log_sink().lock().unwrap() = Some(state);
        TuiLogSinkGuard
    }
}

impl Drop for TuiLogSinkGuard {
    fn drop(&mut self) {
        *log_sink().lock().unwrap() = None;
    }
}

pub(crate) fn capture_log_line(line: String) -> bool {
    let Some(state) = log_sink().lock().unwrap().clone() else {
        return false;
    };
    let line = line.trim();
    if line.is_empty() {
        return true;
    }
    if let Ok(mut st) = state.try_lock() {
        st.push_warn(line.to_string());
    }
    true
}

/// Restores the terminal on drop -- covers every normal/error exit path
/// from `run` (SPEC-UX B5). Does *not* cover panics: this crate builds with
/// `panic = "abort"`, so `Drop::drop` never runs on a panic; that path is
/// handled by the panic hook installed in `run` instead.
struct TerminalGuard;

impl TerminalGuard {
    fn enter(height: u16) -> io::Result<Term> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnableBracketedPaste)?;
        inline_terminal(height)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, DisableBracketedPaste, crossterm::cursor::Show);
        let _ = disable_raw_mode();
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }
}

/// Installs a panic hook that restores the terminal (raw mode, alternate
/// screen, cursor) before printing the redacted panic message -- required
/// because `panic = "abort"` skips `Drop` entirely (SPEC-UX B5).
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, DisableBracketedPaste, crossterm::cursor::Show);
        let _ = disable_raw_mode();
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
        let redacted = crate::secrets::redact(&crate::error::redact(&info.to_string()));
        eprintln!("{redacted}");
        default_hook(info);
    }));
}

fn inline_terminal(height: u16) -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    )
}

type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<TurnOutcome, ApiError>> + Send + 'a>>;

/// Entry point (SPEC-UX B1). `tools_enabled` mirrors `commands::chat::run`.
/// `continue_last` resumes the most recently saved session for the active
/// provider, if one exists.
pub async fn run(
    ctx: &AppContext,
    tools_enabled: bool,
    continue_last: bool,
) -> Result<(), ApiError> {
    install_panic_hook();
    let mut inline_height = INLINE_INITIAL_HEIGHT;
    let mut terminal = TerminalGuard::enter(inline_height)
        .map_err(|e| ApiError::permanent(format!("failed to start TUI: {e}")))?;
    let _guard = TerminalGuard;

    let mut local_ctx = ctx.clone();
    let result = run_app(
        &mut local_ctx,
        tools_enabled,
        continue_last,
        &mut terminal,
        &mut inline_height,
    )
    .await;
    result
}

async fn run_app(
    ctx: &mut AppContext,
    tools_enabled: bool,
    continue_last: bool,
    terminal: &mut Term,
    inline_height: &mut u16,
) -> Result<(), ApiError> {
    let model = crate::commands::chat::initial_model(ctx);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cwd_display = cwd.display().to_string();

    let mut session = Session::new(model.clone(), ctx.cfg.max_context_bytes);
    if tools_enabled {
        sync_agent_prompt(ctx, &mut session, &cwd, InteractionMode::Default);
    }

    // Resume the most recently saved session for this provider, if asked.
    let mut did_resume = false;
    let mut open_resume_picker = false;
    if continue_last {
        let saved = crate::session_store::list(&ctx.cfg.active_provider);
        if saved.len() > 1 {
            open_resume_picker = true;
        } else if let Some(loaded) = crate::session_store::load(&ctx.cfg.active_provider) {
            crate::commands::chat::restore_loaded_session(
                &mut session,
                loaded,
                tools_enabled,
                &cwd,
                ctx,
            );
            did_resume = true;
        }
    }

    let state = Arc::new(Mutex::new(AppState::new(
        session.model.clone(),
        cwd_display,
        cwd.clone(),
    )));
    {
        let mut st = state.lock().unwrap();
        st.ignore = ctx.cfg.ignore.clone();
        if open_resume_picker {
            open_session_picker(&mut st, &ctx.cfg.active_provider);
        }
    }
    let _log_sink = TuiLogSinkGuard::install(state.clone());
    let ui = TuiUi::new(state.clone());
    let mut permissions = Permissions::with_ui(Box::new(TuiUi::new(state.clone())));
    let mut last_finish_reason: Option<String> = None;

    {
        let mut st = state.lock().unwrap();
        st.provider = ctx.cfg.active_provider.clone();
        st.providers = ctx.cfg.providers.keys().cloned().collect();
        st.push_warn(if tools_enabled {
            "insane-cli TUI (tools enabled) -- Enter=send, Esc/Ctrl+C=cancel, Ctrl+L=clear, \
Ctrl+O=toggle thinking, Ctrl+E=toggle command output, /help"
                .to_string()
        } else {
            "insane-cli TUI -- Enter=send, Esc/Ctrl+C=cancel, Ctrl+L=clear, Ctrl+O=toggle thinking, \
Ctrl+E=toggle command output, /help"
                .to_string()
        });
        // Replay the resumed conversation into the visible transcript so the
        // user sees what was said in the previous session (the model already
        // has the raw history; this is purely for the on-screen transcript).
        if did_resume {
            if session.history.iter().any(|m| m.role != "system") {
                st.push_warn(format!(
                    "resumed session ({} messages, model {}) -- /exit to quit, /clear to reset",
                    session.history.len(),
                    session.model
                ));
                for m in &session.history {
                    match m.role.as_str() {
                        "user" => {
                            if let Some(c) = m.content.as_deref() {
                                st.messages.push(app::MsgBlock::User(c.to_string()));
                            }
                        }
                        "assistant" => {
                            if let Some(c) = m.content.as_deref() {
                                st.messages.push(app::MsgBlock::Assistant(c.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
                st.dirty = true;
            } else {
                st.push_warn(
                    "no saved session to resume for this provider; starting a fresh chat"
                        .to_string(),
                );
            }
        } else if continue_last && !open_resume_picker {
            st.push_warn(
                "no saved session to resume for this provider; starting a fresh chat".to_string(),
            );
        }
        st.status.spinner_line = Some("loading available models...".to_string());
    }

    let mut events = EventStream::new();
    let mut render_tick = tokio::time::interval(Duration::from_millis(33));

    render(terminal, &state, inline_height)?;
    match ctx.client.list_models().await {
        Ok(models) => {
            state
                .lock()
                .unwrap()
                .set_models(models.into_iter().map(|model| model.id).collect());
        }
        Err(err) => {
            let mut st = state.lock().unwrap();
            st.set_models(vec![session.model.clone()]);
            st.push_warn(format!(
                "could not load model list; /model still accepts a name ({err})"
            ));
        }
    }
    state.lock().unwrap().status.spinner_line = None;

    loop {
        {
            let st = state.lock().unwrap();
            if st.should_quit {
                break;
            }
        }

        // Idle phase: no turn in flight, so `session`/`permissions` are
        // freely borrowable here. `handle_event` returns `true` when it
        // just submitted a line that should start a turn; the turn itself
        // runs in `run_one_turn` below (never inside this `select!`), so
        // each of `session`/`permissions`'s mutable-borrow spans is scoped
        // to exactly one call, not to a variable that outlives the loop
        // (which is what made the borrow checker reject the `Option<
        // TurnFuture>`-as-a-loop-variable version of this).
        let mut start_turn_now = false;
        let mut start_compact_now = false;
        tokio::select! {
            biased;

            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        start_turn_now = handle_event(
                            event,
                            ctx,
                            &state,
                            &mut session,
                            &mut permissions,
                            &cwd,
                            tools_enabled,
                            &mut last_finish_reason,
                        );
                    }
                    Some(Err(_)) | None => {
                        state.lock().unwrap().should_quit = true;
                    }
                }
            }

            _ = render_tick.tick() => {
                refresh_rate_status(ctx, &state).await;
                if let Some(line) = take_pending_submit(&state) {
                    start_turn_now = submit_line(
                        line,
                        ctx,
                        &state,
                        &mut session,
                        &mut permissions,
                        &cwd,
                        tools_enabled,
                        &mut last_finish_reason,
                    );
                }
                start_compact_now = take_pending_compact(&state);
                let dirty = state.lock().unwrap().dirty;
                if dirty {
                    render(terminal, &state, inline_height)?;
                    state.lock().unwrap().dirty = false;
                }
            }
        }

        if start_turn_now {
            run_one_turn(
                ctx,
                &mut session,
                &mut permissions,
                &cwd,
                &ui,
                &state,
                &mut events,
                &mut render_tick,
                terminal,
                inline_height,
                &mut last_finish_reason,
            )
            .await?;
            // Persist after each turn so a crash/Ctrl+C mid-chat still
            // leaves something for `--continue` to resume. Best-effort.
            crate::session_store::save(&ctx.cfg.active_provider, &session.model, &session.history);
        } else if start_compact_now {
            run_compact_task(
                ctx,
                &mut session,
                &state,
                &mut events,
                &mut render_tick,
                terminal,
                inline_height,
                &mut last_finish_reason,
            )
            .await?;
            crate::session_store::save(&ctx.cfg.active_provider, &session.model, &session.history);
        }
    }

    // Final save on exit so `insane chat --continue` can resume.
    crate::session_store::save(&ctx.cfg.active_provider, &session.model, &session.history);
    Ok(())
}

/// Runs one agentic turn to completion (or until cancelled), rendering and
/// handling cancel/quit input the whole time (SPEC-UX B2/B4). `turn_fut` is
/// a block-local here -- its borrow of `session`/`permissions` ends when
/// this function returns, so the caller's next loop iteration can freely
/// re-borrow them for the *next* turn (see the SPEC-UX B2 architecture note at
/// the top of this file).
#[allow(clippy::too_many_arguments)]
async fn run_one_turn(
    ctx: &AppContext,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    ui: &TuiUi,
    state: &Arc<Mutex<AppState>>,
    events: &mut EventStream,
    render_tick: &mut tokio::time::Interval,
    terminal: &mut Term,
    inline_height: &mut u16,
    last_finish_reason: &mut Option<String>,
) -> Result<(), ApiError> {
    let mode = state.lock().unwrap().mode;
    let cancellation = CancellationToken::new();
    let mut turn_fut = start_turn(
        ctx,
        session,
        permissions,
        cwd,
        ui,
        mode,
        cancellation.clone(),
    );

    loop {
        tokio::select! {
            biased;

            outcome = &mut turn_fut => {
                let mut st = state.lock().unwrap();
                st.processing = false;
                st.status.spinner_line = None;
                match outcome {
                    Ok(o) => *last_finish_reason = o.finish_reason,
                    Err(e) => st.push_warn(format!("error: {e}")),
                }
                st.dirty = true;
                return Ok(());
            }

            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if handle_event_while_processing(event, state) {
                            cancellation.cancel();
                        }
                    }
                    Some(Err(_)) | None => {
                        state.lock().unwrap().should_quit = true;
                        return Ok(());
                    }
                }
            }

            _ = render_tick.tick() => {
                refresh_rate_status(ctx, state).await;
                let dirty = state.lock().unwrap().dirty;
                if dirty {
                    render(terminal, state, inline_height)?;
                    state.lock().unwrap().dirty = false;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_compact_task(
    ctx: &AppContext,
    session: &mut Session,
    state: &Arc<Mutex<AppState>>,
    events: &mut EventStream,
    render_tick: &mut tokio::time::Interval,
    terminal: &mut Term,
    inline_height: &mut u16,
    last_finish_reason: &mut Option<String>,
) -> Result<(), ApiError> {
    {
        let mut st = state.lock().unwrap();
        st.processing = true;
        st.status.spinner_line = Some("compacting conversation...".to_string());
        st.push_warn("compacting conversation...".to_string());
    }

    let cancellation = CancellationToken::new();
    let compact_fut =
        crate::commands::chat::compact_session_with_cancel(ctx, session, &cancellation);
    tokio::pin!(compact_fut);

    loop {
        tokio::select! {
            biased;

            outcome = &mut compact_fut => {
                let mut st = state.lock().unwrap();
                st.processing = false;
                st.status.spinner_line = None;
                match outcome {
                    Ok(Some(stats)) => {
                        st.clear_conversation();
                        st.push_warn(format!(
                            "compacted {} messages into ~{} chars",
                            stats.original_messages, stats.summary_chars
                        ));
                        *last_finish_reason = None;
                    }
                    Ok(None) => {
                        st.push_warn("nothing to compact".to_string());
                    }
                    Err(ApiError::Cancelled) => {
                        st.push_warn("compaction cancelled".to_string());
                    }
                    Err(err) => {
                        st.push_warn(format!("could not compact conversation: {err}"));
                    }
                }
                st.dirty = true;
                return Ok(());
            }

            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if handle_event_while_processing(event, state) {
                            cancellation.cancel();
                        }
                    }
                    Some(Err(_)) | None => {
                        let mut st = state.lock().unwrap();
                        st.should_quit = true;
                        cancellation.cancel();
                    }
                }
            }

            _ = render_tick.tick() => {
                refresh_rate_status(ctx, state).await;
                let dirty = state.lock().unwrap().dirty;
                if dirty {
                    render(terminal, state, inline_height)?;
                    state.lock().unwrap().dirty = false;
                }
            }
        }
    }
}

fn render(
    terminal: &mut Term,
    state: &Arc<Mutex<AppState>>,
    inline_height: &mut u16,
) -> Result<(), ApiError> {
    let width = terminal
        .size()
        .map_err(|e| ApiError::permanent(format!("TUI size failed: {e}")))?
        .width
        .max(1) as usize;

    let mut insert_lines = Vec::new();
    let (desired_height, reset_viewport, purge_terminal) = {
        let mut st = state.lock().unwrap();
        drain_committed_blocks(&mut st, width, &mut insert_lines);
        drain_live_overflow(&mut st, width, &mut insert_lines);
        if let Some(pending) = st.confirm.as_mut() {
            if !pending.printed {
                insert_lines.extend(view::confirm_transcript_lines(pending, width));
                pending.printed = true;
            }
        }
        let desired_height = view::desired_inline_height(&st, width);
        let reset_viewport = st.viewport_reset_requested;
        let purge_terminal = st.terminal_purge_requested;
        st.viewport_reset_requested = false;
        st.terminal_purge_requested = false;
        (desired_height, reset_viewport, purge_terminal)
    };

    if purge_terminal {
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, Clear(ClearType::All), Clear(ClearType::Purge))
            .map_err(|e| ApiError::permanent(format!("TUI clear failed: {e}")))?;
    }

    ensure_viewport_height(
        terminal,
        inline_height,
        desired_height,
        reset_viewport || purge_terminal,
    )
    .map_err(|e| ApiError::permanent(format!("TUI viewport reset failed: {e}")))?;

    insert_before_lines(terminal, insert_lines)
        .map_err(|e| ApiError::permanent(format!("TUI transcript insert failed: {e}")))?;

    let st = state.lock().unwrap();
    terminal
        .draw(|frame| view::draw_inline(frame, &st))
        .map_err(|e| ApiError::permanent(format!("TUI render failed: {e}")))?;
    Ok(())
}

fn drain_committed_blocks(st: &mut AppState, width: usize, out: &mut Vec<Line<'static>>) {
    let frontier = st.commit_frontier().min(st.messages.len());
    if frontier <= st.committed_blocks {
        return;
    }

    let start = st.committed_blocks;
    let live_skip = st.live_committed_chars;
    for (offset, msg) in st.messages[start..frontier].iter().enumerate() {
        if offset == 0 && live_skip > 0 {
            if let MsgBlock::Assistant(text) = msg {
                if let Some(tail) = text.get(live_skip..) {
                    out.extend(view::block_lines(
                        &MsgBlock::Assistant(tail.to_string()),
                        width,
                        st.show_thinking,
                    ));
                }
                continue;
            }
        }
        out.extend(view::block_lines(msg, width, st.show_thinking));
    }
    st.committed_blocks = frontier;
    st.live_committed_chars = 0;
}

fn drain_live_overflow(st: &mut AppState, width: usize, out: &mut Vec<Line<'static>>) {
    if !st.processing || st.committed_blocks >= st.messages.len() {
        return;
    }
    let idx = st.committed_blocks;
    let text = match &st.messages[idx] {
        MsgBlock::Assistant(text) => text.clone(),
        _ => return,
    };
    let old_commit = st.live_committed_chars.min(text.len());
    if !text.is_char_boundary(old_commit) {
        return;
    }
    if view::live_tail_lines(st, width).len() <= view::LIVE_TAIL_BUDGET {
        return;
    }

    let Some(tail) = text.get(old_commit..) else {
        return;
    };
    let mut chosen = None;
    for (rel_idx, ch) in tail.char_indices() {
        if ch != '\n' {
            continue;
        }
        let candidate = old_commit + rel_idx + 1;
        chosen = Some(candidate);
        let mut remaining_lines = view::block_lines(
            &MsgBlock::Assistant(text[candidate..].to_string()),
            width,
            st.show_thinking,
        )
        .len();
        for msg in st.messages.iter().skip(idx + 1) {
            remaining_lines += view::block_lines(msg, width, st.show_thinking).len();
        }
        if remaining_lines <= view::LIVE_TAIL_BUDGET {
            break;
        }
    }

    let Some(new_commit) = chosen else {
        return;
    };
    if new_commit <= old_commit {
        return;
    }
    out.extend(view::block_lines(
        &MsgBlock::Assistant(text[old_commit..new_commit].to_string()),
        width,
        st.show_thinking,
    ));
    st.live_committed_chars = new_commit;
}

fn ensure_viewport_height(
    terminal: &mut Term,
    current_height: &mut u16,
    desired_height: u16,
    force_reset: bool,
) -> io::Result<()> {
    if force_reset || *current_height != desired_height {
        let _ = terminal.clear();
        *terminal = inline_terminal(desired_height)?;
        *current_height = desired_height;
    }
    Ok(())
}

fn insert_before_lines(terminal: &mut Term, lines: Vec<Line<'static>>) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len().min(u16::MAX as usize) as u16;
    terminal.insert_before(height, move |buf| {
        Paragraph::new(lines).render(buf.area, buf);
    })
}

async fn refresh_rate_status(ctx: &AppContext, state: &Arc<Mutex<AppState>>) {
    let metrics = ctx.limiter.metrics().await;
    let mut st = state.lock().unwrap();
    let used = Some(metrics.used as u32);
    let capacity = metrics.capacity.map(|value| value as u32);
    if st.status.rate_used != used
        || st.status.rate_capacity != capacity
        || st.status.min_interval_ms != metrics.min_interval_ms
        || st.status.next_request_ms != metrics.next_request_in_ms
    {
        st.status.rate_used = used;
        st.status.rate_capacity = capacity;
        st.status.min_interval_ms = metrics.min_interval_ms;
        st.status.next_request_ms = metrics.next_request_in_ms;
        st.dirty = true;
    }
}

/// Starts one agentic turn as a directly-polled future (not spawned --
/// SPEC-UX B2 architecture note above). Borrows `session`/`permissions` for
/// as long as the turn is in flight.
fn start_turn<'a>(
    ctx: &'a AppContext,
    session: &'a mut Session,
    permissions: &'a mut Permissions,
    cwd: &'a std::path::Path,
    ui: &'a TuiUi,
    mode: InteractionMode,
    cancellation: CancellationToken,
) -> TurnFuture<'a> {
    if mode == InteractionMode::Plan {
        Box::pin(agent::run_turn_with_tool_defs_and_cancel(
            agent::RunTurnArgs {
                ctx,
                session,
                permissions,
                cwd,
                max_rounds: ctx.cfg.agent_max_rounds,
                ui,
                tool_defs: tools::plan_tool_defs(),
                cancellation,
            },
        ))
    } else {
        Box::pin(agent::run_turn_with_cancel(
            ctx,
            session,
            permissions,
            cwd,
            ctx.cfg.agent_max_rounds,
            ui,
            cancellation,
        ))
    }
}

fn permission_policy(mode: InteractionMode) -> ApprovalPolicy {
    match mode {
        InteractionMode::Default => ApprovalPolicy::Default,
        InteractionMode::Plan => ApprovalPolicy::Default,
        InteractionMode::Auto => ApprovalPolicy::Auto,
        InteractionMode::AcceptEdits => ApprovalPolicy::AcceptEdits,
    }
}

fn mode_description(mode: InteractionMode) -> &'static str {
    match mode {
        InteractionMode::Default => "asks before edits and commands",
        InteractionMode::Plan => "can inspect files and run commands; edits disabled",
        InteractionMode::Auto => "runs edits and commands without prompting",
        InteractionMode::AcceptEdits => "file edits allowed; commands still ask",
    }
}

fn is_ctrl_o(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
}

fn is_shift_tab(key: &KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

#[allow(clippy::too_many_arguments)]
fn cycle_interaction_mode(
    ctx: &mut AppContext,
    state: &Arc<Mutex<AppState>>,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    tools_enabled: bool,
) {
    let mode = {
        let mut st = state.lock().unwrap();
        st.mode = st.mode.next();
        let mode = st.mode;
        st.push_warn(format!(
            "mode: {} — {}",
            mode.label(),
            mode_description(mode)
        ));
        mode
    };
    permissions.set_policy(permission_policy(mode));
    if tools_enabled {
        sync_agent_prompt(ctx, session, cwd, mode);
    }
}

fn sync_agent_prompt(
    ctx: &AppContext,
    session: &mut Session,
    cwd: &std::path::Path,
    mode: InteractionMode,
) {
    let mut prompt = agent::system_prompt(
        cwd,
        &session.model,
        &ctx.cfg.ignore,
        &ctx.cfg.system_prompt_extra,
    );
    prompt.push_str(mode.system_instruction());
    session.push_system(prompt);
}

fn completion_text(value: &str) -> String {
    match value.find('<') {
        Some(idx) => value[..idx].trim_end().to_string() + " ",
        None => value.to_string(),
    }
}

/// Applies a suggestion to `input`/`cursor`, honoring `replace_range`:
/// `@file` mentions replace only the `@query` token at the cursor, while
/// slash commands replace the whole input line (the original behavior).
/// Returns the new `(input, cursor)`.
fn apply_completion(
    input: &str,
    _cursor: usize,
    suggestion: &crate::tui::app::InputSuggestion,
) -> (String, usize) {
    if let Some((start, end)) = suggestion.replace_range {
        let chars: Vec<char> = input.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let before: String = chars[..start].iter().collect();
        let after: String = chars[end..].iter().collect();
        let inserted: Vec<char> = suggestion.value.chars().collect();
        let new_cursor = start + inserted.len();
        let mut new_input = before;
        new_input.push_str(&suggestion.value);
        new_input.push_str(&after);
        (new_input, new_cursor)
    } else {
        // Slash-command completion: replace the whole input line.
        (completion_text(&suggestion.value), 0)
    }
}

/// Expands `@path` mention tokens in `line` into inline fenced file
/// content, mirroring `ask -f`/`context::format_block`. Tokens that don't
/// resolve to a readable, non-ignored, non-denylisted file inside the
/// sandbox are left untouched (so the model still sees the literal `@path`
/// and can decide what to do). Directories are skipped (left as-is) since
/// the agent has `list_files` for those.
fn expand_file_mentions(
    line: &str,
    cwd: &std::path::Path,
    ignore: &[String],
    state: &Arc<Mutex<AppState>>,
) -> String {
    if !line.contains('@') {
        return line.to_string();
    }
    let mut out = String::new();
    let mut rest = line;
    let mut expanded_count: usize = 0;
    while let Some(at_idx) = rest.find('@') {
        // `@` must be at start-of-input or preceded by whitespace to count
        // as a mention (avoids mangling emails like `a@b`).
        let is_mention_start = at_idx == 0
            || rest[..at_idx]
                .chars()
                .next_back()
                .map(|c| c.is_whitespace())
                .unwrap_or(true);
        out.push_str(&rest[..at_idx]);
        if !is_mention_start {
            out.push('@');
            rest = &rest[at_idx + 1..];
            continue;
        }
        // Collect the token after `@`: path chars up to the next whitespace.
        let after = &rest[at_idx + 1..];
        let token_end = after
            .char_indices()
            .skip_while(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .next()
            .unwrap_or(after.len());
        let token = &after[..token_end];
        if token.is_empty() {
            // Bare `@` followed by whitespace: leave it.
            out.push('@');
            rest = after;
            continue;
        }
        match resolve_mention(cwd, token, ignore) {
            Some(content) => {
                out.push_str(&content);
                out.push('\n');
                expanded_count += 1;
            }
            None => {
                // Unresolvable: keep the literal `@token`.
                out.push('@');
                out.push_str(token);
            }
        }
        rest = &after[token_end..];
    }
    out.push_str(rest);

    // Surface a one-line note in the conversation so the user sees what was
    // inlined (the expanded content itself goes to the model, not the
    // visible transcript).
    if expanded_count > 0 {
        let mut st = state.lock().unwrap();
        st.push_warn(format!(
            "inlined {expanded_count} file mention(s) (@path) into the message"
        ));
    }
    out
}

/// Resolves a single `@token` to inline fenced content, applying the same
/// sandbox/denylist/ignore checks as the `read_file` tool. Returns `None`
/// for directories or anything that fails a check.
fn resolve_mention(cwd: &std::path::Path, token: &str, ignore: &[String]) -> Option<String> {
    let resolved = crate::tools::sandbox::resolve_in_sandbox(cwd, token).ok()?;
    crate::context::check_denylist(&resolved).ok()?;
    crate::context::check_ignored(&resolved, cwd, ignore).ok()?;
    if !resolved.is_file() {
        return None;
    }
    let bytes = std::fs::read(&resolved).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(crate::context::format_block(token, &text))
}

/// Routes a key to the open confirmation modal, if any (SPEC-UX B3).
/// Returns `true` when a modal was open -- the key is consumed either way,
/// so callers must not process it further.
fn handle_confirm_key(key: &KeyEvent, state: &Arc<Mutex<AppState>>) -> bool {
    let mut st = state.lock().unwrap();
    let Some(pending) = st.confirm.as_mut() else {
        return false;
    };
    let decision = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Yes),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
        KeyCode::Char('a') | KeyCode::Char('A') if pending.option_count() == 3 => {
            Some(Decision::Always)
        }
        KeyCode::Up => {
            pending.selected = pending.selected.saturating_sub(1);
            st.dirty = true;
            None
        }
        KeyCode::Down => {
            pending.selected = (pending.selected + 1).min(pending.option_count() - 1);
            st.dirty = true;
            None
        }
        KeyCode::Enter => {
            if pending.option_count() == 2 {
                Some(if pending.selected == 0 {
                    Decision::Yes
                } else {
                    Decision::No
                })
            } else {
                Some(match pending.selected {
                    0 => Decision::Yes,
                    1 => Decision::Always,
                    _ => Decision::No,
                })
            }
        }
        _ => None,
    };
    if let Some(decision) = decision {
        let pending = st.confirm.take().unwrap();
        let _ = pending.responder.send(decision);
        st.dirty = true;
    }
    true
}

/// Handles input while a turn is in flight: confirmation-modal keys,
/// Ctrl+C/Ctrl+D (cancel), resize, and scroll are meaningful -- none of
/// them need `session`/`permissions`, which the in-flight turn future is
/// currently borrowing exclusively. Returns `true` if the turn should be
/// cancelled.
fn handle_event_while_processing(event: Event, state: &Arc<Mutex<AppState>>) -> bool {
    match event {
        Event::Resize(_, _) => {
            // Inline viewports can desync on resize in some terminals; rebuild
            // the ratatui terminal on the next render tick.
            state.lock().unwrap().request_viewport_reset();
            false
        }
        Event::Key(key) => {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return false;
            }
            if is_ctrl_o(&key) {
                state.lock().unwrap().toggle_thinking();
                return false;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
                state.lock().unwrap().toggle_latest_command_output();
                return false;
            }
            let cancel_key = if key.code == KeyCode::Esc {
                Some("Esc")
            } else if key.modifiers.contains(KeyModifiers::CONTROL)
                && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('d'))
            {
                Some("^C")
            } else {
                None
            };
            if let Some(cancel_key) = cancel_key {
                let mut st = state.lock().unwrap();
                st.processing = false;
                st.status.spinner_line = None;
                if let Some(pending) = st.confirm.take() {
                    let _ = pending.responder.send(Decision::No);
                }
                st.push_warn(format!("{cancel_key} received, cancelling this turn"));
                return true;
            }
            if handle_confirm_key(&key, state) {
                return false;
            }
            false
        }
        _ => false,
    }
}

/// Handles input in the idle phase (no turn in flight). Returns `true` when
/// a line was just submitted and a turn should now be started by the
/// caller (`run_one_turn`).
#[allow(clippy::too_many_arguments)]
fn handle_event(
    event: Event,
    ctx: &mut AppContext,
    state: &Arc<Mutex<AppState>>,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    tools_enabled: bool,
    last_finish_reason: &mut Option<String>,
) -> bool {
    match event {
        Event::Resize(_, _) => {
            state.lock().unwrap().request_viewport_reset();
            false
        }
        Event::Key(key) => handle_key(
            key,
            ctx,
            state,
            session,
            permissions,
            cwd,
            tools_enabled,
            last_finish_reason,
        ),
        Event::Paste(text) => {
            let mut st = state.lock().unwrap();
            restore_pending_submit_as_input(&mut st);
            st.insert_text(&text.replace("\r\n", "\n").replace('\r', "\n"));
            false
        }
        _ => false,
    }
}

fn take_pending_submit(state: &Arc<Mutex<AppState>>) -> Option<String> {
    state.lock().unwrap().pending_submit.take()
}

fn take_pending_compact(state: &Arc<Mutex<AppState>>) -> bool {
    let mut st = state.lock().unwrap();
    let pending = st.pending_compact;
    st.pending_compact = false;
    pending
}

fn restore_pending_submit_as_input(st: &mut AppState) {
    let Some(line) = st.pending_submit.take() else {
        return;
    };
    st.set_input(line);
    st.insert_newline();
}

enum SessionPickerKey {
    NoPicker,
    Consumed,
    Chosen(usize),
}

fn open_session_picker(st: &mut AppState, provider: &str) {
    let sessions = crate::session_store::list(provider);
    if sessions.is_empty() {
        st.push_warn("no saved session to resume for this provider".to_string());
    } else {
        st.session_picker = Some(PendingSessionPicker {
            sessions,
            selected: 0,
        });
        st.dirty = true;
    }
}

fn handle_session_picker_key(key: &KeyEvent, state: &Arc<Mutex<AppState>>) -> SessionPickerKey {
    let mut st = state.lock().unwrap();
    let Some(picker) = st.session_picker.as_mut() else {
        return SessionPickerKey::NoPicker;
    };
    match key.code {
        KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
            st.dirty = true;
            SessionPickerKey::Consumed
        }
        KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(picker.sessions.len().saturating_sub(1));
            st.dirty = true;
            SessionPickerKey::Consumed
        }
        KeyCode::Esc => {
            st.session_picker = None;
            st.dirty = true;
            SessionPickerKey::Consumed
        }
        KeyCode::Enter => {
            let selected = picker.selected_index();
            st.session_picker = None;
            st.dirty = true;
            selected
                .map(SessionPickerKey::Chosen)
                .unwrap_or(SessionPickerKey::Consumed)
        }
        _ => SessionPickerKey::Consumed,
    }
}

fn restore_loaded_session_for_tui(
    state: &Arc<Mutex<AppState>>,
    session: &mut Session,
    loaded: crate::session_store::LoadedSession,
    tools_enabled: bool,
    cwd: &std::path::Path,
    ctx: &AppContext,
) {
    crate::commands::chat::restore_loaded_session(session, loaded, tools_enabled, cwd, ctx);
    if tools_enabled {
        sync_agent_prompt(ctx, session, cwd, state.lock().unwrap().mode);
    }
    let mut st = state.lock().unwrap();
    st.model = session.model.clone();
    st.clear_conversation();
    st.push_warn(format!(
        "resumed session ({} messages, model {})",
        session.history.len(),
        session.model
    ));
    for m in &session.history {
        match m.role.as_str() {
            "user" => {
                if let Some(c) = m.content.as_deref() {
                    st.messages.push(app::MsgBlock::User(c.to_string()));
                }
            }
            "assistant" => {
                if let Some(c) = m.content.as_deref() {
                    st.messages.push(app::MsgBlock::Assistant(c.to_string()));
                }
            }
            _ => {}
        }
    }
    st.dirty = true;
}

/// Returns `true` when a line was just submitted and a turn should now be
/// started by the caller.
#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    ctx: &mut AppContext,
    state: &Arc<Mutex<AppState>>,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    tools_enabled: bool,
    last_finish_reason: &mut Option<String>,
) -> bool {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return false;
    }

    if is_ctrl_o(&key) {
        state.lock().unwrap().toggle_thinking();
        return false;
    }

    if is_shift_tab(&key) {
        cycle_interaction_mode(ctx, state, session, permissions, cwd, tools_enabled);
        return false;
    }

    match handle_session_picker_key(&key, state) {
        SessionPickerKey::NoPicker => {}
        SessionPickerKey::Consumed => return false,
        SessionPickerKey::Chosen(index) => {
            if let Some(loaded) = crate::session_store::load_at(&ctx.cfg.active_provider, index) {
                restore_loaded_session_for_tui(state, session, loaded, tools_enabled, cwd, ctx);
                *last_finish_reason = None;
            }
            return false;
        }
    }

    // A confirmation modal, if open, captures all keys.
    if handle_confirm_key(&key, state) {
        return false;
    }

    // Ctrl+C with empty input while idle exits (SPEC-UX B4); while a turn
    // is in flight, Ctrl+C is handled by `handle_event_while_processing`
    // instead, so this branch only ever sees the idle case.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        let mut st = state.lock().unwrap();
        if st.input.is_empty() {
            st.should_quit = true;
        } else {
            st.set_input(String::new());
        }
        return false;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        state.lock().unwrap().should_quit = true;
        return false;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
        let mut st = state.lock().unwrap();
        st.clear_conversation();
        st.request_terminal_purge();
        session.clear();
        return false;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
        state.lock().unwrap().toggle_latest_command_output();
        return false;
    }

    let alt_or_shift_enter = key.code == KeyCode::Enter
        && (key.modifiers.contains(KeyModifiers::ALT)
            || key.modifiers.contains(KeyModifiers::SHIFT));

    match key.code {
        KeyCode::Enter if alt_or_shift_enter => {
            let mut st = state.lock().unwrap();
            restore_pending_submit_as_input(&mut st);
            st.insert_newline();
            false
        }
        KeyCode::Enter => {
            {
                let mut st = state.lock().unwrap();
                if st.pending_submit.is_some() {
                    restore_pending_submit_as_input(&mut st);
                    st.insert_newline();
                    return false;
                }
            }
            let line = {
                let mut st = state.lock().unwrap();
                if let Some(suggestion) = st.selected_suggestion() {
                    if st.input != suggestion.value {
                        let (new_input, new_cursor) =
                            apply_completion(&st.input, st.cursor, &suggestion);
                        st.set_input(new_input);
                        st.cursor = new_cursor;
                        st.dirty = true;
                        return false;
                    }
                }
                st.dirty = true;
                st.cursor = 0;
                st.suggestion_idx = 0;
                std::mem::take(&mut st.input)
            };
            if line.trim().is_empty() {
                false
            } else {
                state.lock().unwrap().pending_submit = Some(line);
                false
            }
        }
        KeyCode::Backspace => {
            let mut st = state.lock().unwrap();
            restore_pending_submit_as_input(&mut st);
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                st.backspace_word();
            } else {
                st.backspace();
            }
            false
        }
        KeyCode::Delete => {
            state.lock().unwrap().delete();
            false
        }
        KeyCode::Left => {
            let mut st = state.lock().unwrap();
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let chars: Vec<char> = st.input.chars().collect();
                while st.cursor > 0 && chars[st.cursor - 1].is_whitespace() {
                    st.cursor -= 1;
                }
                while st.cursor > 0 && !chars[st.cursor - 1].is_whitespace() {
                    st.cursor -= 1;
                }
            } else {
                st.cursor = st.cursor.saturating_sub(1);
            }
            st.dirty = true;
            false
        }
        KeyCode::Right => {
            let mut st = state.lock().unwrap();
            let chars: Vec<char> = st.input.chars().collect();
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                while st.cursor < chars.len() && !chars[st.cursor].is_whitespace() {
                    st.cursor += 1;
                }
                while st.cursor < chars.len() && chars[st.cursor].is_whitespace() {
                    st.cursor += 1;
                }
            } else {
                st.cursor = (st.cursor + 1).min(chars.len());
            }
            st.dirty = true;
            false
        }
        KeyCode::Tab => {
            let mut st = state.lock().unwrap();
            if let Some(suggestion) = st.selected_suggestion() {
                let (new_input, new_cursor) = apply_completion(&st.input, st.cursor, &suggestion);
                st.set_input(new_input);
                st.cursor = new_cursor;
                st.dirty = true;
            } else {
                st.insert_char('\t');
            }
            false
        }
        KeyCode::Up => {
            let mut st = state.lock().unwrap();
            let suggestions = st.suggestions();
            if !suggestions.is_empty() {
                st.suggestion_idx = st.suggestion_idx.saturating_sub(1);
                st.dirty = true;
            } else if st.input.contains('\n') && st.move_cursor_vertical(-1) {
                // Cursor moved within the multi-line editor.
            } else if !st.input_history.is_empty() {
                let idx = st
                    .history_idx
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(st.input_history.len() - 1);
                st.history_idx = Some(idx);
                let input = st.input_history[idx].clone();
                st.set_input(input);
            }
            false
        }
        KeyCode::Down => {
            let mut st = state.lock().unwrap();
            let suggestions = st.suggestions();
            if !suggestions.is_empty() {
                st.suggestion_idx = (st.suggestion_idx + 1).min(suggestions.len() - 1);
                st.dirty = true;
            } else if st.input.contains('\n') && st.move_cursor_vertical(1) {
                // Cursor moved within the multi-line editor.
            } else if let Some(idx) = st.history_idx {
                if idx + 1 < st.input_history.len() {
                    st.history_idx = Some(idx + 1);
                    let input = st.input_history[idx + 1].clone();
                    st.set_input(input);
                } else {
                    st.history_idx = None;
                    st.set_input(String::new());
                }
            }
            false
        }
        KeyCode::Home => {
            let mut st = state.lock().unwrap();
            let before: String = st.input.chars().take(st.cursor).collect();
            let line_start = before
                .rfind('\n')
                .map(|idx| before[..=idx].chars().count())
                .unwrap_or(0);
            st.cursor = line_start;
            st.dirty = true;
            false
        }
        KeyCode::End => {
            let mut st = state.lock().unwrap();
            let after: String = st.input.chars().skip(st.cursor).collect();
            let to_end = after
                .find('\n')
                .map(|idx| after[..idx].chars().count())
                .unwrap_or_else(|| after.chars().count());
            st.cursor += to_end;
            st.dirty = true;
            false
        }
        KeyCode::Char(c) => {
            let mut st = state.lock().unwrap();
            restore_pending_submit_as_input(&mut st);
            st.insert_char(c);
            false
        }
        _ => false,
    }
}

/// Returns `true` when a turn should now be started (a plain message was
/// submitted, or `/continue` had something to resend).
#[allow(clippy::too_many_arguments)]
fn submit_line(
    line: String,
    ctx: &mut AppContext,
    state: &Arc<Mutex<AppState>>,
    session: &mut Session,
    permissions: &mut Permissions,
    cwd: &std::path::Path,
    tools_enabled: bool,
    last_finish_reason: &mut Option<String>,
) -> bool {
    if let Some(cmd) = session::parse_command(&line) {
        match cmd {
            ReplCommand::Exit => {
                state.lock().unwrap().should_quit = true;
                return false;
            }
            ReplCommand::Clear => {
                session.clear();
                {
                    let mut st = state.lock().unwrap();
                    st.clear_conversation();
                    st.request_terminal_purge();
                }
                // Also drop the saved session so a later `--continue`
                // doesn't resurrect a conversation the user wiped.
                let _ = crate::session_store::clear(&ctx.cfg.active_provider);
                return false;
            }
            ReplCommand::SetModel(m) => {
                let mut st = state.lock().unwrap();
                if m.is_empty() {
                    st.push_warn(format!("current model: {}", session.model));
                } else {
                    session.model = m.clone();
                    crate::session_store::save_model(&ctx.cfg.active_provider, &session.model);
                    st.model = m.clone();
                    st.push_warn(format!("model set to {m}"));
                    let mode = st.mode;
                    drop(st);
                    if tools_enabled {
                        sync_agent_prompt(ctx, session, cwd, mode);
                    }
                }
            }
            ReplCommand::Models => {
                let mut st = state.lock().unwrap();
                if st.models.is_empty() {
                    st.push_warn("no models available; use /model <name>".to_string());
                } else {
                    let models = st.models.clone();
                    st.push_warn(format!("available models ({})", models.len()));
                    for model in models {
                        let marker = if model == session.model {
                            "\u{25cf}"
                        } else {
                            " "
                        };
                        st.push_warn(format!(" {marker} {model}"));
                    }
                }
            }
            ReplCommand::Providers => {
                let mut st = state.lock().unwrap();
                let providers = st.providers.clone();
                for provider in providers {
                    let marker = if provider == ctx.cfg.active_provider {
                        "\u{25cf}"
                    } else {
                        " "
                    };
                    st.push_warn(format!(" {marker} {provider}"));
                }
            }
            ReplCommand::SetProvider(name) => {
                if name.is_empty() {
                    state
                        .lock()
                        .unwrap()
                        .push_warn(format!("current provider: {}", ctx.cfg.active_provider));
                    return false;
                }
                if name == ctx.cfg.active_provider {
                    state
                        .lock()
                        .unwrap()
                        .push_warn(format!("provider already active: {name}"));
                    return false;
                }
                let next = match ctx.switched_provider(&name) {
                    Ok(next) => next,
                    Err(err) => {
                        state
                            .lock()
                            .unwrap()
                            .push_warn(format!("could not switch provider: {err}"));
                        return false;
                    }
                };
                *ctx = next;
                let model = crate::commands::chat::initial_model(ctx);
                *session = Session::new(model.clone(), ctx.cfg.max_context_bytes);
                if tools_enabled {
                    sync_agent_prompt(ctx, session, cwd, state.lock().unwrap().mode);
                }
                let mut st = state.lock().unwrap();
                st.clear_conversation();
                st.provider = ctx.cfg.active_provider.clone();
                st.model = model.clone();
                st.set_models(vec![model]);
                st.push_warn(format!(
                    "switched to provider '{}' and started a new chat",
                    ctx.cfg.active_provider
                ));
            }
            ReplCommand::SetMode(value) => {
                let Some(mode) = InteractionMode::parse(&value) else {
                    state
                        .lock()
                        .unwrap()
                        .push_warn("usage: /mode <default|plan|accept-edits|auto>".to_string());
                    return false;
                };
                {
                    let mut st = state.lock().unwrap();
                    st.mode = mode;
                    st.push_warn(format!(
                        "mode: {} — {}",
                        mode.label(),
                        mode_description(mode)
                    ));
                }
                permissions.set_policy(permission_policy(mode));
                if tools_enabled {
                    sync_agent_prompt(ctx, session, cwd, mode);
                }
            }
            ReplCommand::Cwd => {
                state.lock().unwrap().push_warn(cwd.display().to_string());
            }
            ReplCommand::Help => {
                let mut st = state.lock().unwrap();
                st.push_warn(HELP_COMMANDS.to_string());
                st.push_warn(
                    "keys: Enter=send  Alt/Shift+Enter=newline  arrows=edit/navigate  \
Shift+Tab=cycle mode  Ctrl+O=toggle thinking  Ctrl+E=toggle command output  Esc/Ctrl+C=cancel  Ctrl+L=clear"
                        .to_string(),
                );
            }
            ReplCommand::Tools => {
                let mut st = state.lock().unwrap();
                if !tools_enabled {
                    st.push_warn("tools are disabled (--no-tools)".to_string());
                } else {
                    let always = permissions.always_allowed_tools();
                    let mode = st.mode;
                    for def in tools::all_tool_defs() {
                        let name = def.function.name.as_str();
                        let status = if mode == InteractionMode::Plan
                            && matches!(name, "write_file" | "edit_file")
                        {
                            "not available in PLAN"
                        } else {
                            match (permissions.policy(), name) {
                                (
                                    ApprovalPolicy::Default,
                                    "write_file" | "edit_file" | "run_command",
                                ) => "asks each time",
                                (ApprovalPolicy::AcceptEdits, "write_file" | "edit_file") => {
                                    "auto-approved"
                                }
                                (
                                    ApprovalPolicy::Auto,
                                    "write_file" | "edit_file" | "run_command",
                                ) => "auto-approved",
                                _ if always.contains(&name) => "always-allowed",
                                _ if matches!(name, "write_file" | "edit_file" | "run_command") => {
                                    "asks each time"
                                }
                                _ => "allowed",
                            }
                        };
                        st.push_warn(format!("  {name} ({status})"));
                    }
                    st.push_warn(format!(
                        "  run_command: {} exact command(s) always-allowed",
                        permissions.always_allowed_command_count()
                    ));
                }
                return false;
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
                            Ok(()) => state.lock().unwrap().push_warn("Última mensagem do assistente copiada para a área de transferência".to_string()),
                            Err(err) => state.lock().unwrap().push_warn(format!("Não foi possível copiar para a área de transferência: {err}")),
                        }
                    } else {
                        state.lock().unwrap().push_warn(
                            "Última mensagem do assistente não tem conteúdo".to_string(),
                        );
                    }
                } else {
                    state.lock().unwrap().push_warn(
                        "Nenhuma mensagem do assistente encontrada para copiar".to_string(),
                    );
                }
                return false;
            }
            ReplCommand::Continue => {
                if !tools_enabled {
                    state
                        .lock()
                        .unwrap()
                        .push_warn("/continue only applies to tool-calling turns".to_string());
                    return false;
                } else if last_finish_reason.as_deref() == Some("stop")
                    || last_finish_reason.as_deref() == Some("tool_calls")
                    || last_finish_reason.is_none()
                {
                    state.lock().unwrap().push_warn(
                        "nothing to continue -- the last turn finished normally".to_string(),
                    );
                    return false;
                } else {
                    session.push_user(CONTINUE_MESSAGE.to_string());
                    let mut st = state.lock().unwrap();
                    st.processing = true;
                    return true;
                }
            }
            ReplCommand::Compact => {
                state.lock().unwrap().pending_compact = true;
                return false;
            }
            ReplCommand::Resume(choice) => {
                if let Some(n) = choice {
                    let index = n.saturating_sub(1);
                    if n > 0 {
                        if let Some(loaded) =
                            crate::session_store::load_at(&ctx.cfg.active_provider, index)
                        {
                            restore_loaded_session_for_tui(
                                state,
                                session,
                                loaded,
                                tools_enabled,
                                cwd,
                                ctx,
                            );
                            *last_finish_reason = None;
                            return false;
                        }
                    }
                }
                open_session_picker(&mut state.lock().unwrap(), &ctx.cfg.active_provider);
                return false;
            }
        }
        return false;
    }

    {
        let mut st = state.lock().unwrap();
        st.push_user(line.clone());
        st.history_idx = None;
    }
    let expanded = expand_file_mentions(&line, cwd, &ctx.cfg.ignore, state);
    session.push_user(expanded);

    if tools_enabled {
        state.lock().unwrap().processing = true;
        true
    } else {
        // The no-tools plain-streaming path isn't wired into the TUI yet;
        // `--plain` (or `config ui = "plain"`) gets the full line-mode
        // fallback that does support it.
        state.lock().unwrap().push_warn(
            "note: --no-tools chat isn't supported in the TUI yet; pass --plain for the \
line-mode fallback"
                .to_string(),
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::PendingConfirm;
    use crate::ui::ConfirmRequest;

    #[tokio::test]
    async fn approval_picker_uses_arrows_and_enter() {
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            ".".into(),
        )));
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.lock().unwrap().confirm = Some(PendingConfirm {
            req: ConfirmRequest {
                tool: "edit_file".into(),
                prompt: "edit?".into(),
                details: None,
                diff: None,
                command: None,
            },
            responder: tx,
            printed: true,
            selected: 0,
        });
        handle_confirm_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &state);
        handle_confirm_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state);
        assert_eq!(rx.await.unwrap(), Decision::Always);
    }

    #[tokio::test]
    async fn sensitive_approval_never_offers_always() {
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            ".".into(),
        )));
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.lock().unwrap().confirm = Some(PendingConfirm {
            req: ConfirmRequest {
                tool: "read_file".into(),
                prompt: "include secret?".into(),
                details: None,
                diff: None,
                command: None,
            },
            responder: tx,
            printed: true,
            selected: 0,
        });
        handle_confirm_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &state);
        handle_confirm_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state);
        assert_eq!(rx.await.unwrap(), Decision::No);
    }

    #[test]
    fn ctrl_o_toggles_thinking_while_processing() {
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            ".".into(),
        )));
        assert!(state.lock().unwrap().show_thinking);

        let cancel = handle_event_while_processing(
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            &state,
        );

        assert!(!cancel);
        assert!(!state.lock().unwrap().show_thinking);
    }

    #[test]
    fn compact_cancel_event_clears_processing_and_spinner() {
        let state = Arc::new(Mutex::new(AppState::new(
            "model".to_string(),
            ".".to_string(),
            std::path::PathBuf::from("."),
        )));
        {
            let mut app = state.lock().unwrap();
            app.processing = true;
            app.status.spinner_line = Some("compacting conversation...".to_string());
        }

        let cancel = handle_event_while_processing(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &state,
        );

        assert!(cancel);
        let app = state.lock().unwrap();
        assert!(!app.processing);
        assert!(app.status.spinner_line.is_none());
    }

    #[test]
    fn ctrl_e_toggles_command_output_while_processing() {
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            ".".into(),
        )));
        {
            let mut app = state.lock().unwrap();
            app.processing = true;
            app.push_tool_running("run_command(echo one)".into());
            app.append_command_output(crate::ui::CommandStream::Stdout, "one\n");
            assert!(matches!(
                app.messages.last(),
                Some(MsgBlock::Tool { expanded: true, .. })
            ));
        }

        let cancel = handle_event_while_processing(
            Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            &state,
        );

        assert!(!cancel);
        assert!(matches!(
            state.lock().unwrap().messages.last(),
            Some(MsgBlock::Tool {
                expanded: false,
                ..
            })
        ));
    }

    #[test]
    fn apply_completion_replaces_only_at_token_for_mentions() {
        let suggestion = crate::tui::app::InputSuggestion {
            value: "@README.md".to_string(),
            label: "README.md".to_string(),
            description: "arquivo".to_string(),
            replace_range: Some((8, 13)),
        };
        // "explain @READ" with the @READ token at chars [8,13).
        let (new_input, new_cursor) = apply_completion("explain @READ", 13, &suggestion);
        assert_eq!(new_input, "explain @README.md");
        assert_eq!(new_cursor, "explain @README.md".len());
    }

    #[test]
    fn apply_completion_replaces_whole_line_for_slash_commands() {
        let suggestion = crate::tui::app::InputSuggestion {
            value: "/model meta/llama <name>".to_string(),
            label: "/model meta/llama".to_string(),
            description: "modelo NIM".to_string(),
            replace_range: None,
        };
        let (new_input, new_cursor) = apply_completion("/mod", 4, &suggestion);
        assert_eq!(new_input, "/model meta/llama ");
        assert_eq!(new_cursor, 0);
    }

    #[test]
    fn multiline_paste_text_is_normalized_before_insertion() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.insert_text(
            &"linha 1\r\nlinha 2\nlinha 3\rlinha 4"
                .replace("\r\n", "\n")
                .replace('\r', "\n"),
        );
        assert_eq!(app.input, "linha 1\nlinha 2\nlinha 3\nlinha 4");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    #[test]
    fn expand_file_mentions_inlines_readable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            dir.path().to_path_buf(),
        )));
        let expanded = expand_file_mentions("explain @README.md please", dir.path(), &[], &state);
        assert!(expanded.contains("File: README.md"));
        assert!(expanded.contains("# hi"));
        assert!(expanded.contains("please"));
        // The literal `@README.md` is gone (replaced by the fenced block).
        assert!(!expanded.contains("@README.md"));
        // A warn note was pushed.
        assert!(state
            .lock()
            .unwrap()
            .messages
            .iter()
            .any(|m| matches!(m, crate::tui::app::MsgBlock::Warn(t) if t.contains("inlined"))));
    }

    #[test]
    fn expand_file_mentions_leaves_unresolvable_tokens_intact() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            dir.path().to_path_buf(),
        )));
        let expanded = expand_file_mentions("see @does-not-exist.txt", dir.path(), &[], &state);
        assert_eq!(expanded, "see @does-not-exist.txt");
    }

    #[test]
    fn expand_file_mentions_ignores_email_like_at() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            dir.path().to_path_buf(),
        )));
        let expanded = expand_file_mentions("contact me@host.com", dir.path(), &[], &state);
        assert_eq!(expanded, "contact me@host.com");
    }
}
