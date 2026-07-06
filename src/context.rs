//! Context assembly for commands that read source files (`ask -f`,
//! `explain`, `review`, `fix`, `refactor`, `test`): selective reads bounded
//! by `max_context_bytes`, `--lines A:B` slicing, `.gitignore`/config
//! filtering, and high-risk filename blocking before content is handed to a
//! command's prompt builder.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::error::ApiError;

/// Fixed denylist for high-risk key/certificate filenames. `.env*` patterns
/// are intentionally allowed.
const DENYLIST: &[&str] = &[
    "*.pem",
    "*.key",
    "id_rsa*",
    "*.pfx",
    "credentials*",
    "secrets*",
];

/// Result of loading a single source (a real file or `-` for stdin).
pub struct LoadedFile {
    pub display_path: String,
    pub content: String,
    /// Whether `content` was cut short by `max_context_bytes`. A warning is
    /// already emitted to stderr when this happens; kept on the struct for
    /// callers that want to surface it too (e.g. future `--json` output).
    #[allow(dead_code)]
    pub truncated: bool,
}

/// Parses a `--lines A:B` value into an inclusive, 1-indexed `(start, end)`
/// range.
pub fn parse_lines_arg(spec: &str) -> Result<(usize, usize), ApiError> {
    let invalid = || ApiError::Usage {
        message: format!("invalid --lines value `{spec}`, expected A:B (e.g. 10:42)"),
    };
    let (a, b) = spec.split_once(':').ok_or_else(invalid)?;
    let start: usize = a.trim().parse().map_err(|_| invalid())?;
    let end: usize = b.trim().parse().map_err(|_| invalid())?;
    if start == 0 || end < start {
        return Err(invalid());
    }
    Ok((start, end))
}

