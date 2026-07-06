//! Filesystem tools exposed to the model (SPEC-AGENT §2): `list_files`,
//! `read_file`, `search_files`, `write_file`, `edit_file`. All paths are
//! sandboxed to the cwd (`sandbox::resolve_in_sandbox`); all writes show a
//! diff on stderr and require confirmation (`permission::Permissions`).

use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use super::sandbox::{display_relative, resolve_in_sandbox};
use super::ToolExecCtx;
use crate::{context, fileops};

const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_RESULTS: usize = 100;

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Whether `path` is excluded by `.gitignore` or the config's extra
/// `ignore` globs, rooted at `cwd`. Errors from a malformed matcher are
/// treated as "not ignored" (same fallback as `context::check_ignored`).
fn is_extra_ignored(path: &Path, cwd: &Path, extra: &[String]) -> bool {
    context::check_ignored(path, cwd, extra).is_err()
}

/// Builds a unified diff between `old` and `new` (tool output must never
/// land on stdout, which is reserved for the model's text -- SPEC-AGENT §5).
/// The `AgentUi` handling the confirmation is responsible for how (or
/// whether) it's displayed -- colored stderr in line mode, a scrollable
/// modal in the TUI (SPEC-UX B3).
fn build_diff(old: &str, new: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let proposed = format!("{path} (proposed)");
    diff.unified_diff()
        .context_radius(3)
        .header(path, &proposed)
        .to_string()
}

/// A project snapshot for the agent's system prompt (SPEC-UX A1): every
/// non-ignored, non-denylisted high-risk key/certificate entry under `cwd` (dirs suffixed with `/`),
/// sorted, capped at `cap` with a `(+N more)` trailer when truncated. Shares
/// the same ignore/denylist rules as `list_files` so the snapshot never
/// leaks anything a `list_files` call wouldn't already show.
pub fn snapshot_listing(cwd: &Path, extra_ignore: &[String], cap: usize) -> String {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd).build();
    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == cwd {
            continue;
        }
        if context::check_denylist(path).is_err() {
            continue;
        }
        if is_extra_ignored(path, cwd, extra_ignore) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut rel_path = display_relative(path, cwd);
        if is_dir {
            rel_path.push('/');
        }
        entries.push(rel_path);
    }
    entries.sort();
    let total = entries.len();
    entries.truncate(cap);
    let mut out = entries.join("\n");
    if total > cap {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("(+{} more)", total - cap));
    }
    out
}

#[derive(Deserialize, Default)]
struct ListFilesArgs {
    path: Option<String>,
    max_entries: Option<usize>,
}

pub fn list_files(arguments: &str, ectx: &mut ToolExecCtx<'_>) -> Result<String, String> {
    let args: ListFilesArgs = parse_args(arguments)?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_in_sandbox(&ectx.cwd, &rel)?;
    if !root.is_dir() {
        return Err(format!("`{rel}` is not a directory"));
    }
    let cap = args
        .max_entries
        .unwrap_or(MAX_LIST_ENTRIES)
        .min(MAX_LIST_ENTRIES);

    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(&root).build();
    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if context::check_denylist(path).is_err() {
            continue;
        }
        if is_extra_ignored(path, &ectx.cwd, ectx.extra_ignore) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut rel_path = display_relative(path, &ectx.cwd);
        if is_dir {
            rel_path.push('/');
        }
        entries.push(rel_path);
        if entries.len() >= cap {
            break;
        }
    }
    entries.sort();
    Ok(entries.join("\n"))
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

