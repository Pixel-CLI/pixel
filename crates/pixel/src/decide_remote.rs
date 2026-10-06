// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` — OpenAI-compatible chat completion.
//!
//! The remote engine maps the wire contract — `text` + `context` +
//! `labels` + `criteria` → a probability distribution — onto a chat
//! completion through any endpoint exposing OpenAI's `/chat/completions`
//! shape: OpenRouter, Ollama Cloud, or a local `llama-server`/Ollama on
//! localhost (the same adapter serves all three; only the base URL and the
//! API-key env var differ).
//!
//! Remote is the only decision engine — the local `static`/`verdict`
//! backends were removed. The path is non-deterministic: the probabilities
//! are verbalized by the model (self-reported), not a native head output.
//! That is disclosed in `snapshot.deterministic = false` and
//! `snapshot.basis`, and the response is renormalized to sum 1 before it
//! reaches the caller.
//!
//! Config: a preset (`openrouter` | `ollama` | `local` | `deepseek` |
//! `opencode-go` | `jev`) selects the base URL, the default model, and the
//! env var that names the API key; the preset is overridable by
//! `--remote-model` / `PIXEL_REMOTE_MODEL`, `PIXEL_REMOTE_BASE`, and
//! `PIXEL_REMOTE_KEY_ENV`. The key resolves from the env var first, then
//! from `remote_keys.<preset>` in `~/.pixel/config.json`
//! (`pixel config remote-key`), then — when Infisical is configured — from
//! the project's secrets (see `decide_infisical`). The key value itself is
//! only ever written into the Authorization header — never into logs, error
//! text, or documents — and is passed by env-var name only.
//!
//! `jev` is the exception in transport: TypeSafe's hosted Jev speaks the
//! `/v1/systemone` decision shape, not `/chat/completions`, so its preset
//! config is resolved here (base, model, key) but served by [`decide_jev`].

use crate::classify::Spec;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// A bound on how much of a chat response we are willing to read. A runaway
/// completion must fail the decision, not exhaust the process.
const RESPONSE_CAP_BYTES: usize = 1_048_576; // 1 MiB
const TIMEOUT: Duration = Duration::from_secs(60);

/// Provider presets, selectable with `--remote-preset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum Preset {
    #[default]
    Openrouter,
    Ollama,
    Local,
    Deepseek,
    OpencodeGo,
    Jev,
}

impl Preset {
    /// Base URL for `/chat/completions`.
    fn base(&self) -> &'static str {
        match self {
            Preset::Openrouter => "https://openrouter.ai/api/v1",
            Preset::Ollama => "https://ollama.com/v1",
            Preset::Local => "http://localhost:11434/v1",
            Preset::Deepseek => "https://api.deepseek.com",
            Preset::OpencodeGo => "https://opencode.ai/zen/go/v1",
            // TypeSafe's hosted Jev decision model. Unlike the chat presets
            // it speaks TypeSafe's `/v1/systemone` shape, served by
            // `decide_jev` — the base stays bare so the shared
            // `PIXEL_REMOTE_BASE` override keeps working for it too.
            Preset::Jev => "https://api.typesafe.ai",
        }
    }

    /// The environment variable that names the API key, if the preset needs
    /// one. `local` talks to localhost and takes no key.
    pub fn key_env(&self) -> Option<&'static str> {
        match self {
            Preset::Openrouter => Some("OPENROUTER_API_KEY"),
            Preset::Ollama => Some("OLLAMA_API_KEY"),
            Preset::Local => None,
            Preset::Deepseek => Some("DEEPSEEK_API_KEY"),
            Preset::OpencodeGo => Some("OPENCODE_API_KEY"),
            Preset::Jev => Some("TYPESAFE_API_KEY"),
        }
    }

    /// A sensible default model for the preset; overridable by
    /// `--remote-model` / `PIXEL_REMOTE_MODEL`.
    fn default_model(&self) -> &'static str {
        match self {
            Preset::Openrouter => "deepseek/deepseek-v4.1-flash",
            Preset::Ollama => "deepseek-v4.1-flash:cloud",
            Preset::Local => "qwen3.5:4b",
            Preset::Deepseek => "deepseek-flash",
            Preset::OpencodeGo => "deepseek-v4.1-flash",
            Preset::Jev => "jev-latest",
        }
    }

    /// Stable id surfaced in `snapshot.provider` and the `remote_keys`
    /// config map.
    pub fn display(&self) -> &'static str {
        match self {
            Preset::Openrouter => "openrouter",
            Preset::Ollama => "ollama",
            Preset::Local => "local",
            Preset::Deepseek => "deepseek",
            Preset::OpencodeGo => "opencode-go",
            Preset::Jev => "jev",
        }
    }

    /// OpenCode Go requires each conversation to carry a stable session id
    /// (`x-opencode-session`) and rejects generic SDK user agents.
    fn wants_session_header(&self) -> bool {
        matches!(self, Preset::OpencodeGo)
    }

    /// The preset behind its `display()` name (install prompts, config).
    pub fn parse_name(name: &str) -> Option<Preset> {
        let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
        [
            Preset::Openrouter,
            Preset::Ollama,
            Preset::Local,
            Preset::Deepseek,
            Preset::OpencodeGo,
            Preset::Jev,
        ]
        .into_iter()
        .find(|preset| preset.display() == normalized)
    }
}

