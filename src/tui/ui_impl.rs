//! `AgentUi` implementation backed by the shared `AppState` (SPEC-UX B0/B3).
//! Every method just mutates state and marks it dirty -- rendering happens
//! in the main loop, throttled to ~30fps. `confirm` is the one method that
//! actually waits: it stashes a `PendingConfirm` (with a `oneshot` sender)
//! into the state and awaits the receiver, which the main loop's key
//! handling resolves once the user answers `y`/`n`/`a`/Esc.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::client::Usage;
use crate::ui::{AgentUi, CommandStream, ConfirmRequest, Decision};

use super::app::{AppState, PendingConfirm};

pub struct TuiUi {
    pub state: Arc<Mutex<AppState>>,
}

impl TuiUi {
    pub fn new(state: Arc<Mutex<AppState>>) -> Self {
        TuiUi { state }
    }
}

#[async_trait::async_trait]
impl AgentUi for TuiUi {
    async fn confirm_with_cancel(
        &self,
        req: ConfirmRequest,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Decision {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.state.lock().unwrap();
            st.confirm = Some(PendingConfirm {
                req,
                responder: tx,
                printed: false,
                selected: 0,
            });
            st.dirty = true;
        }
        tokio::select! {
            answer = rx => answer.unwrap_or(Decision::No),
            _ = cancellation.cancelled() => {
                self.cancel_pending_confirmation();
                Decision::Cancelled
            }
        }
    }

    fn tool_trace(&self, name: &str, arguments: &str) {
        let summary = crate::tools::tool_call_label(name, arguments);
        let mut st = self.state.lock().unwrap();
        st.push_tool_running(tool_call_text(name, &summary));
    }

    fn tool_summary(&self, name: &str, arguments: &str, result: &str, elapsed: Duration) {
        let value: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let line = crate::agent::tool_summary_line(name, arguments, result, elapsed);
        let summary = crate::tools::tool_call_label(name, arguments);
        let mut st = self.state.lock().unwrap();
        st.finish_tool(ok, tool_call_text(name, &summary), line);
    }

    fn warn(&self, msg: &str) {
        let mut st = self.state.lock().unwrap();
        st.push_warn(msg.to_string());
    }

    fn stream_text(&self, chunk: &str) {
        let mut st = self.state.lock().unwrap();
        st.push_assistant_chunk(chunk);
    }

    fn stream_thinking(&self, chunk: &str) {
        let mut st = self.state.lock().unwrap();
        st.push_thinking_chunk(chunk);
    }

    fn command_output(&self, stream: CommandStream, chunk: &str) {
        let mut st = self.state.lock().unwrap();
        st.append_command_output(stream, chunk);
    }

    fn cancel_pending_confirmation(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(pending) = st.confirm.take() {
            let _ = pending.responder.send(Decision::No);
            st.dirty = true;
        }
    }

    fn discard_last_assistant_message(&self) {
        let mut st = self.state.lock().unwrap();
        st.discard_last_assistant_message();
    }

    fn replace_last_assistant_message(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        st.replace_last_assistant_message(text);
    }

    fn end_of_stream(&self) {
        let mut st = self.state.lock().unwrap();
        st.start_new_assistant_message_boundary();
    }

    fn spinner_tick(&self, line: &str) {
        let mut st = self.state.lock().unwrap();
        st.status.spinner_line = Some(line.to_string());
        st.dirty = true;
    }

    fn clear_status(&self) {
        let mut st = self.state.lock().unwrap();
        st.status.spinner_line = None;
        st.dirty = true;
    }

    fn turn_summary(
        &self,
        rounds: u32,
        tools_executed: u32,
        usage: Option<&Usage>,
        elapsed: Duration,
    ) {
        let mut st = self.state.lock().unwrap();
        let total = st.set_usage(usage);
        let line = crate::agent::turn_summary_line_with_total(
            rounds,
            tools_executed,
            usage,
            elapsed,
            total,
        );
        st.push_turn_summary(line);
    }
}

fn tool_call_text(name: &str, summary: &str) -> String {
    if summary.trim().is_empty() {
        format!("{name}()")
    } else {
        format!("{name}({summary})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::AgentUi;

    #[tokio::test]
    async fn cancel_pending_confirmation_closes_modal_and_answers_no() {
        let state = Arc::new(Mutex::new(AppState::new(
            "m".into(),
            ".".into(),
            ".".into(),
        )));
        let ui = TuiUi::new(state.clone());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let confirm = ui.confirm_with_cancel(
            ConfirmRequest {
                tool: "run_command".into(),
                prompt: "Run?".into(),
                details: None,
                diff: None,
                command: Some("sleep 1".into()),
            },
            &cancellation,
        );
        tokio::pin!(confirm);
        tokio::select! {
            _ = &mut confirm => panic!("confirmation must wait"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(state.lock().unwrap().confirm.is_some());
        ui.cancel_pending_confirmation();
        assert!(state.lock().unwrap().confirm.is_none());
        assert_eq!(confirm.await, Decision::No);
    }
}
