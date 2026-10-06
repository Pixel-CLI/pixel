// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` — Ollaya local decision daemon (TypeSafe-compatible).
//!
//! Ollaya is "Ollama for decision models": one binary pulls and serves typed
//! decision models and speaks TypeSafe's `/v1/systemone` wire format, so a
//! Jev-shaped client points at it by changing the base URL. One prefill and
//! one forward pass per question, no decoding: the probabilities are native
//! head outputs, not verbalized text.
//!
//! Response semantics (TypeSafe's `choice` shape, which Ollaya implements
//! field-for-field): `probabilities` sum to 1 over the caller's options,
//! `choice` is the argmax label, and `confidence` is the normalized top
//! probability `(K·p_max − 1) / (K − 1)`. There is no unknown/abstention
//! mass to disclose — Ollaya's TypeSafe `choice` wire has no such fields —
//! so the label distribution is passed through unchanged and only
//! `confidence` is surfaced in the snapshot.
//!
//! Limits are enforced here as a preflight, before any request leaves the
//! process, and rejections are errors — never truncation: a `choice`
//! question carries 2–255 labels (the 2-floor is already enforced by
//! `Spec::checked`). Ollaya's other limits need no client preflight: state
//! is capped at 65,536 tokens, but Pixel's own 32,768-character cap keeps
//! every component far below that; the 8 MiB request-body cap is likewise
//! unreachable from a Pixel spec. The per-model option budget (e.g.
//! ~125 options for `laya:en`) is only known to the server and surfaces as
//! a clear `422 TOO_MANY_OPTIONS` error.
//!
//! Determinism: a single forward pass *should* be deterministic on a fixed
//! host and precision, but Ollaya documents that fp16 answers can differ
//! from the fp32 reference on near-ties, and that has not been measured on
//! this Mac's runtime, so `deterministic()` stays `false` until it is.

use crate::classify::Spec;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// How many labels a `choice` question accepts (TypeSafe's and Ollaya's
/// shared schema ceiling).
pub const MAX_LABELS: usize = 255;

/// Options one `choice` question may offer the local model, well below the
/// [`MAX_LABELS`] schema ceiling. Measured on this daemon: 64 labels answer,
/// 65 is refused with `422`, and the refusal is an error rather than a
/// truncated decision — so a caller that builds a question (`pixel
/// ultraflow`, one per browser cycle) has to know the number before it
/// offers.
pub const MAX_OPTIONS: usize = 64;
/// The single question this adapter sends (Pixel's contract is one decision
/// per spec; Ollaya allows 1–256 questions).
const QUESTION_ID: &str = "q1";
/// First call can include the model load on a cold server; later calls are
/// one forward pass.
const TIMEOUT: Duration = Duration::from_secs(120);
const RESPONSE_CAP_BYTES: usize = 1_048_576; // 1 MiB

/// Default local daemon address (Ollaya's `OLLAYA_HOST` default).
pub const DEFAULT_BASE: &str = "http://127.0.0.1:11435";
/// The model the local setup pulls and serves. `winnow:e4b` is Ollaya's
/// recommended model: 0.722 typed-decisions accuracy (TypeSafe's Jev:
/// 0.738). On Apple silicon it runs on Metal via llama.cpp.
pub const DEFAULT_MODEL: &str = "winnow:e4b";

/// Where the local Ollaya daemon lives, the model name to disclose, and how
/// long one request may take.
///
/// The option budget below is not a schema limit but the model's own: a
/// caller that builds a question (`pixel ultraflow`, one per browser cycle)
/// has to know it before it offers.
#[derive(Debug, Clone)]
pub struct OllayaConfig {
    pub base: String,
    pub model_name: String,
    /// Whole-request cap: [`TIMEOUT`] by default; the prompt hook sets a
    /// sub-second one so a server still loading its model fails open.
    pub timeout: Duration,
}

impl Default for OllayaConfig {
    fn default() -> Self {
        OllayaConfig {
            base: DEFAULT_BASE.to_string(),
            model_name: DEFAULT_MODEL.to_string(),
            timeout: TIMEOUT,
        }
    }
}

/// The disclosed parts of an Ollaya answer beyond the label distribution.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerMeta {
    pub confidence: f64,
}

impl AnswerMeta {
    fn from_answer(answer: &Value, engine: &str) -> Result<AnswerMeta, String> {
        let confidence = answer
            .get("confidence")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
            .ok_or_else(|| {
                format!("{engine} answer confidence is not a finite number in [0, 1]")
            })?;
        Ok(AnswerMeta { confidence })
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({ "confidence": self.confidence })
    }
}

