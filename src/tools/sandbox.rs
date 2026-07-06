//! Path sandboxing shared by every filesystem tool (SPEC-AGENT §2): every
//! tool operates only inside the process's cwd. Resolves `..` and symlink
//! escapes by canonicalizing both the candidate path and the cwd and
//! comparing the canonical forms -- on Windows `canonicalize()` prefixes
//! `\\?\`, but since *both* sides go through the same call, the prefix
//! cancels out in the comparison.

use std::path::{Path, PathBuf};

/// Resolves `requested` (an absolute or cwd-relative path, possibly not
/// existing yet -- e.g. a new file for `write_file`) against `cwd`,
/// rejecting anything that escapes it. Returns the resolved (not
/// necessarily canonical, since the leaf component may not exist) absolute
/// path on success.
pub fn resolve_in_sandbox(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    let req_path = Path::new(requested);
    let joined = if req_path.is_absolute() {
        req_path.to_path_buf()
    } else {
        cwd.join(req_path)
    };

    // Walk up to the nearest existing ancestor so paths that don't exist
    // yet (new files/directories) can still be sandbox-checked: canonicalize
    // what exists, then re-append the non-existing suffix.
    let mut base = joined.clone();
    let mut suffix_parts: Vec<std::ffi::OsString> = Vec::new();
    while !base.exists() {
        let file_name = base.file_name().map(|n| n.to_os_string());
        match base.parent() {
            Some(parent) => {
                if let Some(name) = file_name {
                    suffix_parts.push(name);
                }
                base = parent.to_path_buf();
            }
            None => break,
        }
    }

    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("invalid path `{requested}`: {e}"))?;
    let canon_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("invalid cwd: {e}"))?;

    let mut full = canon_base;
    for part in suffix_parts.into_iter().rev() {
        full.push(part);
    }

    if !full.starts_with(&canon_cwd) {
        return Err(format!(
            "path `{requested}` escapes the sandbox (must stay within {})",
            cwd.display()
        ));
    }

    Ok(full)
}

/// Renders `path` relative to `cwd` with forward slashes, for tool output
/// (stable across platforms, and matches how the model referred to it).
pub fn display_relative(path: &Path, cwd: &Path) -> String {
    let rel = path.strip_prefix(cwd).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dotdot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir(cwd.join("sub")).unwrap();
        let err = resolve_in_sandbox(cwd, "../outside.txt").unwrap_err();
        assert!(err.contains("escapes"));
    }

    #[test]
    fn rejects_absolute_path_outside_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside_file = other.path().join("x.txt");
        std::fs::write(&outside_file, "x").unwrap();
        let err = resolve_in_sandbox(dir.path(), outside_file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("escapes"));
    }

    #[test]
    fn allows_relative_path_inside_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let resolved = resolve_in_sandbox(dir.path(), "a.txt").unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().join("a.txt").canonicalize().unwrap()
        );
    }

    #[test]
    fn allows_new_file_in_existing_subdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let resolved = resolve_in_sandbox(dir.path(), "sub/new.txt").unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
        assert_eq!(resolved.file_name().unwrap(), "new.txt");
    }

    #[test]
    fn rejects_new_file_under_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_in_sandbox(dir.path(), "../new.txt").unwrap_err();
        assert!(err.contains("escapes"));
    }

    #[test]
    fn dotdot_that_stays_inside_cwd_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        // sub/../a.txt resolves back inside cwd -- must be allowed.
        let resolved = resolve_in_sandbox(dir.path(), "sub/../a.txt").unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().join("a.txt").canonicalize().unwrap()
        );
    }
}
