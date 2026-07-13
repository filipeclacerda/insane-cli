//! Shared TUI render state (SPEC-UX B2/B3): the conversation, the input
//! box, scroll position, status-line info, and any pending confirmation
//! modal. Wrapped in `Arc<Mutex<..>>` so `TuiUi` (running inside the
//! agentic turn's future) and the render loop (polling the same task via
//! `tokio::select!`) can share it without channels.

use std::path::PathBuf;

use crate::client::Usage;
use crate::context;
use crate::session::SLASH_COMMANDS;
use crate::session_store::SessionSummary;
use crate::ui::{CommandStream, ConfirmRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionMode {
    Default,
    Plan,
    Auto,
    AcceptEdits,
}

impl InteractionMode {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Plan,
            Self::Plan => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
            Self::Auto => Self::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Plan => "PLAN",
            Self::Auto => "AUTO",
            Self::AcceptEdits => "ACCEPT EDITS",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "auto" => Some(Self::Auto),
            "accept-edits" | "accept_edits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            _ => None,
        }
    }

    pub fn system_instruction(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Plan => {
                "\n\nInteraction mode: PLAN. You may inspect the project with read/list/search \
tools and run diagnostic commands when useful, but do not edit files or write new files. Respond \
with a concise plan, assumptions, risks, and recommended next steps. If implementation is needed, \
wait for the user to explicitly ask you to execute it."
            }
            Self::Auto => {
                "\n\nInteraction mode: AUTO. Execute tool calls directly without waiting for user \
approval prompts, including file edits and shell commands. Keep working until the request is complete."
            }
            Self::AcceptEdits => {
                "\n\nInteraction mode: ACCEPT EDITS. File edits are pre-approved for this session. \
Shell commands still require explicit user confirmation."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputLine {
    pub stream: CommandStream,
    pub text: String,
}

/// One entry in the transcript.
#[derive(Debug, Clone)]
pub enum MsgBlock {
    User(String),
    /// Assistant text; appended to incrementally while streaming.
    Assistant(String),
    /// Provider-supplied model reasoning/thinking; shown by default and
    /// replaced by a compact placeholder when toggled off.
    Thinking(String),
    /// A tool call trace/result, rendered as a Claude Code-style bullet.
    Tool {
        status: ToolStatus,
        call: String,
        result: Option<String>,
        output: Vec<ToolOutputLine>,
        output_truncated: bool,
        expanded: bool,
    },
    /// A warning/notice (finish_reason, rate limit, recovered text call).
    Warn(String),
    /// The end-of-turn metrics line.
    TurnSummary(String),
}

/// A confirmation waiting on the user, plus how to send the answer back to
/// the (suspended) tool call that's `.await`ing it.
pub struct PendingConfirm {
    pub req: ConfirmRequest,
    pub responder: tokio::sync::oneshot::Sender<crate::ui::Decision>,
    /// The prompt/details/diff are printed into native scrollback once.
    pub printed: bool,
    /// Highlighted decision in the bottom approval picker.
    pub selected: usize,
}

impl PendingConfirm {
    pub fn option_count(&self) -> usize {
        if self.req.tool == "read_file" || self.req.tool == "search_files" {
            2
        } else {
            3
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingSessionPicker {
    pub sessions: Vec<SessionSummary>,
    /// Highlighted saved session in the bottom picker.
    pub selected: usize,
}

impl PendingSessionPicker {
    pub fn selected_index(&self) -> Option<usize> {
        self.sessions
            .get(self.selected)
            .map(|session| session.index)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub rate_used: Option<u32>,
    pub rate_capacity: Option<u32>,
    pub min_interval_ms: u64,
    pub next_request_ms: u64,
    pub tokens_this_turn: Option<u32>,
    pub tokens_total: u64,
    pub spinner_line: Option<String>,
}

pub struct AppState {
    pub model: String,
    pub models: Vec<String>,
    pub provider: String,
    pub providers: Vec<String>,
    pub mode: InteractionMode,
    pub cwd_display: String,
    /// The process cwd, used for `@file` mention suggestions and for
    /// expanding `@path` tokens into inline file content on submit.
    pub cwd: PathBuf,
    /// Extra ignore globs from config, applied to `@file` suggestions so the
    /// palette never offers gitignored/denylisted paths.
    pub ignore: Vec<String>,
    pub messages: Vec<MsgBlock>,
    pub input: String,
    /// Cursor position within `input`, in chars.
    pub cursor: usize,
    /// Enter-to-submit is delayed by one render tick so terminals that emit
    /// pasted newlines as Enter key events can still keep multiline paste intact.
    pub pending_submit: Option<String>,
    /// Set by `/compact`; consumed by the main TUI loop to run the
    /// async compaction request outside normal tool-calling turns.
    pub pending_compact: bool,
    pub input_history: Vec<String>,
    /// Index into `input_history` while browsing with Up/Down; `None` means
    /// "not currently browsing" (editing the live input).
    pub history_idx: Option<usize>,
    /// Currently highlighted entry in the live slash-command palette.
    pub suggestion_idx: usize,
    /// Blocks before this index have already been pushed into native scrollback.
    pub committed_blocks: usize,
    /// Bytes of the current live assistant block already pushed into scrollback.
    pub live_committed_chars: usize,
    /// The inline viewport should be rebuilt on the next render tick.
    pub viewport_reset_requested: bool,
    /// The terminal scrollback/screen should be purged on the next render tick.
    pub terminal_purge_requested: bool,
    pub show_thinking: bool,
    pub processing: bool,
    pub status: StatusInfo,
    pub confirm: Option<PendingConfirm>,
    pub session_picker: Option<PendingSessionPicker>,
    /// Set whenever render state changes; the main loop clears it after a
    /// redraw (SPEC-UX B3: throttled ~30fps rendering).
    pub dirty: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(model: String, cwd_display: String, cwd: PathBuf) -> Self {
        AppState {
            model,
            models: Vec::new(),
            provider: "nvidia".to_string(),
            providers: Vec::new(),
            mode: InteractionMode::Default,
            cwd_display,
            cwd,
            ignore: Vec::new(),
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            pending_submit: None,
            pending_compact: false,
            input_history: Vec::new(),
            history_idx: None,
            suggestion_idx: 0,
            committed_blocks: 0,
            live_committed_chars: 0,
            viewport_reset_requested: false,
            terminal_purge_requested: false,
            show_thinking: true,
            processing: false,
            status: StatusInfo::default(),
            confirm: None,
            session_picker: None,
            dirty: true,
            should_quit: false,
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.input_history.push(text.clone());
        self.messages.push(MsgBlock::User(text));
        self.dirty = true;
    }

    pub fn push_assistant_chunk(&mut self, chunk: &str) {
        let mut append_idx = None;
        for (idx, msg) in self.messages.iter().enumerate().rev() {
            match msg {
                MsgBlock::Assistant(_) => {
                    append_idx = Some(idx);
                    break;
                }
                MsgBlock::Thinking(_) => {}
                MsgBlock::User(_)
                | MsgBlock::Tool { .. }
                | MsgBlock::Warn(_)
                | MsgBlock::TurnSummary(_) => break,
            }
        }

        if let Some(idx) = append_idx {
            if let MsgBlock::Assistant(s) = &mut self.messages[idx] {
                s.push_str(chunk);
            }
        } else {
            self.messages.push(MsgBlock::Assistant(chunk.to_string()));
        }
        self.dirty = true;
    }

    pub fn push_thinking_chunk(&mut self, chunk: &str) {
        if let Some(MsgBlock::Thinking(s)) = self.messages.last_mut() {
            s.push_str(chunk);
        } else {
            self.messages.push(MsgBlock::Thinking(chunk.to_string()));
        }
        self.dirty = true;
    }

    pub fn toggle_thinking(&mut self) -> bool {
        self.show_thinking = !self.show_thinking;
        self.dirty = true;
        self.show_thinking
    }

    pub fn discard_last_assistant_message(&mut self) {
        let last_idx = self
            .messages
            .iter()
            .rposition(|msg| matches!(msg, MsgBlock::Assistant(text) if !text.trim().is_empty()));
        if let Some(idx) = last_idx {
            if self.live_committed_chars > 0 {
                if let MsgBlock::Assistant(text) = &mut self.messages[idx] {
                    let cut = clamp_to_char_boundary(text, self.live_committed_chars);
                    text.truncate(cut);
                }
                self.messages.push(MsgBlock::Warn(
                    "assistant text already printed to scrollback; only the live tail was removed"
                        .to_string(),
                ));
                self.dirty = true;
                return;
            }
            self.messages.remove(idx);
            self.dirty = true;
        }
    }

    pub fn replace_last_assistant_message(&mut self, text: &str) {
        let last_idx = self
            .messages
            .iter()
            .rposition(|msg| matches!(msg, MsgBlock::Assistant(existing) if !existing.is_empty()));
        if self.live_committed_chars > 0 {
            if let Some(idx) = last_idx {
                if let MsgBlock::Assistant(existing) = &mut self.messages[idx] {
                    let cut = clamp_to_char_boundary(existing, self.live_committed_chars);
                    let replacement_tail = text.get(cut..).unwrap_or("");
                    existing.truncate(cut);
                    existing.push_str(replacement_tail);
                    self.messages.push(MsgBlock::Warn(
                        "assistant text already printed to scrollback; only the live tail was replaced"
                            .to_string(),
                    ));
                    self.dirty = true;
                    return;
                }
            }
        }
        match (last_idx, text.trim().is_empty()) {
            (Some(idx), true) => {
                self.messages.remove(idx);
            }
            (Some(idx), false) => {
                self.messages[idx] = MsgBlock::Assistant(text.to_string());
            }
            (None, false) => {
                self.messages.push(MsgBlock::Assistant(text.to_string()));
            }
            (None, true) => {}
        }
        self.dirty = true;
    }

    /// Ensures the *next* streamed chunk starts a fresh assistant message
    /// rather than appending to a previous one (called at the start of a
    /// round, so tool-call rounds don't get glued to the following text).
    pub fn start_new_assistant_message_boundary(&mut self) {
        if !matches!(self.messages.last(), Some(MsgBlock::Assistant(s)) if s.is_empty()) {
            self.messages.push(MsgBlock::Assistant(String::new()));
        }
    }

    pub fn push_tool_running(&mut self, call: String) {
        self.messages.push(MsgBlock::Tool {
            status: ToolStatus::Running,
            call,
            result: None,
            output: Vec::new(),
            output_truncated: false,
            expanded: true,
        });
        self.dirty = true;
    }

    pub fn append_command_output(&mut self, stream: CommandStream, chunk: &str) {
        const MAX_OUTPUT_LINES: usize = 200;
        const MAX_OUTPUT_BYTES: usize = 16 * 1024;
        if let Some(MsgBlock::Tool {
            status: ToolStatus::Running,
            output,
            output_truncated,
            ..
        }) = self.messages.iter_mut().rev().find(|msg| {
            matches!(msg, MsgBlock::Tool { status: ToolStatus::Running, call, .. } if call.starts_with("run_command("))
        }) {
            for text in chunk.lines() {
                output.push(ToolOutputLine { stream, text: text.to_string() });
            }
            while output.len() > MAX_OUTPUT_LINES || output.iter().map(|line| line.text.len()).sum::<usize>() > MAX_OUTPUT_BYTES {
                output.remove(0);
                *output_truncated = true;
            }
            self.dirty = true;
        }
    }

    pub fn toggle_latest_command_output(&mut self) -> bool {
        if let Some(MsgBlock::Tool {
            output, expanded, ..
        }) = self.messages.iter_mut().rev().find(
            |msg| matches!(msg, MsgBlock::Tool { call, .. } if call.starts_with("run_command(")),
        ) {
            if !output.is_empty() {
                *expanded = !*expanded;
                self.dirty = true;
                return true;
            }
        }
        false
    }

    pub fn finish_tool(&mut self, ok: bool, call: String, result: String) {
        let running = self.messages.iter().rposition(|msg| {
            matches!(
                msg,
                MsgBlock::Tool {
                    status: ToolStatus::Running,
                    ..
                }
            )
        });
        if let Some(idx) = running {
            if let MsgBlock::Tool {
                status,
                call: existing_call,
                result: existing_result,
                expanded,
                ..
            } = &mut self.messages[idx]
            {
                *status = if ok {
                    ToolStatus::Success
                } else {
                    ToolStatus::Failed
                };
                *existing_call = call;
                *existing_result = Some(result);
                *expanded = !ok;
            }
        } else {
            self.messages.push(MsgBlock::Tool {
                status: if ok {
                    ToolStatus::Success
                } else {
                    ToolStatus::Failed
                },
                call,
                result: Some(result),
                output: Vec::new(),
                output_truncated: false,
                expanded: false,
            });
        }
        self.dirty = true;
    }

    pub fn set_models(&mut self, mut models: Vec<String>) {
        if !models.iter().any(|m| m == &self.model) {
            models.push(self.model.clone());
        }
        models.sort();
        models.dedup();
        self.models = models;
        self.suggestion_idx = 0;
        self.dirty = true;
    }

    pub fn suggestions(&self) -> Vec<InputSuggestion> {
        // `@file` mention: detected anywhere in the input at the cursor
        // position (e.g. "explain @READ" with the cursor right after READ).
        // Mirrors the slash palette's UX: typing `@` opens the list, further
        // chars filter it.
        if let Some(suggestions) = self.file_mention_suggestions() {
            return suggestions;
        }

        if self.input.contains('\n') || !self.input.starts_with('/') {
            return Vec::new();
        }

        if let Some(query) = self.input.strip_prefix("/model ") {
            let query = query.trim().to_ascii_lowercase();
            return self
                .models
                .iter()
                .filter(|model| model.to_ascii_lowercase().contains(&query))
                .map(|model| InputSuggestion {
                    value: format!("/model {model}"),
                    label: model.clone(),
                    description: "modelo NIM".to_string(),
                    replace_range: None,
                })
                .collect();
        }

        if let Some(query) = self.input.strip_prefix("/provider ") {
            let query = query.trim().to_ascii_lowercase();
            return self
                .providers
                .iter()
                .filter(|provider| provider.to_ascii_lowercase().contains(&query))
                .map(|provider| InputSuggestion {
                    value: format!("/provider {provider}"),
                    label: provider.clone(),
                    description: "provider profile".to_string(),
                    replace_range: None,
                })
                .collect();
        }

        if let Some(query) = self.input.strip_prefix("/mode ") {
            let query = query.trim().to_ascii_lowercase();
            return [
                ("default", "ask before edits and commands"),
                ("plan", "inspect and plan; no edits"),
                ("accept-edits", "allow edits; ask before commands"),
                ("auto", "run edits and commands without prompts"),
            ]
            .into_iter()
            .filter(|(mode, _)| mode.contains(&query))
            .map(|(mode, description)| InputSuggestion {
                value: format!("/mode {mode}"),
                label: mode.to_string(),
                description: description.to_string(),
                replace_range: None,
            })
            .collect();
        }

        if self.input.contains(' ') {
            return Vec::new();
        }

        SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.name.starts_with(&self.input))
            .map(|cmd| InputSuggestion {
                value: cmd.usage.to_string(),
                label: cmd.usage.to_string(),
                description: cmd.description.to_string(),
                replace_range: None,
            })
            .collect()
    }

    /// Detects a `@<query>` token immediately left of the cursor and returns
    /// matching file/directory paths under `cwd` (respecting `.gitignore`,
    /// the config `ignore` list, and the fixed high-risk filename denylist).
    /// Returns `None` when the cursor isn't sitting right after such a token
    /// (so the caller falls back to slash-command suggestions).
    fn file_mention_suggestions(&self) -> Option<Vec<InputSuggestion>> {
        // Find the `@` that opens the mention: scan left from the cursor
        // over non-whitespace chars until we hit `@` or a whitespace char.
        let chars: Vec<char> = self.input.chars().collect();
        if self.cursor > chars.len() {
            return None;
        }
        let mut i = self.cursor;
        // The token must be non-empty only if the user typed something after
        // `@`; a bare `@` right at the cursor still opens the palette.
        while i > 0 {
            let prev = chars[i - 1];
            if prev.is_whitespace() {
                return None;
            }
            if prev == '@' {
                break;
            }
            i -= 1;
        }
        // `i` now points just after the `@`. Ensure the char at i-1 is `@`
        // and that it's itself preceded by start-of-input or whitespace (so
        // an email-like `a@b` mid-word doesn't trigger).
        if i == 0 || chars[i - 1] != '@' {
            return None;
        }
        let at_index = i - 1;
        if at_index > 0 && !chars[at_index - 1].is_whitespace() {
            return None;
        }
        let query: String = chars[i..self.cursor].iter().collect();
        let matches = self.list_files_for_mention(&query);
        let replace_range = (at_index, self.cursor);
        let suggestions = matches
            .into_iter()
            .map(|path| {
                let is_dir = path.ends_with('/');
                let description = if is_dir {
                    "diretório".to_string()
                } else {
                    "arquivo".to_string()
                };
                InputSuggestion {
                    // `value` is the literal text to insert in place of the
                    // `@query` token (the `@` is kept, the path follows).
                    value: format!("@{path}"),
                    label: path,
                    description,
                    replace_range: Some(replace_range),
                }
            })
            .collect();
        Some(suggestions)
    }

    /// Lists non-ignored, non-denylisted entries under `cwd` whose relative
    /// path starts with `query` (case-insensitive on the last path segment),
    /// capped to a small number for the palette. Directories get a trailing
    /// `/`.
    fn list_files_for_mention(&self, query: &str) -> Vec<String> {
        const MAX_MENTIONS: usize = 50;
        let query_lower = query.to_ascii_lowercase();
        let mut entries: Vec<String> = Vec::new();
        let walker = ignore::WalkBuilder::new(&self.cwd).build();
        for result in walker {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path == self.cwd {
                continue;
            }
            if context::check_denylist(path).is_err() {
                continue;
            }
            if context::check_ignored(path, &self.cwd, &self.ignore).is_err() {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let mut rel = path
                .strip_prefix(&self.cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if is_dir {
                rel.push('/');
            }
            // Filter: the query matches a path-segment prefix (so `READ`
            // matches `README.md` and `docs/REPORT.md` matches `docs/RE`).
            let last_segment = rel.rsplit('/').next().unwrap_or(&rel).to_ascii_lowercase();
            if !query_lower.is_empty() && !last_segment.starts_with(&query_lower) {
                // Also allow matching against the full path prefix for
                // nested queries like `src/app`.
                if !rel.to_ascii_lowercase().starts_with(&query_lower) {
                    continue;
                }
            }
            entries.push(rel);
            if entries.len() >= MAX_MENTIONS {
                break;
            }
        }
        entries.sort();
        entries
    }

    pub fn selected_suggestion(&self) -> Option<InputSuggestion> {
        let suggestions = self.suggestions();
        suggestions
            .get(self.suggestion_idx.min(suggestions.len().saturating_sub(1)))
            .cloned()
    }

    pub fn reset_suggestion(&mut self) {
        self.suggestion_idx = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = byte_index_for_char(&self.input, self.cursor);
        self.input.insert(byte_idx, c);
        self.cursor += 1;
        self.history_idx = None;
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let byte_idx = byte_index_for_char(&self.input, self.cursor);
        self.input.insert_str(byte_idx, text);
        self.cursor += text.chars().count();
        self.history_idx = None;
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = byte_index_for_char(&self.input, self.cursor);
        let start = byte_index_for_char(&self.input, self.cursor - 1);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn backspace_word(&mut self) {
        if self.cursor == 0 {
            return;
        }

        let chars: Vec<char> = self.input.chars().collect();
        let end_cursor = self.cursor;
        let mut start_cursor = end_cursor;
        while start_cursor > 0 && chars[start_cursor - 1].is_whitespace() {
            start_cursor -= 1;
        }
        while start_cursor > 0 && !chars[start_cursor - 1].is_whitespace() {
            start_cursor -= 1;
        }

        let start = byte_index_for_char(&self.input, start_cursor);
        let end = byte_index_for_char(&self.input, end_cursor);
        self.input.replace_range(start..end, "");
        self.cursor = start_cursor;
        self.history_idx = None;
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        let chars = self.input.chars().count();
        if self.cursor >= chars {
            return;
        }
        let start = byte_index_for_char(&self.input, self.cursor);
        let end = byte_index_for_char(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.cursor = self.input.chars().count();
        self.reset_suggestion();
        self.dirty = true;
    }

    pub fn move_cursor_vertical(&mut self, direction: isize) -> bool {
        let before: String = self.input.chars().take(self.cursor).collect();
        let current_col = before.chars().rev().take_while(|c| *c != '\n').count();
        let current_line = before.chars().filter(|c| *c == '\n').count();
        let lines: Vec<&str> = self.input.split('\n').collect();
        let target_line = current_line as isize + direction;
        if target_line < 0 || target_line >= lines.len() as isize {
            return false;
        }
        let prefix: usize = lines
            .iter()
            .take(target_line as usize)
            .map(|line| line.chars().count() + 1)
            .sum();
        self.cursor = prefix + current_col.min(lines[target_line as usize].chars().count());
        self.dirty = true;
        true
    }

    pub fn push_warn(&mut self, msg: String) {
        self.messages.push(MsgBlock::Warn(msg));
        self.dirty = true;
    }

    pub fn push_turn_summary(&mut self, line: String) {
        self.messages.push(MsgBlock::TurnSummary(line));
        self.dirty = true;
    }

    pub fn clear_conversation(&mut self) {
        self.messages.clear();
        self.committed_blocks = 0;
        self.live_committed_chars = 0;
        self.status.tokens_this_turn = None;
        self.status.tokens_total = 0;
        self.dirty = true;
    }

    pub fn request_viewport_reset(&mut self) {
        self.viewport_reset_requested = true;
        self.dirty = true;
    }

    pub fn request_terminal_purge(&mut self) {
        self.terminal_purge_requested = true;
        self.request_viewport_reset();
    }

    pub fn commit_frontier(&self) -> usize {
        if !self.processing {
            return self.messages.len();
        }

        let mut frontier = self.messages.len();
        while frontier > 0 {
            match &self.messages[frontier - 1] {
                MsgBlock::Assistant(_) | MsgBlock::Thinking(_) => {
                    frontier -= 1;
                }
                MsgBlock::Tool {
                    status: ToolStatus::Running,
                    ..
                } => {
                    return frontier - 1;
                }
                _ => break,
            }
        }
        frontier
    }

    pub fn set_usage(&mut self, usage: Option<&Usage>) -> u64 {
        if let Some(usage) = usage {
            self.status.tokens_this_turn = Some(usage.total_tokens);
            self.status.tokens_total = self
                .status
                .tokens_total
                .saturating_add(usage.total_tokens as u64);
        } else {
            self.status.tokens_this_turn = None;
        }
        self.dirty = true;
        self.status.tokens_total
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSuggestion {
    pub value: String,
    pub label: String,
    pub description: String,
    /// When set, completing this suggestion replaces only the char range
    /// `[start, end)` within `input` (used by `@file` mentions, which can
    /// appear mid-line) instead of the whole input line (slash commands).
    pub replace_range: Option<(usize, usize)>,
}

fn byte_index_for_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn clamp_to_char_boundary(text: &str, mut byte_index: usize) -> usize {
    byte_index = byte_index.min(text.len());
    while byte_index > 0 && !text.is_char_boundary(byte_index) {
        byte_index -= 1;
    }
    byte_index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_input_at_unicode_cursor() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("olá".into());
        app.cursor = 2;
        app.insert_char('X');
        assert_eq!(app.input, "olXá");
        app.backspace();
        assert_eq!(app.input, "olá");
    }

    #[test]
    fn backspace_word_deletes_previous_word_and_padding() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("alpha beta   ".into());
        app.backspace_word();
        assert_eq!(app.input, "alpha ");
        assert_eq!(app.cursor, "alpha ".chars().count());
    }

    #[test]
    fn backspace_word_deletes_at_cursor_across_multiline_input() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("one two\nthree four".into());
        app.cursor = "one two\nthree".chars().count();
        app.backspace_word();
        assert_eq!(app.input, "one two\n four");
        assert_eq!(app.cursor, "one two\n".chars().count());
    }

    #[test]
    fn discards_last_non_empty_assistant_message() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_assistant_chunk("Vou explorar mais arquivos.");
        app.start_new_assistant_message_boundary();
        app.discard_last_assistant_message();

        assert!(!app
            .messages
            .iter()
            .any(|msg| matches!(msg, MsgBlock::Assistant(text) if text.contains("explorar"))));
        assert!(matches!(app.messages.last(), Some(MsgBlock::Assistant(text)) if text.is_empty()));
    }

    #[test]
    fn commit_frontier_keeps_live_stream_tail_while_processing() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_user("hi".into());
        app.push_assistant_chunk("live");
        app.processing = true;

        assert_eq!(app.commit_frontier(), 1);
        app.processing = false;
        assert_eq!(app.commit_frontier(), 2);
    }

    #[test]
    fn commit_frontier_stops_before_running_tool() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_user("hi".into());
        app.push_tool_running("read_file(src/lib.rs)".into());
        app.processing = true;

        assert_eq!(app.commit_frontier(), 1);
    }

    #[test]
    fn replaces_last_assistant_message_with_cleaned_text() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_assistant_chunk("Vou ler.\n{\"name\":\"read_file\"}");
        app.start_new_assistant_message_boundary();
        app.replace_last_assistant_message("Vou ler.");

        assert!(app
            .messages
            .iter()
            .any(|msg| matches!(msg, MsgBlock::Assistant(text) if text == "Vou ler.")));
        assert!(!app
            .messages
            .iter()
            .any(|msg| matches!(msg, MsgBlock::Assistant(text) if text.contains("read_file"))));
    }

    #[test]
    fn replace_after_partial_live_commit_keeps_committed_prefix() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_assistant_chunk("prefix\nbad tool json");
        app.live_committed_chars = "prefix\n".len();

        app.replace_last_assistant_message("prefix\nclean tail");

        assert!(matches!(
            app.messages.first(),
            Some(MsgBlock::Assistant(text)) if text == "prefix\nclean tail"
        ));
        assert!(app
            .messages
            .iter()
            .any(|msg| matches!(msg, MsgBlock::Warn(text) if text.contains("live tail"))));
    }

    #[test]
    fn thinking_chunks_accumulate_and_visibility_toggles() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        assert!(app.show_thinking);

        app.push_thinking_chunk("plan ");
        app.push_thinking_chunk("step");

        assert!(matches!(
            app.messages.last(),
            Some(MsgBlock::Thinking(text)) if text == "plan step"
        ));
        assert!(!app.toggle_thinking());
        assert!(app.toggle_thinking());
    }

    #[test]
    fn assistant_chunks_continue_after_interleaved_thinking() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_assistant_chunk("Olá ");
        app.push_thinking_chunk("plan ");
        app.push_assistant_chunk("mundo");

        let assistants: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|msg| match msg {
                MsgBlock::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["Olá mundo"]);
    }

    #[test]
    fn assistant_chunks_do_not_cross_user_boundary() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_assistant_chunk("resposta antiga");
        app.push_user("nova pergunta".to_string());
        app.push_assistant_chunk("resposta nova");

        let assistants: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|msg| match msg {
                MsgBlock::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["resposta antiga", "resposta nova"]);
    }

    #[test]
    fn slash_palette_switches_to_model_results() {
        let mut app = AppState::new("current".into(), ".".into(), ".".into());
        app.set_models(vec!["meta/llama".into(), "mistral/nemo".into()]);
        app.set_input("/model llama".into());
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/model meta/llama");
    }

    #[test]
    fn slash_palette_suggests_interaction_modes() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("/mode acc".into());
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/mode accept-edits");
    }

    #[test]
    fn slash_palette_suggests_plan_mode() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("/mode pl".into());
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/mode plan");
    }

    #[test]
    fn interaction_mode_parse_and_cycle_include_plan() {
        assert_eq!(InteractionMode::parse("plan"), Some(InteractionMode::Plan));
        assert_eq!(InteractionMode::Default.next(), InteractionMode::Plan);
        assert_eq!(InteractionMode::Plan.next(), InteractionMode::AcceptEdits);
    }

    #[test]
    fn vertical_cursor_navigation_preserves_column() {
        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.set_input("abcd\nxy".into());
        assert!(app.move_cursor_vertical(-1));
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn at_mention_opens_palette_at_cursor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "x").unwrap();
        let mut app = AppState::new("m".into(), ".".into(), dir.path().to_path_buf());
        app.set_input("explain @READ".into());
        // cursor lands at end of input after set_input (13 chars).
        let suggestions = app.suggestions();
        assert!(suggestions.iter().any(|s| s.label == "README.md"));
        // Each suggestion carries a replace_range covering the `@READ` token.
        let first = suggestions.iter().find(|s| s.label == "README.md").unwrap();
        assert_eq!(first.value, "@README.md");
        assert_eq!(first.replace_range, Some((8, 13)));
    }

    #[test]
    fn at_mention_requires_at_preceded_by_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let mut app = AppState::new("m".into(), ".".into(), dir.path().to_path_buf());
        // `a@READ` mid-word must NOT trigger the file palette.
        app.set_input("contact a@READ".into());
        let suggestions = app.suggestions();
        assert!(suggestions.is_empty());
    }

    #[test]
    fn command_output_appends_only_to_latest_running_run_command() {
        use crate::ui::CommandStream;

        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_tool_running("run_command(first)".into());
        app.push_tool_running("read_file(src/lib.rs)".into());
        app.append_command_output(CommandStream::Stdout, "one\n");

        assert!(matches!(
            &app.messages[0],
            MsgBlock::Tool { output, .. }
                if output == &vec![ToolOutputLine { stream: CommandStream::Stdout, text: "one".into() }]
        ));
        assert!(matches!(&app.messages[1], MsgBlock::Tool { output, .. } if output.is_empty()));
    }

    #[test]
    fn command_output_is_bounded_to_a_tail_with_one_truncation_marker() {
        use crate::ui::CommandStream;

        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_tool_running("run_command(noisy)".into());
        for index in 0..250 {
            app.append_command_output(CommandStream::Stderr, &format!("line-{index}\n"));
        }

        let MsgBlock::Tool {
            output,
            output_truncated,
            ..
        } = &app.messages[0]
        else {
            panic!("missing tool")
        };
        assert!(*output_truncated);
        assert!(output.len() <= 200);
        assert_eq!(output.first().unwrap().text, "line-50");
        assert_eq!(output.last().unwrap().text, "line-249");
    }

    #[test]
    fn command_output_survives_finish_and_failure_stays_expanded() {
        use crate::ui::CommandStream;

        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_tool_running("run_command(echo one)".into());
        app.append_command_output(CommandStream::Stdout, "one\n");
        app.finish_tool(false, "run_command(echo one)".into(), "failed".into());

        assert!(matches!(
            &app.messages[0],
            MsgBlock::Tool { output, result: Some(result), expanded: true, .. }
                if output == &vec![ToolOutputLine { stream: CommandStream::Stdout, text: "one".into() }]
                    && result == "failed"
        ));
    }

    #[test]
    fn latest_command_output_can_be_toggled_after_finish() {
        use crate::ui::CommandStream;

        let mut app = AppState::new("m".into(), ".".into(), ".".into());
        app.push_tool_running("run_command(echo one)".into());
        app.append_command_output(CommandStream::Stdout, "one\n");
        app.finish_tool(true, "run_command(echo one)".into(), "done".into());

        assert!(app.toggle_latest_command_output());
        assert!(matches!(
            &app.messages[0],
            MsgBlock::Tool { expanded: true, .. }
        ));
    }

    #[test]
    fn at_mention_respects_gitignore_and_denylist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "x").unwrap();
        std::fs::write(dir.path().join("id_rsa"), "x").unwrap();
        let mut app = AppState::new("m".into(), ".".into(), dir.path().to_path_buf());
        app.set_input("@".into());
        let suggestions = app.suggestions();
        let labels: Vec<&str> = suggestions.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"kept.rs"));
        assert!(!labels.contains(&"ignored.txt"));
        assert!(!labels.contains(&"id_rsa"));
    }
}
