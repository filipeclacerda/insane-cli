//! In-memory chat session state: message history with approximate
//! byte/token-based trimming (grouping tool_calls/tool pairs so they are
//! never split, SPEC-AGENT §4), and the `/exit`, `/clear`, `/model`,
//! `/tools`, `/cwd` REPL commands.

use crate::client::{ChatMessage, ToolCall};

/// Rough characters-per-token ratio used to approximate a token budget from
/// `max_context_bytes` without pulling in a real tokenizer (out of scope for
/// phase 1).
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Parses a line of chat input into a REPL command or a plain message.
/// Returns `None` for command lines needing an extracted argument (handled
/// by the caller via `parse_command`).
pub fn parse_command(line: &str) -> Option<Command> {
    let trimmed = line.trim();
    if trimmed == "/exit" {
        Some(Command::Exit)
    } else if trimmed == "/clear" {
        Some(Command::Clear)
    } else if trimmed == "/tools" {
        Some(Command::Tools)
    } else if trimmed == "/cwd" {
        Some(Command::Cwd)
    } else if trimmed == "/continue" {
        Some(Command::Continue)
    } else if trimmed == "/compact" {
        Some(Command::Compact)
    } else if trimmed == "/resume" {
        Some(Command::Resume(None))
    } else if let Some(rest) = trimmed.strip_prefix("/resume ") {
        Some(Command::Resume(rest.trim().parse::<usize>().ok()))
    } else if trimmed == "/help" {
        Some(Command::Help)
    } else if trimmed == "/models" {
        Some(Command::Models)
    } else if trimmed == "/providers" {
        Some(Command::Providers)
    } else if trimmed == "/provider" {
        Some(Command::SetProvider(String::new()))
    } else if let Some(rest) = trimmed.strip_prefix("/provider ") {
        Some(Command::SetProvider(rest.trim().to_string()))
    } else if trimmed == "/mode" {
        Some(Command::SetMode(String::new()))
    } else if let Some(rest) = trimmed.strip_prefix("/mode ") {
        Some(Command::SetMode(rest.trim().to_string()))
    } else if trimmed == "/model" {
        Some(Command::SetModel(String::new()))
    } else if trimmed == "/copy" {
        Some(Command::Copy)
    } else {
        trimmed
            .strip_prefix("/model ")
            .map(|rest| Command::SetModel(rest.trim().to_string()))
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Exit,
    Clear,
    SetModel(String),
    /// Lists the models returned by the NIM `/models` endpoint.
    Models,
    Providers,
    SetProvider(String),
    /// Changes the TUI interaction mode (`default`, `plan`, `accept-edits`, `auto`).
    SetMode(String),
    Tools,
    Cwd,
    /// Resends the conversation with an instruction to pick up exactly where
    /// the model left off (SPEC-UX A3) -- meant for after a `finish_reason
    /// != stop/tool_calls` (e.g. `length`) cut a response short.
    Continue,
    /// Compacts the current conversation into a short working summary to
    /// reduce tokens on future turns.
    Compact,
    /// Copies the last assistant message to the system clipboard.
    Copy,
    /// Reloads the most recently saved session for the active provider,
    /// replacing the current conversation. Used to recover a chat that was
    /// closed in a previous `insane` invocation.
    Resume(Option<usize>),
    /// Lists available commands and (in the TUI) keybindings (SPEC-UX B4).
    Help,
}

/// Text for `/help`: slash commands, shared by line mode and the TUI. The
/// TUI appends its own keybinding list after this (SPEC-UX B4).
pub const HELP_COMMANDS: &str =
    "commands: /provider <name> /providers /model <name> /models /mode <default|plan|accept-edits|auto> /clear /tools /cwd /continue /compact /copy /resume [1-3] /help /exit";

/// Metadata used by `/help` and the TUI's live slash-command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/provider",
        usage: "/provider <name>",
        description: "trocar provider e iniciar novo chat",
    },
    SlashCommand {
        name: "/providers",
        usage: "/providers",
        description: "listar providers configurados",
    },
    SlashCommand {
        name: "/model",
        usage: "/model <name>",
        description: "trocar o modelo NIM",
    },
    SlashCommand {
        name: "/models",
        usage: "/models",
        description: "listar modelos disponíveis",
    },
    SlashCommand {
        name: "/mode",
        usage: "/mode <default|plan|accept-edits|auto>",
        description: "trocar o modo de interação",
    },
    SlashCommand {
        name: "/clear",
        usage: "/clear",
        description: "limpar a conversa",
    },
    SlashCommand {
        name: "/tools",
        usage: "/tools",
        description: "mostrar permissões das tools",
    },
    SlashCommand {
        name: "/cwd",
        usage: "/cwd",
        description: "mostrar o diretório atual",
    },
    SlashCommand {
        name: "/continue",
        usage: "/continue",
        description: "continuar uma resposta interrompida",
    },
    SlashCommand {
        name: "/compact",
        usage: "/compact",
        description: "resumir a conversa para reduzir tokens",
    },
    SlashCommand {
        name: "/resume",
        usage: "/resume [1-3]",
        description: "listar/retomar sessões salvas",
    },
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "mostrar ajuda e atalhos",
    },
    SlashCommand {
        name: "/exit",
        usage: "/exit",
        description: "sair do chat",
    },
    SlashCommand {
        name: "/copy",
        usage: "/copy",
        description: "copiar a última mensagem do assistente",
    },
];