/// Resolved remote configuration: the values actually used, independent of
/// how they were chosen (flag, env, or preset default).
#[derive(Clone)]
pub struct Config {
    pub preset: Preset,
    pub base: String,
    pub model: String,
    /// The API key value, read once from an env var by name. Never logged.
    /// `pub(crate)` accessor below: the hosted Jev engine takes the same
    /// resolved key.
    key: Option<String>,
    /// Stable per-invocation session id for providers that require one
    /// (`x-opencode-session` for OpenCode Go); `None` elsewhere.
    pub session_id: Option<String>,
}

/// `Debug` masks the key — `Config` appears in error paths and a derived
/// impl would dump the secret into logs.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("preset", &self.preset)
            .field("base", &self.base)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The resolved API-key value, for callers that build their own
    /// transport from a resolved [`Config`] (hosted Jev). Keep it out of
    /// logs and documents, like `key` itself.
    pub(crate) fn key_value(&self) -> Option<String> {
        self.key.clone()
    }
}

/// One stable session id per invocation: providers that require a session
/// header get the same value on every request of this run. Pure so a test
/// can pin the shape and the stability.
fn new_session_id(now_millis: u128, pid: u32) -> String {
    format!("pixel-classify-{pid}-{now_millis}")
}

/// The env var the key is read from: `PIXEL_REMOTE_KEY_ENV` when it names
/// one, else the preset's own.
pub fn key_env_name(preset: Preset, explicit: Option<String>) -> Option<String> {
    explicit
        .filter(|s| !s.is_empty())
        .or_else(|| preset.key_env().map(str::to_string))
}

/// Resolve a `Config` from the preset and the per-invocation overrides,
/// applying `PIXEL_REMOTE_*` env vars on top; callers must not propagate
/// the key value into logs or JSON.
pub fn resolve_config(
    preset: Preset,
    model_override: Option<String>,
    key_value: Option<String>,
) -> Result<Config, String> {
    resolve_config_from(preset, model_override, key_value, |name| {
        std::env::var(name).ok()
    })
}

/// [`resolve_config`] with the environment injected. Refuses, before any
/// network call, the two configurations that would fail or leak remotely:
/// a preset that needs a key but has none (the provider would answer an
/// opaque 401), and a key bound for a non-loopback `http://` base (it would
/// cross the network in clear text).
pub(crate) fn resolve_config_from(
    preset: Preset,
    model_override: Option<String>,
    key_value: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Config, String> {
    let set = |name: &str| env(name).filter(|s| !s.is_empty());
    let base_override = set("PIXEL_REMOTE_BASE");
    let key = key_value.filter(|s| !s.is_empty());
    if key.is_none()
        && base_override.is_none()
        && preset.key_env().is_some()
        && let Some(var) = key_env_name(preset, set("PIXEL_REMOTE_KEY_ENV"))
    {
        let name = preset.display();
        return Err(format!(
            "remote preset {name} needs an API key: set {var} or run `pixel config remote-key {name} -`"
        ));
    }
    let base = base_override.unwrap_or_else(|| preset.base().to_string());
    if key.is_some() && sends_in_clear_text(&base) {
        return Err(format!(
            "refusing to send the API key to {base} over cleartext http; use https or a loopback base"
        ));
    }
    let model = model_override
        .filter(|s| !s.is_empty())
        .or_else(|| set("PIXEL_REMOTE_MODEL"))
        .unwrap_or_else(|| preset.default_model().to_string());
    let session_id = if preset.wants_session_header() {
        Some(new_session_id(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            std::process::id(),
        ))
    } else {
        None
    };
    Ok(Config {
        preset,
        base,
        model,
        key,
        session_id,
    })
}

/// Whether `base` is plain `http://` to a host other than this machine.
/// `pub(crate)`: the Infisical lookup reuses it before sending its token.
pub(crate) fn sends_in_clear_text(base: &str) -> bool {
    let Some(rest) = base
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .map(|_| &base[7..])
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = host.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or_default()
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    !loopback
}

/// The injected HTTP seam: a single chat completion given the config and the
/// request body. Production uses `http_chat`; tests substitute a scripted fn.
type ChatFn = Box<dyn Fn(&Config, &Value) -> Result<Value, String>>;

/// The network edge. `ureq` is a small enough surface that a single
/// implementation lives here; tests drive the same `Remote::decide` via a
/// `chat` injection point, so no network happens in unit tests.
pub struct Remote {
    config: Config,
    /// Injected here so the full request→parse path is testable offline.
    chat: ChatFn,
}

impl Remote {
    /// Production handle: real HTTP through `ureq`.
    pub fn open(config: Config) -> Remote {
        Remote {
            config,
            chat: Box::new(http_chat),
        }
    }

    /// Test handle: the same `Remote` with a scripted `chat` closure.
    #[cfg(test)]
    fn with_chat(
        config: Config,
        chat: impl Fn(&Config, &Value) -> Result<Value, String> + 'static,
    ) -> Remote {
        Remote {
            config,
            chat: Box::new(chat),
        }
    }

    /// Model id surfaced in `snapshot.model`.
    pub fn model_id(&self) -> &str {
        &self.config.model
    }

    /// Provider preset surfaced in `snapshot.provider`.
    pub fn provider(&self) -> &'static str {
        self.config.preset.display()
    }

    /// Always non-deterministic at this point in the code path; the caller
    /// still checks it so a future deterministic remote never lies.
    pub fn deterministic(&self) -> bool {
        false
    }

    /// The full decision: build the request, POST it, parse and renormalize.
    pub fn decide(&self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        let body = build_request(&self.config.model, spec);
        let response = (self.chat)(&self.config, &body)?;
        parse_probs(&response, &spec.labels)
    }
}

