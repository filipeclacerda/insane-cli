//! `run_command` tool (SPEC-AGENT §2): always confirms, runs via
//! PowerShell (`pwsh`, falling back to `powershell`) on Windows / Bash
//! (falling back to `sh`) on Unix inside the cwd, merges stdout+stderr capped
//! at 32KiB, and kills on timeout (capped at 300s).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::permission::Permissions;

const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_TIMEOUT_SECS: u64 = 300;

pub fn shell_display_name() -> &'static str {
    if cfg!(windows) {
        "PowerShell (pwsh/powershell)"
    } else {
        "Bash (sh fallback)"
    }
}

fn shell_commands(command: &str) -> Vec<Command> {
    if cfg!(windows) {
        ["pwsh", "powershell"]
            .into_iter()
            .map(|program| {
                let mut cmd = Command::new(program);
                cmd.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command,
                ]);
                cmd
            })
            .collect()
    } else {
        let mut bash = Command::new("bash");
        bash.args(["--noprofile", "--norc", "-c", command]);
        let mut sh = Command::new("sh");
        sh.args(["-c", command]);
        vec![bash, sh]
    }
}

#[derive(Deserialize)]
struct RunCommandArgs {
    command: String,
    timeout_secs: Option<u64>,
}

pub async fn run_command(
    arguments: &str,
    cwd: &Path,
    permissions: &mut Permissions,
) -> Result<String, String> {
    let args: RunCommandArgs =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;

    if !permissions.confirm_command(&args.command).await {
        return Err("user denied command execution".to_string());
    }

    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(MAX_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );

    let mut child = None;
    let mut spawn_errors = Vec::new();
    for mut cmd in shell_commands(&args.command) {
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        match cmd.spawn() {
            Ok(spawned) => {
                child = Some(spawned);
                break;
            }
            Err(err) => spawn_errors.push(err.to_string()),
        }
    }
    let mut child = child.ok_or_else(|| {
        format!(
            "failed to start {}: {}",
            shell_display_name(),
            spawn_errors.join("; ")
        )
    })?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let mut merged = stdout_task.await.unwrap_or_default();
            merged.extend(stderr_task.await.unwrap_or_default());
            let mut truncated = false;
            if merged.len() > MAX_OUTPUT_BYTES {
                merged.truncate(MAX_OUTPUT_BYTES);
                truncated = true;
            }
            let mut text = String::from_utf8_lossy(&merged).into_owned();
            if truncated {
                text.push_str("\n...[output truncated at 32KiB]");
            }
            Ok(format!(
                "exit_code: {}\n{text}",
                status.code().unwrap_or(-1)
            ))
        }
        Ok(Err(e)) => Err(format!("failed to run command: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(format!(
                "command timed out after {}s and was killed",
                timeout.as_secs()
            ))
        }
    }
}
