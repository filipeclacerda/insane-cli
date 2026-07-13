//! Tools exposed to the model in agent mode (SPEC-AGENT §2): `list_files`,
//! `read_file`, `search_files`, `write_file`, `edit_file`, `run_command`.
//! Every tool is sandboxed to the process's cwd (`sandbox`), writes go
//! through `fileops` (diff + confirm + atomic + backup), and confirmations
//! go through `permission::Permissions` (y/n/a on stderr). Errors from a
//! tool are returned as a string result to the model -- they never abort
//! the session.

pub mod exec;
pub mod fs;
pub mod permission;
pub mod sandbox;

use std::path::PathBuf;

use serde_json::json;

use crate::client::ToolDef;
use crate::ui::AgentUi;
use permission::Permissions;
use tokio_util::sync::CancellationToken;

/// Per-call context threaded into every tool implementation: the sandbox
/// root, size caps, extra ignore globs from config, and the session's
/// standing permissions.
pub struct ToolExecCtx<'a> {
    pub cwd: PathBuf,
    pub max_context_bytes: usize,
    pub extra_ignore: &'a [String],
    pub permissions: &'a mut Permissions,
    pub cancellation: &'a CancellationToken,
    pub ui: &'a dyn AgentUi,
    pub turn_id: u64,
}

/// The full set of tool definitions advertised to the model.
pub fn all_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "list_files",
            "List files and directories recursively under a path (respects .gitignore, the \
configured ignore list, and the high-risk key/certificate filename denylist). Directories are shown with a \
trailing '/'. Capped at 500 entries.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list, relative to the working directory (default \".\")"
                    },
                    "max_entries": {
                        "type": "integer",
                        "description": "Maximum entries to return (default and cap 500)"
                    }
                }
            }),
        ),
        ToolDef::new(
            "read_file",
            "Read a file's contents, optionally restricted to a line range. Fails for files \
matching the high-risk key/certificate filename denylist (*.pem, *.key, id_rsa*, etc). \
.env files are allowed. Content is capped at max_context_bytes.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the working directory"},
                    "start_line": {"type": "integer", "description": "First line to include (1-indexed, inclusive)"},
                    "end_line": {"type": "integer", "description": "Last line to include (1-indexed, inclusive)"}
                },
                "required": ["path"]
            }),
        ),
        ToolDef::new(
            "search_files",
            "Search non-ignored files under a path for lines matching a regex. Returns up to 100 \
matching lines as 'path:line: text'.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern to search for"},
                    "path": {"type": "string", "description": "Directory to search under, relative to the working directory (default \".\")"},
                    "max_results": {"type": "integer", "description": "Maximum matching lines to return (default and cap 100)"}
                },
                "required": ["pattern"]
            }),
        ),
        ToolDef::new(
            "write_file",
            "Write a new file or completely overwrite an existing one. Shows a diff and asks the \
user for confirmation before writing; writes are atomic with a backup of any prior content.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the working directory"},
                    "content": {"type": "string", "description": "Full new content of the file"}
                },
                "required": ["path", "content"]
            }),
        ),
        ToolDef::new(
            "edit_file",
            "Make a targeted edit to an existing file by replacing one exact occurrence of \
old_string with new_string. old_string must be unique in the file unless replace_all is set. \
Shows a diff and asks the user for confirmation; writes are atomic with a backup.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the working directory"},
                    "old_string": {"type": "string", "description": "Exact text to find (must be unique unless replace_all is true)"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique match"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        ToolDef::new(
            "run_command",
            "Run a shell command in the working directory (PowerShell on Windows, Bash on Unix). \
Always asks the user for confirmation first. Output (stdout+stderr merged) is capped at 32KiB; \
the command is killed if it exceeds its timeout (default and cap 300s).",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run"},
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default and cap 300)"}
                },
                "required": ["command"]
            }),
        ),
    ]
}

/// Tool definitions allowed while the TUI is in PLAN mode. These let the
/// model inspect the project and run diagnostic commands, but do not expose
/// file-writing tools.
pub fn plan_tool_defs() -> Vec<ToolDef> {
    all_tool_defs()
        .into_iter()
        .filter(|def| {
            matches!(
                def.function.name.as_str(),
                "list_files" | "read_file" | "search_files" | "run_command"
            )
        })
        .collect()
}

/// Executes a tool call by name, returning a compact JSON string
/// `{"ok":true,"output":...}` or `{"ok":false,"error":...}` -- this is what
/// gets sent back as the `role: "tool"` message content. Never panics: any
/// failure (bad arguments, sandbox violation, denied permission, I/O error)
/// is captured as an `error` string.
pub async fn execute(name: &str, arguments: &str, ectx: &mut ToolExecCtx<'_>) -> String {
    let result: Result<String, String> = match name {
        "list_files" => fs::list_files(arguments, ectx).await,
        "read_file" => fs::read_file(arguments, ectx).await,
        "search_files" => fs::search_files(arguments, ectx).await,
        "write_file" => fs::write_file(arguments, ectx).await,
        "edit_file" => fs::edit_file(arguments, ectx).await,
        "run_command" => {
            exec::run_command_with_services(
                arguments,
                &ectx.cwd,
                ectx.permissions,
                ectx.cancellation,
                ectx.ui,
            )
            .await
        }
        other => Err(format!("unknown tool: {other}")),
    };
    match result {
        Ok(output) => json!({"ok": true, "output": output}).to_string(),
        Err(error) => json!({"ok": false, "error": error}).to_string(),
    }
}

