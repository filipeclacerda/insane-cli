//! Atomic writes + backup/rollback round-trip (`fileops.rs`) and secret
//! redaction end-to-end (`error::redact` + `secrets::redact`).

#[path = "common/mod.rs"]
mod common;

use std::io::Write as _;

use assert_cmd::Command;
use common::{EndpointMode, MockServer};
use insane_cli::fileops;

fn insane_cmd() -> Command {
    Command::cargo_bin("insane").expect("binary should build")
}

fn config_pointing_at(base_url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("base_url = \"{base_url}\"\n"),
    )
    .unwrap();
    dir
}

// ---------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------

#[test]
fn write_atomic_never_leaves_partial_content_for_large_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.txt");

    // A few MB of deterministic, verifiable content.
    let mut content = String::new();
    for i in 0..200_000usize {
        content.push_str(&format!("line-{i:08}\n"));
    }

    fileops::write_atomic(&path, &content).unwrap();

    let read_back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        read_back.len(),
        content.len(),
        "content must be all-or-nothing"
    );
    assert_eq!(read_back, content);
    // No stray temp files should remain in the directory (rename replaces
    // in place; `tempfile::NamedTempFile::persist` cleans up on success).
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != "big.txt")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp/partial files should remain: {leftovers:?}"
    );
}

#[test]
fn repeated_atomic_writes_each_fully_replace_the_previous_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");

    for i in 0..20 {
        let content = "x".repeat(1000 + i * 137);
        fileops::write_atomic(&path, &content).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}

// ---------------------------------------------------------------------
// Backup + rollback round trip
// ---------------------------------------------------------------------

#[test]
fn backup_and_rollback_round_trip_multiple_generations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("code.rs");

    std::fs::write(&path, "version 1").unwrap();
    fileops::write_atomic(&path, "version 2").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version 2");
    assert_eq!(
        std::fs::read_to_string(fileops::backup_path(&path)).unwrap(),
        "version 1"
    );

    fileops::rollback(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version 1");

    // Writing again after a rollback creates a fresh backup of the
    // just-restored content, and the cycle continues to round-trip cleanly.
    fileops::write_atomic(&path, "version 3").unwrap();
    assert_eq!(
        std::fs::read_to_string(fileops::backup_path(&path)).unwrap(),
        "version 1"
    );
    fileops::rollback(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version 1");
}

#[test]
fn fix_rollback_without_backup_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nobak.rs");
    std::fs::write(&path, "content").unwrap();

    let mut cmd = insane_cmd();
    cmd.env_remove("NVIDIA_API_KEY")
        .env("NVIDIA_API_KEY", "nvapi-test-fake-key-000")
        .args(["fix", path.to_str().unwrap(), "--rollback"]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.code(2);
}

// ---------------------------------------------------------------------
// Redaction, end-to-end
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn api_key_never_survives_in_stderr_when_upstream_echoes_it_back() {
    // The mock deliberately echoes the Authorization header value into its
    // error body -- simulating a (hypothetical) upstream error message that
    // leaks the credential -- so this test actually exercises the redaction
    // path instead of vacuously passing because the key was never present.
    let server = MockServer::start(
        EndpointMode::EchoAuthInError { status: 400 },
        EndpointMode::Ok,
        true,
    )
    .await;

    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("base_url = \"{}\"\n", server.base_url),
    )
    .unwrap();

    let fake_key = "nvapi-test-fake-key-000";
    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", fake_key).args([
        "--config",
        config_path.to_str().unwrap(),
        "--no-stream",
        "ask",
        "hello",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    let assert = assert.failure();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stderr.contains(fake_key) && !stdout.contains(fake_key),
        "raw API key leaked into output!\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("nvapi-***"),
        "expected redacted placeholder in stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------
// Secret-like content in `ask -f` no longer opens a confirmation prompt.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ask_dash_f_with_secret_like_content_does_not_abort() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("has_secret.env");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "AWS_KEY=AKIAABCDEFGHIJKLMNOP").unwrap();
    drop(f);

    let mut cmd = insane_cmd();
    cmd.current_dir(dir.path())
        .env("NVIDIA_API_KEY", "nvapi-test-fake-key-000")
        .args([
            "--config",
            config_dir.path().join("config.toml").to_str().unwrap(),
            "--no-stream",
            "ask",
            "explain this",
            "-f",
            path.file_name().unwrap().to_str().unwrap(),
        ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.success();
}

#[test]
fn ask_dash_f_with_denylisted_key_filename_aborts_with_no_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.pem");
    std::fs::write(&path, "not actual key material").unwrap();

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", "nvapi-test-fake-key-000").args([
        "--yes",
        "ask",
        "explain this",
        "-f",
        path.to_str().unwrap(),
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.failure();
}
