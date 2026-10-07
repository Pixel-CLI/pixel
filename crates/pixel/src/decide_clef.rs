// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` — Cloudflare's Clef-flash decision model.
//!
//! Clef-flash is Cloudflare's 9B decision model (Apache 2.0, a fine-tune of
//! Qwen3.5-9B; `ollama.com/library/clef-flash`, `@cf/cloudflare/clef-flash`
//! on Workers AI). It is fully Jev/System One compatible: the request is the
//! typed-questions body `decide_ollaya` already builds and the answer is the
//! same `answers.<id>.probabilities` object, so this adapter reuses Ollaya's
//! request builder, label preflight and answer parser verbatim and only the
//! transport differs. Two hosts serve it, each behind its own key:
//!
//! - [`Transport::Ollama`]: Ollama's `POST {base}/v1/systemone` (Ollama
//!   0.35.1 or later; `ollama pull clef-flash`). A local server takes no key;
//!   a remote Ollama host takes `OLLAMA_API_KEY` as a bearer.
//! - [`Transport::Cloudflare`]: Workers AI's REST endpoint,
//!   `POST {base}/accounts/{account}/ai/run/@cf/cloudflare/{model}` with
//!   `Authorization: Bearer <Cloudflare API token>`. The REST API wraps the
//!   model output in its usual `{"result": …, "success": …, "errors": […]}`
//!   envelope; the reply is unwrapped here (a bare reply is accepted too), and
//!   a `success: false` envelope fails with Cloudflare's own error messages.
//!
//! Both transports disclose the model's calibrated head output in
//! `snapshot.basis`; the probabilities are renormalized to sum 1 like every
//! other engine's.

use crate::classify::Spec;
use crate::decide_ollaya::{self, AnswerMeta};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

/// Ollama's default local server.
pub const OLLAMA_DEFAULT_BASE: &str = "http://127.0.0.1:11434";
/// Workers AI's REST root, before `/accounts/{account}`.
pub const CLOUDFLARE_DEFAULT_BASE: &str = "https://api.cloudflare.com/client/v4";
/// The latency-first 9B variant; `clef` is the larger, more accurate one.
pub const DEFAULT_MODEL: &str = "clef-flash";
/// The environment variable that names the Cloudflare account id.
pub const CLOUDFLARE_ACCOUNT_ENV: &str = "CLOUDFLARE_ACCOUNT_ID";
/// Cloudflare's documented token variable first, the one `wrangler` and the
/// Workers AI docs examples use second.
pub const CLOUDFLARE_KEY_ENVS: [&str; 2] = ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_AUTH_TOKEN"];
/// A cold local Ollama loads an 11 GB model on the first call.
const OLLAMA_TIMEOUT: Duration = Duration::from_secs(120);
const CLOUDFLARE_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_CAP_BYTES: usize = 1_048_576; // 1 MiB
const MODEL_NAMESPACE: &str = "@cf/cloudflare";

/// The disclosed basis for a Clef decision served by Ollama.
pub const CLEF_OLLAMA_BASIS: &str = "Cloudflare Clef-flash decision model via Ollama (/v1/systemone); native calibrated probabilities, confidence disclosed; determinism untested";
/// The disclosed basis for a Clef decision served by Workers AI.
pub const CLEF_CLOUDFLARE_BASIS: &str = "Cloudflare Clef-flash decision model on Workers AI (@cf/cloudflare); native calibrated probabilities, confidence disclosed; non-deterministic";

/// Which host serves the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ollama,
    Cloudflare,
}

impl Transport {
    /// The error prefix and `snapshot.provider` of this transport.
    pub const fn label(self) -> &'static str {
        match self {
            Transport::Ollama => "clef-ollama",
            Transport::Cloudflare => "clef-cloudflare",
        }
    }

    pub const fn basis(self) -> &'static str {
        match self {
            Transport::Ollama => CLEF_OLLAMA_BASIS,
            Transport::Cloudflare => CLEF_CLOUDFLARE_BASIS,
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Transport::Ollama => OLLAMA_TIMEOUT,
            Transport::Cloudflare => CLOUDFLARE_TIMEOUT,
        }
    }
}