/// Assemble the `/chat/completions` body: the spec's `context` frames the
/// system message, `text` is the state, and every label with its criterion
/// (or label fallback) is enumerated verbatim — arbitrary runtime labels are
/// preserved, exactly as the static and verdict backends preserve them.
fn build_request(model: &str, spec: &Spec) -> Value {
    let system = if spec.context.is_empty() {
        "Classify the state below into exactly one of the given options."
    } else {
        spec.context.as_str()
    };
    let mut options = String::new();
    for label in &spec.labels {
        let criterion = spec
            .criteria
            .get(label)
            .map_or(label.as_str(), String::as_str);
        options.push_str(&format!("- {label}: {criterion}\n"));
    }
    let user = format!(
        "State:\n{}\n\nOptions:\n{}Choose exactly one option. Reply with a JSON object mapping each option to a probability, summing to 1.",
        spec.text, options
    );
    json!({
        "model": model,
        "temperature": 0.0,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "decision",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"probs": probs_schema(&spec.labels)},
                    "required": ["probs"],
                    "additionalProperties": false
                }
            }
        }
    })
}

/// The `probs` object as strict structured outputs require it: one number
/// property per label, all required, nothing else (OpenAI's strict mode
/// rejects an open `additionalProperties` map).
fn probs_schema(labels: &[String]) -> Value {
    let properties: serde_json::Map<String, Value> = labels
        .iter()
        .map(|label| (label.clone(), json!({"type": "number"})))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": labels,
        "additionalProperties": false
    })
}

/// The production transport: [`http_chat_within`] with the production caps.
#[cfg_attr(test, mutants::skip)] // passes two constants; the transport is tested through http_chat_within
fn http_chat(config: &Config, body: &Value) -> Result<Value, String> {
    http_chat_within(config, body, TIMEOUT, RESPONSE_CAP_BYTES)
}

