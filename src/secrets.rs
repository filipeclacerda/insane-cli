//! Secret detection over text that is about to be sent to the model (SPEC
//! §7), plus a general-purpose `redact` used to sanitize what gets echoed
//! back to the user (complements `error::redact`, which only covers
//! `nvapi-...` keys).

use std::sync::LazyLock;

use regex::Regex;

/// One potential secret found while scanning a piece of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    pub kind: String,
    /// 1-indexed line number within the scanned text.
    pub line: usize,
    /// The offending line with the match itself replaced by `***`.
    pub redacted_excerpt: String,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
}

// Compiled once (SPEC §7 requirement) and reused across every scan/redact
// call for the lifetime of the process.
static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    vec![
        Pattern {
            kind: "AWS access key",
            re: Regex::new(r"AKIA[0-9A-Z]{16}").expect("valid regex"),
        },
        Pattern {
            kind: "GitHub token",
            re: Regex::new(r"(?:ghp|gho|github_pat)_[A-Za-z0-9_]{20,}").expect("valid regex"),
        },
        Pattern {
            kind: "NVIDIA API key",
            re: Regex::new(r"nvapi-[A-Za-z0-9_-]+").expect("valid regex"),
        },
        Pattern {
            kind: "private key header",
            re: Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----").expect("valid regex"),
        },
        Pattern {
            kind: "JWT",
            re: Regex::new(r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
                .expect("valid regex"),
        },
        Pattern {
            kind: "URL with embedded credentials",
            re: Regex::new(r"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s:@/]+:[^\s:@/]+@").expect("valid regex"),
        },
        Pattern {
            kind: "generic secret assignment",
            re: Regex::new(
                r#"(?i)(?:api[_-]?key|secret|password|token)\s*[:=]\s*['"]?[^\s'"]{8,}"#,
            )
            .expect("valid regex"),
        },
    ]
});

/// Scans `text` line by line against every known secret pattern, returning
/// every match found (a line may produce more than one finding).
pub fn scan(text: &str) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        for pattern in PATTERNS.iter() {
            if let Some(m) = pattern.re.find(line) {
                findings.push(SecretFinding {
                    kind: pattern.kind.to_string(),
                    line: idx + 1,
                    redacted_excerpt: redact_match(line, m.range()),
                });
            }
        }
    }
    findings
}

fn redact_match(line: &str, range: std::ops::Range<usize>) -> String {
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..range.start]);
    out.push_str("***");
    out.push_str(&line[range.end..]);
    out
}

/// Replaces every secret-shaped substring in `text` with `***`. Safe to run
/// on arbitrary text (e.g. before logging file contents or model prompts
/// that included file context).
pub fn redact(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in PATTERNS.iter() {
        result = pattern.re.replace_all(&result, "***").into_owned();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aws_key() {
        let findings = scan("aws_key = AKIAABCDEFGHIJKLMNOP");
        assert!(findings.iter().any(|f| f.kind == "AWS access key"));
    }

    #[test]
    fn detects_github_token() {
        let findings = scan("token: ghp_abcdefghijklmnopqrstuvwxyz0123");
        assert!(findings.iter().any(|f| f.kind == "GitHub token"));
    }

    #[test]
    fn detects_nvapi_key() {
        let findings = scan("NVIDIA_API_KEY=nvapi-abcDEF123_-xyz");
        assert!(findings.iter().any(|f| f.kind == "NVIDIA API key"));
    }

    #[test]
    fn detects_private_key_header() {
        let findings = scan("-----BEGIN RSA PRIVATE KEY-----\nMIIB...\n");
        assert!(findings.iter().any(|f| f.kind == "private key header"));
    }

    #[test]
    fn detects_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQ";
        let findings = scan(jwt);
        assert!(findings.iter().any(|f| f.kind == "JWT"));
    }

    #[test]
    fn detects_url_with_password() {
        let findings = scan("postgres://user:hunter2@db.example.com:5432/app");
        assert!(findings
            .iter()
            .any(|f| f.kind == "URL with embedded credentials"));
    }

    #[test]
    fn detects_generic_secret_assignment() {
        let findings = scan(r#"password = "supersecretvalue""#);
        assert!(findings
            .iter()
            .any(|f| f.kind == "generic secret assignment"));
    }

    #[test]
    fn no_findings_on_clean_text() {
        assert!(scan("fn main() { println!(\"hello\"); }").is_empty());
    }

    #[test]
    fn line_numbers_are_1_indexed() {
        let findings = scan("line one\nline two\nAKIAABCDEFGHIJKLMNOP\n");
        assert_eq!(findings[0].line, 3);
    }

    #[test]
    fn redact_replaces_secrets_but_keeps_rest() {
        let redacted = redact("key=AKIAABCDEFGHIJKLMNOP end");
        assert!(!redacted.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(redacted.contains("end"));
    }

    #[test]
    fn redacted_excerpt_hides_the_match() {
        let findings = scan("key=AKIAABCDEFGHIJKLMNOP");
        assert!(!findings[0]
            .redacted_excerpt
            .contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(findings[0].redacted_excerpt.contains("***"));
    }
}