/// The user message `/continue` sends: an instruction to resume without
/// repeating what was already said (SPEC-UX A3).
pub const CONTINUE_MESSAGE: &str = "Continue exactly where you stopped.";

pub struct Session {
    pub model: String,
    pub history: Vec<ChatMessage>,
    pub max_context_bytes: usize,
}

impl Session {
    pub fn new(model: String, max_context_bytes: usize) -> Self {
        Session {
            model,
            history: Vec::new(),
            max_context_bytes,
        }
    }

    /// Resets the conversation, but preserves a leading `system` message
    /// (the agent's system prompt, if one was pushed) so `/clear` doesn't
    /// also disable tool use.
    pub fn clear(&mut self) {
        let system = self.history.first().filter(|m| m.role == "system").cloned();
        self.history.clear();
        if let Some(sys) = system {
            self.history.push(sys);
        }
    }

    /// Inserts (or replaces) the leading system prompt. A no-op-safe way to
    /// set up the agent's instructions once at session start.
    pub fn push_system(&mut self, content: String) {
        if self
            .history
            .first()
            .map(|m| m.role == "system")
            .unwrap_or(false)
        {
            self.history[0] = ChatMessage::text("system", content);
        } else {
            self.history.insert(0, ChatMessage::text("system", content));
        }
    }

    pub fn push_user(&mut self, content: String) {
        self.history.push(ChatMessage::text("user", content));
        self.trim();
    }

    pub fn push_assistant(&mut self, content: String) {
        self.history.push(ChatMessage::text("assistant", content));
        self.trim();
    }

    /// Appends an assistant message that carries tool calls (content may be
    /// empty/absent -- SPEC-AGENT §1).
    pub fn push_assistant_tool_calls(
        &mut self,
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) {
        self.history.push(ChatMessage {
            role: "assistant".to_string(),
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        });
        self.trim();
    }

    /// Appends the `role: "tool"` reply for one tool call.
    pub fn push_tool_result(&mut self, tool_call_id: String, content: String) {
        self.history.push(ChatMessage {
            role: "tool".to_string(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
            name: None,
        });
        self.trim();
    }

    fn msg_size(m: &ChatMessage) -> usize {
        let tool_calls_size = m
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| c.id.len() + c.function.name.len() + c.function.arguments.len())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        m.role.len() + m.content.as_deref().map(str::len).unwrap_or(0) + tool_calls_size
    }

    fn total_bytes(&self) -> usize {
        self.history.iter().map(Self::msg_size).sum()
    }

    /// Number of messages in the "leading group" starting at `start`: 1 for
    /// a plain message, or an assistant-with-`tool_calls` message plus every
    /// contiguous `tool` reply answering one of its calls. Trimming always
    /// evicts a whole group, so a `tool` message is never left orphaned
    /// from its assistant/tool_calls message (SPEC-AGENT §4).
    fn group_len_from(&self, start: usize) -> usize {
        if start >= self.history.len() {
            return 0;
        }
        let first = &self.history[start];
        match &first.tool_calls {
            Some(calls) => {
                let ids: std::collections::HashSet<&str> =
                    calls.iter().map(|c| c.id.as_str()).collect();
                let mut n = 1;
                while start + n < self.history.len() {
                    let m = &self.history[start + n];
                    let is_matching_tool_reply = m.role == "tool"
                        && m.tool_call_id
                            .as_deref()
                            .map(|id| ids.contains(id))
                            .unwrap_or(false);
                    if is_matching_tool_reply {
                        n += 1;
                    } else {
                        break;
                    }
                }
                n
            }
            None => 1,
        }
    }

    /// Trims the oldest message groups until total history size fits within
    /// `max_context_bytes` (approximated in bytes; SPEC §7). A leading
    /// `system` message is never evicted, and at least one non-system group
    /// is always kept.
    pub fn trim(&mut self) {
        loop {
            if self.total_bytes() <= self.max_context_bytes {
                break;
            }
            let start = if self
                .history
                .first()
                .map(|m| m.role == "system")
                .unwrap_or(false)
            {
                1
            } else {
                0
            };
            if start >= self.history.len() {
                break;
            }
            let group_len = self.group_len_from(start);
            if group_len == 0 || start + group_len >= self.history.len() {
                break; // never evict the last remaining group
            }
            self.history.drain(start..start + group_len);
        }
    }

