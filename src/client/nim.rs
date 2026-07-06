//! NIM / OpenAI-compatible `LlmClient` implementation: single pooled
//! `reqwest::Client`, rate-limited + retried requests, SSE streaming.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;

use super::sse::parse_sse;
use super::{ChatRequest, ChatResponse, ChatStream, LlmClient, ModelInfo};
use crate::error::ApiError;
use crate::limiter::RateLimiter;

const MAX_ATTEMPTS: u32 = 5;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a 400 error body looks like it's complaining about tool/function
/// calling support (SPEC-AGENT §1: surface a clear, permanent hint instead
/// of a raw upstream message).
fn mentions_tool_calling(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("tool") || lower.contains("function"))
        && (lower.contains("support") || lower.contains("not allowed") || lower.contains("invalid"))
}

#[derive(Clone)]
pub struct NimClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    limiter: Arc<RateLimiter>,
}

#[derive(serde::Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

impl NimClient {
    pub fn new(
        base_url: String,
        api_key: String,
        timeout: Duration,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self, ApiError> {
        Self::new_with_auth(base_url, Some(api_key), timeout, limiter)
    }

    pub fn new_with_auth(
        base_url: String,
        api_key: Option<String>,
        timeout: Duration,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| ApiError::permanent(format!("failed to build HTTP client: {e}")))?;
        Ok(NimClient {
            http,
            base_url,
            api_key,
            limiter,
        })
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Computes exponential backoff with base 500ms, factor 2, capped at
    /// 30s, ±50% uniform jitter. `attempt` is 0-indexed.
    fn backoff_delay(attempt: u32) -> Duration {
        let exp = BASE_BACKOFF.saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX));
        let capped = exp.min(MAX_BACKOFF);
        let jitter_frac = rand::thread_rng().gen_range(0.5..=1.5);
        Duration::from_secs_f64(capped.as_secs_f64() * jitter_frac).min(MAX_BACKOFF)
    }

    /// Classifies a non-success HTTP status into an `ApiError`, extracting
    /// `Retry-After` for 429s.
    async fn classify_error(&self, resp: reqwest::Response) -> ApiError {
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.text().await.unwrap_or_default();
        let message = if body.is_empty() {
            status.to_string()
        } else {
            body
        };

        match status.as_u16() {
            401 | 403 => ApiError::Auth { message },
            400 => {
                let message = if mentions_tool_calling(&message) {
                    format!(
                        "{message}\n\nhint: this model may not support function/tool calling; \
retry with a tool-capable --model (e.g. meta/llama-3.3-70b-instruct or a `*-instruct` model \
documented as tool-calling capable), or run `chat --no-tools` / `ask` without `--tools`."
                    )
                } else {
                    message
                };
                ApiError::Permanent {
                    message,
                    status: Some(400),
                }
            }
            404 => ApiError::Permanent {
                message,
                status: Some(status.as_u16()),
            },
            429 => ApiError::RateLimited {
                message,
                retry_after,
            },
            s if s >= 500 => ApiError::Transient {
                message,
                status: Some(s),
            },
            s => ApiError::Permanent {
                message,
                status: Some(s),
            },
        }
    }

    /// Runs `make_request` with rate limiting and retry/backoff. Every
    /// attempt (including retries) goes through `limiter.acquire()` first.
    async fn send_with_retry<F, Fut>(&self, make_request: F) -> Result<reqwest::Response, ApiError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let mut attempt = 0u32;
        loop {
            self.limiter.acquire().await;

            let result = make_request().await;
            match result {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let err = self.classify_error(resp).await;
                    if let ApiError::RateLimited {
                        retry_after: Some(d),
                        ..
                    } = &err
                    {
                        self.limiter.penalize(*d).await;
                    }
                    if !err.is_retryable() || attempt + 1 >= MAX_ATTEMPTS {
                        return Err(err);
                    }
                    let delay = match &err {
                        ApiError::RateLimited {
                            retry_after: Some(d),
                            ..
                        } => *d,
                        _ => Self::backoff_delay(attempt),
                    };
                    tracing::warn!("request failed ({err}), retrying in {:?}", delay);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    let err = ApiError::transient(format!("network error: {e}"));
                    if attempt + 1 >= MAX_ATTEMPTS {
                        return Err(err);
                    }
                    let delay = Self::backoff_delay(attempt);
                    tracing::warn!("network error ({e}), retrying in {:?}", delay);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }
}

impl LlmClient for NimClient {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, ApiError> {
        let mut req = req;
        req.stream = false;
        let url = self.url("chat/completions");
        let resp = self
            .send_with_retry(|| self.auth(self.http.post(&url)).json(&req).send())
            .await?;
        resp.json::<ChatResponse>()
            .await
            .map_err(|e| ApiError::permanent(format!("failed to parse chat response: {e}")))
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream, ApiError> {
        let mut req = req;
        req.stream = true;
        let url = self.url("chat/completions");
        let resp = self
            .send_with_retry(|| self.auth(self.http.post(&url)).json(&req).send())
            .await?;
        let byte_stream = resp.bytes_stream();
        Ok(parse_sse(byte_stream))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ApiError> {
        let url = self.url("models");
        let resp = self
            .send_with_retry(|| self.auth(self.http.get(&url)).send())
            .await?;
        let parsed: ModelsResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::permanent(format!("failed to parse models response: {e}")))?;
        Ok(parsed.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tool_calling_mentions() {
        assert!(mentions_tool_calling(
            "400: model does not support tool calling"
        ));
        assert!(mentions_tool_calling("invalid function parameter"));
        assert!(!mentions_tool_calling(
            "400: missing required field 'model'"
        ));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let d0 = NimClient::backoff_delay(0);
        assert!(d0 >= Duration::from_millis(250) && d0 <= Duration::from_millis(750));
        let d_high = NimClient::backoff_delay(10);
        assert!(d_high <= MAX_BACKOFF);
    }
}