pub async fn read_file(arguments: &str, ectx: &mut ToolExecCtx<'_>) -> Result<String, String> {
    let args: ReadFileArgs = parse_args(arguments)?;
    let resolved = resolve_in_sandbox(&ectx.cwd, &args.path)?;
    context::check_denylist(&resolved).map_err(|e| e.to_string())?;
    if !resolved.is_file() {
        return Err(format!("`{}` is not a regular file", args.path));
    }

    let bytes =
        std::fs::read(&resolved).map_err(|e| format!("failed to read {}: {e}", args.path))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let range = match (args.start_line, args.end_line) {
        (None, None) => None,
        (start, end) => Some((start.unwrap_or(1), end.unwrap_or(usize::MAX))),
    };
    let sliced = context::slice_lines(&text, range);
    let (mut truncated_text, truncated) = context::truncate_bytes(&sliced, ectx.max_context_bytes);

    if truncated {
        truncated_text.push_str("\n...[truncated at max_context_bytes]");
    }
    Ok(truncated_text)
}

#[derive(Deserialize)]
struct SearchFilesArgs {
    pattern: String,
    path: Option<String>,
    max_results: Option<usize>,
}

pub async fn search_files(arguments: &str, ectx: &mut ToolExecCtx<'_>) -> Result<String, String> {
    let args: SearchFilesArgs = parse_args(arguments)?;
    let re = Regex::new(&args.pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_in_sandbox(&ectx.cwd, &rel)?;
    let cap = args
        .max_results
        .unwrap_or(MAX_SEARCH_RESULTS)
        .min(MAX_SEARCH_RESULTS);

    let mut results = Vec::new();
    let walker = ignore::WalkBuilder::new(&root).build();
    'outer: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if context::check_denylist(path).is_err() {
            continue;
        }
        if is_extra_ignored(path, &ectx.cwd, ectx.extra_ignore) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // skip unreadable/binary files
        };
        let rel_path = display_relative(path, &ectx.cwd);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}: {}", rel_path, i + 1, line));
                if results.len() >= cap {
                    break 'outer;
                }
            }
        }
    }

    Ok(results.join("\n"))
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

pub async fn write_file(arguments: &str, ectx: &mut ToolExecCtx<'_>) -> Result<String, String> {
    let args: WriteFileArgs = parse_args(arguments)?;
    let resolved = resolve_in_sandbox(&ectx.cwd, &args.path)?;
    context::check_denylist(&resolved).map_err(|e| e.to_string())?;

    let old = std::fs::read_to_string(&resolved).unwrap_or_default();
    let diff = build_diff(&old, &args.content, &args.path);

    if !ectx
        .permissions
        .confirm_with_diff(
            "write_file",
            &format!("Write to {}?", args.path),
            Some(&diff),
        )
        .await
    {
        return Err("user denied write".to_string());
    }

    fileops::write_atomic(&resolved, &args.content).map_err(|e| e.to_string())?;
    Ok(format!("wrote {}", args.path))
}

#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

pub async fn edit_file(arguments: &str, ectx: &mut ToolExecCtx<'_>) -> Result<String, String> {
    let args: EditFileArgs = parse_args(arguments)?;
    if args.old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    let resolved = resolve_in_sandbox(&ectx.cwd, &args.path)?;
    context::check_denylist(&resolved).map_err(|e| e.to_string())?;

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("failed to read {}: {e}", args.path))?;
    let count = content.matches(args.old_string.as_str()).count();
    if count == 0 {
        return Err(format!("old_string not found in {}", args.path));
    }
    let replace_all = args.replace_all.unwrap_or(false);
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string is ambiguous in {} ({count} occurrences); pass replace_all=true or make \
old_string more specific",
            args.path
        ));
    }

    let new_content = if replace_all {
        content.replace(&args.old_string, &args.new_string)
    } else {
        content.replacen(&args.old_string, &args.new_string, 1)
    };

    let diff = build_diff(&content, &new_content, &args.path);
    if !ectx
        .permissions
        .confirm_with_diff(
            "edit_file",
            &format!("Apply this edit to {}?", args.path),
            Some(&diff),
        )
        .await
    {
        return Err("user denied edit".to_string());
    }

    fileops::write_atomic(&resolved, &new_content).map_err(|e| e.to_string())?;
    Ok(format!("edited {}", args.path))
}
