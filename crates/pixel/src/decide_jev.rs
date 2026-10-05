// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` — TypeSafe's hosted Jev decision model.
//!
//! Jev is the hosted decision model the bakeoff bar measures against
//! (`docs/bench/decide-bakeoff.md`): published typed-decisions accuracy
//! 0.738, against the local `winnow:e4b` 0.722. It speaks TypeSafe's
//! `/v1/systemone` decision shape over HTTPS with a bearer key — the same
//! wire the local Ollaya daemon implements (`decide_ollaya`) — so this
//! adapter reuses Ollaya's request builder, label preflight and answer
//! parser verbatim and only the transport differs: the base is TypeSafe's
//! (`PIXEL_REMOTE_BASE` overrides it, as for every remote preset), the
//! request carries `Authorization: Bearer TYPESAFE_API_KEY` (or the key
//! from `pixel config remote-key jev`, or from a configured Infisical
//! project — see `decide_infisical`), and the default model is `jev-latest`.
//!
//! The probabilities come back as the model's calibrated head output, not
//! verbalized text — disclosed in `snapshot.basis` — and are renormalized
//! to sum 1 exactly like every other engine's.

use crate::classify::Spec;
use crate::decide_ollaya::{self, AnswerMeta};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

/// TypeSafe's hosted Jev API host, bare — the transport appends the
/// `/v1/systemone` path, matching the Ollaya daemon's base layout.
pub const DEFAULT_BASE: &str = "https://api.typesafe.ai";
/// TypeSafe's current Jev release line; overridable with `--remote-model`.
pub const DEFAULT_MODEL: &str = "jev-latest";
/// First call can include cold start on the hosted side too.
const TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_CAP_BYTES: usize = 1_048_576; // 1 MiB

/// The error prefix naming this engine in transport and parse errors.
const JEV_LABEL: &str = "jev";

/// The disclosed basis for a hosted Jev decision.
pub const JEV_BASIS: &str = "Jev hosted decision model (TypeSafe /v1/systemone); native calibrated probabilities, confidence disclosed; non-deterministic";

/// Where the hosted Jev endpoint lives, the model name to disclose, the
/// bearer key, and how long one request may take.
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub base: String,
    pub model_name: String,
    /// The TypeSafe API key value. Never logged, never written to a
    /// document; `None` only reaches the transport in tests.
    pub key: Option<String>,
    pub timeout: Duration,
}

impl Default for JevConfig {
    fn default() -> Self {
        JevConfig {
            base: DEFAULT_BASE.to_string(),
            model_name: DEFAULT_MODEL.to_string(),
            key: None,
            timeout: TIMEOUT,
        }
    }
}

/// The injected transport: one POST to the hosted endpoint. Production
/// uses [`http_post`]; tests substitute a scripted closure so no network
/// happens.
type PostFn = Box<dyn Fn(&JevConfig, &Value) -> Result<Value, String>>;

pub struct Jev {
    config: JevConfig,
    post: PostFn,
    /// The meta of the most recent [`Jev::decide`] — the classify document
    /// reads it through [`Jev::last_meta`] after `decide`.
    last_meta: Option<AnswerMeta>,
}

impl Jev {
    pub fn open(config: JevConfig) -> Jev {
        Jev {
            config,
            post: Box::new(http_post),
            last_meta: None,
        }
    }

    /// Test handle: the same `Jev` with a scripted transport. `pub(crate)`
    /// so classify.rs's adapter tests can drive the `DecisionEngine` trait
    /// impl without a network.
    #[cfg(test)]
    pub(crate) fn with_post(
        config: JevConfig,
        post: impl Fn(&JevConfig, &Value) -> Result<Value, String> + 'static,
    ) -> Jev {
        Jev {
            config,
            post: Box::new(post),
            last_meta: None,
        }
    }

    /// Model id surfaced in `snapshot.model`.
    pub fn model_id(&self) -> &str {
        &self.config.model_name
    }

    pub fn last_meta(&self) -> Option<&AnswerMeta> {
        self.last_meta.as_ref()
    }

    /// One decision: the shared TypeSafe request shape, one answer parsed
    /// and renormalized. The label ceiling is Ollaya's preflight (TypeSafe's
    /// own schema); anything over it is refused before a byte leaves.
    pub fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        decide_ollaya::preflight(spec)?;
        let body = decide_ollaya::build_request(spec, &self.config.model_name);
        let response = (self.post)(&self.config, &body)?;
        let (probs, meta) = decide_ollaya::parse_answer_for(&response, &spec.labels, JEV_LABEL)?;
        self.last_meta = Some(meta);
        Ok(probs)
    }
}