/// A short, single-line summary of a tool call's arguments for the `→
/// name(args)` trace line (SPEC-AGENT §3/§4). Truncates long values so the
/// trace stays readable.
pub fn summarize_args(arguments: &str) -> String {
    const MAX_LEN: usize = 80;
    let compact: String = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > MAX_LEN {
        let truncated: String = compact.chars().take(MAX_LEN).collect();
        format!("{truncated}...")
    } else {
        compact
    }
}

/// Human-friendly label for an in-flight tool call. This deliberately
/// surfaces the useful fields instead of dumping JSON into the conversation.
pub fn tool_call_label(name: &str, arguments: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return summarize_args(arguments),
    };
    let string = |field: &str| value.get(field).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "read_file" => {
            let path = string("path");
            match (
                value.get("start_line").and_then(|v| v.as_u64()),
                value.get("end_line").and_then(|v| v.as_u64()),
            ) {
                (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
                (Some(start), None) => format!("{path}:{start}"),
                _ => path.to_string(),
            }
        }
        "list_files" => {
            let path = string("path");
            if path.is_empty() {
                ".".to_string()
            } else {
                path.to_string()
            }
        }
        "search_files" => {
            let pattern = string("pattern");
            let path = string("path");
            if path.is_empty() {
                format!("/{pattern}/ in .")
            } else {
                format!("/{pattern}/ in {path}")
            }
        }
        "write_file" | "edit_file" => string("path").to_string(),
        "run_command" => format!("$ {}", summarize_args(string("command"))),
        _ => summarize_args(arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    use crate::ui::{test_support::FakeUi, Decision};

    static TEST_UI: FakeUi = FakeUi {
        answer: Decision::No,
    };
    static TEST_CANCELLATION: OnceLock<CancellationToken> = OnceLock::new();

    #[test]
    fn all_tool_defs_has_six_tools_with_unique_names() {
        let defs = all_tool_defs();
        assert_eq!(defs.len(), 6);
        let mut names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn plan_tool_defs_exclude_write_tools() {
        let names: Vec<String> = plan_tool_defs()
            .iter()
            .map(|def| def.function.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "list_files".to_string(),
                "read_file".to_string(),
                "search_files".to_string(),
                "run_command".to_string(),
            ]
        );
        assert!(!names.iter().any(|name| name == "write_file"));
        assert!(!names.iter().any(|name| name == "edit_file"));
    }

    #[test]
    fn summarize_args_truncates_long_input() {
        let long = "x".repeat(200);
        let s = summarize_args(&long);
        assert!(s.len() < long.len());
        assert!(s.ends_with("..."));
    }

    #[test]
    fn tool_call_label_extracts_useful_fields() {
        assert_eq!(
            tool_call_label(
                "read_file",
                r#"{"path":"src/main.rs","start_line":4,"end_line":8}"#
            ),
            "src/main.rs:4-8"
        );
        assert_eq!(
            tool_call_label("run_command", r#"{"command":"cargo test"}"#),
            "$ cargo test"
        );
    }

    fn ctx<'a>(
        cwd: PathBuf,
        extra_ignore: &'a [String],
        perms: &'a mut Permissions,
    ) -> ToolExecCtx<'a> {
        ToolExecCtx {
            cwd,
            max_context_bytes: 192 * 1024,
            extra_ignore,
            permissions: perms,
            cancellation: TEST_CANCELLATION.get_or_init(CancellationToken::new),
            ui: &TEST_UI,
            turn_id: 0,
        }
    }

    #[tokio::test]
    async fn read_file_rejects_path_outside_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("secret.txt");
        std::fs::write(&outside, "top secret").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"path": outside.to_str().unwrap()}).to_string();
        let err = fs::read_file(&args, &mut c).await.unwrap_err();
        assert!(err.contains("escapes"));
        // File must be untouched.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "top secret");
    }

    #[tokio::test]
    async fn read_file_allows_clean_env_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "APP_MODE=local").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"path": ".env"}).to_string();
        let out = fs::read_file(&args, &mut c).await.unwrap();
        assert!(out.contains("APP_MODE=local"));
    }

    #[tokio::test]
    async fn edit_file_unique_match_succeeds_without_confirmation_prompted_denied() {
        // A `FakeUi` that always refuses -- this proves a *unique* match
        // still reaches the confirmation step (rather than failing earlier)
        // and that a refusal leaves the file untouched. Using `FakeUi`
        // (instead of relying on stdin not being a TTY) keeps the test from
        // blocking when run from an interactive terminal.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello world").unwrap();

        let mut perms = Permissions::with_ui(Box::new(crate::ui::test_support::FakeUi::deny()));
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args =
            json!({"path": "f.txt", "old_string": "world", "new_string": "there"}).to_string();
        let err = fs::edit_file(&args, &mut c).await.unwrap_err();
        assert!(err.contains("denied"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_ambiguous_without_replace_all_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "aa bb aa").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"path": "f.txt", "old_string": "aa", "new_string": "cc"}).to_string();
        let err = fs::edit_file(&args, &mut c).await.unwrap_err();
        assert!(err.contains("ambiguous"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aa bb aa");
    }

    #[tokio::test]
    async fn edit_file_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello world").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"path": "f.txt", "old_string": "missing", "new_string": "x"}).to_string();
        let err = fs::edit_file(&args, &mut c).await.unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn write_file_denied_on_non_tty_leaves_no_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut perms = Permissions::with_ui(Box::new(crate::ui::test_support::FakeUi::deny()));
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"path": "new.txt", "content": "hi"}).to_string();
        let err = fs::write_file(&args, &mut c).await.unwrap_err();
        assert!(err.contains("denied"));
        assert!(!dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn active_write_tool_reports_cancelled_differently_from_denied() {
        let dir = tempfile::tempdir().unwrap();
        let ignore: Vec<String> = vec![];
        let cancellation = CancellationToken::new();
        let ui = FakeUi::deny();

        let mut denied_permissions = Permissions::with_ui(Box::new(FakeUi::deny()));
        let mut denied_ctx = ToolExecCtx {
            cwd: dir.path().to_path_buf(),
            max_context_bytes: 1024,
            extra_ignore: &ignore,
            permissions: &mut denied_permissions,
            cancellation: &cancellation,
            ui: &ui,
            turn_id: 0,
        };
        let denied = execute(
            "write_file",
            r#"{"path":"denied.txt","content":"x"}"#,
            &mut denied_ctx,
        )
        .await;

        let mut cancelled_permissions = Permissions::with_ui(Box::new(FakeUi::cancelled()));
        let mut cancelled_ctx = ToolExecCtx {
            cwd: dir.path().to_path_buf(),
            max_context_bytes: 1024,
            extra_ignore: &ignore,
            permissions: &mut cancelled_permissions,
            cancellation: &cancellation,
            ui: &ui,
            turn_id: 0,
        };
        let cancelled = execute(
            "write_file",
            r#"{"path":"cancelled.txt","content":"x"}"#,
            &mut cancelled_ctx,
        )
        .await;

        assert!(denied.contains("user denied write"));
        assert!(cancelled.contains("cancelled by user"));
    }

    #[tokio::test]
    async fn list_files_respects_max_entries_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"max_entries": 3}).to_string();
        let out = fs::list_files(&args, &mut c).await.unwrap();
        assert_eq!(out.lines().count(), 3);
    }

    #[tokio::test]
    async fn list_files_skips_hidden_gitignored_and_denylisted_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.path().join(".env"), "APP_MODE=local").unwrap();
        std::fs::write(dir.path().join("server.pem"), "key material").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "x").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let out = fs::list_files("{}", &mut c).await.unwrap();
        assert!(out.contains("kept.txt"));
        assert!(!out.contains("ignored.txt"));
        assert!(!out.contains(".env"));
        assert!(!out.contains("server.pem"));
    }

    #[tokio::test]
    async fn search_files_finds_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nneedle here\nbye").unwrap();

        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let args = json!({"pattern": "needle"}).to_string();
        let out = fs::search_files(&args, &mut c).await.unwrap();
        assert!(out.contains("a.txt:2: needle here"));
    }

    #[tokio::test]
    async fn cancelled_list_files_returns_structured_cancelled_result() {
        let dir = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::deny()));
        let ui = FakeUi::deny();
        let mut ctx = ToolExecCtx {
            cwd: dir.path().to_path_buf(),
            max_context_bytes: 1024,
            extra_ignore: &[],
            permissions: &mut permissions,
            cancellation: &cancellation,
            ui: &ui,
            turn_id: 0,
        };

        let result = execute("list_files", "{}", &mut ctx).await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap()["error"],
            "cancelled by user"
        );
    }

    #[tokio::test]
    async fn cancelled_search_files_returns_structured_cancelled_result() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::deny()));
        let ui = FakeUi::deny();
        let mut ctx = ToolExecCtx {
            cwd: dir.path().to_path_buf(),
            max_context_bytes: 1024,
            extra_ignore: &[],
            permissions: &mut permissions,
            cancellation: &cancellation,
            ui: &ui,
            turn_id: 0,
        };

        let result = execute("search_files", r#"{"pattern":"needle"}"#, &mut ctx).await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap()["error"],
            "cancelled by user"
        );
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error_json_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let mut perms = Permissions::new();
        let ignore: Vec<String> = vec![];
        let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
        let out = execute("not_a_real_tool", "{}", &mut c).await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[tokio::test]
    async fn run_command_denied_on_non_tty() {
        let dir = tempfile::tempdir().unwrap();
        let mut perms = Permissions::with_ui(Box::new(crate::ui::test_support::FakeUi::deny()));
        let args = json!({"command": "echo hi"}).to_string();
        let err = exec::run_command(&args, dir.path(), &mut perms)
            .await
            .unwrap_err();
        assert!(err.contains("denied"));
    }
}