pub(crate) fn slice_lines(text: &str, range: Option<(usize, usize)>) -> String {
    match range {
        None => text.to_string(),
        Some((start, end)) => text
            .lines()
            .enumerate()
            .filter(|(idx, _)| {
                let line_no = idx + 1;
                line_no >= start && line_no <= end
            })
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Truncates `text` to at most `max_bytes` bytes, respecting UTF-8 character
/// boundaries. Returns whether truncation happened.
pub(crate) fn truncate_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn matches_glob(pattern: &str, name: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix('*') {
        name.ends_with(rest)
    } else if let Some(rest) = pattern.strip_suffix('*') {
        name.starts_with(rest)
    } else {
        name.eq_ignore_ascii_case(pattern)
    }
}

/// Rejects paths matching the fixed high-risk filename denylist.
pub fn check_denylist(path: &Path) -> Result<(), ApiError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    for pattern in DENYLIST {
        if matches_glob(pattern, name) {
            return Err(ApiError::Usage {
                message: format!(
                    "{}: filename matches the fixed denylist pattern `{pattern}` \
and cannot be included as context; there is no bypass for this check",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

/// Rejects paths matched by `.gitignore` (under `root`) or the config's
/// extra `ignore` globs.
///
/// Both `root` and the candidate path are canonicalized before matching.
/// This matters because a directory-only pattern (e.g. `build/`) is only
/// checked against the *exact* path handed to `Gitignore::matched`, not its
/// ancestors, unless the root and the candidate agree on prefix form (a bare
/// `strip_prefix` is used internally) -- on Windows, `root` here is
/// typically the un-prefixed cwd while callers like `tools::fs::list_files`
/// hand in an already-canonicalized (`\\?\`-prefixed) path, so without this
/// normalization the prefix never strips and a whole ignored subtree (e.g.
/// `target/`) silently stops being ignored below its top directory. Using
/// `matched_path_or_any_parents` (rather than `matched`) is what actually
/// makes directory-only patterns apply to files nested under them; this was
/// found and fixed while writing `tests/tools_sandbox.rs`
/// (`list_files_respects_gitignore_and_denylist`) -- see `docs/REPORT.md`.
pub fn check_ignored(path: &Path, root: &Path, extra: &[String]) -> Result<(), ApiError> {
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut builder = ignore::gitignore::GitignoreBuilder::new(&canon_root);
    let gitignore_path = canon_root.join(".gitignore");
    if gitignore_path.exists() {
        if let Some(e) = builder.add(&gitignore_path) {
            tracing::warn!("failed to parse {}: {e}", gitignore_path.display());
        }
    }
    for pat in extra {
        if let Err(e) = builder.add_line(None, pat) {
            tracing::warn!("invalid ignore pattern `{pat}`: {e}");
        }
    }
    match builder.build() {
        Ok(gi) => {
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else {
                canon_root.join(path)
            };
            let canon_candidate = candidate.canonicalize().unwrap_or(candidate);
            if gi
                .matched_path_or_any_parents(&canon_candidate, canon_candidate.is_dir())
                .is_ignore()
            {
                return Err(ApiError::Usage {
                    message: format!(
                        "{}: matches .gitignore or the configured `ignore` list; not including in context",
                        path.display()
                    ),
                });
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!("failed to build ignore matcher: {e}");
            Ok(())
        }
    }
}

fn read_stdin_to_string() -> Result<String, ApiError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| ApiError::permanent(format!("failed to read stdin: {e}")))?;
    Ok(buf)
}

/// Maps a file extension to a fenced-code-block language tag.
fn language_for(display_path: &str) -> &'static str {
    let ext = Path::new(display_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" => "bash",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        _ => "",
    }
}

/// Formats a loaded file as a labeled, fenced code block suitable for
/// inclusion in a model prompt.
pub fn format_block(display_path: &str, content: &str) -> String {
    let lang = language_for(display_path);
    format!("File: {display_path}\n```{lang}\n{content}\n```\n")
}

/// Loads a source (a real file path, or `-` for stdin) for use as model
/// context: applies the fixed high-risk filename denylist and `.gitignore`/config ignore
/// filters (skipped for stdin), slices by `--lines` if given, truncates to
/// `max_bytes`, and returns the content.
pub fn load(
    source: &str,
    max_bytes: usize,
    lines: Option<(usize, usize)>,
    extra_ignore: &[String],
    quiet: bool,
) -> Result<LoadedFile, ApiError> {
    let raw = if source == "-" {
        read_stdin_to_string()?
    } else {
        let path = Path::new(source);
        check_denylist(path)?;
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        check_ignored(path, &root, extra_ignore)?;
        let bytes = std::fs::read(path)
            .map_err(|e| ApiError::permanent(format!("failed to read {}: {e}", path.display())))?;
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let sliced = slice_lines(&raw, lines);
    let (text, truncated) = truncate_bytes(&sliced, max_bytes);
    if truncated {
        eprintln!("warning: {source}: truncated to {max_bytes} bytes (max_context_bytes)");
    }

    let _ = quiet;

    Ok(LoadedFile {
        display_path: source.to_string(),
        content: text,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_lines_arg() {
        assert_eq!(parse_lines_arg("10:42").unwrap(), (10, 42));
    }

    #[test]
    fn rejects_malformed_lines_arg() {
        assert!(parse_lines_arg("abc").is_err());
        assert!(parse_lines_arg("10").is_err());
        assert!(parse_lines_arg("0:5").is_err());
        assert!(parse_lines_arg("10:5").is_err());
    }

    #[test]
    fn slices_requested_line_range() {
        let text = "one\ntwo\nthree\nfour\n";
        assert_eq!(slice_lines(text, Some((2, 3))), "two\nthree");
    }

    #[test]
    fn slice_lines_none_returns_whole_text() {
        let text = "one\ntwo\n";
        assert_eq!(slice_lines(text, None), text);
    }

    #[test]
    fn truncates_at_byte_limit() {
        let (text, truncated) = truncate_bytes("hello world", 5);
        assert_eq!(text, "hello");
        assert!(truncated);
    }

    #[test]
    fn does_not_truncate_under_limit() {
        let (text, truncated) = truncate_bytes("hi", 100);
        assert_eq!(text, "hi");
        assert!(!truncated);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        let s = "a\u{00e9}b"; // 'a', 2-byte é, 'b' => 4 bytes total
        let (text, truncated) = truncate_bytes(s, 2);
        assert!(text.len() <= 2);
        assert!(truncated);
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn denylist_allows_env_files_but_blocks_key_material() {
        assert!(check_denylist(Path::new(".env")).is_ok());
        assert!(check_denylist(Path::new(".env.local")).is_ok());
        assert!(check_denylist(Path::new("id_rsa")).is_err());
        assert!(check_denylist(Path::new("id_rsa.pub")).is_err());
        assert!(check_denylist(Path::new("server.pem")).is_err());
        assert!(check_denylist(Path::new("app.key")).is_err());
        assert!(check_denylist(Path::new("cert.pfx")).is_err());
        assert!(check_denylist(Path::new("credentials.json")).is_err());
        assert!(check_denylist(Path::new("secrets.yaml")).is_err());
    }

    #[test]
    fn denylist_allows_normal_files() {
        assert!(check_denylist(Path::new("main.rs")).is_ok());
        assert!(check_denylist(Path::new("src/lib.rs")).is_ok());
    }

    #[test]
    fn gitignore_blocks_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "secret stuff").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "fine").unwrap();

        assert!(check_ignored(Path::new("ignored.txt"), dir.path(), &[]).is_err());
        assert!(check_ignored(Path::new("kept.txt"), dir.path(), &[]).is_ok());
    }

    #[test]
    fn extra_ignore_globs_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scratch.tmp"), "x").unwrap();
        let extra = vec!["*.tmp".to_string()];
        assert!(check_ignored(Path::new("scratch.tmp"), dir.path(), &extra).is_err());
    }

    #[test]
    fn format_block_includes_path_and_fence() {
        let block = format_block("src/main.rs", "fn main() {}");
        assert!(block.contains("src/main.rs"));
        assert!(block.contains("```rust"));
        assert!(block.contains("fn main() {}"));
    }

    #[test]
    fn language_for_unknown_extension_is_empty() {
        assert_eq!(language_for("Makefile"), "");
    }
}