/// Where the endpoint lives, the model to disclose, the optional bearer key
/// and how long one request may take.
#[derive(Debug, Clone)]
pub struct ClefConfig {
    pub transport: Transport,
    /// Ollama: the server root. Cloudflare: the REST root *including* the
    /// account (`…/client/v4/accounts/{id}`), see [`cloudflare_account_base`].
    pub base: String,
    pub model_name: String,
    /// The Ollama or Cloudflare key value. Never logged, never written to a
    /// document; `None` is a keyless local Ollama (and tests).
    pub key: Option<String>,
    pub timeout: Duration,
}

impl ClefConfig {
    pub fn new(
        transport: Transport,
        base: String,
        model_name: String,
        key: Option<String>,
    ) -> Self {
        ClefConfig {
            transport,
            base,
            model_name,
            key,
            timeout: transport.timeout(),
        }
    }
}

/// The injected transport: one POST to the endpoint. Production uses
/// [`http_post`]; tests substitute a scripted closure so no network happens.
type PostFn = Box<dyn Fn(&ClefConfig, &Value) -> Result<Value, String>>;

pub struct Clef {
    config: ClefConfig,
    post: PostFn,
    /// The meta of the most recent [`Clef::decide`] — the classify document
    /// reads it through [`Clef::last_meta`] after `decide`.
    last_meta: Option<AnswerMeta>,
}

impl Clef {
    pub fn open(config: ClefConfig) -> Clef {
        Clef {
            config,
            post: Box::new(http_post),
            last_meta: None,
        }
    }

    /// Test handle: the same `Clef` with a scripted transport.
    #[cfg(test)]
    pub(crate) fn with_post(
        config: ClefConfig,
        post: impl Fn(&ClefConfig, &Value) -> Result<Value, String> + 'static,
    ) -> Clef {
        Clef {
            config,
            post: Box::new(post),
            last_meta: None,
        }
    }

    /// Model id surfaced in `snapshot.model`.
    pub fn model_id(&self) -> &str {
        &self.config.model_name
    }

    pub fn transport(&self) -> Transport {
        self.config.transport
    }

    pub fn last_meta(&self) -> Option<&AnswerMeta> {
        self.last_meta.as_ref()
    }

    /// One decision: the shared TypeSafe request shape, one answer parsed and
    /// renormalized. The label ceiling is Ollaya's preflight; anything over
    /// it is refused before a byte leaves.
    pub fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        decide_ollaya::preflight(spec)?;
        let body = decide_ollaya::build_request(spec, bare_model(&self.config.model_name));
        let response = (self.post)(&self.config, &body)?;
        let label = self.config.transport.label();
        let response = unwrap_envelope(response, label)?;
        let (probs, meta) = decide_ollaya::parse_answer_for(&response, &spec.labels, label)?;
        self.last_meta = Some(meta);
        Ok(probs)
    }
}

/// The model name the request body carries: Workers AI addresses the model
/// by `@cf/cloudflare/clef-flash` in the URL and by `clef-flash` in the body.
fn bare_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The Workers AI REST root with the account folded in. A base that already
/// names an account (`…/accounts/{id}`, as `pixel install` stores it) is used
/// as is; otherwise `account` — `CLOUDFLARE_ACCOUNT_ID` — is required. The id
/// becomes a URL path segment, so only an alphanumeric one is accepted.
pub fn cloudflare_account_base(base: &str, account: Option<&str>) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    if base.contains("/accounts/") {
        return Ok(base.to_string());
    }
    let account = account.map(str::trim).filter(|a| !a.is_empty()).ok_or_else(|| {
        format!(
            "clef-cloudflare needs your Cloudflare account id: set {CLOUDFLARE_ACCOUNT_ENV} or run `pixel config remote-preset clef-cloudflare --base {CLOUDFLARE_DEFAULT_BASE}/accounts/<account-id>`"
        )
    })?;
    if !account.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "{CLOUDFLARE_ACCOUNT_ENV} must be the alphanumeric Cloudflare account id"
        ));
    }
    Ok(format!("{base}/accounts/{account}"))
}

/// The key variable a Cloudflare token is read from: the first of
/// [`CLOUDFLARE_KEY_ENVS`] that is set and non-empty, else the first (so the
/// "needs a key" error names the variable to set).
pub fn cloudflare_key_env(get: impl Fn(&str) -> Option<String>) -> &'static str {
    CLOUDFLARE_KEY_ENVS
        .into_iter()
        .find(|name| get(name).is_some_and(|value| !value.is_empty()))
        .unwrap_or(CLOUDFLARE_KEY_ENVS[0])
}

