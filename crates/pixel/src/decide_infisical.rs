//! Remote classify API keys fetched from an Infisical project.
//!
//! Infisical (infisical.com) is a secrets manager. When configured, it is
//! the third key source for a remote classify preset, after the preset's
//! env var and `pixel config remote-key`: the secret named like the key's
//! env variable (`TYPESAFE_API_KEY`, `OPENROUTER_API_KEY`, …) is read once
//! per invocation through the v4 read-secret API and held like any other
//! key value — never logged, never written to a document, never stored in
//! the Pixel config.
//!
//! Configuration (all environment, all optional; the source is off when
//! both `INFISICAL_TOKEN` and `PIXEL_INFISICAL_PROJECT_ID` are absent):
//!
//! - `INFISICAL_TOKEN` — the bearer token (a service or access token).
//!   `PIXEL_INFISICAL_TOKEN` wins when both are set.
//! - `PIXEL_INFISICAL_PROJECT_ID` — the project to read from (the v4 API
//!   takes it as a query parameter).
//! - `PIXEL_INFISICAL_URL` — self-hosted instances; default
//!   `https://app.infisical.com`. A non-loopback plain-`http://` URL is
//!   refused: the token would cross the network in clear text.
//! - `PIXEL_INFISICAL_ENV` — environment slug; default `prod`.
//! - `PIXEL_INFISICAL_SECRET_NAME` — overrides the secret's name when it
//!   should differ from the key's env-var name.
//!
//! Contract: a configured lookup that fails — transport error, HTTP other
//! than 404, a hidden secret — fails the decision loudly, because a
//! mis-permissioned source must not silently fall back to "no key". A 404
//! (no such secret) and an empty secret count as absent, so the standard
//! missing-key error can name the fix. A half-configured source (one of
//! the two required variables set) is also a loud error.

use crate::decide_remote::Preset;
use serde_json::Value;
use std::time::Duration;

/// Infisical's cloud host; self-hosted instances set `PIXEL_INFISICAL_URL`.
const DEFAULT_URL: &str = "https://app.infisical.com";
const TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_CAP_BYTES: usize = 262_144; // 256 KiB

/// The settings resolved once per lookup.
#[derive(Debug, Clone, PartialEq)]
struct Settings {
    base: String,
    token: String,
    project_id: String,
    environment: String,
    secret_name: String,
}

/// Transport failures, split so a 404 can mean "absent" while every other
/// status is loud.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HttpError {
    Status(u16),
    Transport(String),
}

/// The transport seam: one GET with the bearer token, returning the body
/// text. Production is [`http_get`]; tests substitute a closure.
type GetFn = Box<dyn Fn(&str, &str) -> Result<String, HttpError>>;

/// Read the preset's key from Infisical, once per invocation. `Ok(None)`
/// means the source is not configured or has no such secret — the caller
/// falls through to the standard missing-key error.
#[cfg_attr(test, mutants::skip)] // env adapter over `lookup_key_from`; the policy is tested there
pub(crate) fn lookup_key(preset: Preset) -> Result<Option<String>, String> {
    lookup_key_from(preset, &|name| std::env::var(name).ok(), Box::new(http_get))
}

fn lookup_key_from(
    preset: Preset,
    env: &dyn Fn(&str) -> Option<String>,
    get: GetFn,
) -> Result<Option<String>, String> {
    let Some(settings) = settings_from(preset, env)? else {
        return Ok(None);
    };
    if crate::decide_remote::sends_in_clear_text(&settings.base) {
        return Err(format!(
            "refusing to send the Infisical token to {} over cleartext http; use https or a loopback base",
            settings.base
        ));
    }
    let url = format!(
        "{}/api/v4/secrets/{}?projectId={}&environment={}&type=shared",
        settings.base.trim_end_matches('/'),
        encode(&settings.secret_name),
        encode(&settings.project_id),
        encode(&settings.environment),
    );
    let body = match get(&url, &settings.token) {
        // No such secret: the source is configured but has nothing, which
        // is "absent", not an error — the standard missing-key message can
        // then name the env var and `pixel config remote-key` fix.
        Err(HttpError::Status(404)) => return Ok(None),
        Err(HttpError::Status(code)) => {
            return Err(format!("infisical {url}: HTTP {code}"));
        }
        Err(HttpError::Transport(detail)) => {
            return Err(format!("infisical {url}: {detail}"));
        }
        Ok(body) => body,
    };
    parse_secret(&body, &settings.secret_name)
}

