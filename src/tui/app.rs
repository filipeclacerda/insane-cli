//! Shared TUI render state (SPEC-UX B2/B3): the conversation, the input
//! box, scroll position, status-line info, and any pending confirmation
//! modal. Wrapped in `Arc<Mutex<..>>` so `TuiUi` (running inside the
//! agentic turn's future) and the render loop (polling the same task via
//! `tokio::select!`) can share it without channels.

use crate::client::Usage;
use crate::session::SLASH_COMMANDS;
use crate::ui::ConfirmRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionMode {
    Auto,
    Plan,
    AcceptEdits,
}

impl InteractionMode {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Plan,
            Self::Plan => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Plan => "PLAN",
            Self::AcceptEdits => "ACCEPT EDITS",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "plan" => Some(Self::Plan),
            "accept-edits" | "accept_edits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            _ => None,
        }
    }

    pub fn system_instruction(self) -> &'static str {
        match self {
            Self::Auto => "",
            Self::Plan => {
                "\n\nInteraction mode: PLAN. Explore with read-only tools and produce a concrete \
implementation plan. Do not call write_file, edit_file, or run_command, and do not modify the workspace."
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

/// One entry in the conversation viewport.
#[derive(Debug, Clone)]
pub enum MsgBlock {
    User(String),
    /// Assistant text; appended to incrementally while streaming.
    Assistant(String),
    /// A tool call trace/result line, e.g. `✓ read_file agent.rs (14.2 KB, 3ms)`.
    Tool {
        status: ToolStatus,
        line: String,
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
    /// Scroll offset into a long diff/command, in lines.
    pub scroll: usize,
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

#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub round: Option<(u32, u32)>,
    pub rate_used: Option<u32>,
    pub rate_capacity: Option<u32>,
    pub min_interval_ms: u64,
    pub next_request_ms: u64,
    pub tokens_this_turn: Option<u32>,
    pub spinner_line: Option<String>,
}

pub struct AppState {
    pub model: String,
    pub models: Vec<String>,
    pub provider: String,
    pub providers: Vec<String>,
    pub mode: InteractionMode,
    pub cwd_display: String,
    pub messages: Vec<MsgBlock>,
    pub input: String,
    /// Cursor position within `input`, in chars.
    pub cursor: usize,
    pub input_history: Vec<String>,
    /// Index into `input_history` while browsing with Up/Down; `None` means
    /// "not currently browsing" (editing the live input).
    pub history_idx: Option<usize>,
    /// Currently highlighted entry in the live slash-command palette.
    pub suggestion_idx: usize,
    /// Lines scrolled up from the bottom of the conversation viewport.
    pub scroll: usize,
    pub processing: bool,
    pub status: StatusInfo,
    pub confirm: Option<PendingConfirm>,
    /// Set whenever render state changes; the main loop clears it after a
    /// redraw (SPEC-UX B3: throttled ~30fps rendering).
    pub dirty: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(model: String, cwd_display: String) -> Self {
        AppState {
            model,
            models: Vec::new(),
            provider: "nvidia".to_string(),
            providers: Vec::new(),
            mode: InteractionMode::Auto,
            cwd_display,
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_history: Vec::new(),
            history_idx: None,
            suggestion_idx: 0,
            scroll: 0,
            processing: false,
            status: StatusInfo::default(),
            confirm: None,
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
        if let Some(MsgBlock::Assistant(s)) = self.messages.last_mut() {
            s.push_str(chunk);
        } else {
            self.messages.push(MsgBlock::Assistant(chunk.to_string()));
        }
        self.dirty = true;
    }

    pub fn discard_last_assistant_message(&mut self) {
        let last_idx = self
            .messages
            .iter()
            .rposition(|msg| matches!(msg, MsgBlock::Assistant(text) if !text.trim().is_empty()));
        if let Some(idx) = last_idx {
            self.messages.remove(idx);
            self.dirty = true;
        }
    }

    /// Ensures the *next* streamed chunk starts a fresh assistant message
    /// rather than appending to a previous one (called at the start of a
    /// round, so tool-call rounds don't get glued to the following text).
    pub fn start_new_assistant_message_boundary(&mut self) {
        if !matches!(self.messages.last(), Some(MsgBlock::Assistant(s)) if s.is_empty()) {
            self.messages.push(MsgBlock::Assistant(String::new()));
        }
    }

    pub fn push_tool_running(&mut self, line: String) {
        self.messages.push(MsgBlock::Tool {
            status: ToolStatus::Running,
            line,
        });
        self.dirty = true;
    }

    pub fn finish_tool(&mut self, ok: bool, line: String) {
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
            self.messages[idx] = MsgBlock::Tool {
                status: if ok {
                    ToolStatus::Success
                } else {
                    ToolStatus::Failed
                },
                line,
            };
        } else {
            self.messages.push(MsgBlock::Tool {
                status: if ok {
                    ToolStatus::Success
                } else {
                    ToolStatus::Failed
                },
                line,
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
                })
                .collect();
        }

        if let Some(query) = self.input.strip_prefix("/mode ") {
            let query = query.trim().to_ascii_lowercase();
            return [
                ("auto", "ask before edits and commands"),
                ("plan", "read-only exploration and planning"),
                ("accept-edits", "allow edits; ask before commands"),
            ]
            .into_iter()
            .filter(|(mode, _)| mode.contains(&query))
            .map(|(mode, description)| InputSuggestion {
                value: format!("/mode {mode}"),
                label: mode.to_string(),
                description: description.to_string(),
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
            })
            .collect()
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
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn set_usage(&mut self, usage: Option<&Usage>) {
        self.status.tokens_this_turn = usage.map(|u| u.total_tokens);
        self.dirty = true;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSuggestion {
    pub value: String,
    pub label: String,
    pub description: String,
}

fn byte_index_for_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_input_at_unicode_cursor() {
        let mut app = AppState::new("m".into(), ".".into());
        app.set_input("olá".into());
        app.cursor = 2;
        app.insert_char('X');
        assert_eq!(app.input, "olXá");
        app.backspace();
        assert_eq!(app.input, "olá");
    }

    #[test]
    fn discards_last_non_empty_assistant_message() {
        let mut app = AppState::new("m".into(), ".".into());
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
    fn slash_palette_switches_to_model_results() {
        let mut app = AppState::new("current".into(), ".".into());
        app.set_models(vec!["meta/llama".into(), "mistral/nemo".into()]);
        app.set_input("/model llama".into());
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/model meta/llama");
    }

    #[test]
    fn slash_palette_suggests_interaction_modes() {
        let mut app = AppState::new("m".into(), ".".into());
        app.set_input("/mode acc".into());
        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/mode accept-edits");
    }

    #[test]
    fn vertical_cursor_navigation_preserves_column() {
        let mut app = AppState::new("m".into(), ".".into());
        app.set_input("abcd\nxy".into());
        assert!(app.move_cursor_vertical(-1));
        assert_eq!(app.cursor, 2);
    }
}
