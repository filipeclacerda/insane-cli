//! Classified error types used across the CLI, with exit-code mapping and
//! secret redaction on `Display`.

use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// Matches NVIDIA API keys (`nvapi-...`) so they never leak into logs, error
/// messages, or panic output.
static NVAPI_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"nvapi-[A-Za-z0-9_-]+").expect("valid regex"));

/// Redacts any `nvapi-...` substrings in `input`, replacing them with
/// `nvapi-***`. Safe to call on arbitrary text (logs, error strings, panic
/// payloads).
pub fn redact(input: &str) -> String {
    NVAPI_KEY_RE.replace_all(input, "nvapi-***").into_owned()
}

/// Errors originating from the NIM/OpenAI-compatible API, classified by
/// retry-ability.
#[derive(Debug)]
pub enum ApiError {
    /// Non-retryable errors: bad request, auth failure, not found, invalid
    /// usage, etc.
    Permanent {
        message: String,
        status: Option<u16>,
    },
    /// Retryable errors: network failures, 5xx, timeouts.
    Transient {
        message: String,
        status: Option<u16>,
    },
    /// 429 responses; `retry_after` is the server-provided delay, if any.
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },
    /// Authentication is missing or invalid (maps to exit code 3). Distinct
    /// from `Permanent` so callers can special-case "no API key configured".
    Auth { message: String },
    /// Invalid CLI usage (maps to exit code 2).
    Usage { message: String },
    /// The rate limiter's budget has been fully exhausted for this run
    /// (e.g. penalized and no time left to wait). Maps to exit code 4.
    RateLimitExhausted { message: String },
    /// Operation was cancelled (Ctrl+C). Maps to exit code 130.
    Cancelled,
}

impl ApiError {
    pub fn permanent(message: impl Into<String>) -> Self {
        ApiError::Permanent {
            message: message.into(),
            status: None,
        }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        ApiError::Transient {
            message: message.into(),
            status: None,
        }
    }

    /// Whether this error should be retried by the HTTP client's backoff
    /// loop (429/5xx/network already classified as Transient/RateLimited).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ApiError::Transient { .. } | ApiError::RateLimited { .. }
        )
    }

    /// Maps this error to the process exit code defined in SPEC §10.
    pub fn exit_code(&self) -> i32 {
        match self {
            ApiError::Auth { .. } => 3,
            ApiError::RateLimitExhausted { .. } => 4,
            ApiError::Usage { .. } => 2,
            ApiError::Cancelled => 130,
            ApiError::Permanent { .. }
            | ApiError::Transient { .. }
            | ApiError::RateLimited { .. } => 1,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw = match self {
            ApiError::Permanent { message, status } => match status {
                Some(s) => format!("permanent error ({s}): {message}"),
                None => format!("permanent error: {message}"),
            },
            ApiError::Transient { message, status } => match status {
                Some(s) => format!("transient error ({s}): {message}"),
                None => format!("transient error: {message}"),
            },
            ApiError::RateLimited {
                message,
                retry_after,
            } => match retry_after {
                Some(d) => format!("rate limited (retry after {}s): {message}", d.as_secs()),
                None => format!("rate limited: {message}"),
            },
            ApiError::Auth { message } => format!("authentication error: {message}"),
            ApiError::Usage { message } => format!("usage error: {message}"),
            ApiError::RateLimitExhausted { message } => format!("rate limit exhausted: {message}"),
            ApiError::Cancelled => "operation cancelled".to_string(),
        };
        write!(f, "{}", redact(&raw))
    }
}

impl std::error::Error for ApiError {}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::permanent(redact(&e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_api_keys() {
        let msg = "failed with key nvapi-AbC123_-xyz in header";
        assert_eq!(redact(msg), "failed with key nvapi-*** in header");
    }

    #[test]
    fn redacts_multiple_keys() {
        let msg = "nvapi-aaa and nvapi-bbb_CCC";
        assert_eq!(redact(msg), "nvapi-*** and nvapi-***");
    }

    #[test]
    fn leaves_normal_text_untouched() {
        let msg = "no secrets here";
        assert_eq!(redact(msg), msg);
    }

    #[test]
    fn display_redacts() {
        let err = ApiError::Auth {
            message: "bad key nvapi-secret123".to_string(),
        };
        assert_eq!(err.to_string(), "authentication error: bad key nvapi-***");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            ApiError::Usage {
                message: "x".into()
            }
            .exit_code(),
            2
        );
        assert_eq!(
            ApiError::Auth {
                message: "x".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(
            ApiError::RateLimitExhausted {
                message: "x".into()
            }
            .exit_code(),
            4
        );
        assert_eq!(ApiError::Cancelled.exit_code(), 130);
        assert_eq!(ApiError::permanent("x").exit_code(), 1);
    }
}