/// Resolve the settings from the environment, or `None` when the source is
/// off. One required variable without the other is a configuration error,
/// not a silent skip.
fn settings_from(
    preset: Preset,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Settings>, String> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    let token = set("PIXEL_INFISICAL_TOKEN").or_else(|| set("INFISICAL_TOKEN"));
    let project_id = set("PIXEL_INFISICAL_PROJECT_ID");
    match (token, project_id) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(
            "Infisical is half-configured: set PIXEL_INFISICAL_PROJECT_ID alongside the token, or unset both"
                .to_string(),
        ),
        (None, Some(_)) => Err(
            "Infisical is half-configured: set INFISICAL_TOKEN (or PIXEL_INFISICAL_TOKEN) alongside PIXEL_INFISICAL_PROJECT_ID, or unset both"
                .to_string(),
        ),
        (Some(token), Some(project_id)) => Ok(Some(Settings {
            base: set("PIXEL_INFISICAL_URL").unwrap_or_else(|| DEFAULT_URL.to_string()),
            token,
            project_id,
            environment: set("PIXEL_INFISICAL_ENV").unwrap_or_else(|| "prod".to_string()),
            secret_name: set("PIXEL_INFISICAL_SECRET_NAME").unwrap_or_else(|| {
                preset
                    .key_env()
                    .unwrap_or("PIXEL_CLASSIFY_API_KEY")
                    .to_string()
            }),
        })),
    }
}

/// Read `secret.secretValue` from the v4 read-secret response. A hidden
/// value is a permission problem, not an empty secret, and must be loud;
/// an empty value counts as absent.
fn parse_secret(body: &str, secret_name: &str) -> Result<Option<String>, String> {
    let parsed: Value =
        serde_json::from_str(body).map_err(|e| format!("infisical response is not JSON: {e}"))?;
    let secret = parsed
        .get("secret")
        .ok_or("infisical response carries no secret object")?;
    if secret
        .get("secretValueHidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!(
            "infisical secret {secret_name} is hidden — the token lacks view-secret permission"
        ));
    }
    let value = secret
        .get("secretValue")
        .and_then(Value::as_str)
        .ok_or("infisical secret response carries no secretValue string")?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

/// The production transport: one GET with the bearer token, bounded by
/// `TIMEOUT` and by `RESPONSE_CAP_BYTES`. The token never enters an error
/// string — errors carry the URL only, and the token is a header value.
fn http_get(url: &str, token: &str) -> Result<String, HttpError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent("pixel-cli classify-infisical")
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut response = agent
        .get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => HttpError::Status(code),
            other => HttpError::Transport(format!("{other}")),
        })?;
    response
        .body_mut()
        .with_config()
        .limit(RESPONSE_CAP_BYTES as u64)
        .read_to_string()
        .map_err(|e| HttpError::Transport(format!("{e}")))
}