/// The URL one request goes to.
fn endpoint(config: &ClefConfig) -> String {
    let base = config.base.trim_end_matches('/');
    match config.transport {
        Transport::Ollama => format!("{base}/v1/systemone"),
        Transport::Cloudflare => {
            let model = if config.model_name.starts_with('@') {
                config.model_name.clone()
            } else {
                format!("{MODEL_NAMESPACE}/{}", config.model_name)
            };
            let run = if base.ends_with("/ai/run") {
                base.to_string()
            } else {
                format!("{base}/ai/run")
            };
            format!("{run}/{model}")
        }
    }
}

/// Workers AI's REST API answers `{"result": <model output>, "success": bool,
/// "errors": [{"code", "message"}], …}`; Ollama's reply is the model output
/// itself. Unwrap the envelope when there is one, fail with Cloudflare's own
/// messages when it reports failure, and pass any other reply through.
fn unwrap_envelope(response: Value, label: &str) -> Result<Value, String> {
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        let messages: Vec<String> = response
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let detail = if messages.is_empty() {
            "no error message".to_string()
        } else {
            messages.join("; ")
        };
        return Err(format!("{label} request failed: {detail}"));
    }
    if response.get("answers").is_none()
        && let Some(result) = response.get("result").filter(|r| r.is_object())
    {
        return Ok(result.clone());
    }
    Ok(response)
}

