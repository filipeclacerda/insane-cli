//! In-process tests for the tool sandbox and filesystem tools (`src/tools/`,
//! SPEC-AGENT §2/§6). Drives `insane_cli::tools` directly against tempdirs --
//! no subprocess, no network, no real API key.
//!
//! Note on what *isn't* tested here: `write_file`/`edit_file`/`run_command`
//! all gate on `Permissions::confirm(...)`, which always refuses when stdin
//! isn't a terminal (never destructive by default -- see
//! `src/tools/permission.rs`). This test binary's stdin is never a TTY, so
//! the "user approved" branch of those three tools is unreachable without
//! either a real terminal or a permission-bypass API that would violate the
//! project's "no global bypass" rule (SPEC-AGENT §3: "`--yolo` NÃO será
//! implementado"). Consistent with the existing unit tests in
//! `src/tools/mod.rs` (e.g. `edit_file_unique_match_succeeds_without_confirmation_prompted_denied`),
//! these tests instead prove the tool reaches the confirmation gate with the
//! *correct* outcome already decided (unique vs. ambiguous vs. not-found)
//! and that a refusal always leaves the filesystem untouched.

use std::path::PathBuf;

use insane_cli::tools::permission::Permissions;
use insane_cli::tools::{fs as tool_fs, sandbox, ToolExecCtx};
use insane_cli::ui::test_support::FakeUi;
use serde_json::json;

fn ctx<'a>(cwd: PathBuf, ignore: &'a [String], perms: &'a mut Permissions) -> ToolExecCtx<'a> {
    ToolExecCtx {
        cwd,
        max_context_bytes: 192 * 1024,
        extra_ignore: ignore,
        permissions: perms,
    }
}

/// Builds a `Permissions` wired to a `FakeUi` that always refuses, so tests
/// never block on a real stdin (even when run from an interactive terminal).
fn deny_perms() -> Permissions {
    Permissions::with_ui(Box::new(FakeUi::deny()))
}

// ---------------------------------------------------------------------
// Sandbox escapes: `..`, absolute paths outside cwd, symlinks.
// ---------------------------------------------------------------------

#[tokio::test]
async fn read_file_rejects_dotdot_escape_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    std::fs::write(outer.path().join("outside.txt"), "outside secret").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": "../outside.txt"}).to_string();
    let err = tool_fs::read_file(&args, &mut c).await.unwrap_err();
    assert!(err.contains("escapes"));
}

#[tokio::test]
async fn read_file_rejects_absolute_path_outside_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    let outside = outer.path().join("outside.txt");
    std::fs::write(&outside, "outside secret").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": outside.to_str().unwrap()}).to_string();
    let err = tool_fs::read_file(&args, &mut c).await.unwrap_err();
    assert!(err.contains("escapes"));
    // Untouched: still readable with its original content.
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "outside secret");
}

#[test]
fn list_files_rejects_dotdot_escape() {
    let dir = tempfile::tempdir().unwrap();
    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": ".."}).to_string();
    let err = tool_fs::list_files(&args, &mut c).unwrap_err();
    assert!(err.contains("escapes"));
}

#[tokio::test]
async fn edit_file_rejects_path_outside_sandbox_and_leaves_target_intact() {
    let dir = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    let outside = outer.path().join("victim.txt");
    std::fs::write(&outside, "do not touch").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({
        "path": outside.to_str().unwrap(),
        "old_string": "not",
        "new_string": "definitely not"
    })
    .to_string();
    let err = tool_fs::edit_file(&args, &mut c).await.unwrap_err();
    assert!(err.contains("escapes"));
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
}

#[cfg(windows)]
#[tokio::test]
async fn read_file_rejects_symlink_escaping_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    let target = outer.path().join("real_secret.txt");
    std::fs::write(&target, "real secret content").unwrap();

    let link = dir.path().join("escape_link.txt");
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        eprintln!(
            "SKIP: creating a symlink requires Developer Mode or admin privilege on this machine"
        );
        return;
    }

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": "escape_link.txt"}).to_string();
    let result = tool_fs::read_file(&args, &mut c).await;
    // The symlink resolves (via canonicalize) outside the sandbox root, so
    // this must be rejected -- never silently followed.
    assert!(
        result.is_err(),
        "symlink escape must be rejected: {result:?}"
    );
    assert!(result.unwrap_err().contains("escapes"));
}

// ---------------------------------------------------------------------
// High-risk filename denylist; .env itself is allowed.
// ---------------------------------------------------------------------

#[tokio::test]
async fn read_file_allows_clean_env_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "APP_MODE=local").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": ".env"}).to_string();
    let out = tool_fs::read_file(&args, &mut c).await.unwrap();
    assert!(out.contains("APP_MODE=local"));
}

// Note: the agent's `read_file` tool only applies the *filename* denylist
// (`check_denylist`); it does not content-scan for secrets or ask a separate
// approval question for read content.

// ---------------------------------------------------------------------
// edit_file: unique / ambiguous / replace_all / not-found.
// ---------------------------------------------------------------------

#[tokio::test]
async fn edit_file_unique_match_reaches_confirmation_and_denial_leaves_file_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unique.txt");
    std::fs::write(&path, "the quick brown fox").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args =
        json!({"path": "unique.txt", "old_string": "quick", "new_string": "slow"}).to_string();
    let err = tool_fs::edit_file(&args, &mut c).await.unwrap_err();
    // Reaching "denied" (not "ambiguous"/"not found") proves the unique
    // match was accepted and only the confirmation step refused it.
    assert!(err.contains("denied"), "expected denial, got: {err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "the quick brown fox"
    );
    assert!(!fileops_backup_exists(&path));
}

