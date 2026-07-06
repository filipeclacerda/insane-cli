//! Optional on-disk response cache (SPEC §8). Off by default; only ever
//! consulted for deterministic, non-streaming requests (`ask --cache`,
//! `explain`, `review`), never for interactive `chat`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::{ChatMessage, ChatResponse};
use crate::config::EffectiveConfig;
use crate::error::ApiError;

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    response: ChatResponse,
    stored_at_secs: u64,
}

/// Computes the cache key: `sha256(base_url + model + messages + params)`
/// (SPEC §8).
pub fn compute_key(
    base_url: &str,
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_url.as_bytes());
    hasher.update([0u8]);
    hasher.update(model.as_bytes());
    hasher.update([0u8]);
    if let Ok(json) = serde_json::to_string(messages) {
        hasher.update(json.as_bytes());
    }
    hasher.update([0u8]);
    hasher.update(format!("{temperature}:{max_tokens}").as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Cache {
    dir: PathBuf,
    ttl: Option<Duration>,
}

impl Cache {
    pub fn new(dir: PathBuf, ttl_secs: Option<u64>) -> Self {
        Cache {
            dir,
            ttl: ttl_secs.map(Duration::from_secs),
        }
    }

    pub fn from_config(cfg: &EffectiveConfig) -> Self {
        Cache::new(crate::config::cache_dir(), cfg.cache.ttl_secs)
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Returns the cached response for `key`, if present and not expired.
    pub fn get(&self, key: &str) -> Option<ChatResponse> {
        let text = std::fs::read_to_string(self.entry_path(key)).ok()?;
        let entry: CacheEntry = serde_json::from_str(&text).ok()?;
        if let Some(ttl) = self.ttl {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            if now.saturating_sub(entry.stored_at_secs) > ttl.as_secs() {
                return None;
            }
        }
        Some(entry.response)
    }

    /// Stores `response` under `key`. Failures are logged and swallowed --
    /// the cache is a best-effort optimization, never load-bearing.
    pub fn put(&self, key: &str, response: &ChatResponse) {
        if let Err(e) = self.try_put(key, response) {
            tracing::warn!("failed to write cache entry: {e}");
        }
    }

    fn try_put(&self, key: &str, response: &ChatResponse) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let stored_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = CacheEntry {
            response: response.clone(),
            stored_at_secs,
        };
        let text = serde_json::to_string(&entry).unwrap_or_default();
        std::fs::write(self.entry_path(key), text)
    }

    /// Removes every cached entry (`config cache-clear`).
    pub fn clear(&self) -> Result<(), ApiError> {
        if !self.dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.dir)
            .map_err(|e| ApiError::permanent(format!("failed to clear cache: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChatChoice, Usage};

    fn sample_response(content: &str) -> ChatResponse {
        ChatResponse {
            id: "id-1".to_string(),
            choices: vec![ChatChoice {
                message: ChatMessage::text("assistant", content),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Usage::default(),
        }
    }

    #[test]
    fn compute_key_is_deterministic() {
        let messages = vec![ChatMessage::text("user", "hi")];
        let a = compute_key("https://x/v1", "m", &messages, 0.7, 100);
        let b = compute_key("https://x/v1", "m", &messages, 0.7, 100);
        assert_eq!(a, b);
    }

    #[test]
    fn compute_key_differs_by_model() {
        let messages = vec![ChatMessage::text("user", "hi")];
        let a = compute_key("https://x/v1", "model-a", &messages, 0.7, 100);
        let b = compute_key("https://x/v1", "model-b", &messages, 0.7, 100);
        assert_ne!(a, b);
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf(), None);
        assert!(cache.get("missing").is_none());
    }

    #[test]
    fn put_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf(), None);
        let resp = sample_response("hello there");
        cache.put("key1", &resp);

        let fetched = cache.get("key1").expect("entry should be present");
        assert_eq!(fetched.content(), "hello there");
    }

    #[test]
    fn expired_entry_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf(), Some(1));

        // Write an entry stamped far in the past so it is expired
        // regardless of test execution speed.
        let entry = CacheEntry {
            response: sample_response("stale"),
            stored_at_secs: 0,
        };
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("key2.json"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();

        assert!(cache.get("key2").is_none());
    }

    #[test]
    fn clear_removes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf(), None);
        cache.put("key3", &sample_response("x"));
        assert!(cache.get("key3").is_some());

        cache.clear().unwrap();
        assert!(cache.get("key3").is_none());
    }
}
