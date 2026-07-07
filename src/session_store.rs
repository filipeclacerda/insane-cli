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
struct LegacyStoredSession {
    /// Schema version, for forward-compatible migrations later.
    version: u32,
    model: String,
    /// History *without* the leading system message (regenerated on load).
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    model: String,
    /// History *without* the leading system message (regenerated on load).
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSessions {
    /// Schema version, for forward-compatible migrations later.
    version: u32,
    #[serde(default)]
    last_model: Option<String>,
    #[serde(default)]
    sessions: Vec<StoredSession>,
}

const STORE_VERSION: u32 = 2;
const MAX_SESSIONS: usize = 3;

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

/// Persists just the model preference for `provider`, preserving any saved
/// resumable history if one exists.
pub fn save_model(provider: &str, model: &str) {
    let mut stored = load_store(provider).unwrap_or_else(empty_store);
    stored.last_model = Some(model.to_string());
    if let Err(e) = write_store(provider, &stored) {
        tracing::warn!("could not save model preference: {e}");
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
    let mut stored = load_store(provider).unwrap_or_else(empty_store);
    stored.last_model = Some(model.to_string());
    if !messages.is_empty() {
        let session = StoredSession {
            model: model.to_string(),
            messages,
        };
        if stored
            .sessions
            .first()
            .map(|latest| is_continuation(latest, &session))
            .unwrap_or(false)
        {
            stored.sessions[0] = session;
        } else {
            stored.sessions.insert(0, session);
        }
        stored.sessions.truncate(MAX_SESSIONS);
    }
    write_store(provider, &stored)
}

/// Loads the most recently saved session for `provider`, if any. Returns
/// `None` when there's no saved session or it can't be parsed (the user
/// simply starts a fresh chat in that case).
pub fn load(provider: &str) -> Option<LoadedSession> {
    load_at(provider, 0)
}

pub fn load_at(provider: &str, index: usize) -> Option<LoadedSession> {
    load_store(provider)?
        .sessions
        .into_iter()
        .nth(index)
        .map(LoadedSession::from)
}

pub fn list(provider: &str) -> Vec<SessionSummary> {
    load_store(provider)
        .map(|stored| {
            stored
                .sessions
                .into_iter()
                .take(MAX_SESSIONS)
                .enumerate()
                .map(|(index, session)| SessionSummary {
                    index,
                    model: session.model,
                    messages: session.messages.len(),
                    preview: preview(&session.messages),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_store(provider: &str) -> Option<StoredSessions> {
    let path = session_path(provider);
    let text = std::fs::read_to_string(&path).ok()?;
    parse_store(&text)
}

fn parse_store(text: &str) -> Option<StoredSessions> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
    if version == 1 {
        let legacy: LegacyStoredSession = serde_json::from_value(value).ok()?;
        return Some(StoredSessions {
            version: STORE_VERSION,
            last_model: Some(legacy.model.clone()),
            sessions: vec![StoredSession {
                model: legacy.model,
                messages: legacy.messages,
            }],
        });
    }
    let stored: StoredSessions = serde_json::from_value(value).ok()?;
    if stored.version != STORE_VERSION {
        tracing::warn!(
            "ignoring saved session with unsupported version {}",
            stored.version
        );
        return None;
    }
    Some(stored)
}

fn empty_store() -> StoredSessions {
    StoredSessions {
        version: STORE_VERSION,
        last_model: None,
        sessions: Vec::new(),
    }
}

fn write_store(provider: &str, stored: &StoredSessions) -> std::io::Result<()> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string(stored).unwrap_or_default();
    std::fs::write(session_path(provider), text)
}

fn is_continuation(old: &StoredSession, new: &StoredSession) -> bool {
    old.model == new.model
        && !old.messages.is_empty()
        && old.messages.len() <= new.messages.len()
        && old
            .messages
            .iter()
            .zip(&new.messages)
            .all(|(a, b)| serde_json::to_value(a).ok() == serde_json::to_value(b).ok())
}

fn preview(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .map(|text| {
            let mut short: String = text.chars().take(80).collect();
            if text.chars().count() > 80 {
                short.push_str("...");
            }
            short.replace('\n', " ")
        })
        .unwrap_or_else(|| "(sem mensagem de usuario)".to_string())
}

/// Loads the last model used with `provider`, without restoring its chat
/// history.
pub fn last_model(provider: &str) -> Option<String> {
    load_store(provider).and_then(|stored| {
        stored.last_model.or_else(|| {
            stored
                .sessions
                .into_iter()
                .next()
                .map(|session| session.model)
        })
    })
}

/// The result of [`load`]: the model and non-system history to restore.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

impl From<StoredSession> for LoadedSession {
    fn from(value: StoredSession) -> Self {
        LoadedSession {
            model: value.model,
            messages: value.messages,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// Zero-based index for [`load_at`].
    pub index: usize,
    pub model: String,
    pub messages: usize,
    pub preview: String,
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
    fn save_model_updates_preference_without_rewriting_saved_session() {
        let provider = "__insane_test_provider_save_model__";
        let history = vec![ChatMessage::text("user", "hello")];
        try_save(provider, "old-model", &history).unwrap();

        save_model(provider, "new-model");

        let loaded = load(provider).unwrap();
        assert_eq!(loaded.model, "old-model");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content.as_deref(), Some("hello"));
        assert_eq!(last_model(provider).as_deref(), Some("new-model"));
        let _ = clear(provider);
    }

    #[test]
    fn keeps_only_three_most_recent_distinct_sessions() {
        let provider = "__insane_test_provider_three_sessions__";
        let _ = clear(provider);

        for idx in 1..=4 {
            try_save(
                provider,
                "m",
                &[ChatMessage::text("user", format!("session {idx}"))],
            )
            .unwrap();
        }

        let sessions = list(provider);
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].preview, "session 4");
        assert_eq!(sessions[1].preview, "session 3");
        assert_eq!(sessions[2].preview, "session 2");
        assert_eq!(
            load_at(provider, 2).unwrap().messages[0].content.as_deref(),
            Some("session 2")
        );
        let _ = clear(provider);
    }

    #[test]
    fn saving_continuation_updates_latest_session_instead_of_prepending() {
        let provider = "__insane_test_provider_continuation__";
        let _ = clear(provider);

        try_save(provider, "m", &[ChatMessage::text("user", "hello")]).unwrap();
        try_save(
            provider,
            "m",
            &[
                ChatMessage::text("user", "hello"),
                ChatMessage::text("assistant", "hi"),
            ],
        )
        .unwrap();

        let sessions = list(provider);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages, 2);
        let _ = clear(provider);
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
