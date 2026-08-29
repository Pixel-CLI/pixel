//! Credential redaction for anything captured from a git subprocess
//! (primarily stderr embedded in `GitError::NonZeroExit`) before it can ever
//! be logged, displayed, or surfaced to a caller.
//!
//! Concept ported from usable-git's `runner.ts` (which redacts captured
//! stdout/stderr before returning/logging it), reimplemented here rather
//! than copied verbatim.

use std::sync::LazyLock;

use regex::Regex;

/// `https://user:token@host/...` -> `https://<redacted>@host/...`
static CRED_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https://[^/\s:@]+:[^/\s@]+@").unwrap());

/// `Authorization: Bearer <token>` (or any single token after the colon),
/// case-insensitive.
static AUTH_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(authorization\s*:\s*)(?:bearer\s+)?\S+").unwrap());

/// `token=<value>` / `password=<value>`, case-insensitive.
static TOKEN_KV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(token|password)\s*=\s*[^\s&]+").unwrap());

/// Scrub credential-shaped substrings out of `text`. Best-effort: this is a
/// defense-in-depth scrub for logging/error-display, not a security
/// boundary — never rely on it to keep a secret out of process memory.
pub fn redact(text: &str) -> String {
    let s = CRED_URL.replace_all(text, "https://<redacted>@");
    let s = AUTH_HEADER.replace_all(&s, "${1}<redacted>");
    let s = TOKEN_KV.replace_all(&s, "${1}=<redacted>");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credential_url() {
        let input = "fatal: unable to access 'https://alice:ghp_abcdef1234567890@github.com/x/y.git/'";
        let out = redact(input);
        assert!(!out.contains("ghp_abcdef1234567890"));
        assert!(!out.contains("alice:ghp_abcdef1234567890"));
        assert!(out.contains("https://<redacted>@github.com"));
    }

    #[test]
    fn redacts_authorization_header() {
        let input = "sent header Authorization: Bearer sk-verysecrettoken123";
        let out = redact(input);
        assert!(!out.contains("sk-verysecrettoken123"));
        assert!(out.to_lowercase().contains("authorization:"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redacts_token_and_password_kv() {
        let input = "url?token=abcDEF123&other=1 password=hunter2xyz";
        let out = redact(input);
        assert!(!out.contains("abcDEF123"));
        assert!(!out.contains("hunter2xyz"));
        assert!(out.contains("other=1"));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let input = "fatal: pathspec 'foo.txt' did not match any files";
        assert_eq!(redact(input), input);
    }
}