#[tokio::test]
async fn edit_file_ambiguous_without_replace_all_errors_before_any_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup.txt");
    std::fs::write(&path, "aa bb aa cc aa").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": "dup.txt", "old_string": "aa", "new_string": "zz"}).to_string();
    let err = tool_fs::edit_file(&args, &mut c).await.unwrap_err();
    assert!(err.contains("ambiguous"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "aa bb aa cc aa");
}

#[tokio::test]
async fn edit_file_replace_all_reaches_confirmation_not_ambiguous_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup2.txt");
    std::fs::write(&path, "aa bb aa cc aa").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({
        "path": "dup2.txt",
        "old_string": "aa",
        "new_string": "zz",
        "replace_all": true
    })
    .to_string();
    let err = tool_fs::edit_file(&args, &mut c).await.unwrap_err();
    // With replace_all, multiple matches must reach the confirmation gate
    // (denied) rather than erroring out as "ambiguous".
    assert!(err.contains("denied"), "expected denial, got: {err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "aa bb aa cc aa");
}

#[tokio::test]
async fn edit_file_old_string_not_found_errors_before_any_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "hello world").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"path": "f.txt", "old_string": "goodbye", "new_string": "x"}).to_string();
    let err = tool_fs::edit_file(&args, &mut c).await.unwrap_err();
    assert!(err.contains("not found"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
}

fn fileops_backup_exists(path: &std::path::Path) -> bool {
    let mut name = path.as_os_str().to_os_string();
    name.push(".insane-bak");
    std::path::PathBuf::from(name).exists()
}

// ---------------------------------------------------------------------
// run_command: non-TTY refuses before spawning anything.
// ---------------------------------------------------------------------

#[tokio::test]
async fn run_command_denied_on_non_tty_never_creates_the_file_it_would_have() {
    use insane_cli::tools::exec;

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("would_exist.txt");
    let command = if cfg!(windows) {
        format!(
            "New-Item -Path '{}' -ItemType File | Out-Null",
            marker.display()
        )
    } else {
        format!("touch '{}'", marker.display())
    };

    let mut perms = deny_perms();
    let args = json!({"command": command}).to_string();
    let err = exec::run_command(&args, dir.path(), &mut perms)
        .await
        .unwrap_err();
    assert!(err.contains("denied"));
    assert!(!marker.exists(), "command must never have run");
}

// ---------------------------------------------------------------------
// list_files: .gitignore respected, cap enforced.
// ---------------------------------------------------------------------

#[test]
fn list_files_respects_hidden_gitignore_and_denylist() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "build/\n*.log\n").unwrap();
    std::fs::create_dir(dir.path().join("build")).unwrap();
    std::fs::write(dir.path().join("build/artifact.o"), "x").unwrap();
    std::fs::write(dir.path().join("debug.log"), "x").unwrap();
    std::fs::write(dir.path().join(".env"), "APP_MODE=local").unwrap();
    std::fs::write(dir.path().join("server.pem"), "key material").unwrap();
    std::fs::write(dir.path().join("src_main.rs"), "fn main() {}").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let out = tool_fs::list_files("{}", &mut c).unwrap();
    assert!(out.contains("src_main.rs"));
    assert!(!out.contains("artifact.o"));
    assert!(!out.contains("debug.log"));
    assert!(!out.contains(".env"));
    assert!(!out.contains("server.pem"));
}

#[test]
fn list_files_caps_entries_even_with_many_files() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..50 {
        std::fs::write(dir.path().join(format!("file_{i:03}.txt")), "x").unwrap();
    }

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"max_entries": 10}).to_string();
    let out = tool_fs::list_files(&args, &mut c).unwrap();
    assert_eq!(out.lines().count(), 10);
}

// ---------------------------------------------------------------------
// search_files: basic match + cap.
// ---------------------------------------------------------------------

#[tokio::test]
async fn search_files_finds_matches_across_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nlet needle = 1;\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "// nothing here\n").unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"pattern": "needle"}).to_string();
    let out = tool_fs::search_files(&args, &mut c).await.unwrap();
    assert!(out.contains("a.rs:2: let needle = 1;"));
    assert!(!out.contains("b.rs"));
}

#[tokio::test]
async fn search_files_caps_results() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for _ in 0..150 {
        content.push_str("needle\n");
    }
    std::fs::write(dir.path().join("many.txt"), content).unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"pattern": "needle", "max_results": 30}).to_string();
    let out = tool_fs::search_files(&args, &mut c).await.unwrap();
    assert_eq!(out.lines().count(), 30);
}

#[tokio::test]
async fn search_files_default_cap_is_100() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for _ in 0..150 {
        content.push_str("needle\n");
    }
    std::fs::write(dir.path().join("many.txt"), content).unwrap();

    let mut perms = deny_perms();
    let ignore: Vec<String> = vec![];
    let mut c = ctx(dir.path().to_path_buf(), &ignore, &mut perms);
    let args = json!({"pattern": "needle"}).to_string();
    let out = tool_fs::search_files(&args, &mut c).await.unwrap();
    assert_eq!(out.lines().count(), 100);
}

// ---------------------------------------------------------------------
// sandbox::resolve_in_sandbox sanity check reused at the tool boundary
// (complements src/tools/sandbox.rs's own unit tests).
// ---------------------------------------------------------------------

#[test]
fn resolve_in_sandbox_allows_paths_that_stay_inside_after_dotdot_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let resolved = sandbox::resolve_in_sandbox(dir.path(), "sub/../a.txt").unwrap();
    assert_eq!(
        resolved.canonicalize().unwrap(),
        dir.path().join("a.txt").canonicalize().unwrap()
    );
}
