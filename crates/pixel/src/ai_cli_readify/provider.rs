// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The provider, the honest probe, and the failure classification.
//!
//! The probe is honest because it is representative: it sends the same shape
//! of request an agent's first real turn sends, so a provider that answers a
//! toy request but throttles the real one is not reported ready. Concretely
//! that means no system message and a `max_tokens` reservation in the range
//! a real turn books — a 16-token reservation costs nothing against a
//! tokens-per-minute budget and reports Ready on a provider whose next real
//! request comes back 429. Ollama Cloud answers both ways this module's
//! classification exists to tell apart: a 429 session limit, and a 404 for a
//! model name it stopped serving — each reported as its own condition instead
//! of collapsed into "unreachable".

use std::time::Duration;

use serde_json::json;

use super::agents::READY_TOKEN;

/// Response bytes we are willing to read from a probe. A provider answering
/// with an HTML error page or a runaway completion must not be read whole.
pub(crate) const RESPONSE_CAP_BYTES: usize = 16_384;

/// Characters of a provider's error body that reach the report. Long enough
/// to carry the provider's own message ("no usable credit"), short enough to
/// stay one line.
pub(crate) const DETAIL_CAP_CHARS: usize = 240;

/// Tokens the probe reserves. Deliberately not 16: see the module comment.
/// A real first turn books hundreds, and the reservation is what a
/// tokens-per-minute limiter accounts for, so a toy reservation is exactly
/// the dishonesty the probe exists to avoid.
pub(crate) const PROBE_MAX_TOKENS: u32 = 1_024;

/// The prompt every probe sends. A 200 is a pass only when its reply carries
/// `READY` ([`carries_ready`]), so a 200 that carries prose, a refusal, or an
/// empty completion is visible in the report as its own failure rather than
/// counted as a pass.
pub(crate) const PROBE_PROMPT: &str = "Reply exactly READY.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Ollama,
}

impl Provider {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
        }
    }

    /// OpenAI-compatible base URL; the probe posts to `{base}/chat/completions`.
    pub(crate) const fn base_url(self) -> &'static str {
        match self {
            Self::Ollama => "https://ollama.com/v1",
        }
    }

    /// The model an agent is pointed at when this provider wins.
    ///
    /// A name the provider currently serves, and Ollama's changes without
    /// notice: `deepseek-v3.1:cloud` was answered with `404 model not found`,
    /// which failed the probe on every run while the report blamed the
    /// request. `deepseek-v4.1-flash` is the name
    /// `GET https://ollama.com/v1/models` returns. A 404 here reports itself
    /// as [`ProbeFailure::NoModel`] rather than as a refused request.
    pub(crate) const fn model(self) -> &'static str {
        match self {
            Self::Ollama => "deepseek-v4.1-flash",
        }
    }

    /// The env var holding the key. Read by name only; the value never
    /// reaches a log, an error string, or a written config.
    pub(crate) const fn key_env(self) -> &'static str {
        match self {
            Self::Ollama => "OLLAMA_API_KEY",
        }
    }
}

/// Why a provider is not ready. Each arm names the provider's own condition
/// so the report can say "402, no usable credit" instead of "failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeFailure {
    /// No key in the environment and none typed at the prompt.
    MissingKey,
    /// 429 — the account's session or per-minute budget is spent.
    RateLimited,
    /// 402 — the credential is valid and the account has no credit.
    NoCredit,
    /// 401/403 — the credential was rejected.
    Credential,
    /// 400 — the request itself was refused. The 2026-09 gateway returned
    /// `400 No connected db` here, a gateway-side condition, not the
    /// provider's, and it must not read as "unreachable".
    Upstream,
    /// 404 — the provider does not serve the model that was asked for. Its
    /// own condition because the remedy is the model name and nothing else:
    /// read as a refused request it sends the reader to their credential or
    /// their plan, and read as a transport failure it sends them to the
    /// network. The name is a constant in this module, so this arm means the
    /// constant is stale.
    NoModel,
    /// 2xx, and the body carried no completion. The only arm that comes from
    /// a success status: the provider is up and took the request, and it did
    /// not answer the question the probe asked. Counted as ready, it would
    /// be selected and `--apply` would point four agents' configs at a
    /// provider that serves nothing.
    EmptyCompletion,
    /// 2xx, and the completion is not the `READY` the probe asked for:
    /// prose, a refusal, or a gateway's own canned notice. Distinct from
    /// [`Self::EmptyCompletion`] because something did come back, and distinct
    /// from a pass because the provider did not answer the question the probe
    /// asked. Counted as ready, it would be selected and `--apply` would
    /// point four agents' configs at a gateway that only ever says something
    /// else.
    NoReadyMarker,
    /// 5xx — the provider is up and failing.
    Server,
    /// The request never produced a status: DNS, TLS, connect or read.
    Transport,
}