/// Percent-encode a URL path or query component: everything outside
/// RFC 3986's unreserved set, so secret names and slugs cannot reshape the
/// request.
fn encode(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for byte in component.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    /// One transport call, recorded: (URL, bearer token).
    type RecordedCall = (String, String);
    /// The recorded calls plus a get seam answering the fixture body.
    type RecordedGet = (
        Arc<Mutex<Vec<RecordedCall>>>,
        Box<dyn Fn(&str, &str) -> Result<String, HttpError>>,
    );

    /// A get seam that records every call and answers the fixture body.
    fn recorded_get(body: &str) -> RecordedGet {
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let body = body.to_string();
        let get = move |url: &str, token: &str| -> Result<String, HttpError> {
            sink.lock()
                .unwrap()
                .push((url.to_string(), token.to_string()));
            Ok(body.clone())
        };
        (calls, Box::new(get))
    }

    fn configured_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let mut all: Vec<(&str, &str)> = vec![
            ("INFISICAL_TOKEN", "st-secret-token"),
            ("PIXEL_INFISICAL_PROJECT_ID", "proj-7"),
        ];
        all.extend_from_slice(pairs);
        let map: std::collections::BTreeMap<String, String> = all
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_source_is_off_when_no_infisical_variable_is_set() {
        let env = env_of(&[("OPENROUTER_API_KEY", "unrelated")]);
        assert_eq!(settings_from(Preset::Jev, &env).unwrap(), None);
        let (_calls, get) = recorded_get("{}");
        assert_eq!(lookup_key_from(Preset::Jev, &env, get).unwrap(), None);
    }

    #[test]
    fn half_configured_sources_are_errors_not_silent_skips() {
        let token_only = env_of(&[("INFISICAL_TOKEN", "st-token")]);
        let error = settings_from(Preset::Jev, &token_only).unwrap_err();
        assert!(error.contains("PIXEL_INFISICAL_PROJECT_ID"), "{error}");
        let project_only = env_of(&[("PIXEL_INFISICAL_PROJECT_ID", "proj")]);
        let error = settings_from(Preset::Jev, &project_only).unwrap_err();
        assert!(error.contains("INFISICAL_TOKEN"), "{error}");
        // Both errors are loud even at lookup time.
        let (_calls, get) = recorded_get("{}");
        assert!(lookup_key_from(Preset::Jev, &token_only, get).is_err());
    }

    #[test]
    fn settings_take_the_pixel_token_and_the_documented_defaults() {
        let env = env_of(&[
            ("PIXEL_INFISICAL_TOKEN", "pixel-token"),
            ("INFISICAL_TOKEN", "generic-token"),
            ("PIXEL_INFISICAL_PROJECT_ID", "proj-7"),
        ]);
        assert_eq!(
            settings_from(Preset::Jev, &env).unwrap(),
            Some(Settings {
                base: "https://app.infisical.com".to_string(),
                token: "pixel-token".to_string(),
                project_id: "proj-7".to_string(),
                environment: "prod".to_string(),
                secret_name: "TYPESAFE_API_KEY".to_string(),
            })
        );
        // The generic token is used when the pixel-prefixed one is absent,
        // and the overrides move base, environment and secret name.
        let env = env_of(&[
            ("INFISICAL_TOKEN", "generic-token"),
            ("PIXEL_INFISICAL_PROJECT_ID", "proj-7"),
            ("PIXEL_INFISICAL_URL", "https://infisical.internal/"),
            ("PIXEL_INFISICAL_ENV", "staging"),
            ("PIXEL_INFISICAL_SECRET_NAME", "classify/jev"),
        ]);
        let settings = settings_from(Preset::Jev, &env).unwrap().unwrap();
        assert_eq!(settings.token, "generic-token");
        assert_eq!(settings.base, "https://infisical.internal/");
        assert_eq!(settings.environment, "staging");
        assert_eq!(settings.secret_name, "classify/jev");
    }

    #[test]
    fn lookup_builds_the_v4_read_url_and_sends_the_bearer_token() {
        let body = json!({"secret": {"secretValue": "tsk-live-key"}}).to_string();
        let (calls, get) = recorded_get(&body);
        let key = lookup_key_from(Preset::Jev, &configured_env(&[]), get).unwrap();
        assert_eq!(key.as_deref(), Some("tsk-live-key"));
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "one lookup, one request: {calls:?}");
        assert_eq!(
            calls[0].0,
            "https://app.infisical.com/api/v4/secrets/TYPESAFE_API_KEY?projectId=proj-7&environment=prod&type=shared"
        );
        assert_eq!(calls[0].1, "st-secret-token");
    }

    #[test]
    fn lookup_encodes_names_that_could_reshape_the_url() {
        let body = json!({"secret": {"secretValue": "v"}}).to_string();
        let (calls, get) = recorded_get(&body);
        let env = configured_env(&[
            ("PIXEL_INFISICAL_SECRET_NAME", "classify/jev key"),
            ("PIXEL_INFISICAL_ENV", "dev,1"),
        ]);
        let _ = lookup_key_from(Preset::Jev, &env, get).unwrap();
        let calls = calls.lock().unwrap().clone();
        assert!(
            calls[0]
                .0
                .starts_with("https://app.infisical.com/api/v4/secrets/classify%2Fjev%20key?"),
            "a raw `/` or space would address a different secret: {}",
            calls[0].0
        );
        assert!(
            calls[0].0.contains("environment=dev%2C1&"),
            "{}",
            calls[0].0
        );
    }

    #[test]
    fn encode_passes_unreserved_bytes_and_escapes_everything_else() {
        assert_eq!(encode("TYPESAFE_API_KEY-._~0aZ"), "TYPESAFE_API_KEY-._~0aZ");
        assert_eq!(encode("a b/c&d=e"), "a%20b%2Fc%26d%3De");
        assert_eq!(encode("café"), "caf%C3%A9");
    }

    #[test]
    fn a_missing_secret_is_absent_but_other_statuses_are_loud() {
        let missing =
            |_url: &str, _token: &str| -> Result<String, HttpError> { Err(HttpError::Status(404)) };
        assert_eq!(
            lookup_key_from(Preset::Jev, &configured_env(&[]), Box::new(missing)).unwrap(),
            None
        );
        let forbidden =
            |_url: &str, _token: &str| -> Result<String, HttpError> { Err(HttpError::Status(403)) };
        let error =
            lookup_key_from(Preset::Jev, &configured_env(&[]), Box::new(forbidden)).unwrap_err();
        assert!(error.contains("HTTP 403"), "{error}");
        assert!(error.contains("infisical"), "{error}");
    }

    #[test]
    fn transport_failures_name_the_url_without_the_token() {
        let broken = |_url: &str, _token: &str| -> Result<String, HttpError> {
            Err(HttpError::Transport("connection refused".to_string()))
        };
        let error =
            lookup_key_from(Preset::Jev, &configured_env(&[]), Box::new(broken)).unwrap_err();
        assert!(error.contains("connection refused"), "{error}");
        assert!(error.contains("https://app.infisical.com"), "{error}");
        assert!(!error.contains("st-secret-token"), "{error}");
    }

    #[test]
    fn parse_secret_reads_the_value_and_refuses_hidden_or_shapeless_ones() {
        let body = json!({
            "secret": {"secretKey": "TYPESAFE_API_KEY", "secretValue": "tsk-live", "secretValueHidden": false}
        })
        .to_string();
        assert_eq!(
            parse_secret(&body, "TYPESAFE_API_KEY").unwrap().as_deref(),
            Some("tsk-live")
        );
        // Hidden: a permission problem, reported as such.
        let hidden = json!({"secret": {"secretValue": "", "secretValueHidden": true}}).to_string();
        let error = parse_secret(&hidden, "TYPESAFE_API_KEY").unwrap_err();
        assert!(error.contains("hidden"), "{error}");
        assert!(error.contains("view-secret"), "{error}");
        // Empty value: absent, so the standard missing-key error can speak.
        let empty =
            json!({"secret": {"secretValue": "  ", "secretValueHidden": false}}).to_string();
        assert_eq!(parse_secret(&empty, "s").unwrap(), None);
        // Shape violations are errors, not silent defaults.
        for body in [json!({}).to_string(), json!({"secret": {}}).to_string()] {
            assert!(parse_secret(&body, "s").is_err());
        }
    }

    #[test]
    fn a_cleartext_nonloopback_infisical_url_is_refused() {
        let env = configured_env(&[("PIXEL_INFISICAL_URL", "http://infisical.internal")]);
        let never = |_url: &str, _token: &str| -> Result<String, HttpError> {
            unreachable!("the guard must refuse before any request")
        };
        let error = lookup_key_from(Preset::Jev, &env, Box::new(never)).unwrap_err();
        assert!(error.contains("cleartext"), "{error}");
        // Loopback http stays allowed: that is a local, trusted hop.
        let body = json!({"secret": {"secretValue": "tsk-live-key"}}).to_string();
        let (calls, get) = recorded_get(&body);
        let env = configured_env(&[("PIXEL_INFISICAL_URL", "http://localhost:8080")]);
        let key = lookup_key_from(Preset::Jev, &env, get).unwrap();
        assert_eq!(key.as_deref(), Some("tsk-live-key"));
        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls[0].0,
            "http://localhost:8080/api/v4/secrets/TYPESAFE_API_KEY?projectId=proj-7&environment=prod&type=shared"
        );
    }
}
