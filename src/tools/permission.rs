//! Confirmation prompts for tool execution (SPEC-AGENT §3), routed through
//! an `AgentUi` (SPEC-UX B0) so line mode and the TUI show the same y/n/a
//! semantics through different surfaces (stderr/stdin vs. a modal).
//! Permission state ("always allow") lives for the duration of one chat
//! session.
//!
//! `a` means "always" for a given *tool* (e.g. always allow `write_file`
//! for the rest of this session) -- except for `run_command`, where `a`
//! only remembers the exact command string that was approved, never shell
//! commands in general.

use std::collections::HashSet;

use crate::ui::{AgentUi, ConfirmRequest, Decision, PlainUi};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    /// Ask before edits and shell commands.
    #[default]
    Default,
    /// Pre-approve everything, including shell commands.
    Auto,
    /// Pre-approve file writes/edits, but continue asking for shell commands.
    AcceptEdits,
}

/// Per-session record of which tools/commands the user has already
/// approved with "always".
pub struct Permissions {
    always_tools: HashSet<String>,
    always_commands: HashSet<String>,
    policy: ApprovalPolicy,
    ui: Box<dyn AgentUi>,
}

impl Permissions {
    /// Line-mode permissions: confirmations go through a default `PlainUi`
    /// (stderr/stdin), exactly as before the TUI existed.
    pub fn new() -> Self {
        Permissions::with_ui(Box::new(PlainUi::default()))
    }

    /// Permissions whose confirmations are routed through `ui` (e.g. the
    /// TUI's modal, or a `PlainUi` built from the real `AppContext`).
    pub fn with_ui(ui: Box<dyn AgentUi>) -> Self {
        Permissions {
            always_tools: HashSet::new(),
            always_commands: HashSet::new(),
            policy: ApprovalPolicy::Default,
            ui,
        }
    }

    pub fn set_policy(&mut self, policy: ApprovalPolicy) {
        self.policy = policy;
    }

    pub fn policy(&self) -> ApprovalPolicy {
        self.policy
    }

    /// Confirms a non-shell tool action (`write_file`, `edit_file`, or a
    /// secret-detected `read_file`/`search_files`). `tool` scopes the
    /// "always" answer.
    pub async fn confirm(&mut self, tool: &str, prompt: &str) -> bool {
        self.confirm_with_diff(tool, prompt, None).await
    }

    /// Confirms an action while keeping its explanatory text inside the
    /// same modal as the decision.
    pub async fn confirm_with_details(&mut self, tool: &str, prompt: &str, details: &str) -> bool {
        self.confirm_request(tool, prompt, Some(details), None)
            .await
    }

    /// Like [`confirm`](Self::confirm), but also carries a pre-rendered
    /// unified diff for the UI to show (write/edit confirmations).
    pub async fn confirm_with_diff(
        &mut self,
        tool: &str,
        prompt: &str,
        diff: Option<&str>,
    ) -> bool {
        self.confirm_request(tool, prompt, None, diff).await
    }

    async fn confirm_request(
        &mut self,
        tool: &str,
        prompt: &str,
        details: Option<&str>,
        diff: Option<&str>,
    ) -> bool {
        if matches!(tool, "write_file" | "edit_file") {
            match self.policy {
                ApprovalPolicy::Auto => return true,
                ApprovalPolicy::AcceptEdits => return true,
                ApprovalPolicy::Default => {}
            }
        }
        if self.always_tools.contains(tool) {
            return true;
        }
        let req = ConfirmRequest {
            tool: tool.to_string(),
            prompt: prompt.to_string(),
            details: details.map(str::to_string),
            diff: diff.map(str::to_string),
            command: None,
        };
        match self.ui.confirm(req).await {
            Decision::Yes => true,
            Decision::Always => {
                if !matches!(tool, "read_file" | "search_files") {
                    self.always_tools.insert(tool.to_string());
                }
                true
            }
            Decision::No => false,
        }
    }

    /// Confirms `run_command`. Always prompts (SPEC-AGENT §2/§3); `a` only
    /// remembers this exact command string, never shell commands in
    /// general.
    pub async fn confirm_command(&mut self, command: &str) -> bool {
        if self.policy == ApprovalPolicy::Auto {
            return true;
        }
        if self.always_commands.contains(command) {
            return true;
        }
        let req = ConfirmRequest {
            tool: "run_command".to_string(),
            prompt: "Run this command?".to_string(),
            details: None,
            diff: None,
            command: Some(command.to_string()),
        };
        match self.ui.confirm(req).await {
            Decision::Yes => true,
            Decision::Always => {
                self.always_commands.insert(command.to_string());
                true
            }
            Decision::No => false,
        }
    }

    /// For `/tools`: which tool names have a standing "always allow".
    pub fn always_allowed_tools(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.always_tools.iter().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// For `/tools`: how many distinct `run_command` strings have a
    /// standing "always allow".
    pub fn always_allowed_command_count(&self) -> usize {
        self.always_commands.len()
    }

    /// Routes a non-fatal warning/notice (e.g. "potential secrets
    /// detected...") through the same `AgentUi` used for confirmations, so
    /// it never bypasses the TUI's render loop (SPEC-UX B3/B5).
    pub fn warn(&self, msg: &str) {
        self.ui.warn(msg);
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::FakeUi;

    #[tokio::test]
    async fn non_tty_stdin_always_refuses() {
        // A `FakeUi` that always answers `No` -- tests never touch real stdin.
        let mut p = Permissions::with_ui(Box::new(FakeUi::deny()));
        assert!(!p.confirm("write_file", "write?").await);
        assert!(!p.confirm_command("echo hi").await);
    }

    #[test]
    fn always_tools_starts_empty() {
        let p = Permissions::new();
        assert!(p.always_allowed_tools().is_empty());
        assert_eq!(p.always_allowed_command_count(), 0);
    }

    #[tokio::test]
    async fn accept_edits_skips_file_prompt_but_not_shell_prompt() {
        let mut p = Permissions::with_ui(Box::new(FakeUi::deny()));
        p.set_policy(ApprovalPolicy::AcceptEdits);
        assert!(p.confirm("write_file", "write?").await);
        assert!(p.confirm("edit_file", "edit?").await);
        assert!(!p.confirm_command("echo hi").await);
    }

    #[tokio::test]
    async fn auto_mode_skips_file_and_shell_prompts() {
        let mut p = Permissions::with_ui(Box::new(FakeUi::deny()));
        p.set_policy(ApprovalPolicy::Auto);
        assert!(p.confirm("write_file", "write?").await);
        assert!(p.confirm_command("echo hi").await);
    }
}
