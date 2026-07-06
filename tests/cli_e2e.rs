//! End-to-end CLI behavior via `assert_cmd`, against the mock NIM server
//! where network access is needed. Never touches the real NVIDIA API or a
//! real API key.

#[path = "common/mod.rs"]
mod common;

use assert_cmd::Command;
use common::{EndpointMode, MockServer};
use predicates::prelude::*;

const FAKE_KEY: &str = "nvapi-test-fake-key-000";

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
// --help
// ---------------------------------------------------------------------

#[test]
fn top_level_help_succeeds() {
    let mut cmd = insane_cmd();
    cmd.arg("--help");
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.success().stdout(predicate::str::contains("insane"));
}

#[test]
fn every_subcommand_help_succeeds() {
    for cmd in [
        "ask", "chat", "explain", "review", "fix", "refactor", "test", "config", "models",
        "status", "doctor",
    ] {
        let mut c = insane_cmd();
        c.args([cmd, "--help"]);
        let Some(assert) = common::assert_or_skip(c) else {
            return;
        };
        assert.success();
    }
}

#[test]
fn config_subcommand_help_succeeds() {
    for cmd in [
        "get",
        "set",
        "list",
        "path",
        "set-key",
        "unset-key",
        "cache-clear",
    ] {
        let mut c = insane_cmd();
        c.args(["config", cmd, "--help"]);
        let Some(assert) = common::assert_or_skip(c) else {
            return;
        };
        assert.success();
    }
}

// ---------------------------------------------------------------------
// Usage errors (exit 2)
// ---------------------------------------------------------------------

#[test]
fn ask_without_prompt_exits_2() {
    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).arg("ask");
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.code(2);
}

#[test]
fn fix_rollback_without_backup_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.rs");
    std::fs::write(&path, "fn main() {}").unwrap();

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY)
        .args(["fix", path.to_str().unwrap(), "--rollback"]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert.code(2);
}

// ---------------------------------------------------------------------
// Auth (exit 3) -- resilient to a real key possibly sitting in the OS
// keyring on the machine running the tests.
// ---------------------------------------------------------------------

#[test]
fn ask_without_any_api_key_source_fails_auth_or_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    // Point at a dead local port with a short timeout: if this machine's OS
    // keyring happens to hold a real key, the fallback attempt must fail
    // fast locally instead of hitting the real NIM API (this once made the
    // suite stall ~300s on a machine with a keyring key present).
    std::fs::write(
        dir.path().join("config.toml"),
        "base_url = \"http://127.0.0.1:9\"\ntimeout_secs = 2\n",
    )
    .unwrap();

    let mut cmd = insane_cmd();
    cmd.env_remove("NVIDIA_API_KEY").args([
        "--config",
        dir.path().join("config.toml").to_str().unwrap(),
        "ask",
        "hello",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };

    let output = assert.get_output();
    match output.status.code() {
        Some(3) => {} // no key anywhere: expected auth failure
        Some(other) => {
            // A real key happens to be present in this machine's OS keyring;
            // don't fail the suite over environmental state we don't
            // control, but do sanity-check we didn't get a usage error.
            assert_ne!(
                other, 2,
                "expected either exit 3 (no key) or a real attempt, got usage error"
            );
        }
        None => panic!("process terminated by signal"),
    }
}

// ---------------------------------------------------------------------
// ask end-to-end against the mock (text and --json)
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ask_text_end_to_end_against_mock() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).args([
        "--config",
        config_dir.path().join("config.toml").to_str().unwrap(),
        "--no-stream",
        "ask",
        "hello there",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert
        .success()
        .stdout(predicate::str::contains("mock response"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_json_end_to_end_validates_shape() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).args([
        "--config",
        config_dir.path().join("config.toml").to_str().unwrap(),
        "--no-stream",
        "--json",
        "ask",
        "hello there",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    let assert = assert.success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout should be a single JSON object: {e}\n{stdout}"));

    assert!(json.get("response").and_then(|v| v.as_str()).is_some());
    assert!(json.get("model").and_then(|v| v.as_str()).is_some());
    assert!(json.get("usage").is_some());
    assert!(json["usage"].get("total_tokens").is_some());
    assert!(json.get("rate_limiter").is_some());
    assert!(json["rate_limiter"].get("used").is_some());
    assert!(json["rate_limiter"].get("remaining").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_streaming_json_end_to_end() {
    // Default `stream` is true; confirm the streamed path also produces a
    // valid single JSON object under --json (accumulated, per the known
    // SPEC deviation documented in docs/REPORT.md).
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).args([
        "--config",
        config_dir.path().join("config.toml").to_str().unwrap(),
        "--json",
        "ask",
        "hello there",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    let assert = assert.success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout should be a single JSON object: {e}\n{stdout}"));
    assert_eq!(json["response"], "Hello!");
}

// ---------------------------------------------------------------------
// models / status against the mock
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn models_end_to_end_against_mock() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).args([
        "--config",
        config_dir.path().join("config.toml").to_str().unwrap(),
        "models",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert
        .success()
        .stdout(predicate::str::contains("meta/llama-3.3-70b-instruct"));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_end_to_end_against_mock() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY).args([
        "--config",
        config_dir.path().join("config.toml").to_str().unwrap(),
        "--json",
        "status",
    ]);
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert
        .success()
        .stdout(predicate::str::contains("\"api_reachable\":true"));
}

// ---------------------------------------------------------------------
// stdin input (`ask -`)
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ask_reads_prompt_from_stdin() {
    let server = MockServer::start(EndpointMode::Ok, EndpointMode::Ok, true).await;
    let config_dir = config_pointing_at(&server.base_url);

    let mut cmd = insane_cmd();
    cmd.env("NVIDIA_API_KEY", FAKE_KEY)
        .args([
            "--config",
            config_dir.path().join("config.toml").to_str().unwrap(),
            "--no-stream",
            "ask",
            "-",
        ])
        .write_stdin("what does this do?");
    let Some(assert) = common::assert_or_skip(cmd) else {
        return;
    };
    assert
        .success()
        .stdout(predicate::str::contains("mock response"));
}