impl ProbeFailure {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::MissingKey => "no key",
            Self::RateLimited => "rate limited (429)",
            Self::NoCredit => "no credit (402)",
            Self::Credential => "credential rejected",
            // Carries no status of its own: `_` in `classify_status` also
            // lands here, so a hardcoded "(400)" would label a 422 as a 400.
            // The real status is already in the detail, which appends
            // "(HTTP {status})".
            Self::Upstream => "request refused",
            Self::NoModel => "model not found (404)",
            // Names no status: this arm is always a 2xx, and the detail
            // appends the one it really saw.
            Self::EmptyCompletion => "no completion",
            // Same: a 2xx, and the detail carries the status and the reply.
            Self::NoReadyMarker => "no READY in the reply",
            Self::Server => "provider error (5xx)",
            Self::Transport => "unreachable",
        }
    }
}

/// Map an HTTP status onto a failure. Only called for a status that is not
/// a success, so the 2xx range has no arm.
pub(crate) fn classify_status(status: u16) -> ProbeFailure {
    match status {
        429 => ProbeFailure::RateLimited,
        402 => ProbeFailure::NoCredit,
        401 => ProbeFailure::Credential,
        403 => ProbeFailure::Credential,
        404 => ProbeFailure::NoModel,
        500..=599 => ProbeFailure::Server,
        _ => ProbeFailure::Upstream,
    }
}

/// What one provider's probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeOutcome {
    pub(crate) provider: Provider,
    /// The provider answered, and the answer carried the `READY` the probe
    /// asked for — not a placeholder refusal or a gateway's canned notice.
    pub(crate) ready: bool,
    /// The reply text on success, or the classified failure plus whatever the
    /// provider said, redacted and capped.
    pub(crate) detail: String,
    /// The classified failure, `None` when ready.
    pub(crate) failure: Option<ProbeFailure>,
}

impl ProbeOutcome {
    pub(crate) const fn ready(provider: Provider, detail: String) -> Self {
        Self {
            provider,
            ready: true,
            detail,
            failure: None,
        }
    }

    pub(crate) const fn failed(provider: Provider, failure: ProbeFailure, detail: String) -> Self {
        Self {
            provider,
            ready: false,
            detail,
            failure: Some(failure),
        }
    }
}

/// The body every probe posts. Public to the module so a test asserts the
/// exact shape — in particular that no `system` role is present, which is
/// what keeps the probe cheap enough not to trip the very limits it is
/// measuring.
pub(crate) fn probe_body(provider: Provider) -> serde_json::Value {
    json!({
        "model": provider.model(),
        "messages": [{"role": "user", "content": PROBE_PROMPT}],
        "max_tokens": PROBE_MAX_TOKENS,
    })
}

/// One `POST {base}/chat/completions`.
///
/// `base` is a parameter rather than a read of `provider.base_url()` so a
/// test drives this exact function against a loopback server; production
/// passes [`Provider::base_url`]. Status handling is disabled in the agent
/// config so a 429 arrives as a status and a body to classify, not as an
/// opaque error string.
pub(crate) fn probe(provider: Provider, base: &str, key: &str, timeout: Duration) -> ProbeOutcome {
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .user_agent("pixel-cli ai-cli-readify")
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {key}"))
        .send_json(probe_body(provider));
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            return ProbeOutcome::failed(provider, ProbeFailure::Transport, redact(&e.to_string()));
        }
    };
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .with_config()
        .limit(RESPONSE_CAP_BYTES as u64)
        .read_to_string()
        .unwrap_or_default();
    if (200..300).contains(&status) {
        return match reply_text(&text) {
            Some(reply) if carries_ready(&reply) => ProbeOutcome::ready(provider, reply),
            // A 200 whose completion is not the `READY` the prompt asked for
            // is the provider being up and not answering: prose, a refusal,
            // or a gateway's own canned notice. It is a failure with the same
            // detail shape as the rest, so the reply it did send stays
            // readable.
            Some(reply) => {
                let failure = ProbeFailure::NoReadyMarker;
                let line = detail(&reply, status, &failure);
                ProbeOutcome::failed(provider, failure, line)
            }
            // A 200 whose body carries no completion at all.
            None => {
                let failure = ProbeFailure::EmptyCompletion;
                let line = detail(&text, status, &failure);
                ProbeOutcome::failed(provider, failure, line)
            }
        };
    }
    let failure = classify_status(status);
    let detail = detail(&text, status, &failure);
    ProbeOutcome::failed(provider, failure, detail)
}

