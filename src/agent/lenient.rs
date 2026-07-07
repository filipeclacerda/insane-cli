//! Lenient recovery of a tool call emitted as plain text instead of the
//! provider's structured `tool_calls` field (SPEC-UX A4). Some models
//! (observed with `z-ai/glm-5.2`-family models) emit the call as JSON text
//! in the assistant's message content rather than using the API's function
//! calling mechanism. When a round finishes with `finish_reason == "stop"`
//! and no structured tool calls, `detect` inspects the accumulated text for
//! one of a few narrow, deliberately conservative shapes and -- only on a
//! confident match -- returns the visible assistant text with the tool-call
//! markup removed plus the recovered tool name/arguments.
//!
//! False positives are treated as worse than false negatives: incidental
//! JSON in the middle of prose must never trigger this, so every accepted
//! shape requires the JSON to be either the *entire* content, the *last
//! line* in isolation, a `<tool_call>...</tool_call>` tagged block, or a
//! fenced ```json ... ``` block terminating the content.

use serde_json::Value;

/// A tool call recovered from text: the tool's name and its arguments
/// re-serialized as a JSON string (matching the `ToolCallFunction.arguments`
/// contract used for structured calls).
pub struct RecoveredCall {
    pub name: String,
    pub arguments: String,
}

/// Attempts to recover a tool call from `content`. `known_tools` is the list
/// of tool names actually available this turn -- a JSON object naming any
/// other tool is not a match (it's either prose that happens to look like
/// JSON, or a call to a tool that doesn't exist, which must fall through to
/// the model normally rather than being silently "recovered").
///
/// Returns `(visible_text, call)` where `visible_text` is the assistant text
/// with the recovered call removed.
pub fn detect(content: &str, known_tools: &[&str]) -> Option<(String, RecoveredCall)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 1. The entire content (trimmed) is one JSON tool-call object.
    if let Some(call) = try_parse_call(trimmed, known_tools) {
        return Some((String::new(), call));
    }

    // 2. A <tool_call>...</tool_call> tagged block anywhere in the content.
    if let Some(pos) = content.find("<tool_call>") {
        let after = pos + "<tool_call>".len();
        if let Some(end_rel) = content[after..].find("</tool_call>") {
            let inner = content[after..after + end_rel].trim();
            if let Some(call) = try_parse_call(inner, known_tools) {
                let close_end = after + end_rel + "</tool_call>".len();
                let visible = join_visible_text(&content[..pos], &content[close_end..]);
                return Some((visible, call));
            }
        }
    }

    // 3. The last non-empty line, in isolation, is one JSON tool-call object.
    let mut lines: Vec<&str> = content.lines().collect();
    while let Some(last) = lines.last() {
        if last.trim().is_empty() {
            lines.pop();
            continue;
        }
        break;
    }
    if let Some(&last) = lines.last() {
        if let Some(call) = try_parse_call(last.trim(), known_tools) {
            lines.pop();
            let prefix = lines.join("\n").trim_end().to_string();
            return Some((prefix, call));
        }
    }

    // 4. A single fenced ```json ... ``` block terminating the content.
    if let Some((prefix, inner)) = trailing_json_fence(content) {
        if let Some(call) = try_parse_call(inner.trim(), known_tools) {
            return Some((prefix, call));
        }
    }

    None
}

fn join_visible_text(before: &str, after: &str) -> String {
    let before = before.trim_end();
    let after = after.trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (false, true) => before.to_string(),
        (true, false) => after.to_string(),
        (false, false) => format!("{before}\n{after}"),
    }
}

/// Parses `s` as a JSON object shaped like `{"name": <tool>, "arguments":
/// {...}}` or `{"tool": <tool>, "parameters"|"arguments": {...}}`, where
/// `<tool>` is one of `known_tools`. Anything else (invalid JSON, a JSON
/// array/scalar, an unknown tool name, missing name/tool field) is `None`.
fn try_parse_call(s: &str, known_tools: &[&str]) -> Option<RecoveredCall> {
    let value: Value = serde_json::from_str(s).ok()?;
    let obj = value.as_object()?;
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .and_then(Value::as_str)?;
    if !known_tools.contains(&name) {
        return None;
    }
    let args = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    Some(RecoveredCall {
        name: name.to_string(),
        arguments: args.to_string(),
    })
}