    /// Approximate token count for the current history, for diagnostics.
    pub fn approx_tokens(&self) -> usize {
        let bytes: usize = self
            .history
            .iter()
            .map(|m| m.content.as_deref().map(str::len).unwrap_or(0))
            .sum();
        bytes / APPROX_BYTES_PER_TOKEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ToolCallFunction;

    #[test]
    fn trims_oldest_first_when_over_budget() {
        let mut s = Session::new("m".to_string(), 20);
        s.push_user("a".repeat(10));
        s.push_assistant("b".repeat(10));
        s.push_user("c".repeat(10));
        // Oldest ("a"*10) should have been evicted to make room.
        assert!(s.history.iter().all(|m| !m.content_str().starts_with('a')));
    }

    #[test]
    fn always_keeps_at_least_one_message() {
        let mut s = Session::new("m".to_string(), 1);
        s.push_user("x".repeat(50));
        assert_eq!(s.history.len(), 1);
    }

    #[test]
    fn parses_slash_commands() {
        assert!(matches!(parse_command("/exit"), Some(Command::Exit)));
        assert!(matches!(parse_command("/clear"), Some(Command::Clear)));
        assert!(matches!(parse_command("/tools"), Some(Command::Tools)));
        assert!(matches!(parse_command("/cwd"), Some(Command::Cwd)));
        assert!(matches!(
            parse_command("/continue"),
            Some(Command::Continue)
        ));
        assert!(matches!(parse_command("/compact"), Some(Command::Compact)));
        assert!(matches!(parse_command("/copy"), Some(Command::Copy)));
        assert!(matches!(
            parse_command("/resume"),
            Some(Command::Resume(None))
        ));
        assert!(matches!(
            parse_command("/resume 2"),
            Some(Command::Resume(Some(2)))
        ));
        assert!(matches!(parse_command("/help"), Some(Command::Help)));
        assert!(matches!(parse_command("/models"), Some(Command::Models)));
        assert!(matches!(
            parse_command("/providers"),
            Some(Command::Providers)
        ));
        match parse_command("/provider local") {
            Some(Command::SetProvider(provider)) => assert_eq!(provider, "local"),
            _ => panic!("expected SetProvider"),
        }
        match parse_command("/mode default") {
            Some(Command::SetMode(mode)) => assert_eq!(mode, "default"),
            _ => panic!("expected SetMode"),
        }
        match parse_command("/model gpt-x") {
            Some(Command::SetModel(m)) => assert_eq!(m, "gpt-x"),
            _ => panic!("expected SetModel"),
        }
        assert!(parse_command("/modelish").is_none());
        assert!(parse_command("/modeled").is_none());
        assert!(parse_command("hello world").is_none());
    }

    #[test]
    fn system_prompt_survives_clear() {
        let mut s = Session::new("m".to_string(), 10_000);
        s.push_system("you are an agent".to_string());
        s.push_user("hi".to_string());
        s.clear();
        assert_eq!(s.history.len(), 1);
        assert_eq!(s.history[0].role, "system");
    }

    #[test]
    fn system_prompt_is_never_evicted_by_trim() {
        let mut s = Session::new("m".to_string(), 20);
        s.push_system("sys".to_string());
        s.push_user("a".repeat(10));
        s.push_assistant("b".repeat(10));
        s.push_user("c".repeat(10));
        assert_eq!(s.history[0].role, "system");
    }

    #[test]
    fn trim_never_orphans_a_tool_reply_from_its_assistant_message() {
        let mut s = Session::new("m".to_string(), 30);
        s.push_user("a".repeat(20));
        s.push_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: ToolCallFunction {
                    name: "list_files".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        );
        s.push_tool_result("call_1".to_string(), "x".repeat(20));
        s.push_user("d".repeat(20));

        // Either the assistant/tool_calls message and its tool reply are
        // both present, or both are gone -- never just one of them.
        let has_assistant_tool_calls = s.history.iter().any(|m| m.tool_calls.is_some());
        let has_tool_reply = s.history.iter().any(|m| m.role == "tool");
        assert_eq!(has_assistant_tool_calls, has_tool_reply);
    }

    #[test]
    fn approx_tokens_counts_content_bytes() {
        let mut s = Session::new("m".to_string(), 10_000);
        s.push_user("a".repeat(8));
        assert_eq!(s.approx_tokens(), 2);
    }
}