/// The assistant's reply from an OpenAI-shaped body, or `None` when the body
/// carried none. The caller turns that `None` into [`ProbeFailure::
/// EmptyCompletion`]: a 200 is not the provider answering the question, and a
/// probe that read one as ready would be reporting its own request, not the
/// provider.
fn reply_text(body: &str) -> Option<String> {
    let reply = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .as_array()?
                .first()?
                .get("message")?
                .get("content")?
                .as_str()
                .map(str::to_string)
        });
    match reply {
        Some(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

/// Whether a reply carries the marker [`PROBE_PROMPT`] asked for.
///
/// Case-insensitive, and anywhere in the reply: a chatty model wraps `READY`
/// in a sentence, and what the probe needs to know is that the round trip
/// reached a model, not that it obeyed the formatting. The token is
/// [`agents::READY_TOKEN`], the same one the four agents' own probes require
/// — the two lanes port the same reference round trip, so they ask for the
/// same word.
fn carries_ready(reply: &str) -> bool {
    reply.to_ascii_uppercase().contains(READY_TOKEN)
}

/// One line for a failed probe: the classified label, then the provider's own
/// message when it sent one.
/// The classified failure plus the provider's own words, redacted before it
/// leaves this module.
///
/// The body is the provider's, and a provider is free to quote the request
/// back — an error body is exactly where a key would reappear. Every other
/// path out of this module already passes through [`redact`]; this one did
/// not, which the module doc claimed it did. Nothing here writes to a file,
/// but the report is printed, copied and (with the handoff) persisted, so the
/// mismatch was a leak waiting for a sink.
fn detail(body: &str, status: u16, failure: &ProbeFailure) -> String {
    let provider_text = provider_message(body);
    if provider_text.is_empty() {
        return format!("{} (HTTP {status})", failure.label());
    }
    let capped = cap_chars(&provider_text, DETAIL_CAP_CHARS);
    redact(&format!("{} (HTTP {status}): {capped}", failure.label()))
}

/// The provider's own error message from an OpenAI-shaped error body, or the
/// body's first line when it is not JSON.
fn provider_message(body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let error = v.get("error")?;
            // OpenAI's shape nests the message; some gateways put the string
            // where the object belongs, so both are read.
            error
                .get("message")
                .or_else(|| v.get("message"))
                .or(Some(error))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        });
    let text = parsed.unwrap_or_else(|| body.lines().next().unwrap_or("").to_string());
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate at a character boundary, marking the cut so a clipped message is
/// never read as the whole one.
fn cap_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let kept: String = text.chars().take(cap).collect();
    format!("{kept}…")
}

/// Replace anything that looks like a credential with `<redacted>`. The key
/// is never interpolated into a message this module builds, but a provider
/// can echo it back in an error body and a transport error can quote a URL
/// carrying one, so every string on its way to the report passes here.
pub(crate) fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        if looks_like_a_secret(token) {
            out.push_str("<redacted>");
            if let Some(space) = token.chars().last().filter(|c| c.is_whitespace()) {
                out.push(space);
            }
        } else {
            out.push_str(token);
        }
    }
    out
}

/// The prefixes a key carries in the provider's own documentation. A run
/// matching one of these and longer than a bare word is masked whatever else
/// it looks like.
const SECRET_PREFIXES: [&str; 4] = ["sk-", "sk_", "Bearer", "bearer"];

/// The longest run of alphanumerics, dashes, underscores and dots that a
/// provider's key is made of.
const KEY_RUN_CHARS: usize = 32;

/// The characters a provider wraps a value in: a quote, a bracket, or the
/// punctuation of the sentence around it. Trimmed from each end before a
/// token is classified, because none of them is in a key's alphabet — left
/// on, `'sk-…'` is judged as the token that starts with a quote, and every
/// rule below misses it.
const WRAPPER_PUNCTUATION: &[char] = &[
    '\'', '"', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '=', '*',
];

