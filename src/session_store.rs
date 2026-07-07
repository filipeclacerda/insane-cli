//! On-disk persistence of chat sessions so a conversation that just ended
//! can be resumed with `insane chat --continue` (or the `/resume` REPL
//! command). One JSON file per provider profile, stored under the OS
//! cache dir alongside `cache.rs`'s response cache.
//!
//! The persisted shape is intentionally minimal: the model name and the
//! `Vec<ChatMessage>` history. The system prompt is *not* reloaded -- it
//! is regenerated from the live config/cwd on resume (so a config change
//! like `system_prompt_extra` still takes effect), and the leading system
//! message is dropped before saving to avoid leaking stale instructions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::client::ChatMessage;
use crate::config;
use crate::error::ApiError;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    /// Schema version, for forward-compatible migrations later.
    version: u32,
    model: String,
    /// History *without* the leading system message (regenerated on load).
    messages: Vec<ChatMessage>,
}

const STORE_VERSION: u32 = 1;

/// Directory where per-provider session files live.
fn sessions_dir() -> PathBuf {
    config::cache_dir().join("sessions")
}

/// Per-provider file path. Keeping one file per profile means switching
/// providers (which already resets the in-memory session) doesn't
/// silently overwrite another provider's resumable chat.
fn session_path(provider: &str) -> PathBuf {
    // Sanitize the provider name into a safe filename stem.
    let safe: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if safe.is_empty() {
        "default".to_string()
    } else {
        safe
    };
    sessions_dir().join(format!("{stem}.json"))
}

/// Persists `session` to disk so it can be resumed later. The leading
/// `system` message is stripped (it's regenerated on resume). Failures
/// are logged and swallowed -- saving is a best-effort convenience, never
/// load-bearing.
pub fn save(provider: &str, model: &str, history: &[ChatMessage]) {
    if let Err(e) = try_save(provider, model, history) {
        tracing::warn!("could not save session for resume: {e}");
    }
}

fn try_save(provider: &str, model: &str, history: &[ChatMessage]) -> std::io::Result<()> {
    // Drop the leading system message: it's regenerated on resume from
    // the live config, so persisting it would only leak stale
    // instructions (and bloat the file).
    let messages: Vec<ChatMessage> = history
        .iter()
        .skip_while(|m| m.role == "system")
        .cloned()
        .collect();
    let stored = StoredSession {
        version: STORE_VERSION,
        model: model.to_string(),
        messages,
    };
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string(&stored).unwrap_or_default();
    std::fs::write(session_path(provider), text)
}

/// Loads the most recently saved session for `provider`, if any. Returns
/// `None` when there's no saved session or it can't be parsed (the user
/// simply starts a fresh chat in that case).
pub fn load(provider: &str) -> Option<LoadedSession> {
    let path = session_path(provider);
    let text = std::fs::read_to_string(&path).ok()?;
    let stored: StoredSession = serde_json::from_str(&text).ok()?;
    if stored.version != STORE_VERSION {
        tracing::warn!(
            "ignoring saved session with unsupported version {}",
            stored.version
        );
        return None;
    }
    Some(LoadedSession {
        model: stored.model,
        messages: stored.messages,
    })
}

/// The result of [`load`]: the model and non-system history to restore.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

/// Removes the saved session for `provider` (e.g. after `/clear` so a
/// subsequent `--continue` doesn't resurrect a conversation the user
/// explicitly wiped). Best-effort: errors are swallowed.
pub fn clear(provider: &str) -> Result<(), ApiError> {
    let path = session_path(provider);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ApiError::permanent(format!(
            "could not remove saved session: {e}"
        ))),
    }
}

/// Returns `true` if a saved session exists for `provider` (used to decide
/// whether `--continue` has something to load).
pub fn exists(provider: &str) -> bool {
    session_path(provider).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_roundtrips_model_and_history() {
        // Redirect the sessions dir via env override is not supported here,
        // so instead exercise the pure helpers by writing through `try_save`
        // into the real cache dir and cleaning up. To keep this hermetic we
        // rely on `clear` at the end.
        let provider = "__insane_test_provider_save_load__";
        let history = vec![
            ChatMessage::text("system", "you are an agent"),
            ChatMessage::text("user", "hello"),
            ChatMessage::text("assistant", "hi there"),
        ];
        try_save(provider, "model-x", &history).unwrap();
        let loaded = load(provider).expect("session should load");
        assert_eq!(loaded.model, "model-x");
        // System message is stripped on save.
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, "user");
        assert_eq!(loaded.messages[1].role, "assistant");
        let _ = clear(provider);
        assert!(!exists(provider));
    }

    #[test]
    fn load_missing_returns_none() {
        assert!(load("__definitely_not_a_real_provider__").is_none());
    }

    #[test]
    fn clear_missing_is_ok() {
        assert!(clear("__another_nonexistent_provider__").is_ok());
    }

    #[test]
    fn save_strips_leading_system_message() {
        let provider = "__insane_test_provider_strip__";
        let history = vec![
            ChatMessage::text("system", "secret instructions"),
            ChatMessage::text("user", "hi"),
        ];
        try_save(provider, "m", &history).unwrap();
        let loaded = load(provider).unwrap();
        assert!(loaded.messages.iter().all(|m| m.role != "system"));
        let _ = clear(provider);
    }
}