/// One POST to `{base}/chat/completions` with the bearer key in the header,
/// bounded by `timeout` and by `cap` bytes of response. The key value never
/// enters the error string or the body.
fn http_chat_within(
    config: &Config,
    body: &Value,
    timeout: Duration,
    cap: usize,
) -> Result<Value, String> {
    let url = format!("{}/chat/completions", config.base.trim_end_matches('/'));
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent("pixel-cli classify-remote")
        // Non-2xx handled below so the provider's error body (which usually
        // names the real cause — blocked model, bad key, no route) reaches
        // the error string instead of a bare status code.
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut request = agent.post(&url);
    if let Some(key) = &config.key {
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    if let Some(session) = &config.session_id {
        request = request.header("x-opencode-session", session);
    }
    let mut response = request
        .send_json(body)
        .map_err(|e| format!("remote chat {url}: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(cap as u64)
        .read_to_string();
    if !(200..300).contains(&status) {
        // The status is the fact; the body is a best-effort explanation, so
        // an oversized or non-UTF-8 error body still reports the status.
        let snippet: String = text.unwrap_or_default().chars().take(400).collect();
        return Err(format!(
            "remote chat {url}: http status {status}: {snippet}"
        ));
    }
    let text = text.map_err(|e| format!("remote chat read {url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("remote chat JSON {url}: {e}"))
}

/// Extract `choices[0].message.content` (a JSON string), parse its
/// `probs` object, and renormalize to sum 1 over the caller's labels.
/// Unknown keys are an error, not a silent drop: a label the provider added
/// changes the distribution shape and must not be renormalized away.
fn parse_probs(response: &Value, labels: &[String]) -> Result<BTreeMap<String, f64>, String> {
    let content = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or("remote response missing choices[0].message.content")?;
    let parsed: Value = serde_json::from_str(content)
        .map_err(|e| format!("remote response content is not JSON: {e}"))?;
    // Accept both shapes providers actually emit: the schema-requested
    // `{"probs": {...}}` (OpenRouter/Ollama honouring `response_format`)
    // and the flat `{label: prob}` mapping (what some local/Ollama models
    // return despite the schema). Prefer the nested form; a non-object
    // `probs` is only the flat form when `probs` is itself a label.
    let probs_is_a_label = labels.iter().any(|label| label == "probs");
    let probs = match parsed.get("probs") {
        Some(Value::Object(map)) => map,
        Some(_) if !probs_is_a_label => {
            return Err("remote response probs is not an object".to_string());
        }
        _ => parsed
            .as_object()
            .ok_or("remote response has neither a probs object nor a flat label→probability map")?,
    };
    let mut out: BTreeMap<String, f64> = labels.iter().map(|l| (l.clone(), 0.0)).collect();
    let mut sum = 0.0f64;
    for (label, value) in probs {
        let p = value
            .as_f64()
            .filter(|p| p.is_finite() && *p >= 0.0)
            .ok_or_else(|| {
                format!("remote probs[{label:?}] is not a finite non-negative number")
            })?;
        if !out.contains_key(label) {
            return Err(format!(
                "remote returned probability for unknown label {label:?} (expected only: {labels:?})"
            ));
        }
        out.insert(label.clone(), p);
        sum += p;
    }
    // Finite values can still overflow the sum (2 × 1e308 = inf), which
    // would normalize every label to 0 and report success.
    if !sum.is_finite() || sum <= 0.0 {
        return Err("remote probs must sum to a finite positive value".to_string());
    }
    for p in out.values_mut() {
        *p /= sum;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(text: &str, context: &str, labels: &[&str], criteria: &[(&str, &str)]) -> Spec {
        Spec::checked(
            text.to_string(),
            context.to_string(),
            labels.iter().map(ToString::to_string).collect(),
            criteria
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    /// A response object with the given `probs` content, wrapped in the
    /// chat-completion shape `parse_probs` reads.
    fn chat_with(content: &str) -> Value {
        json!({
            "choices": [{"message": {"content": content}}]
        })
    }

    #[test]
    fn each_preset_points_at_its_provider_model_and_key_variable() {
        let table = [
            (
                Preset::Openrouter,
                "https://openrouter.ai/api/v1",
                "deepseek/deepseek-v4.1-flash",
                "openrouter",
                Some("OPENROUTER_API_KEY"),
            ),
            (
                Preset::Ollama,
                "https://ollama.com/v1",
                "deepseek-v4.1-flash:cloud",
                "ollama",
                Some("OLLAMA_API_KEY"),
            ),
            (
                Preset::Local,
                "http://localhost:11434/v1",
                "qwen3.5:4b",
                "local",
                None,
            ),
            (
                Preset::Deepseek,
                "https://api.deepseek.com",
                "deepseek-flash",
                "deepseek",
                Some("DEEPSEEK_API_KEY"),
            ),
            (
                Preset::OpencodeGo,
                "https://opencode.ai/zen/go/v1",
                "deepseek-v4.1-flash",
                "opencode-go",
                Some("OPENCODE_API_KEY"),
            ),
            (
                Preset::Jev,
                "https://api.typesafe.ai",
                "jev-latest",
                "jev",
                Some("TYPESAFE_API_KEY"),
            ),
        ];
        for (preset, base, model, display, key_env) in table {
            assert_eq!(
                (
                    preset.base(),
                    preset.default_model(),
                    preset.display(),
                    preset.key_env()
                ),
                (base, model, display, key_env),
                "{preset:?}"
            );
        }
        // OpenCode Go requires a per-conversation session header; nobody
        // else sends one.
        assert!(Preset::OpencodeGo.wants_session_header());
        assert!(!Preset::Openrouter.wants_session_header());
        assert!(!Preset::Local.wants_session_header());
        assert!(!Preset::Jev.wants_session_header());
    }

    #[test]
    fn parse_name_accepts_normalized_preset_names_and_rejects_unknown_ones() {
        assert_eq!(Preset::parse_name("OpenRouter"), Some(Preset::Openrouter));
        assert_eq!(Preset::parse_name("opencode_go"), Some(Preset::OpencodeGo));
        assert_eq!(Preset::parse_name("  DEEPSEEK  "), Some(Preset::Deepseek));
        assert_eq!(Preset::parse_name("jev"), Some(Preset::Jev));
        assert_eq!(Preset::parse_name("not-a-provider"), None);
    }

    #[test]
    fn key_value_hands_the_resolved_key_to_custom_transports() {
        // Hosted Jev builds its own transport from a resolved `Config`; the
        // key must survive resolution without re-reading the environment.
        let keyed =
            resolve_config_from(Preset::Jev, None, Some("tsk-resolved".into()), env_of(&[]))
                .unwrap();
        assert_eq!(keyed.key_value().as_deref(), Some("tsk-resolved"));
        let keyless = resolve_config_from(Preset::Jev, None, None, env_of(&[]));
        assert!(keyless.is_err(), "Jev always needs a key");
    }

    #[test]
    fn the_session_id_is_stable_and_well_shaped() {
        let id = new_session_id(1_758_000_000_000, 4242);
        assert_eq!(id, "pixel-classify-4242-1758000000000");
        assert_eq!(id, new_session_id(1_758_000_000_000, 4242));
        assert_ne!(new_session_id(1_758_000_000_001, 4242), id);
        assert_ne!(new_session_id(1_758_000_000_000, 4243), id);
    }

    #[test]
    fn only_the_go_preset_carries_a_session_header() {
        let go = resolve_config_from(Preset::OpencodeGo, None, key(), env_of(&[])).unwrap();
        assert!(
            go.session_id
                .as_deref()
                .is_some_and(|s| s.starts_with("pixel-classify-"))
        );
        let deepseek = resolve_config_from(Preset::Deepseek, None, key(), env_of(&[])).unwrap();
        assert_eq!(deepseek.session_id, None);
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn key() -> Option<String> {
        Some("k".to_string())
    }

    #[test]
    fn the_model_is_the_flag_then_the_env_then_the_preset_default() {
        let env = env_of(&[("PIXEL_REMOTE_MODEL", "from-env")]);
        let flag = resolve_config_from(Preset::Ollama, Some("flag".into()), key(), &env).unwrap();
        assert_eq!(flag.model, "flag");
        let empty_flag =
            resolve_config_from(Preset::Ollama, Some(String::new()), key(), &env).unwrap();
        assert_eq!(empty_flag.model, "from-env", "an empty flag does not count");
        let empty_env = env_of(&[("PIXEL_REMOTE_MODEL", "")]);
        let default = resolve_config_from(Preset::Ollama, None, key(), empty_env).unwrap();
        assert_eq!(default.model, "deepseek-v4.1-flash:cloud");
        assert_eq!(default.base, "https://ollama.com/v1");
        let based = resolve_config_from(
            Preset::Ollama,
            None,
            key(),
            env_of(&[("PIXEL_REMOTE_BASE", "https://proxy.example/v1")]),
        )
        .unwrap();
        assert_eq!(based.base, "https://proxy.example/v1");
    }

    #[test]
    fn a_missing_key_fails_before_the_request_and_names_what_to_set() {
        let error = resolve_config_from(Preset::Openrouter, None, Some(String::new()), env_of(&[]))
            .unwrap_err();
        assert_eq!(
            error,
            "remote preset openrouter needs an API key: set OPENROUTER_API_KEY or run `pixel config remote-key openrouter -`"
        );
        let renamed = resolve_config_from(
            Preset::Ollama,
            None,
            None,
            env_of(&[("PIXEL_REMOTE_KEY_ENV", "MY_KEY")]),
        )
        .unwrap_err();
        assert!(renamed.contains("set MY_KEY"), "{renamed}");
        // A keyless preset, or a base the user pointed elsewhere, needs none.
        assert!(resolve_config_from(Preset::Local, None, None, env_of(&[])).is_ok());
        let proxied = resolve_config_from(
            Preset::Openrouter,
            None,
            None,
            env_of(&[("PIXEL_REMOTE_BASE", "http://localhost:4000/v1")]),
        )
        .unwrap();
        assert_eq!(proxied.key, None);
    }

    #[test]
    fn a_key_never_leaves_the_machine_over_cleartext_http() {
        let with_base = |base: &str| {
            resolve_config_from(
                Preset::Openrouter,
                None,
                key(),
                env_of(&[("PIXEL_REMOTE_BASE", base)]),
            )
        };
        let error = with_base("http://llm.example.com/v1").unwrap_err();
        assert!(error.contains("cleartext http"), "{error}");
        assert!(with_base("https://llm.example.com/v1").is_ok());
        assert!(with_base("http://127.0.0.1:8080/v1").is_ok());
        let keyless = resolve_config_from(
            Preset::Local,
            None,
            None,
            env_of(&[("PIXEL_REMOTE_BASE", "http://llm.example.com/v1")]),
        );
        assert!(keyless.is_ok(), "no key, nothing to leak");
    }

    #[test]
    fn only_plain_http_to_another_host_counts_as_clear_text() {
        for base in [
            "http://example.com",
            "HTTP://example.com/v1",
            "http://localhost.example.com/v1",
            "http://localhost@example.com/v1",
            "http://10.0.0.2:8080/v1",
        ] {
            assert!(sends_in_clear_text(base), "{base}");
        }
        for base in [
            "https://example.com/v1",
            "http://localhost:11434/v1",
            "http://LOCALHOST/v1",
            "http://127.0.0.1:8080/v1",
            "http://127.1.2.3/v1",
            "http://[::1]:8080/v1",
            "http://user@localhost/v1",
            "ftp",
        ] {
            assert!(!sends_in_clear_text(base), "{base}");
        }
    }

    #[test]
    fn an_open_remote_reports_its_model_provider_and_nondeterminism() {
        let config =
            resolve_config_from(Preset::Ollama, Some("m1".into()), key(), env_of(&[])).unwrap();
        let remote = Remote::open(config);
        assert_eq!(remote.model_id(), "m1");
        assert_eq!(remote.provider(), "ollama");
        assert!(
            !remote.deterministic(),
            "verbalized probabilities are never deterministic"
        );
    }

    /// A one-shot HTTP server on loopback: records the request head and
    /// body, answers `reply` (or nothing, when `None`). Polls with a
    /// deadline so a client that never connects fails instead of hanging.
    fn http_once(reply: Option<String>) -> (String, std::thread::JoinHandle<(String, String)>) {
        http_once_with("200 OK", reply)
    }

    /// [`http_once`] answering with `status` instead of `200 OK`.
    fn http_once_with(
        status: &str,
        reply: Option<String>,
    ) -> (String, std::thread::JoinHandle<(String, String)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let status = status.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                let Ok((stream, _)) = listener.accept() else {
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
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                match reply {
                    Some(reply) => {
                        let mut stream = stream;
                        write!(
                            stream,
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .unwrap();
                    }
                    // Hold the connection open past the client's timeout.
                    None => std::thread::sleep(Duration::from_secs(2)),
                }
                return (head, String::from_utf8(body).unwrap());
            }
            (String::new(), String::new())
        });
        (base, server)
    }

    fn config_for(base: &str, key: Option<&str>) -> Config {
        Config {
            preset: Preset::Local,
            base: base.to_string(),
            model: "m".to_string(),
            key: key.map(str::to_string),
            session_id: None,
        }
    }

    #[test]
    fn the_transport_posts_to_chat_completions_with_the_bearer_key() {
        let reply = chat_with(r#"{"probs": {"a": 1}}"#).to_string();
        let (base, server) = http_once(Some(reply.clone()));
        let body = json!({"model": "m"});
        let response = http_chat_within(
            &config_for(&format!("{base}/"), Some("sekret")),
            &body,
            Duration::from_secs(5),
            RESPONSE_CAP_BYTES,
        )
        .unwrap();
        assert_eq!(response.to_string(), reply);
        let (head, sent) = server.join().unwrap();
        assert!(
            head.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sekret"),
            "{head}"
        );
        assert_eq!(serde_json::from_str::<Value>(&sent).unwrap(), body);

        let (base, server) = http_once(Some(reply));
        http_chat_within(
            &config_for(&base, None),
            &body,
            Duration::from_secs(5),
            RESPONSE_CAP_BYTES,
        )
        .unwrap();
        let (head, _) = server.join().unwrap();
        assert!(
            !head.to_ascii_lowercase().contains("authorization"),
            "no key, no header: {head}"
        );
    }

    #[test]
    fn the_transport_reports_a_non_2xx_status_with_the_provider_error_body() {
        let body = format!(
            r#"{{"error":"model blocked by guardrail","pad":"{}"}}"#,
            "x".repeat(600)
        );
        let (base, server) = http_once_with("404 Not Found", Some(body.clone()));
        let error = http_chat_within(
            &config_for(&base, Some("sekret")),
            &json!({}),
            Duration::from_secs(5),
            RESPONSE_CAP_BYTES,
        )
        .unwrap_err();
        server.join().unwrap();
        let expected: String = body.chars().take(400).collect();
        assert_eq!(
            error,
            format!("remote chat {base}/chat/completions: http status 404: {expected}")
        );
        assert!(!error.contains("sekret"), "{error}");

        // An error body over the cap still reports the status, not a read error.
        let (base, server) = http_once_with("500 Internal Server Error", Some(body));
        let error = http_chat_within(
            &config_for(&base, None),
            &json!({}),
            Duration::from_secs(5),
            16,
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error,
            format!("remote chat {base}/chat/completions: http status 500: ")
        );

        // 2xx is the success edge: a 299 still parses as an answer.
        let reply = chat_with(r#"{"probs": {"a": 1}}"#).to_string();
        let (base, server) = http_once_with("299 OK", Some(reply.clone()));
        let response = http_chat_within(
            &config_for(&base, None),
            &json!({}),
            Duration::from_secs(5),
            RESPONSE_CAP_BYTES,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(response.to_string(), reply);
    }

    #[test]
    fn the_transport_is_bounded_in_bytes_and_in_time() {
        let big = json!({"pad": "x".repeat(4096)}).to_string();
        let (base, server) = http_once(Some(big.clone()));
        let capped = http_chat_within(
            &config_for(&base, None),
            &json!({}),
            Duration::from_secs(5),
            1024,
        );
        assert!(capped.is_err(), "a reply over the cap fails the decision");
        server.join().unwrap();
        let (base, server) = http_once(Some(big));
        assert!(
            http_chat_within(
                &config_for(&base, None),
                &json!({}),
                Duration::from_secs(5),
                8192
            )
            .is_ok(),
            "the same reply fits a larger cap"
        );
        server.join().unwrap();

        let (base, server) = http_once(None);
        let started = std::time::Instant::now();
        let slow = http_chat_within(
            &config_for(&base, None),
            &json!({}),
            Duration::from_millis(300),
            8192,
        );
        assert!(slow.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    #[test]
    fn the_schema_names_every_label_and_nothing_else() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let probs = &build_request("m", &s)["response_format"]["json_schema"]["schema"]["properties"]
            ["probs"];
        assert_eq!(probs["type"], "object");
        assert_eq!(
            probs["properties"],
            json!({"yes": {"type": "number"}, "no": {"type": "number"}})
        );
        assert_eq!(probs["required"], json!(["yes", "no"]));
        assert_eq!(probs["additionalProperties"], false);
    }

    #[test]
    fn a_flat_reply_is_read_even_when_probs_is_one_of_the_labels() {
        let s = spec("t", "", &["probs", "other"], &[]);
        let flat = parse_probs(&chat_with(r#"{"probs": 0.4, "other": 0.6}"#), &s.labels).unwrap();
        assert!((flat["probs"] - 0.4).abs() < 1e-9);
        let nested = parse_probs(
            &chat_with(r#"{"probs": {"probs": 0.3, "other": 0.7}}"#),
            &s.labels,
        )
        .unwrap();
        assert!(
            (nested["other"] - 0.7).abs() < 1e-9,
            "the envelope still wins"
        );
    }

    #[test]
    fn finite_values_whose_sum_overflows_are_rejected() {
        let s = spec("t", "", &["a", "b"], &[]);
        let e = parse_probs(&chat_with(r#"{"a": 1e308, "b": 1e308}"#), &s.labels).unwrap_err();
        assert!(e.contains("finite positive"), "{e}");
    }

    #[test]
    fn build_request_frames_context_as_system_and_enumerates_labels() {
        let s = spec(
            "deploy now",
            "Under the policy, decide whether the change is permitted",
            &["yes", "no"],
            &[("yes", "Every condition holds")],
        );
        let body = build_request("m", &s);
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            "Under the policy, decide whether the change is permitted"
        );
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("deploy now"), "{user}");
        assert!(user.contains("- yes: Every condition holds"), "{user}");
        // The omitted criterion falls back to the label name.
        assert!(user.contains("- no: no"), "{user}");
        // response_format requests a strict JSON schema around `probs`.
        assert_eq!(body["response_format"]["type"], "json_schema");
    }

    #[test]
    fn build_request_with_empty_context_uses_the_fallback_framing() {
        let s = spec("x", "", &["a", "b"], &[]);
        let body = build_request("m", &s);
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("Classify"), "{system}");
    }

    #[test]
    fn parse_probs_renormalizes_and_preserves_label_order() {
        let s = spec("t", "", &["a", "b"], &[]);
        // 0.8 / 0.1 does not sum to 1; renormalized → 8/9 and 1/9.
        let resp = chat_with(r#"{"probs": {"a": 0.8, "b": 0.1}}"#);
        let probs = parse_probs(&resp, &s.labels).unwrap();
        assert!((probs["a"] - 8.0 / 9.0).abs() < 1e-9);
        assert!((probs["b"] - 1.0 / 9.0).abs() < 1e-9);
        assert!((probs.values().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_probs_accepts_a_flat_label_to_probability_map() {
        // Some local/Ollama models return the mapping flat (no `probs`
        // key) even when the request asked for the schema.
        let s = spec("t", "", &["works", "fails"], &[]);
        let resp = chat_with(r#"{"works": 1, "fails": 0}"#);
        let probs = parse_probs(&resp, &s.labels).unwrap();
        assert_eq!(probs["works"], 1.0);
        assert_eq!(probs["fails"], 0.0);
        // Renormalization still applies to the flat form.
        let resp = chat_with(r#"{"works": 0.8, "fails": 0.2}"#);
        let probs = parse_probs(&resp, &s.labels).unwrap();
        assert!((probs["works"] - 0.8).abs() < 1e-9);
        assert!((probs.values().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_probs_defaults_an_omitted_label_to_zero_before_renormalizing() {
        let s = spec("t", "", &["a", "b"], &[]);
        let resp = chat_with(r#"{"probs": {"a": 0.9}}"#);
        let probs = parse_probs(&resp, &s.labels).unwrap();
        assert_eq!(probs["a"], 1.0);
        assert_eq!(probs["b"], 0.0);
    }

    #[test]
    fn parse_probs_rejects_unknown_labels_and_non_finite_values() {
        let s = spec("t", "", &["a", "b"], &[]);
        let e =
            parse_probs(&chat_with(r#"{"probs": {"a": 0.5, "zz": 0.5}}"#), &s.labels).unwrap_err();
        assert!(e.contains("unknown label"), "{e}");
        let e = parse_probs(&chat_with(r#"{"probs": {"a": "x"}}"#), &s.labels).unwrap_err();
        assert!(e.contains("not a finite"), "{e}");
        let e = parse_probs(&chat_with(r#"{"probs": {"a": -1.0}}"#), &s.labels).unwrap_err();
        assert!(e.contains("not a finite"), "{e}");
        // All-zero is not a probability distribution.
        let e =
            parse_probs(&chat_with(r#"{"probs": {"a": 0.0, "b": 0.0}}"#), &s.labels).unwrap_err();
        assert!(e.contains("positive value"), "{e}");
    }

    #[test]
    fn parse_probs_rejects_a_malformed_wire_shape() {
        let s = spec("t", "", &["a", "b"], &[]);
        let e = parse_probs(&json!({}), &s.labels).unwrap_err();
        assert!(e.contains("choices[0]"), "{e}");
        let e = parse_probs(&chat_with("not json"), &s.labels).unwrap_err();
        assert!(e.contains("is not JSON"), "{e}");
        let e = parse_probs(&chat_with(r#"{"probs": []}"#), &s.labels).unwrap_err();
        assert!(e.contains("not an object"), "{e}");
    }

    #[test]
    fn decide_posts_and_returns_renormalized_probs_end_to_end() {
        let s = spec("deploy now", "", &["yes", "no"], &[]);
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
        let seen = std::sync::Arc::clone(&recorded);
        let config = Config {
            preset: Preset::Local,
            base: "http://example.invalid/v1".to_string(),
            model: "m".to_string(),
            key: Some("topsecret".to_string()),
            session_id: None,
        };
        let remote = Remote::with_chat(config, move |cfg, body| {
            *seen.lock().unwrap() = Some(body.clone());
            // The key must never appear in captured bodies or errors.
            assert!(!format!("{cfg:?}").contains("topsecret"));
            Ok(chat_with(r#"{"probs": {"yes": 0.8, "no": 0.2}}"#))
        });
        let probs = remote.decide(&s).unwrap();
        assert!((probs["yes"] - 0.8).abs() < 1e-9);
        let body = recorded.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn key_value_is_never_printed_by_the_debug_impl_of_config() {
        let config = Config {
            preset: Preset::Openrouter,
            base: "https://example.invalid/v1".to_string(),
            model: "m".to_string(),
            key: Some("sekret".to_string()),
            session_id: None,
        };
        assert!(!format!("{config:?}").contains("sekret"));
    }
}