/// The characters that separate a name from its value in an error body
/// (`api_key=…`, `"token": "…"`). What follows the first one is the
/// credential when the token is a pair; the name in front of it is not.
const ASSIGNMENT_SEPARATORS: [char; 2] = ['=', ':'];

/// A token that carries a credential: a bearer value, a long run of
/// key-shaped characters, or either of those behind the quotes or the
/// `name=` an error body puts around it. Deliberately generous — a false
/// positive costs a less readable message, a false negative leaks a key into
/// the report.
fn looks_like_a_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_ascii_graphic());
    if trimmed.is_empty() {
        return false;
    }
    let bare = trimmed.trim_matches(WRAPPER_PUNCTUATION);
    if is_a_key(bare) {
        return true;
    }
    // `api_key=sk-…`, `key:sk-…`: the credential is the value, and the name
    // before the separator makes the whole token stop looking like one.
    assigned_value(bare).is_some_and(|value| is_a_key(value.trim_matches(WRAPPER_PUNCTUATION)))
}

/// The value half of a `name=value` or `name:value` token, or `None` when
/// the token carries neither. The first separator wins: a URL's `https:` is
/// handled by the value it yields failing [`is_a_key`], not by skipping it.
fn assigned_value(token: &str) -> Option<&str> {
    let at = token.find(ASSIGNMENT_SEPARATORS)?;
    Some(&token[at + 1..])
}