/// The production transport: one POST with the bearer key in the header,
/// bounded by `timeout` and by [`RESPONSE_CAP_BYTES`] of response. The key
/// value never enters the error string or the body.
fn http_post(config: &ClefConfig, body: &Value) -> Result<Value, String> {
    let label = config.transport.label();
    let url = endpoint(config);
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        .user_agent("pixel-cli classify-clef")
        // Non-2xx handled below: the provider's error body names the real
        // cause (unknown model, quota, bad token, Ollama too old).
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut request = agent.post(&url);
    if let Some(key) = &config.key {
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let mut response = request
        .send_json(body)
        .map_err(|e| format!("{label} {url}: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_CAP_BYTES as u64)
        .read_to_string();
    if !(200..300).contains(&status) {
        let hint = status_hint(config.transport, status);
        let snippet: String = text.unwrap_or_default().chars().take(400).collect();
        return Err(format!("{label} {url}: HTTP {status}{hint}: {snippet}"));
    }
    let text = text.map_err(|e| format!("{label} read {url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{label} JSON {url}: {e}"))
}

/// What a non-2xx status points at, per host: an authentication refusal is
/// the key; a 404 on Ollama is a server that does not serve the model or the
/// decision endpoint yet.
fn status_hint(transport: Transport, status: u16) -> &'static str {
    match (transport, status) {
        (Transport::Cloudflare, 401 | 403) => {
            "; check the Cloudflare API token (it needs Workers AI access) and the account id"
        }
        (Transport::Ollama, 401 | 403) => "; check OLLAMA_API_KEY and the Ollama base URL",
        (Transport::Ollama, 404) => {
            "; run `ollama pull clef-flash` (Ollama 0.35.1 or later serves /v1/systemone)"
        }
        _ => "",
    }
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

    fn answer(probs: &str, confidence: f64) -> Value {
        json!({
            "model": "clef-flash",
            "answers": {
                "q1": {
                    "type": "choice",
                    "choice": "yes",
                    "probabilities": serde_json::from_str::<Value>(probs).unwrap(),
                    "confidence": confidence
                }
            },
            "usage": {"input_tokens": 12, "output_tokens": 0}
        })
    }

    fn config(transport: Transport, base: &str, model: &str) -> ClefConfig {
        ClefConfig::new(transport, base.to_string(), model.to_string(), None)
    }

    #[test]
    fn decide_reuses_the_typesafe_request_shape_and_parses_the_answer() {
        let s = spec(
            "deploy now",
            "the rubric",
            &["yes", "no"],
            &[("yes", "every gate passes")],
        );
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
        let seen = std::sync::Arc::clone(&recorded);
        let mut clef = Clef::with_post(
            config(Transport::Ollama, OLLAMA_DEFAULT_BASE, DEFAULT_MODEL),
            move |_config, body| {
                *seen.lock().unwrap() = Some(body.clone());
                Ok(answer(r#"{"yes": 0.9, "no": 0.05}"#, 0.85))
            },
        );
        let probs = clef.decide(&s).unwrap();
        assert!((probs["yes"] - 0.9 / 0.95).abs() < 1e-9);
        assert!((probs["no"] - 0.05 / 0.95).abs() < 1e-9);
        let body = recorded.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], json!("clef-flash"));
        assert_eq!(body["state"], json!("deploy now"));
        let q = &body["questions"]["q1"];
        assert_eq!(q["type"], json!("choice"));
        assert_eq!(q["instructions"], json!("the rubric"));
        assert_eq!(q["criteria"]["yes"], json!("every gate passes"));
        assert!((clef.last_meta().unwrap().confidence - 0.85).abs() < 1e-9);
    }

    #[test]
    fn the_request_body_carries_the_bare_model_even_for_a_namespaced_id() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
        let seen = std::sync::Arc::clone(&recorded);
        let mut clef = Clef::with_post(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                "@cf/cloudflare/clef",
            ),
            move |_config, body| {
                *seen.lock().unwrap() = Some(body.clone());
                Ok(answer(r#"{"yes": 1.0, "no": 0.0}"#, 0.9))
            },
        );
        clef.decide(&s).unwrap();
        assert_eq!(
            recorded.lock().unwrap().clone().unwrap()["model"],
            json!("clef")
        );
        assert_eq!(clef.model_id(), "@cf/cloudflare/clef");
    }

    #[test]
    fn decide_unwraps_the_cloudflare_result_envelope() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let mut clef = Clef::with_post(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                DEFAULT_MODEL,
            ),
            |_config, _body| {
                Ok(json!({
                    "result": answer(r#"{"yes": 0.75, "no": 0.25}"#, 0.7),
                    "success": true,
                    "errors": [],
                    "messages": []
                }))
            },
        );
        let probs = clef.decide(&s).unwrap();
        assert!((probs["yes"] - 0.75).abs() < 1e-9);
        assert!((clef.last_meta().unwrap().confidence - 0.7).abs() < 1e-9);
    }

    #[test]
    fn a_bare_cloudflare_reply_without_the_envelope_still_parses() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let mut clef = Clef::with_post(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                DEFAULT_MODEL,
            ),
            |_config, _body| Ok(answer(r#"{"yes": 0.6, "no": 0.4}"#, 0.5)),
        );
        assert!((clef.decide(&s).unwrap()["yes"] - 0.6).abs() < 1e-9);
    }

    #[test]
    fn a_failed_cloudflare_envelope_surfaces_its_messages() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let mut clef = Clef::with_post(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                DEFAULT_MODEL,
            ),
            |_config, _body| {
                Ok(json!({
                    "result": null,
                    "success": false,
                    "errors": [
                        {"code": 10000, "message": "Authentication error"},
                        {"code": 7003, "message": "No route for that URI"}
                    ]
                }))
            },
        );
        assert_eq!(
            clef.decide(&s).unwrap_err(),
            "clef-cloudflare request failed: Authentication error; No route for that URI"
        );
        assert_eq!(
            unwrap_envelope(json!({"success": false}), "clef-ollama").unwrap_err(),
            "clef-ollama request failed: no error message"
        );
    }

    #[test]
    fn parse_errors_name_the_transport_not_ollaya() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let mut clef = Clef::with_post(
            config(Transport::Ollama, OLLAMA_DEFAULT_BASE, DEFAULT_MODEL),
            |_config, _body| Ok(json!({"usage": {}})),
        );
        let error = clef.decide(&s).unwrap_err();
        assert!(
            error.starts_with("clef-ollama response missing answers"),
            "{error}"
        );
        let mut clef = Clef::with_post(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                DEFAULT_MODEL,
            ),
            |_config, _body| {
                Ok(json!({
                    "answers": {"q1": {"type": "choice", "probabilities": {"yes": 1.0, "no": 0.0}}}
                }))
            },
        );
        let error = clef.decide(&s).unwrap_err();
        assert!(
            error.starts_with("clef-cloudflare answer confidence"),
            "{error}"
        );
    }

    #[test]
    fn decide_rejects_more_labels_than_the_typesafe_ceiling_before_any_request() {
        let labels: Vec<String> = (0..decide_ollaya::MAX_LABELS + 1)
            .map(|i| format!("l{i}"))
            .collect();
        let s = Spec::checked("t".into(), String::new(), labels, BTreeMap::new()).unwrap();
        let mut clef = Clef::with_post(
            config(Transport::Ollama, OLLAMA_DEFAULT_BASE, DEFAULT_MODEL),
            |_config, _body| unreachable!("preflight must refuse before the transport runs"),
        );
        assert!(clef.decide(&s).unwrap_err().contains("at most"));
    }

    #[test]
    fn the_endpoint_follows_the_transport() {
        assert_eq!(
            endpoint(&config(
                Transport::Ollama,
                "http://127.0.0.1:11434/",
                DEFAULT_MODEL
            )),
            "http://127.0.0.1:11434/v1/systemone"
        );
        let cloudflare = "https://api.cloudflare.com/client/v4/accounts/abc123";
        assert_eq!(
            endpoint(&config(Transport::Cloudflare, cloudflare, "clef-flash")),
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/run/@cf/cloudflare/clef-flash"
        );
        assert_eq!(
            endpoint(&config(Transport::Cloudflare, cloudflare, "clef")),
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/run/@cf/cloudflare/clef"
        );
        // A base already ending at `/ai/run` is not doubled, and a namespaced
        // model id is not namespaced twice.
        assert_eq!(
            endpoint(&config(
                Transport::Cloudflare,
                "https://gateway.example/accounts/abc123/ai/run/",
                "@cf/cloudflare/clef-flash"
            )),
            "https://gateway.example/accounts/abc123/ai/run/@cf/cloudflare/clef-flash"
        );
    }

    #[test]
    fn the_account_folds_into_the_cloudflare_base_once() {
        assert_eq!(
            cloudflare_account_base(CLOUDFLARE_DEFAULT_BASE, Some("abc123")).unwrap(),
            "https://api.cloudflare.com/client/v4/accounts/abc123"
        );
        // A stored base already names its account: the environment cannot
        // redirect the key to another one.
        assert_eq!(
            cloudflare_account_base(
                "https://api.cloudflare.com/client/v4/accounts/stored/",
                Some("other")
            )
            .unwrap(),
            "https://api.cloudflare.com/client/v4/accounts/stored"
        );
        for missing in [None, Some(""), Some("  ")] {
            let error = cloudflare_account_base(CLOUDFLARE_DEFAULT_BASE, missing).unwrap_err();
            assert!(error.contains("CLOUDFLARE_ACCOUNT_ID"), "{error}");
            assert!(
                error.contains("pixel config remote-preset clef-cloudflare"),
                "{error}"
            );
        }
        let error = cloudflare_account_base(CLOUDFLARE_DEFAULT_BASE, Some("a/../b")).unwrap_err();
        assert!(error.contains("alphanumeric"), "{error}");
    }

    #[test]
    fn the_cloudflare_key_variable_is_the_first_one_set() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert_eq!(cloudflare_key_env(env(&[])), "CLOUDFLARE_API_TOKEN");
        assert_eq!(
            cloudflare_key_env(env(&[("CLOUDFLARE_AUTH_TOKEN", "t")])),
            "CLOUDFLARE_AUTH_TOKEN"
        );
        assert_eq!(
            cloudflare_key_env(env(&[
                ("CLOUDFLARE_AUTH_TOKEN", "t"),
                ("CLOUDFLARE_API_TOKEN", "t")
            ])),
            "CLOUDFLARE_API_TOKEN"
        );
        // An empty value is not a key.
        assert_eq!(
            cloudflare_key_env(env(&[
                ("CLOUDFLARE_API_TOKEN", ""),
                ("CLOUDFLARE_AUTH_TOKEN", "t")
            ])),
            "CLOUDFLARE_AUTH_TOKEN"
        );
    }

    /// A single loopback response server that records the first request,
    /// with a deadline so a missing connection fails fast.
    fn http_once(status: &str, reply: String) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 1024];
                let received = stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                return String::from_utf8_lossy(&request[..received]).into_owned();
            }
            String::new()
        });
        (base, server)
    }

    #[test]
    fn the_ollama_transport_posts_systemone_keyless_and_with_a_bearer_when_keyed() {
        let reply = answer(r#"{"yes": 0.75, "no": 0.25}"#, 0.7).to_string();
        let (base, server) = http_once("200 OK", reply.clone());
        let keyless = config(Transport::Ollama, &base, DEFAULT_MODEL);
        let response = http_post(&keyless, &json!({"model": "clef-flash"})).unwrap();
        assert_eq!(response, serde_json::from_str::<Value>(&reply).unwrap());
        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /v1/systemone HTTP/1.1"),
            "{request}"
        );
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "a keyless local Ollama gets no Authorization header: {request}"
        );

        let (base, server) = http_once("200 OK", reply);
        let mut keyed = config(Transport::Ollama, &base, DEFAULT_MODEL);
        keyed.key = Some("ollama-test-secret".to_string());
        http_post(&keyed, &json!({})).unwrap();
        assert!(
            server
                .join()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization: bearer ollama-test-secret")
        );
    }

    #[test]
    fn the_cloudflare_transport_posts_the_run_path_with_the_bearer_token() {
        let reply = json!({"result": answer(r#"{"yes": 0.5, "no": 0.5}"#, 0.5), "success": true})
            .to_string();
        let (base, server) = http_once("200 OK", reply);
        let mut cloudflare = config(
            Transport::Cloudflare,
            &format!("{base}/client/v4/accounts/acct1"),
            DEFAULT_MODEL,
        );
        cloudflare.key = Some("cf-test-secret".to_string());
        http_post(&cloudflare, &json!({"model": "clef-flash"})).unwrap();
        let request = server.join().unwrap();
        assert!(
            request.starts_with(
                "POST /client/v4/accounts/acct1/ai/run/@cf/cloudflare/clef-flash HTTP/1.1"
            ),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer cf-test-secret"),
            "{request}"
        );
    }

    #[test]
    fn status_errors_hint_at_the_host_and_never_echo_the_key() {
        let (base, server) = http_once("401 Unauthorized", "{}".to_string());
        let mut cloudflare = config(
            Transport::Cloudflare,
            &format!("{base}/accounts/a"),
            DEFAULT_MODEL,
        );
        cloudflare.key = Some("cf-wrong".to_string());
        let error = http_post(&cloudflare, &json!({})).unwrap_err();
        assert_eq!(
            error,
            format!(
                "clef-cloudflare {base}/accounts/a/ai/run/@cf/cloudflare/clef-flash: HTTP 401; check the Cloudflare API token (it needs Workers AI access) and the account id: {{}}"
            )
        );
        assert!(!error.contains("cf-wrong"), "{error}");
        server.join().unwrap();

        let (base, server) = http_once(
            "404 Not Found",
            r#"{"error":"model not found"}"#.to_string(),
        );
        let ollama = config(Transport::Ollama, &base, DEFAULT_MODEL);
        let error = http_post(&ollama, &json!({})).unwrap_err();
        assert_eq!(
            error,
            format!(
                r#"clef-ollama {base}/v1/systemone: HTTP 404; run `ollama pull clef-flash` (Ollama 0.35.1 or later serves /v1/systemone): {{"error":"model not found"}}"#
            )
        );
        server.join().unwrap();

        assert_eq!(
            status_hint(Transport::Ollama, 401),
            "; check OLLAMA_API_KEY and the Ollama base URL"
        );
        assert_eq!(status_hint(Transport::Cloudflare, 404), "");
        assert_eq!(status_hint(Transport::Ollama, 500), "");
    }

    #[test]
    fn defaults_name_clef_flash_and_each_transports_disclosure() {
        let ollama = config(Transport::Ollama, OLLAMA_DEFAULT_BASE, DEFAULT_MODEL);
        assert_eq!(ollama.timeout, OLLAMA_TIMEOUT);
        assert_eq!(
            config(
                Transport::Cloudflare,
                CLOUDFLARE_DEFAULT_BASE,
                DEFAULT_MODEL
            )
            .timeout,
            CLOUDFLARE_TIMEOUT
        );
        assert_eq!(Transport::Ollama.label(), "clef-ollama");
        assert_eq!(Transport::Cloudflare.label(), "clef-cloudflare");
        assert_eq!(Transport::Ollama.basis(), CLEF_OLLAMA_BASIS);
        assert_eq!(Transport::Cloudflare.basis(), CLEF_CLOUDFLARE_BASIS);
        assert_eq!(DEFAULT_MODEL, "clef-flash");
    }
}