/// The production transport: one POST to `{base}/v1/systemone` with the
/// bearer key in the header, bounded by `timeout` and by `cap` bytes of
/// response. The key value never enters the error string or the body.
fn http_post(config: &JevConfig, body: &Value) -> Result<Value, String> {
    let url = format!("{}/v1/systemone", config.base.trim_end_matches('/'));
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        .user_agent("pixel-cli classify-jev")
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut request = agent.post(&url);
    if let Some(key) = &config.key {
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let mut response = request.send_json(body).map_err(|e| {
        let status = match &e {
            ureq::Error::StatusCode(code) => format!(
                " (HTTP {code}; check the TYPESAFE_API_KEY value and the {JEV_LABEL} base URL)"
            ),
            _ => String::new(),
        };
        format!("{JEV_LABEL} {url}: {e}{status}")
    })?;
    let text = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_CAP_BYTES as u64)
        .read_to_string()
        .map_err(|e| format!("{JEV_LABEL} read {url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{JEV_LABEL} JSON {url}: {e}"))
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
            "model": "jev-latest",
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
        let mut jev = Jev::with_post(JevConfig::default(), move |_config, body| {
            *seen.lock().unwrap() = Some(body.clone());
            Ok(answer(r#"{"yes": 0.9, "no": 0.05}"#, 0.85))
        });
        let probs = jev.decide(&s).unwrap();
        assert!((probs["yes"] - 0.9 / 0.95).abs() < 1e-9);
        let body = recorded.lock().unwrap().clone().unwrap();
        // The hosted request is byte-shaped like the local Ollaya one:
        // same question id, same criteria mapping, different model.
        assert_eq!(body["model"], json!("jev-latest"));
        assert_eq!(body["state"], json!("deploy now"));
        let q = &body["questions"]["q1"];
        assert_eq!(q["type"], json!("choice"));
        assert_eq!(q["instructions"], json!("the rubric"));
        assert_eq!(q["criteria"]["yes"], json!("every gate passes"));
        let meta = jev.last_meta().unwrap();
        assert!((meta.confidence - 0.85).abs() < 1e-9);
    }

    #[test]
    fn decide_rejects_more_labels_than_the_typesafe_ceiling_before_any_request() {
        let labels: Vec<String> = (0..decide_ollaya::MAX_LABELS + 1)
            .map(|i| format!("l{i}"))
            .collect();
        let s = Spec::checked("t".into(), String::new(), labels, BTreeMap::new()).unwrap();
        let mut jev = Jev::with_post(JevConfig::default(), |_config, _body| {
            unreachable!("preflight must refuse before the transport runs")
        });
        let error = jev.decide(&s).unwrap_err();
        assert!(error.contains("at most"), "{error}");
    }

    #[test]
    fn parse_errors_name_jev_not_ollaya() {
        let s = spec("t", "", &["yes", "no"], &[]);
        // A malformed answer must be reported as the engine that produced
        // it — including the confidence clause that comes from the shared
        // TypeSafe parser.
        let mut jev = Jev::with_post(JevConfig::default(), |_config, _body| {
            Ok(json!({
                "answers": {"q1": {"type": "choice", "probabilities": {"yes": 1.0, "no": 0.0}}}
            }))
        });
        let error = jev.decide(&s).unwrap_err();
        assert!(error.starts_with("jev answer confidence"), "{error}");
        let mut jev = Jev::with_post(JevConfig::default(), |_config, _body| {
            Ok(json!({"usage": {}}))
        });
        let error = jev.decide(&s).unwrap_err();
        assert!(error.starts_with("jev response missing answers"), "{error}");
    }

    /// A single loopback response server that records the first request
    /// line, with a deadline so a missing connection fails fast.
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
    fn http_post_targets_the_systemone_path_with_the_bearer_key_and_explains_status_errors() {
        let reply = answer(r#"{"yes": 0.75, "no": 0.25}"#, 0.7).to_string();
        let (base, server) = http_once("200 OK", reply.clone());
        let config = JevConfig {
            base: base.clone(),
            key: Some("tsk-test-secret".to_string()),
            ..Default::default()
        };
        let response = http_post(&config, &json!({"model": "jev-latest"})).unwrap();
        assert_eq!(response, serde_json::from_str::<Value>(&reply).unwrap());
        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /v1/systemone HTTP/1.1"),
            "the hosted transport must target TypeSafe's endpoint: {request}"
        );
        // ureq normalizes header names to lowercase on the wire.
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer tsk-test-secret"),
            "the hosted endpoint authenticates with a bearer key: {request}"
        );

        let (base, server) = http_once("401 Unauthorized", "{}".to_string());
        let config = JevConfig {
            base: base.clone(),
            key: Some("tsk-wrong".to_string()),
            ..Default::default()
        };
        let error = http_post(&config, &json!({})).unwrap_err();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(error.starts_with("jev http"), "{error}");
        assert!(
            error.contains("check the TYPESAFE_API_KEY value"),
            "{error}"
        );
        assert!(
            !error.contains("tsk-wrong"),
            "the key value must never enter an error string: {error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn default_config_names_typesafe_jev_latest_and_the_shared_basis() {
        let config = JevConfig::default();
        assert_eq!(config.base, "https://api.typesafe.ai");
        assert_eq!(config.model_name, "jev-latest");
        assert_eq!(config.timeout, TIMEOUT);
        assert_eq!(
            JEV_BASIS,
            "Jev hosted decision model (TypeSafe /v1/systemone); native calibrated probabilities, confidence disclosed; non-deterministic"
        );
    }
}