/// Finds a ```json fenced block that terminates the content (only trailing
/// whitespace, if anything, follows its closing fence) and returns
/// `(prefix_before_fence, inner_json_text)`.
fn trailing_json_fence(content: &str) -> Option<(String, String)> {
    let start_marker = content.rfind("```json")?;
    let after_marker = start_marker + "```json".len();
    let rest = &content[after_marker..];
    let close_rel = rest.find("```")?;
    let inner = &rest[..close_rel];
    let after_close = &rest[close_rel + 3..];
    if !after_close.trim().is_empty() {
        return None; // more content follows -- not a terminating block
    }
    let prefix = content[..start_marker].trim_end().to_string();
    Some((prefix, inner.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLS: &[&str] = &[
        "list_files",
        "read_file",
        "search_files",
        "write_file",
        "edit_file",
        "run_command",
    ];

    // -- Positive cases -----------------------------------------------

    #[test]
    fn detects_pure_json_whole_content() {
        let content = r#"{"name": "read_file", "arguments": {"path": "agent.rs"}}"#;
        let (prefix, call) = detect(content, TOOLS).expect("should detect");
        assert_eq!(prefix, "");
        assert_eq!(call.name, "read_file");
        assert!(call.arguments.contains("agent.rs"));
    }

    #[test]
    fn detects_glm_style_announcement_then_json_on_last_line() {
        // The real-world failure mode this addresses: a model announces the
        // action in prose, then emits the call as JSON text instead of a
        // structured tool_calls entry.
        let content = "Agora vou ler o arquivo para analisar o codigo.\n\n\
            {\"name\": \"read_file\", \"arguments\": {\"path\": \"agent.rs\"}}";
        let (prefix, call) = detect(content, TOOLS).expect("should detect");
        assert!(prefix.contains("Agora vou ler o arquivo"));
        assert_eq!(call.name, "read_file");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["path"], "agent.rs");
    }

    #[test]
    fn detects_tool_and_parameters_shape() {
        let content = r#"{"tool": "list_files", "parameters": {"path": "."}}"#;
        let (_, call) = detect(content, TOOLS).expect("should detect");
        assert_eq!(call.name, "list_files");
    }

    #[test]
    fn detects_tool_call_tagged_block() {
        let content = "I will search the codebase now.\n\
            <tool_call>{\"name\": \"search_files\", \"arguments\": {\"pattern\": \"TODO\"}}</tool_call>";
        let (prefix, call) = detect(content, TOOLS).expect("should detect");
        assert!(prefix.contains("search the codebase"));
        assert_eq!(call.name, "search_files");
    }

    #[test]
    fn tagged_block_preserves_visible_text_around_call() {
        let content = "Antes.\n\
            <tool_call>{\"name\": \"search_files\", \"arguments\": {\"pattern\": \"TODO\"}}</tool_call>\nDepois.";
        let (visible, call) = detect(content, TOOLS).expect("should detect");
        assert_eq!(visible, "Antes.\nDepois.");
        assert_eq!(call.name, "search_files");
    }

    #[test]
    fn detects_trailing_fenced_json_block() {
        let content = "Let me write the file.\n```json\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.txt\", \"content\": \"hi\"}}\n```";
        let (prefix, call) = detect(content, TOOLS).expect("should detect");
        assert!(prefix.contains("Let me write the file"));
        assert_eq!(call.name, "write_file");
    }

    #[test]
    fn detects_call_with_no_arguments_field() {
        let content = r#"{"name": "list_files"}"#;
        let (_, call) = detect(content, TOOLS).expect("should detect");
        assert_eq!(call.name, "list_files");
        assert_eq!(call.arguments, "{}");
    }

    // -- Negative cases (must NOT trigger) -----------------------------

    #[test]
    fn plain_prose_does_not_trigger() {
        let content = "Vou analisar o arquivo e criar um plano de refatoracao.";
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn incidental_json_in_the_middle_of_prose_does_not_trigger() {
        let content = "Here's an example config: {\"name\": \"read_file\", \"arguments\": {}} \
            is what a tool call looks like, but I'm not calling it right now. Let me explain more \
            about how this works in general.";
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn json_naming_an_unknown_tool_does_not_trigger() {
        let content = r#"{"name": "delete_everything", "arguments": {}}"#;
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn json_without_name_or_tool_field_does_not_trigger() {
        let content = r#"{"foo": "bar"}"#;
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn json_array_does_not_trigger() {
        let content = r#"[{"name": "read_file", "arguments": {}}]"#;
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn invalid_json_does_not_trigger() {
        let content = "{name: read_file}";
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn fenced_json_block_followed_by_more_prose_does_not_trigger() {
        let content = "```json\n{\"name\": \"read_file\", \"arguments\": {}}\n```\nAnd then I kept talking about it.";
        assert!(detect(content, TOOLS).is_none());
    }

    #[test]
    fn empty_content_does_not_trigger() {
        assert!(detect("", TOOLS).is_none());
        assert!(detect("   \n  ", TOOLS).is_none());
    }

    #[test]
    fn tool_call_tag_with_unknown_tool_does_not_trigger() {
        let content = "<tool_call>{\"name\": \"rm_rf\", \"arguments\": {}}</tool_call>";
        assert!(detect(content, TOOLS).is_none());
    }
}
