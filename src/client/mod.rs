//! Provider-agnostic LLM client trait and shared request/response types.

pub mod nim;
pub mod sse;

/// Provider-neutral name for the shared OpenAI-compatible transport.
pub type OpenAiCompatibleClient = nim::NimClient;

use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// One function-call the model asked to make. Present on assistant messages
/// (both non-streaming responses and the accumulated result of a streamed
/// one) and echoed back (by `id`) on the `role: "tool"` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// JSON-encoded arguments, as a string (OpenAI/NIM contract) -- may be
    /// assembled from several streamed fragments before it's valid JSON.
    pub arguments: String,
}

/// A single message in a chat history. `content` is optional because an
/// assistant message that only carries `tool_calls` has no text (SPEC-AGENT
/// §1: "content possivelmente nulo"). `tool_call_id`/`name` are only set on
/// `role: "tool"` reply messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// Builds a plain text message (the common case: system/user/assistant
    /// messages with no tool calls attached).
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Convenience accessor: `content` as a `&str`, empty when absent (e.g.
    /// a tool-calls-only assistant message).
    pub fn content_str(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// `{"type": "function", "function": {name, description, parameters}}` --
/// one tool definition advertised to the model (SPEC-AGENT §1/§2).
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatResponse {
    /// Server-assigned completion id; not surfaced yet in phase-1 output
    /// but kept on the contract for phase-2 (e.g. logging/tracing).
    #[serde(default)]
    #[allow(dead_code)]
    pub id: String,
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatChoice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ChatResponse {
    pub fn content(&self) -> &str {
        self.choices
            .first()
            .map(|c| c.message.content_str())
            .unwrap_or_default()
    }

    /// The first choice's tool calls, if the model asked to call any
    /// (non-streaming path; `finish_reason == "tool_calls"`).
    pub fn tool_calls(&self) -> &[ToolCall] {
        self.choices
            .first()
            .and_then(|c| c.message.tool_calls.as_deref())
            .unwrap_or_default()
    }

    pub fn finish_reason(&self) -> Option<&str> {
        self.choices
            .first()
            .and_then(|c| c.finish_reason.as_deref())
    }
}

/// One fragment of a streamed tool-call delta. Per the OpenAI/NIM contract,
/// deltas for the *same* logical tool call share `index`; `arguments` is a
/// fragment that must be concatenated (by the consumer, e.g. `agent.rs`)
/// across every delta with that index until the round ends.
#[derive(Debug, Clone, Default)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

/// One incremental chunk of a streamed chat completion.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    pub delta: String,
    pub tool_calls: Vec<ToolCallDelta>,
    /// Set on the final chunk of a completion (e.g. "stop", "tool_calls",
    /// "length").
    pub finish_reason: Option<String>,
    /// Present on some providers' final chunk when the request opted into
    /// usage reporting; `None` when the stream never carries it (SPEC-UX A5:
    /// tokens are shown in the turn summary only "when available").
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelInfo {
    pub id: String,
}

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ApiError>> + Send>>;

/// Provider-agnostic LLM client trait. Implemented by `nim::NimClient`; kept
/// separate so a different backend could be swapped in later.
pub trait LlmClient: Send + Sync {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<ChatResponse, ApiError>> + Send;

    fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<ChatStream, ApiError>> + Send;

    fn list_models(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ModelInfo>, ApiError>> + Send;
}