/// A run of the characters a provider's key is made of, long enough to be
/// one — or carrying a documented prefix past a bare word's length.
fn is_a_key(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    if candidate.eq_ignore_ascii_case("bearer") {
        return false;
    }
    if SECRET_PREFIXES.iter().any(|p| candidate.starts_with(p)) && candidate.len() > 8 {
        return true;
    }
    // A long unbroken alphanumeric run is a key whatever its prefix: the
    // provider's key is 30+ characters of that shape.
    candidate.len() >= KEY_RUN_CHARS
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    /// A one-shot loopback server answering `status` with `body`. Polls with
    /// a deadline so a client that never connects fails the assertion instead
    /// of hanging the suite.
    fn http_once(status: u16, body: &str) -> (String, std::thread::JoinHandle<(String, String)>) {
        let reply = body.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    let done = line == "\r\n" || line == "\n";
                    head.push_str(&line);
                    if done {
                        break;
                    }
                }
                let mut sent = vec![0u8; length];
                if length > 0 {
                    reader.read_exact(&mut sent).unwrap();
                }
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                return (head, String::from_utf8_lossy(&sent).into_owned());
            }
            (String::new(), String::new())
        });
        (base, server)
    }

    fn probe_against(status: u16, body: &str) -> ProbeOutcome {
        let (base, server) = http_once(status, body);
        let outcome = probe(Provider::Ollama, &base, "test-key", Duration::from_secs(5));
        let _ = server.join();
        outcome
    }

    #[test]
    fn the_probe_body_carries_no_system_message() {
        let body = probe_body(Provider::Ollama);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "{body}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], PROBE_PROMPT);
        assert_eq!(body["model"], "deepseek-v4.1-flash");
    }

    #[test]
    fn the_probe_body_reserves_more_than_a_toy_completion() {
        let body = probe_body(Provider::Ollama);
        assert_eq!(body["max_tokens"], PROBE_MAX_TOKENS);
        // Read back off the payload rather than off the constant: a toy
        // reservation is what lets a probe report Ready while the real
        // request comes back 429, and the payload is what the provider is
        // actually asked for.
        let reserved = body["max_tokens"].as_u64().expect("a reserved token count");
        assert!(
            reserved > 16,
            "a toy reservation is the probe lying: {body}"
        );
        assert_eq!(body["model"], "deepseek-v4.1-flash");
    }

    #[test]
    fn a_200_with_a_completion_is_ready_and_reports_the_reply() {
        let outcome = probe_against(200, r#"{"choices":[{"message":{"content":"READY"}}]}"#);
        assert!(outcome.ready, "{outcome:?}");
        assert_eq!(outcome.detail, "READY");
        assert_eq!(outcome.failure, None);
    }

    #[test]
    fn a_200_with_no_completion_is_not_counted_as_ready() {
        // The provider is up and took the request; it did not answer it.
        // `ready` is what `selected` reads, so a 200 with nothing in it
        // would be the provider `--apply` points four agents at.
        let outcome = probe_against(200, r#"{"choices":[]}"#);
        assert!(!outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, Some(ProbeFailure::EmptyCompletion));
        assert!(
            outcome.detail.starts_with("no completion") && outcome.detail.contains("HTTP 200"),
            "the detail names the condition and the status it really saw: {}",
            outcome.detail
        );
    }

    #[test]
    fn a_200_whose_reply_is_not_the_asked_for_marker_is_not_counted_as_ready() {
        // A gateway that answers 200 with its own prose is the provider up
        // and not answering. `ready` is what `selected` reads and `--apply`
        // acts on, so counting it would point three agent configs at a
        // gateway that only ever says something else.
        let outcome = probe_against(
            200,
            r#"{"choices":[{"message":{"content":"Sure, I'd be happy to help."}}]}"#,
        );
        assert!(!outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, Some(ProbeFailure::NoReadyMarker));
        assert!(
            outcome.detail.starts_with("no READY in the reply")
                && outcome.detail.contains("HTTP 200")
                && outcome.detail.contains("happy to help"),
            "the detail names the condition, the status and the reply: {}",
            outcome.detail
        );
    }

    #[test]
    fn a_200_whose_reply_wraps_the_marker_in_prose_or_case_is_ready() {
        // The marker is required, not the formatting. A chatty model that
        // wraps `READY` in a sentence or lowercases it reached one, and a
        // check that demanded the exact string would fail a working provider.
        for body in [
            r#"{"choices":[{"message":{"content":"Sure, READY."}}]}"#,
            r#"{"choices":[{"message":{"content":"ready"}}]}"#,
        ] {
            let outcome = probe_against(200, body);
            assert!(outcome.ready, "{body}: {outcome:?}");
            assert_eq!(outcome.failure, None, "{body}: {outcome:?}");
        }
    }

    #[test]
    fn a_200_with_an_empty_completion_is_not_counted_as_ready() {
        let outcome = probe_against(200, r#"{"choices":[{"message":{"content":"  "}}]}"#);
        assert!(!outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, Some(ProbeFailure::EmptyCompletion));
        assert!(outcome.detail.starts_with("no completion"), "{outcome:?}");
    }

    #[test]
    fn the_probe_sends_the_bearer_key_and_the_model() {
        let (base, server) = http_once(200, r#"{"choices":[{"message":{"content":"READY"}}]}"#);
        probe(Provider::Ollama, &base, "sekret", Duration::from_secs(5));
        let (head, sent) = server.join().unwrap();
        assert!(
            head.starts_with("POST /chat/completions HTTP/1.1"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sekret"),
            "{head}"
        );
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent, probe_body(Provider::Ollama));
    }

    #[test]
    fn the_base_url_trailing_slash_does_not_double() {
        let (base, server) = http_once(200, r#"{"choices":[{"message":{"content":"READY"}}]}"#);
        probe(
            Provider::Ollama,
            &format!("{base}/"),
            "k",
            Duration::from_secs(5),
        );
        let (head, _) = server.join().unwrap();
        assert!(
            head.starts_with("POST /chat/completions HTTP/1.1"),
            "{head}"
        );
    }

    #[test]
    fn each_failure_status_gets_its_own_label() {
        for (status, failure) in [
            (429, ProbeFailure::RateLimited),
            (402, ProbeFailure::NoCredit),
            (401, ProbeFailure::Credential),
            (403, ProbeFailure::Credential),
            (400, ProbeFailure::Upstream),
            (404, ProbeFailure::NoModel),
            (500, ProbeFailure::Server),
            (599, ProbeFailure::Server),
            // Unmapped to anything of its own, and deliberately not folded
            // into a status it is not: `detail` carries the real number.
            (422, ProbeFailure::Upstream),
        ] {
            assert_eq!(classify_status(status), failure, "status {status}");
            let outcome = probe_against(status, "{}");
            assert!(!outcome.ready, "status {status}");
            assert_eq!(outcome.failure.as_ref(), Some(&failure), "status {status}");
            assert!(
                outcome.detail.starts_with(failure.label()),
                "{status}: {}",
                outcome.detail
            );
        }
    }

    #[test]
    fn the_providers_own_message_reaches_the_detail() {
        let outcome = probe_against(402, r#"{"error":{"message":"no usable account credit"}}"#);
        assert!(
            outcome.detail.contains("no usable account credit"),
            "{}",
            outcome.detail
        );
        assert!(outcome.detail.contains("HTTP 402"), "{}", outcome.detail);
    }

    #[test]
    fn a_gateway_refusal_is_reported_as_refused_not_unreachable() {
        // The 2026-09 gateway answered this way through a valid credential.
        let outcome = probe_against(400, r#"{"error":{"message":"No connected db"}}"#);
        assert_eq!(outcome.failure, Some(ProbeFailure::Upstream));
        assert!(
            outcome.detail.contains("No connected db"),
            "{}",
            outcome.detail
        );
        assert_ne!(outcome.failure, Some(ProbeFailure::Transport));
    }

    #[test]
    fn an_unknown_model_is_reported_as_a_stale_name_not_a_refused_request() {
        // What Ollama Cloud actually answered for `deepseek-v3.1:cloud`.
        let outcome = probe_against(
            404,
            r#"{"error":{"message":"model \"deepseek-v3.1:cloud\" not found"}}"#,
        );
        assert_eq!(outcome.failure, Some(ProbeFailure::NoModel));
        // The label has to name the status it really saw: the previous
        // wording said "(400)" over a 404 and sent the reader to the wrong
        // remedy.
        assert!(
            outcome.detail.contains("404") && !outcome.detail.contains("(400)"),
            "a 404 must not read as a 400: {}",
            outcome.detail
        );
        assert!(
            outcome.detail.contains("model not found"),
            "the label must name the condition: {}",
            outcome.detail
        );
    }

    #[test]
    fn a_key_quoted_back_in_an_error_body_is_redacted_out_of_the_detail() {
        // A provider is free to quote the request back in its error body, and
        // that body is the one string in this module a provider writes. It
        // must leave redacted like every other path out of here.
        // Split so source-level secret scanners do not read the fixture as a key.
        let key = concat!("sk-ant-", "api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        let outcome = probe_against(
            401,
            &format!(r#"{{"error":{{"message":"invalid key {key}"}}}}"#),
        );
        assert_eq!(outcome.failure, Some(ProbeFailure::Credential));
        assert!(
            !outcome.detail.contains(key),
            "the provider echoed the key and the detail kept it: {}",
            outcome.detail
        );
        assert!(outcome.detail.contains("<redacted>"), "{}", outcome.detail);
        assert!(
            outcome.detail.contains("invalid key"),
            "the redaction must not swallow the diagnosis: {}",
            outcome.detail
        );
    }

    #[test]
    fn a_non_json_error_body_still_yields_a_detail() {
        let outcome = probe_against(503, "upstream is having a moment");
        assert_eq!(outcome.failure, Some(ProbeFailure::Server));
        assert!(
            outcome.detail.contains("upstream is having a moment"),
            "{}",
            outcome.detail
        );
    }

    #[test]
    fn a_long_error_message_is_capped_and_marked() {
        // Ordinary words, not one long run: a single unbroken token of that
        // length is key-shaped, and `redact` would replace it whole — the cap
        // would then be untested rather than exercised.
        let long = "upstream refused ".repeat(DETAIL_CAP_CHARS);
        let outcome = probe_against(400, &format!(r#"{{"error":{{"message":"{long}"}}}}"#));
        assert!(outcome.detail.ends_with('…'), "{}", outcome.detail);
        assert!(
            outcome.detail.chars().count() < DETAIL_CAP_CHARS + 64,
            "{}",
            outcome.detail.chars().count()
        );
    }

    #[test]
    fn a_closed_port_is_a_transport_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let outcome = probe(Provider::Ollama, &base, "k", Duration::from_secs(2));
        assert_eq!(outcome.failure, Some(ProbeFailure::Transport));
        assert!(!outcome.ready);
    }

    #[test]
    fn redact_hides_a_bearer_token() {
        let text = "transport: header Bearer sk-abcdefghijklmnop rejected";
        let out = redact(text);
        assert!(!out.contains("sk-abcdefghijklmnop"), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }

    #[test]
    fn redact_hides_a_bare_key_shaped_run() {
        let key = "a".repeat(48);
        let out = redact(&format!("body carried {key} back"));
        assert!(!out.contains(&key), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }

    #[test]
    fn redact_hides_a_long_key_shaped_run_whatever_separators_it_uses() {
        // A provider's key is not a bare alphanumeric run: it is 30+ characters
        // that mix case, digits, dashes, underscores and dots. Dropping any one
        // of those four classes from the accepted set would let a real key
        // through, so the fixture carries all of them and the assertion below
        // pins the length the predicate is about.
        let key = "aB3k9_Lm2-nQ7.xR4tY6uI8oP0sD5fG1hJ9kL3zX8cV2bN";
        assert!(
            key.len() >= KEY_RUN_CHARS,
            "the fixture has to be key-shaped"
        );
        let out = redact(&format!("body carried {key} back"));
        assert!(!out.contains(key), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }

    #[test]
    fn redact_masks_a_prefixed_key_only_past_a_bare_words_length() {
        // The prefix rule deliberately leaves a short token alone: `sk-` plus
        // five characters is a bare word, and masking ordinary prose is the
        // false positive this predicate exists to avoid. One character longer
        // is a key.
        let bare = "sk-abcde";
        assert_eq!(bare.len(), 8, "the fixture is the bare-word length");
        assert_eq!(redact(bare), bare, "an 8-character token stays readable");

        let key = "sk-abcdef";
        let out = redact(key);
        assert!(!out.contains(key), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }

    #[test]
    fn redact_keeps_ordinary_prose_intact() {
        let text = "no usable account credit (HTTP 402)";
        assert_eq!(redact(text), text);
        // A URL is the other long thing a provider's error body carries, and
        // it is not a key: its `:` and `/` sit outside the key alphabet, so
        // the run is not key-shaped however long it is. The predicate has to
        // reject on any one of them — accept on any one and every long token
        // in the report is masked, URL included.
        let url = "https://ollama.com/v1/chat/completions";
        assert_eq!(redact(url), url);
    }

    #[test]
    fn redact_strips_the_wrapper_a_provider_quotes_a_key_with() {
        // A provider quotes the value back — `'sk-…'`, `"sk-…"`, `` `sk-…` `` —
        // and a quote is in no key's alphabet, so a token judged with its
        // wrapper still on matched none of the rules.
        let key = "sk-abcdefghijklmnop";
        for wrapped in [format!("'{key}'"), format!("\"{key}\""), format!("`{key}`")] {
            let out = redact(&format!("invalid key {wrapped}"));
            assert!(!out.contains(key), "the wrapper hid the key: {out}");
            assert!(out.contains("<redacted>"), "{out}");
        }
    }

    #[test]
    fn redact_reads_the_value_of_an_assignment() {
        // `api_key=sk-…` and `key:sk-…`: the name in front of the value is
        // what stopped the token looking like a key. The third is a quoted
        // value with no space after the `=`, where the outer trim takes the
        // closing quote and the value keeps the opening one.
        let key = "sk-abcdefghijklmnop";
        for assigned in [
            format!("api_key={key}"),
            format!("key:{key}"),
            format!("api_key=\"{key}\""),
        ] {
            let out = redact(&format!("body carried {assigned} back"));
            assert!(!out.contains(key), "the assignment hid the key: {out}");
            assert!(out.contains("<redacted>"), "{out}");
        }
    }

    #[test]
    fn redact_keeps_a_url_intact_quoted_or_behind_a_name() {
        // The other long token an error body carries is a URL. Trimming the
        // wrapper and reading an assignment value must not turn one into a
        // false positive: the `:` and the `/` still fail the key-shaped test,
        // on the token and on the value after the first separator alike.
        for url in [
            "https://ollama.com/v1/chat/completions",
            "\"https://ollama.com/v1/chat/completions\"",
            "url=https://ollama.com/v1/chat/completions",
        ] {
            assert_eq!(redact(url), url, "{url}");
        }
    }

    #[test]
    fn redact_masks_a_key_shaped_run_at_the_length_cap_and_keeps_one_under_it() {
        // For a run with no prefix the cap is the whole rule: one character
        // under it is a word, the cap itself is a key.
        let under = "a".repeat(KEY_RUN_CHARS - 1);
        let at_cap = "a".repeat(KEY_RUN_CHARS);
        assert_eq!(redact(&under), under, "one under the cap stays readable");
        assert_eq!(redact(&at_cap), "<redacted>", "the cap itself is a key");
    }

    #[test]
    fn redact_keeps_short_words_that_are_not_keys() {
        let text = "provider unreachable";
        assert_eq!(redact(text), text);
    }
}
