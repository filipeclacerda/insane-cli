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
use crate::ui::{AgentUi, CommandStream};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_RESULT_PREFIX_BYTES: usize = "exit_code: -2147483648\n".len();
const MAX_CAPTURE_BYTES: usize = MAX_OUTPUT_BYTES - MAX_RESULT_PREFIX_BYTES;
const TRUNCATION_MARKER: &[u8] = b"\n...[output truncated at 32KiB]";
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(250);
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTermination {
    Exited(i32),
    Cancelled,
    TimedOut,
}

impl CommandTermination {
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Cancelled | Self::TimedOut => None,
        }
    }
}

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
    let cancellation = CancellationToken::new();
    run_command_with_cancellation(arguments, cwd, permissions, &cancellation, None).await
}

pub async fn run_command_with_services(
    arguments: &str,
    cwd: &Path,
    permissions: &mut Permissions,
    cancellation: &CancellationToken,
    ui: &dyn AgentUi,
) -> Result<String, String> {
    run_command_with_cancellation(arguments, cwd, permissions, cancellation, Some(ui)).await
}

async fn run_command_with_cancellation(
    arguments: &str,
    cwd: &Path,
    permissions: &mut Permissions,
    cancellation: &CancellationToken,
    ui: Option<&dyn AgentUi>,
) -> Result<String, String> {
    let args: RunCommandArgs =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;

    match permissions
        .confirm_command(&args.command, cancellation)
        .await
    {
        crate::tools::permission::PermissionResult::Allowed => {}
        crate::tools::permission::PermissionResult::Denied => {
            return Err("user denied command execution".to_string())
        }
        crate::tools::permission::PermissionResult::Cancelled => {
            return Err("cancelled by user".to_string())
        }
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
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_buf = [0_u8; 4096];
    let mut stderr_buf = [0_u8; 4096];
    let timeout_sleep = tokio::time::sleep(timeout);
    tokio::pin!(timeout_sleep);
    let drain_deadline = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(drain_deadline);
    let mut final_output = Vec::new();
    let mut output_truncated = false;
    let mut redactors = StreamRedactors::default();
    let mut exited = None;
    let termination = loop {
        if let Some(termination) = exited {
            if !stdout_open && !stderr_open {
                break termination;
            }
        }

        tokio::select! {
            status = child.wait(), if exited.is_none() => {
                let status = status.map_err(|e| format!("failed to run command: {e}"))?;
                let termination = CommandTermination::Exited(status.code().unwrap_or(-1));
                exited = Some(termination);
                drain_deadline.as_mut().reset(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
            },
            _ = cancellation.cancelled(), if exited.is_none() => break CommandTermination::Cancelled,
            _ = &mut timeout_sleep, if exited.is_none() => break CommandTermination::TimedOut,
            read = stdout_pipe.read(&mut stdout_buf), if stdout_open => match read {
                Ok(0) => stdout_open = false,
                Ok(read) => {
                    let overflowed = redactors.push(CommandStream::Stdout, &stdout_buf[..read], |text| {
                        append_redacted_text(&mut final_output, &mut output_truncated, CommandStream::Stdout, text, ui)
                    });
                    if overflowed {
                        append_truncation_marker(&mut final_output, &mut output_truncated, ui);
                    }
                    if exited.is_some() {
                        drain_deadline.as_mut().reset(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
                    }
                }
                Err(_) => {
                    stdout_open = false;
                    append_truncation_marker(&mut final_output, &mut output_truncated, ui);
                }
            },
            read = stderr_pipe.read(&mut stderr_buf), if stderr_open => match read {
                Ok(0) => stderr_open = false,
                Ok(read) => {
                    let overflowed = redactors.push(CommandStream::Stderr, &stderr_buf[..read], |text| {
                        append_redacted_text(&mut final_output, &mut output_truncated, CommandStream::Stderr, text, ui)
                    });
                    if overflowed {
                        append_truncation_marker(&mut final_output, &mut output_truncated, ui);
                    }
                    if exited.is_some() {
                        drain_deadline.as_mut().reset(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
                    }
                }
                Err(_) => {
                    stderr_open = false;
                    append_truncation_marker(&mut final_output, &mut output_truncated, ui);
                }
            },
            _ = &mut drain_deadline, if exited.is_some() && (stdout_open || stderr_open) => {
                append_truncation_marker(&mut final_output, &mut output_truncated, ui);
                break exited.expect("exit status was set before draining");
            },
        }
    };

    if matches!(
        termination,
        CommandTermination::Cancelled | CommandTermination::TimedOut
    ) {
        terminate_child(&mut child).await;
    }
    redactors.finish(|stream, text| {
        append_redacted_text(&mut final_output, &mut output_truncated, stream, text, ui)
    });

    match termination {
        CommandTermination::Exited(code) => Ok(format!(
            "exit_code: {code}\n{}",
            String::from_utf8_lossy(&final_output)
        )),
        CommandTermination::Cancelled => Err("command cancelled by user".to_string()),
        CommandTermination::TimedOut => Err(format!(
            "command timed out after {}s and was killed",
            timeout.as_secs()
        )),
    }
}

const MAX_RECORD_BYTES: usize = MAX_CAPTURE_BYTES;

#[derive(Default)]
struct StreamRedactors {
    stdout: StreamRedactor,
    stderr: StreamRedactor,
}

impl StreamRedactors {
    /// Emits only complete, bounded records. `true` means an unterminated
    /// record crossed the safety limit and was discarded without exposing it.
    fn push(&mut self, stream: CommandStream, bytes: &[u8], mut emit: impl FnMut(&str)) -> bool {
        match stream {
            CommandStream::Stdout => self.stdout.push(bytes, &mut emit),
            CommandStream::Stderr => self.stderr.push(bytes, &mut emit),
        }
    }

    fn finish(&mut self, mut emit: impl FnMut(CommandStream, &str)) {
        self.stdout
            .finish(&mut |text| emit(CommandStream::Stdout, text));
        self.stderr
            .finish(&mut |text| emit(CommandStream::Stderr, text));
    }
}

#[derive(Default)]
struct StreamRedactor {
    pending: Vec<u8>,
    discarding_oversized_record: bool,
}

impl StreamRedactor {
    /// Never exposes a byte of an unterminated record.  This makes unlimited
    /// secret patterns safe across arbitrary pipe-read boundaries.
    fn push(&mut self, bytes: &[u8], emit: &mut dyn FnMut(&str)) -> bool {
        let mut overflowed = false;
        for byte in bytes {
            if self.discarding_oversized_record {
                if *byte == b'\n' {
                    self.discarding_oversized_record = false;
                }
                continue;
            }

            self.pending.push(*byte);
            if self.pending.len() > MAX_RECORD_BYTES {
                self.pending.clear();
                self.discarding_oversized_record = true;
                overflowed = true;
            } else if *byte == b'\n' {
                self.flush_pending(emit);
            }
        }
        overflowed
    }

    fn finish(&mut self, emit: &mut dyn FnMut(&str)) {
        if !self.discarding_oversized_record {
            self.flush_pending(emit);
        }
    }

    fn flush_pending(&mut self, emit: &mut dyn FnMut(&str)) {
        let valid_len = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid_len == 0 {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending[..valid_len]).into_owned();
        self.pending.drain(..valid_len);
        emit(&text);
    }
}

fn append_truncation_marker(
    final_output: &mut Vec<u8>,
    output_truncated: &mut bool,
    ui: Option<&dyn AgentUi>,
) {
    if *output_truncated {
        return;
    }
    *output_truncated = true;
    let marker = std::str::from_utf8(TRUNCATION_MARKER).expect("marker is UTF-8");
    if let Some(ui) = ui {
        ui.command_output(CommandStream::Stderr, marker);
    }
    final_output.extend_from_slice(TRUNCATION_MARKER);
}

fn append_redacted_text(
    final_output: &mut Vec<u8>,
    output_truncated: &mut bool,
    stream: CommandStream,
    text: &str,
    ui: Option<&dyn AgentUi>,
) {
    let text = crate::secrets::redact(&crate::error::redact(text));
    if let Some(ui) = ui {
        ui.command_output(stream, &text);
    }
    if *output_truncated {
        return;
    }
    // Preserve room for the marker before accepting a partial final chunk.
    // The returned `exit_code:` line was reserved above as well, so the full
    // tool result never crosses the 32KiB contract.
    let remaining = MAX_CAPTURE_BYTES.saturating_sub(final_output.len() + TRUNCATION_MARKER.len());
    let kept = &text.as_bytes()[..text.floor_char_boundary(remaining)];
    final_output.extend_from_slice(kept);
    if kept.len() < text.len() {
        append_truncation_marker(final_output, output_truncated, ui);
    }
}

async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    {
        // `Child::kill` ends only the shell we spawned. PowerShell commands
        // commonly start builds, servers, or helper processes, so terminate
        // the complete process tree before waiting for the shell to reap.
        // If taskkill itself is unavailable or fails, retain the direct-kill
        // fallback rather than leaving the shell alive.
        let tree_killed = if let Some(pid) = child.id() {
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|status| status.success())
        } else {
            false
        };
        if !tree_killed {
            let _ = child.kill().await;
        }
    }

    #[cfg(not(windows))]
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    static COMMAND_TEST_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    async fn lock_command_test() -> tokio::sync::MutexGuard<'static, ()> {
        COMMAND_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct SlowRecordingUi {
        delay: Duration,
        output: std::sync::Mutex<String>,
    }

    impl SlowRecordingUi {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                output: std::sync::Mutex::new(String::new()),
            }
        }

        fn command_output_text(&self) -> String {
            self.output.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentUi for SlowRecordingUi {
        async fn confirm_with_cancel(
            &self,
            _req: crate::ui::ConfirmRequest,
            _cancellation: &CancellationToken,
        ) -> crate::ui::Decision {
            crate::ui::Decision::No
        }
        fn tool_trace(&self, _name: &str, _arguments: &str) {}
        fn tool_summary(&self, _name: &str, _arguments: &str, _result: &str, _elapsed: Duration) {}
        fn warn(&self, _msg: &str) {}
        fn stream_text(&self, _chunk: &str) {}
        fn command_output(&self, _stream: CommandStream, chunk: &str) {
            std::thread::sleep(self.delay);
            self.output.lock().unwrap().push_str(chunk);
        }
        fn end_of_stream(&self) {}
        fn spinner_tick(&self, _line: &str) {}
        fn clear_status(&self) {}
        fn turn_summary(
            &self,
            _rounds: u32,
            _tools_executed: u32,
            _usage: Option<&crate::client::Usage>,
            _elapsed: Duration,
        ) {
        }
    }

    #[tokio::test]
    async fn command_output_is_emitted_before_process_exit() {
        use crate::ui::test_support::{FakeUi, RecordingUi};
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = RecordingUi::new();
        let arguments = serde_json::json!({"command": delayed_output_command()}).to_string();
        let task = run_command_with_services(&arguments, dir.path(), &mut permissions, &token, &ui);
        tokio::pin!(task);

        let observed_first_chunk = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut task => panic!("command finished before emitting output: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
                if ui.command_output_text().contains("one") {
                    break;
                }
            }
        })
        .await;
        assert!(observed_first_chunk.is_ok());
        assert!(tokio::time::timeout(Duration::from_millis(1), &mut task)
            .await
            .is_err());

        let result = task.await.unwrap();
        assert!(result.contains("one"));
        assert!(result.contains("two"));
    }

    #[tokio::test]
    async fn run_command_stops_when_turn_is_cancelled() {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;

        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let cancelled_at = Arc::new(Mutex::new(None));
        let cancelled_at_task = cancelled_at.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            *cancelled_at_task.lock().unwrap() = Some(Instant::now());
            cancellation.cancel();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let ui = FakeUi::deny();
        let result = run_command_with_services(
            &serde_json::json!({
                "command": long_running_command(),
                "timeout_secs": 10,
            })
            .to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await;

        assert_eq!(result.unwrap_err(), "command cancelled by user");
        assert!(cancelled_at.lock().unwrap().unwrap().elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn run_command_preserves_nonzero_exit_code_as_output() {
        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = FakeUi::deny();
        let output = run_command_with_services(
            &serde_json::json!({"command": exit_with_code_command(7)}).to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await
        .unwrap();

        assert!(output.starts_with("exit_code: 7\n"));
    }

    #[tokio::test]
    async fn run_command_preserves_timeout_error() {
        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = FakeUi::deny();
        let error = run_command_with_services(
            &serde_json::json!({
                "command": long_running_command(),
                "timeout_secs": 0,
            })
            .to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "command timed out after 0s and was killed");
    }

    #[tokio::test]
    async fn output_cap_includes_a_single_truncation_marker() {
        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = FakeUi::deny();
        let output = run_command_with_services(
            &serde_json::json!({"command": noisy_output_command()}).to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await
        .unwrap();

        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert_eq!(output.matches("...[output truncated at 32KiB]").count(), 1);
    }

    #[tokio::test]
    async fn cancellation_returns_promptly_when_descendant_keeps_pipes_open() {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;

        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let cancelled_at = Arc::new(Mutex::new(None));
        let cancelled_at_task = cancelled_at.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            *cancelled_at_task.lock().unwrap() = Some(Instant::now());
            cancellation.cancel();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let ui = FakeUi::deny();
        let result = run_command_with_services(
            &serde_json::json!({"command": descendant_holds_pipes_command(), "timeout_secs": 10})
                .to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await;

        assert_eq!(result.unwrap_err(), "command cancelled by user");
        assert!(cancelled_at.lock().unwrap().unwrap().elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancellation_kills_descendant_before_it_can_write() {
        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-survived.txt");
        let token = CancellationToken::new();
        let cancellation = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancellation.cancel();
        });
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let ui = FakeUi::deny();
        let result = run_command_with_services(
            &serde_json::json!({
                "command": descendant_writes_marker_command(&marker),
                "timeout_secs": 10,
            })
            .to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await;

        assert_eq!(result.unwrap_err(), "command cancelled by user");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !marker.exists(),
            "a descendant process survived cancellation and wrote {}",
            marker.display()
        );
    }

    #[tokio::test]
    async fn real_sub_cap_burst_survives_slow_ui_after_child_exit() {
        use crate::ui::test_support::FakeUi;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = SlowRecordingUi::new(Duration::from_millis(150));
        let expected = "x".repeat(24 * 1024);
        let output = run_command_with_services(
            &serde_json::json!({"command": burst_output_command(expected.len())}).to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await
        .unwrap();

        assert!(output.contains(&expected));
        assert_eq!(ui.command_output_text().trim_end(), expected);
        assert!(!output.contains("...[output truncated at 32KiB]"));
        assert!(!ui
            .command_output_text()
            .contains("...[output truncated at 32KiB]"));
    }

    #[tokio::test]
    async fn child_exit_with_descendant_holding_pipes_reports_loss_without_hanging() {
        use crate::ui::test_support::{FakeUi, RecordingUi};
        use std::time::Instant;
        use tokio_util::sync::CancellationToken;

        let _guard = lock_command_test().await;
        let dir = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_ui(Box::new(FakeUi::accept()));
        let token = CancellationToken::new();
        let ui = RecordingUi::new();
        let started = Instant::now();
        let output = run_command_with_services(
            &serde_json::json!({"command": exited_child_with_descendant_holding_pipes_command()})
                .to_string(),
            dir.path(),
            &mut permissions,
            &token,
            &ui,
        )
        .await
        .unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(output.contains("ready"));
        assert!(output.contains("...[output truncated at 32KiB]"));
        assert!(ui.command_output_text().contains("ready"));
        assert!(ui
            .command_output_text()
            .contains("...[output truncated at 32KiB]"));
    }

    #[test]
    fn github_token_split_across_reads_and_utf8_remain_safe_and_intact() {
        use crate::ui::test_support::RecordingUi;

        let secret = format!("ghp_{}", "a".repeat(64));
        let ui = RecordingUi::new();
        let mut output = Vec::new();
        let mut truncated = false;
        let mut redactors = StreamRedactors::default();
        let first_read = format!("before {}{}", "x".repeat(4090), &secret[..8]);
        let _ = redactors.push(CommandStream::Stdout, first_read.as_bytes(), |text| {
            append_redacted_text(
                &mut output,
                &mut truncated,
                CommandStream::Stdout,
                text,
                Some(&ui),
            )
        });
        let _ = redactors.push(CommandStream::Stdout, &secret.as_bytes()[8..], |text| {
            append_redacted_text(
                &mut output,
                &mut truncated,
                CommandStream::Stdout,
                text,
                Some(&ui),
            )
        });
        let _ = redactors.push(CommandStream::Stdout, b" \xc3", |text| {
            append_redacted_text(
                &mut output,
                &mut truncated,
                CommandStream::Stdout,
                text,
                Some(&ui),
            )
        });
        let _ = redactors.push(CommandStream::Stdout, b"\xa9 after", |text| {
            append_redacted_text(
                &mut output,
                &mut truncated,
                CommandStream::Stdout,
                text,
                Some(&ui),
            )
        });
        redactors.finish(|stream, text| {
            append_redacted_text(&mut output, &mut truncated, stream, text, Some(&ui))
        });
        let visible = ui.command_output_text();
        let result = String::from_utf8(output).unwrap();

        assert!(!visible.contains(&secret));
        assert!(!result.contains(&secret));
        assert!(!visible.contains(&secret[..8]));
        assert!(!result.contains(&secret[..8]));
        assert!(visible.contains("before "));
        assert!(result.contains("before "));
        assert!(visible.ends_with("*** é after"));
        assert!(result.ends_with("*** é after"));
        assert!(!visible.contains('\u{fffd}'));
        assert!(!result.contains('\u{fffd}'));
    }

    fn exit_with_code_command(code: i32) -> String {
        format!("exit {code}")
    }

    fn delayed_output_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output one; Start-Sleep -Milliseconds 500; Write-Output two"
        } else {
            "printf 'one\\n'; sleep 0.5; printf 'two\\n'"
        }
    }

    fn noisy_output_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output ('x' * 40000)"
        } else {
            "head -c 40000 /dev/zero | tr '\\0' x"
        }
    }

    fn burst_output_command(bytes: usize) -> String {
        if cfg!(windows) {
            format!("[Console]::Out.Write(('x' * {bytes}) + [Environment]::NewLine)")
        } else {
            format!("head -c {bytes} /dev/zero | tr '\\0' x; printf '\\n'")
        }
    }

    fn descendant_holds_pipes_command() -> &'static str {
        if cfg!(windows) {
            "Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'Start-Sleep -Seconds 5'; Write-Output ready; Start-Sleep -Seconds 10"
        } else {
            "(sleep 5) & printf 'ready\\n'; sleep 10"
        }
    }

    #[cfg(windows)]
    fn descendant_writes_marker_command(marker: &std::path::Path) -> String {
        let marker = marker.display().to_string().replace('\'', "''");
        format!(
            "Start-Process powershell -ArgumentList '-NoProfile', '-Command', \
             'Start-Sleep -Seconds 1; Set-Content -LiteralPath ''{marker}'' -Value survived'; \
             Start-Sleep -Seconds 10"
        )
    }

    fn exited_child_with_descendant_holding_pipes_command() -> &'static str {
        if cfg!(windows) {
            "python -c 'import subprocess; subprocess.Popen([\"cmd\", \"/c\", \"ping -n 6 127.0.0.1 >nul\"]); print(\"ready\")'"
        } else {
            "(sleep 5) & printf 'ready\\n'"
        }
    }

    fn long_running_command() -> &'static str {
        if cfg!(windows) {
            "Start-Sleep -Seconds 10"
        } else {
            "sleep 10"
        }
    }

    #[test]
    fn command_termination_distinguishes_cancel_timeout_and_exit() {
        assert_ne!(CommandTermination::Cancelled, CommandTermination::TimedOut);
        assert_eq!(CommandTermination::Exited(7).exit_code(), Some(7));
        assert_eq!(CommandTermination::Cancelled.exit_code(), None);
    }
}
