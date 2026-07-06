//! File mutation primitives shared by `fix`, `refactor`, and `test`: unified
//! diffs, an interactive y/N confirmation, and atomic writes with a
//! `.insane-bak` backup/rollback (SPEC §7).

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use crate::error::ApiError;

/// Prints a unified diff between `old` and `new` to stdout, colored when
/// stdout is a terminal.
pub fn show_diff(old: &str, new: &str, path: &str) {
    let diff = similar::TextDiff::from_lines(old, new);
    let proposed = format!("{path} (proposed)");
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(path, &proposed)
        .to_string();

    if std::io::stdout().is_terminal() {
        for line in unified.lines() {
            if (line.starts_with('+') && !line.starts_with("+++"))
                || line.starts_with("@@")
                || line.starts_with("+++")
            {
                println!("\x1b[32m{line}\x1b[0m");
            } else if (line.starts_with('-') && !line.starts_with("---")) || line.starts_with("---")
            {
                println!("\x1b[31m{line}\x1b[0m");
            } else {
                println!("{line}");
            }
        }
    } else {
        print!("{unified}");
    }
}

/// Prompts on stderr and reads a y/N answer from stdin. Returns `false`
/// (never destructive) if stdin is not a terminal or fails to read.
pub fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();

    if !std::io::stdin().is_terminal() {
        return false;
    }

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Path of the backup file created before overwriting `path`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".insane-bak");
    PathBuf::from(name)
}

/// Writes `content` to `path` atomically: a backup of any existing file is
/// taken first, then a temp file in the same directory is written and
/// renamed into place.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), ApiError> {
    if path.exists() {
        std::fs::copy(path, backup_path(path)).map_err(|e| {
            ApiError::permanent(format!(
                "failed to create backup for {}: {e}",
                path.display()
            ))
        })?;
    }

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        ApiError::permanent(format!(
            "failed to create temp file in {}: {e}",
            dir.display()
        ))
    })?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| ApiError::permanent(format!("failed to write temp file: {e}")))?;
    tmp.persist(path)
        .map_err(|e| ApiError::permanent(format!("failed to save {}: {e}", path.display())))?;
    Ok(())
}

/// Restores `path` from its `.insane-bak` backup, if one exists.
pub fn rollback(path: &Path) -> Result<(), ApiError> {
    let bak = backup_path(path);
    if !bak.exists() {
        return Err(ApiError::Usage {
            message: format!(
                "no backup found for {} (expected {})",
                path.display(),
                bak.display()
            ),
        });
    }
    std::fs::copy(&bak, path).map_err(|e| {
        ApiError::permanent(format!(
            "failed to restore backup for {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_file_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();

        write_atomic(&path, "updated").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "original"
        );
    }

    #[test]
    fn write_atomic_without_existing_file_has_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        write_atomic(&path, "content").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn rollback_restores_original_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();

        write_atomic(&path, "updated").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");

        rollback(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn rollback_without_backup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.txt");
        std::fs::write(&path, "x").unwrap();

        assert!(rollback(&path).is_err());
    }

    #[test]
    fn show_diff_does_not_panic_on_identical_content() {
        show_diff("same\n", "same\n", "file.txt");
    }

    #[test]
    fn confirm_is_false_on_non_terminal_stdin() {
        // The test harness's stdin is not a terminal, so this must not
        // block and must default to the safe answer.
        assert!(!confirm("proceed?"));
    }
}