/// The injected transport: one POST to the local daemon. Production uses
/// [`http_post`]; tests substitute a scripted closure so no server is
/// needed to test the full path.
type PostFn = Box<dyn Fn(&OllayaConfig, &Value) -> Result<Value, String>>;

pub struct Ollaya {
    config: OllayaConfig,
    post: PostFn,
    /// The meta of the most recent [`Ollaya::decide`] — the classify
    /// document reads it through [`Ollaya::last_meta`] after `decide`.
    last_meta: Option<AnswerMeta>,
}

impl Ollaya {
    pub fn open(config: OllayaConfig) -> Ollaya {
        Ollaya {
            config,
            post: Box::new(http_post),
            last_meta: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_post(
        config: OllayaConfig,
        post: impl Fn(&OllayaConfig, &Value) -> Result<Value, String> + 'static,
    ) -> Ollaya {
        Ollaya {
            config,
            post: Box::new(post),
            last_meta: None,
        }
    }

    pub fn model_id(&self) -> &str {
        &self.config.model_name
    }

    pub fn last_meta(&self) -> Option<&AnswerMeta> {
        self.last_meta.as_ref()
    }

    pub fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        preflight(spec)?;
        let body = build_request(spec, &self.config.model_name);
        let response = (self.post)(&self.config, &body)?;
        let (probs, meta) = parse_answer(&response, &spec.labels)?;
        self.last_meta = Some(meta);
        Ok(probs)
    }

    /// Ask an arbitrary typed battery (`choice`/`score`/`noul` questions)
    /// and return the raw `answers` object — the label-less `classify`
    /// path, where no single caller choice reduces the response.
    pub fn ask(&mut self, state: &str, questions: &Value) -> Result<Value, String> {
        let body = json!({
            "model": self.config.model_name,
            "state": state,
            "questions": questions,
        });
        let response = (self.post)(&self.config, &body)?;
        response
            .get("answers")
            .filter(|answers| answers.is_object())
            .cloned()
            .ok_or_else(|| "ollaya response answers must be an object".to_string())
    }
}

/// The battery a label-less `classify` sends: Ollaya's own `triage` preset,
/// verbatim — the same default `ollaya run` picks for a model that ships
/// no built-in questions. `noul` answers are 0–1 "the statement holds"
/// probabilities; `score` criteria are the ordered level descriptions.
pub fn default_battery() -> Value {
    json!({
        "intent": {
            "type": "choice",
            "instructions": "What does the customer want in `message`?",
            "criteria": {
                "refund": "money returned or a duplicate charge reversed",
                "technical_help": "a bug, outage or integration problem",
                "billing_question": "a question about an invoice, plan or payment method",
                "information": "general information, pricing or how-to",
                "cancellation": "wants to cancel or downgrade",
                "other": "none of the other options fits"
            }
        },
        "is_urgent": {
            "type": "noul",
            "instructions": "Does `message` communicate time pressure or a deadline?"
        },
        "frustration": {
            "type": "score",
            "instructions": "How frustrated does the customer sound in `message`?",
            "criteria": [
                "calm and neutral",
                "concerned but civil",
                "clearly annoyed",
                "very angry or using strong language"
            ]
        },
        "refund_requested": {
            "type": "noul",
            "instructions": "Does the customer ask for money back?"
        },
        "churn_risk": {
            "type": "noul",
            "instructions": "Does `message` suggest the customer may leave for a competitor or cancel?"
        }
    })
}

/// The public request is TypeSafe's shape as Ollaya documents it: the spec's
/// `text` is the state, `context` becomes the question instructions, every
/// label with its criterion (or label fallback) is an entry in the choice
/// criteria, and `model` names the model to answer — label identities
/// preserved exactly.
pub fn build_request(spec: &Spec, model_name: &str) -> Value {
    let instructions = if spec.context.is_empty() {
        "Classify the state below into exactly one of the given options."
    } else {
        spec.context.as_str()
    };
    let criteria: serde_json::Map<String, Value> = spec
        .labels
        .iter()
        .map(|label| {
            let criterion = spec
                .criteria
                .get(label)
                .map_or(label.as_str(), String::as_str);
            (label.clone(), json!(criterion))
        })
        .collect();
    json!({
        "model": model_name,
        "state": spec.text,
        "questions": {
            QUESTION_ID: {
                "type": "choice",
                "instructions": instructions,
                "criteria": criteria,
            }
        }
    })
}

/// Reject, before any request, the one limit Pixel's own caps do not already
/// cover. An unsupported input must fail here with a clear message — never
/// truncated, never silently reshaped.
pub fn preflight(spec: &Spec) -> Result<(), String> {
    let count = spec.labels.len();
    if count > MAX_LABELS {
        return Err(format!(
            "ollaya accepts at most {MAX_LABELS} options, got {count}; split the decision or use a remote engine"
        ));
    }
    Ok(())
}

/// Read `answers[QUESTION_ID]`: the label distribution (renormalized
/// defensively — Ollaya already sums to 1 over the options) plus the
/// confidence, which is disclosed but never folded into the distribution.
/// Unknown labels and non-finite values are errors, not silent drops.
fn parse_answer(
    response: &Value,
    labels: &[String],
) -> Result<(BTreeMap<String, f64>, AnswerMeta), String> {
    parse_answer_for(response, labels, "ollaya")
}

/// [`parse_answer`] with the engine label the error strings name — hosted
/// Jev (`decide_jev`) reuses the same TypeSafe answer contract.
pub(crate) fn parse_answer_for(
    response: &Value,
    labels: &[String],
    engine: &str,
) -> Result<(BTreeMap<String, f64>, AnswerMeta), String> {
    let answer = response
        .get("answers")
        .and_then(|a| a.get(QUESTION_ID))
        .ok_or_else(|| format!("{engine} response missing answers.{QUESTION_ID}"))?;
    let meta = AnswerMeta::from_answer(answer, engine)?;
    let probs_value = answer
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{engine} answer probabilities is not an object"))?;
    let mut out: BTreeMap<String, f64> = labels.iter().map(|l| (l.clone(), 0.0)).collect();
    let mut sum = 0.0f64;
    for (label, value) in probs_value {
        let p = value
            .as_f64()
            .filter(|p| p.is_finite() && *p >= 0.0)
            .ok_or_else(|| {
                format!("{engine} probabilities[{label:?}] is not a finite non-negative number")
            })?;
        if !out.contains_key(label) {
            return Err(format!(
                "{engine} returned probability for unknown label {label:?} (expected only: {labels:?})"
            ));
        }
        out.insert(label.clone(), p);
        sum += p;
    }
    if !sum.is_finite() || sum <= 0.0 {
        return Err(format!(
            "{engine} probabilities must sum to a finite positive value"
        ));
    }
    for p in out.values_mut() {
        *p /= sum;
    }
    Ok((out, meta))
}

/// The production transport: one POST to `{base}/v1/systemone`, bounded by
/// timeout and response cap. The daemon is local; no API key is involved.
fn http_post(config: &OllayaConfig, body: &Value) -> Result<Value, String> {
    let url = format!("{}/v1/systemone", config.base.trim_end_matches('/'));
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        .user_agent("pixel-cli classify-ollaya")
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut response = agent.post(&url).send_json(body).map_err(|e| {
        let status = match &e {
            ureq::Error::StatusCode(code) => format!(
                " (HTTP {code}; is the ollaya daemon running at {}?)",
                config.base
            ),
            _ => String::new(),
        };
        format!("ollaya {url}: {e}{status}")
    })?;
    let text = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_CAP_BYTES as u64)
        .read_to_string()
        .map_err(|e| format!("ollaya read {url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("ollaya JSON {url}: {e}"))
}

/// The disclosed basis for an Ollaya decision.
pub const OLLAYA_BASIS: &str = "ollaya local decision model, single-pass typed-choice readout (TypeSafe-compatible /v1/systemone, server-side); calibrated probabilities, confidence disclosed; determinism untested";

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
            "model": "winnow:e4b",
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
    fn ask_posts_the_questions_verbatim_and_returns_the_answers_object() {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Value::Null));
        let recorded = sent.clone();
        let mut ollaya = Ollaya::with_post(OllayaConfig::default(), move |_config, body| {
            *recorded.lock().unwrap() = body.clone();
            Ok(json!({
                "answers": {
                    "is_urgent": {"type": "noul", "noul": 0.9}
                }
            }))
        });
        let questions = default_battery();
        let answers = ollaya.ask("the state", &questions).unwrap();
        let body = sent.lock().unwrap();
        assert_eq!(body["model"], "winnow:e4b");
        assert_eq!(body["state"], "the state");
        assert_eq!(body["questions"], questions);
        assert_eq!(answers["is_urgent"]["noul"], 0.9);
    }

    #[test]
    fn ask_errors_when_the_response_has_no_answers() {
        let mut ollaya = Ollaya::with_post(OllayaConfig::default(), |_config, _body| {
            Ok(json!({"model": "winnow:e4b"}))
        });
        let error = ollaya.ask("state", &default_battery()).unwrap_err();
        assert!(error.contains("answers"), "{error}");
    }

    #[test]
    fn ask_should_reject_non_object_answers_instead_of_printing_an_empty_success() {
        for answers in [Value::Null, json!([]), json!("unavailable")] {
            let mut ollaya = Ollaya::with_post(OllayaConfig::default(), move |_, _| {
                Ok(json!({"answers": answers}))
            });
            assert_eq!(
                ollaya.ask("state", &default_battery()).unwrap_err(),
                "ollaya response answers must be an object"
            );
        }
    }

    #[test]
    fn default_battery_has_the_documented_typed_questions() {
        let battery = default_battery();
        assert_eq!(battery["intent"]["type"], "choice");
        assert_eq!(
            battery["intent"]["criteria"]["technical_help"],
            "a bug, outage or integration problem"
        );
        assert_eq!(battery["is_urgent"]["type"], "noul");
        assert_eq!(battery["frustration"]["type"], "score");
        assert_eq!(battery["refund_requested"]["type"], "noul");
        assert_eq!(battery["churn_risk"]["type"], "noul");
    }

    #[test]
    fn model_id_reports_the_configured_model() {
        let ollaya = Ollaya::with_post(
            OllayaConfig {
                base: "http://127.0.0.1:11435".to_string(),
                model_name: "laya:en".to_string(),
                ..Default::default()
            },
            |_config, _body| unreachable!("model_id does not call the transport"),
        );
        assert_eq!(ollaya.model_id(), "laya:en");
    }

    #[test]
    fn build_request_maps_model_state_instructions_and_criteria_verbatim() {
        let s = spec(
            "deploy now",
            "Under the policy, decide whether the change is permitted",
            &["yes", "no"],
            &[("yes", "Every condition holds")],
        );
        let body = build_request(&s, "winnow:e4b");
        assert_eq!(body["model"], json!("winnow:e4b"));
        assert_eq!(body["state"], json!("deploy now"));
        let q = &body["questions"]["q1"];
        assert_eq!(q["type"], json!("choice"));
        assert_eq!(
            q["instructions"],
            json!("Under the policy, decide whether the change is permitted")
        );
        assert_eq!(q["criteria"]["yes"], json!("Every condition holds"));
        // The omitted criterion falls back to the label name.
        assert_eq!(q["criteria"]["no"], json!("no"));
    }

    #[test]
    fn build_request_preserves_arbitrary_label_identity() {
        // Criteria travel as a JSON object, so key order is not semantic;
        // what must survive is every label's identity as a key.
        let s = spec("t", "", &["zz-motor", "alpha", "0x1f"], &[]);
        let request = build_request(&s, "winnow:e4b");
        let criteria = request["questions"]["q1"]["criteria"].as_object().unwrap();
        let keys: Vec<&String> = criteria.keys().collect();
        assert_eq!(
            keys,
            ["0x1f", "alpha", "zz-motor"].iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn preflight_rejects_more_than_the_label_ceiling_without_truncation() {
        let two = spec("t", "", &["a", "b"], &[]);
        assert!(preflight(&two).is_ok());
        // 256 labels: one over the schema ceiling.
        let many: Vec<String> = (0..=MAX_LABELS).map(|i| format!("l{i}")).collect();
        let e =
            preflight(&Spec::checked("t".into(), String::new(), many, BTreeMap::new()).unwrap())
                .unwrap_err();
        assert!(e.contains("at most 255"), "{e}");
        // Exactly at the ceiling is accepted.
        let full: Vec<String> = (0..MAX_LABELS).map(|i| format!("l{i}")).collect();
        assert!(
            preflight(&Spec::checked("t".into(), String::new(), full, BTreeMap::new()).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn parse_answer_renormalizes_and_surfaces_confidence() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let (probs, meta) =
            parse_answer(&answer(r#"{"yes": 0.8, "no": 0.1}"#, 0.6), &s.labels).unwrap();
        assert!((probs["yes"] - 8.0 / 9.0).abs() < 1e-9);
        assert!((probs["no"] - 1.0 / 9.0).abs() < 1e-9);
        assert!((meta.confidence - 0.6).abs() < 1e-9);
    }

    #[test]
    fn parse_answer_rejects_unknown_labels_and_bad_values() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let e = parse_answer(&answer(r#"{"yes": 0.5, "zz": 0.5}"#, 0.5), &s.labels).unwrap_err();
        assert!(e.contains("unknown label"), "{e}");
        let e = parse_answer(&answer(r#"{"yes": -1}"#, 0.5), &s.labels).unwrap_err();
        assert!(e.contains("finite"), "{e}");
        // A negative value must be rejected even when the remaining mass
        // leaves a positive total; otherwise it could be normalized through.
        let e = parse_answer(&answer(r#"{"yes": -0.1, "no": 0.2}"#, 0.5), &s.labels).unwrap_err();
        assert!(e.contains("finite"), "{e}");
        let e = parse_answer(&answer(r#"{"yes": 0.0, "no": 0.0}"#, 0.5), &s.labels).unwrap_err();
        assert!(e.contains("positive"), "{e}");
    }

    #[test]
    fn parse_answer_requires_wellformed_meta_and_shape() {
        let s = spec("t", "", &["yes", "no"], &[]);
        let e = parse_answer(&json!({}), &s.labels).unwrap_err();
        assert!(e.contains("answers.q1"), "{e}");
        // Missing confidence is a contract violation, not a silent default.
        let bad = json!({
            "answers": {"q1": {"type": "choice", "choice": "yes",
                               "probabilities": {"yes": 1.0, "no": 0.0}}},
            "usage": {}
        });
        let e = parse_answer(&bad, &s.labels).unwrap_err();
        assert!(e.contains("confidence"), "{e}");
        // confidence outside [0, 1] is a contract violation.
        let bad = json!({
            "answers": {"q1": {"type": "choice", "choice": "yes",
                               "probabilities": {"yes": 1.0, "no": 0.0},
                               "confidence": 1.5}},
            "usage": {}
        });
        let e = parse_answer(&bad, &s.labels).unwrap_err();
        assert!(e.contains("confidence"), "{e}");
    }

    #[test]
    fn decide_posts_the_question_and_stores_meta_for_the_snapshot() {
        let s = spec("deploy now", "the rubric", &["yes", "no"], &[]);
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
        let seen = std::sync::Arc::clone(&recorded);
        let mut ollaya = Ollaya::with_post(OllayaConfig::default(), move |_cfg, body| {
            *seen.lock().unwrap() = Some(body.clone());
            Ok(answer(r#"{"yes": 0.9, "no": 0.05}"#, 0.85))
        });
        let probs = ollaya.decide(&s).unwrap();
        assert!((probs["yes"] - 0.9 / 0.95).abs() < 1e-9);
        let body = recorded.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], json!("winnow:e4b"));
        assert_eq!(body["state"], json!("deploy now"));
        assert_eq!(body["questions"]["q1"]["instructions"], json!("the rubric"));
        let meta = ollaya.last_meta().unwrap();
        assert!((meta.confidence - 0.85).abs() < 1e-9);
        let snapshot = meta.snapshot();
        assert_eq!(snapshot["confidence"], json!(0.85));
    }

    /// A single loopback response server that records the first request
    /// line. It has a deadline so an accidental missing connection fails.
    #[test]
    fn http_post_should_fail_at_the_configured_timeout_when_the_daemon_stalls() {
        // Never accepted: the kernel completes the handshake from the backlog,
        // so the request is sent and the reply never comes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = OllayaConfig {
            base: format!("http://{}", listener.local_addr().unwrap()),
            timeout: Duration::from_millis(200),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let error = http_post(&config, &json!({"model": "winnow:e4b"})).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cap bounds the call: {:?}",
            started.elapsed()
        );
        assert!(error.starts_with("ollaya http://"), "{error}");
        assert_eq!(OllayaConfig::default().timeout, TIMEOUT);
        drop(listener);
    }

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
    fn http_post_reads_systemone_and_explains_a_daemon_status_error() {
        let reply = answer(r#"{"yes": 0.75, "no": 0.25}"#, 0.7).to_string();
        let (base, server) = http_once("200 OK", reply.clone());
        let config = OllayaConfig {
            base: base.clone(),
            model_name: "winnow:e4b".to_string(),
            ..Default::default()
        };
        let response = http_post(&config, &json!({"model": "winnow:e4b"})).unwrap();
        assert_eq!(response, serde_json::from_str::<Value>(&reply).unwrap());
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("POST /v1/systemone HTTP/1.1"),
            "the Ollaya transport must target the TypeSafe endpoint"
        );

        let (base, server) = http_once("404 Not Found", "{}".to_string());
        let config = OllayaConfig {
            base: base.clone(),
            model_name: "winnow:e4b".to_string(),
            ..Default::default()
        };
        let error = http_post(&config, &json!({})).unwrap_err();
        assert!(error.contains("HTTP 404"), "{error}");
        assert!(
            error.contains(&format!("is the ollaya daemon running at {base}?")),
            "{error}"
        );
        server.join().unwrap();
    }
}
