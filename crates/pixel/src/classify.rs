// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` — bounded label decision through a remote LLM.
//!
//! The decision half of the refinement contract: one OpenAI-compatible chat
//! completion (OpenRouter, Ollama Cloud, or a local llama-server) maps the
//! state, shared framing, labels and criteria onto a probability
//! distribution over the caller's labels — the shape a Jev-class decision
//! model returns. TypeSafe's hosted Jev itself is the `jev` preset: it
//! serves the same decision shape over TypeSafe's `/v1/systemone` wire
//! (`decide_jev`), with native calibrated probabilities instead of
//! verbalized ones. Remote keys resolve from the env var, then
//! `pixel config remote-key`, then — when configured — an Infisical
//! project (`decide_infisical`). Non-deterministic and network-bound by
//! design: the local static/verdict backends were removed because no
//! off-the-shelf local model beat Jev on the coding benchmark (see
//! `docs/bench/decide-bakeoff.md`).
//!
//! `context` stays a separate field — it is the shared framing every
//! candidate sees, never prefixed into the state text.
//!
//! `pixel classify --jsonl` serves one decision per stdin line on one
//! connection, so per-decision latency excludes setup.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::BufRead;

/// Disclosed basis for every remote decision — probabilities are verbalized
/// by the model, not a calibration-head output.
pub(crate) const REMOTE_BASIS: &str = "remote LLM, non-deterministic, verbalized probabilities (self-reported, renormalized to sum 1)";
/// Per-component character cap for state, context, and criterion/fallback text.
/// Components stay separate so context truncation cannot erase a criterion.
pub(crate) const TEXT_CAP_CHARS: usize = 32_768;

/// One decision request: the text to judge, the framing every candidate
/// shares, the allowed labels, and an optional criterion description per
/// label (what the label *means*).
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The part that varies from decision to decision — the state to judge.
    pub text: String,
    /// Framing shared by every candidate: the question being asked and rubric.
    pub context: String,
    pub labels: Vec<String>,
    /// Normalized explicit criteria plus bounded label fallbacks when needed.
    pub criteria: BTreeMap<String, String>,
    clipped_fields: Vec<String>,
}

impl Spec {
    /// Validate identities, then cap every component sent to the model.
    pub fn checked(
        text: String,
        context: String,
        labels: Vec<String>,
        criteria: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        if labels
            .iter()
            .any(|label| label.is_empty() || !seen.insert(label.clone()))
        {
            return Err("labels must be non-empty and distinct".to_string());
        }
        if labels.len() < 2 {
            return Err("at least two labels are required".to_string());
        }
        if let Some(bad) = criteria.keys().find(|key| !labels.contains(key)) {
            return Err(format!("criterion for unknown label {bad:?}"));
        }

        let mut clipped_fields = Vec::new();
        let (text, text_clipped) = clip_text(&text);
        if text_clipped {
            clipped_fields.push("text".to_string());
        }
        let (context, context_clipped) = clip_text(&context);
        if context_clipped {
            clipped_fields.push("context".to_string());
        }
        let mut normalized = BTreeMap::new();
        for (label, criterion) in criteria {
            let (criterion, clipped) = clip_text(&criterion);
            if clipped {
                clipped_fields.push(format!("criteria.{label}"));
            }
            normalized.insert(label, criterion);
        }
        for label in &labels {
            if !normalized.contains_key(label) {
                let (fallback, clipped) = clip_text(label);
                if clipped {
                    clipped_fields.push(format!("label_fallback.{label}"));
                    normalized.insert(label.clone(), fallback);
                }
            }
        }

        Ok(Spec {
            text,
            context,
            labels,
            criteria: normalized,
            clipped_fields,
        })
    }

    fn was_clipped(&self) -> bool {
        !self.clipped_fields.is_empty()
    }
}

/// The decision engine seam behind one invocation: production is the remote
/// chat adapter or the local Ollaya server; tests inject a fake with the
/// same surface — a probability distribution over the caller's labels.
///
/// `pub(crate)` so a caller that asks many decisions itself (`pixel
/// ultraflow`, one question per cycle) can hold one engine open instead of
/// one process per decision.
pub(crate) trait DecisionEngine {
    fn model_id(&self) -> String;
    /// Provider preset surfaced in the snapshot (`None` for test engines).
    fn provider(&self) -> Option<&'static str>;
    /// Remote decisions are verbalized, so this is always false in production.
    fn deterministic(&self) -> bool;
    /// The disclosed basis string: how this engine produces probabilities.
    fn basis(&self) -> String;
    /// Engine-specific snapshot fields (Ollaya: confidence meta);
    /// merged into the document's `snapshot` object when present.
    fn extra_snapshot(&self) -> Option<Value>;
    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String>;
    /// Answer a label-less typed battery over the state — the local Ollaya
    /// engine's path when `--label` is absent. The remote engine has no
    /// battery: its prompt is built from the caller's labels.
    fn decide_battery(&mut self, _state: &str) -> Result<Value, String> {
        Err(
            "no --label given: bare classify needs the local Ollaya engine \
             (`--engine ollaya` or `pixel config classify-engine local`)"
                .to_string(),
        )
    }
}

impl DecisionEngine for crate::decide_remote::Remote {
    fn model_id(&self) -> String {
        self.model_id().to_string()
    }

    fn provider(&self) -> Option<&'static str> {
        Some(self.provider())
    }

    fn deterministic(&self) -> bool {
        self.deterministic()
    }

    fn basis(&self) -> String {
        REMOTE_BASIS.to_string()
    }

    fn extra_snapshot(&self) -> Option<Value> {
        None
    }

    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        crate::decide_remote::Remote::decide(self, spec)
    }
}

impl DecisionEngine for crate::decide_ollaya::Ollaya {
    fn model_id(&self) -> String {
        self.model_id().to_string()
    }

    fn provider(&self) -> Option<&'static str> {
        Some("ollaya")
    }

    fn deterministic(&self) -> bool {
        // A single readout pass is deterministic in principle, but that is
        // untested on a supported MLX runtime; disclose until measured.
        false
    }

    fn basis(&self) -> String {
        crate::decide_ollaya::OLLAYA_BASIS.to_string()
    }

    fn extra_snapshot(&self) -> Option<Value> {
        self.last_meta()
            .map(crate::decide_ollaya::AnswerMeta::snapshot)
    }

    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        crate::decide_ollaya::Ollaya::decide(self, spec)
    }

    fn decide_battery(&mut self, state: &str) -> Result<Value, String> {
        self.ask(state, &crate::decide_ollaya::default_battery())
    }
}

impl DecisionEngine for crate::decide_jev::Jev {
    fn model_id(&self) -> String {
        self.model_id().to_string()
    }

    fn provider(&self) -> Option<&'static str> {
        Some("jev")
    }

    fn deterministic(&self) -> bool {
        false
    }

    fn basis(&self) -> String {
        crate::decide_jev::JEV_BASIS.to_string()
    }

    fn extra_snapshot(&self) -> Option<Value> {
        self.last_meta()
            .map(crate::decide_ollaya::AnswerMeta::snapshot)
    }

    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        crate::decide_jev::Jev::decide(self, spec)
    }
}

/// The argmax label — first in the caller's label order on a tie
/// (deterministic, never alphabetical accident).
pub(crate) fn predicted<'a>(probs: &BTreeMap<String, f64>, labels: &'a [String]) -> &'a str {
    // max_by returns the LAST maximum; reversing makes a tie resolve to the
    // first label in the caller's order.
    labels
        .iter()
        .rev()
        .max_by(|a, b| probs[*a].total_cmp(&probs[*b]))
        .map_or("", String::as_str)
}

/// Build the per-decision JSON document and disclose any input clipping.
fn document(engine: &dyn DecisionEngine, spec: &Spec, probs: &BTreeMap<String, f64>) -> Value {
    let mut basis = engine.basis();
    if spec.was_clipped() {
        basis.push_str(&format!(
            "; caps: {TEXT_CAP_CHARS} characters per input component; affected fields: {}",
            spec.clipped_fields.join(", ")
        ));
    }
    let marker = if spec.was_clipped() {
        "capped"
    } else {
        "complete"
    };
    let mut out = json!({
        "marker": marker,
        "predicted": predicted(probs, &spec.labels),
        "probs": probs,
        "epistemics": {
            "closed_world": false,
            "lower_bound": false,
            "basis": basis,
            "confidence": marker,
        },
        "snapshot": {
            "model": engine.model_id(),
            "temperature": 0.0,
            "labels": spec.labels,
            "deterministic": engine.deterministic(),
        },
    });
    if let Some(provider) = engine.provider() {
        out["snapshot"]["provider"] = json!(provider);
    }
    let extra = engine.extra_snapshot();
    if let Some(fields) = extra.as_ref().and_then(Value::as_object) {
        for (key, value) in fields {
            out["snapshot"][key.clone()] = value.clone();
        }
    }
    if spec.was_clipped() {
        let message = format!(
            "input capped at {TEXT_CAP_CHARS} characters per component; affected fields: {}",
            spec.clipped_fields.join(", ")
        );
        out["caps"] = json!([{
            "name": "input_chars_per_component",
            "limit": TEXT_CAP_CHARS,
            "affected_fields": spec.clipped_fields,
        }]);
        out["warnings"] = json!([{"code": "INPUT_CAPPED", "message": message}]);
    }
    out
}

/// Return the bounded text and whether at least one character was removed.
fn clip_text(text: &str) -> (String, bool) {
    let mut chars = text.chars();
    let clipped = chars.by_ref().take(TEXT_CAP_CHARS).collect();
    (clipped, chars.next().is_some())
}

/// Open the remote decision engine from the resolved preset/model config.
/// A preset that needs a key and has none fails here (see
/// `decide_remote::resolve_config`), naming the env var to set.
#[cfg_attr(test, mutants::skip)] // thin adapter over the real env; logic lives in decide_remote
fn open_engine(
    preset: crate::decide_remote::Preset,
    model: Option<String>,
) -> Result<crate::decide_remote::Remote, String> {
    crate::decide_remote::resolve_config(preset, model, remote_key_value(preset)?)
        .map(crate::decide_remote::Remote::open)
}

/// Open the hosted Jev engine from the same preset config the chat
/// adapters resolve: base, model override and key — so `--remote-model`,
/// `PIXEL_REMOTE_*`, `pixel config remote-key jev` and the Infisical
/// source all behave identically whichever preset is chosen.
#[cfg_attr(test, mutants::skip)] // thin adapter over the real env; the policy is decide_jev's
fn open_jev_engine(model: Option<String>) -> Result<crate::decide_jev::Jev, String> {
    crate::decide_remote::resolve_config(
        crate::decide_remote::Preset::Jev,
        model,
        remote_key_value(crate::decide_remote::Preset::Jev)?,
    )
    .map(|config| {
        let key = config.key_value();
        crate::decide_jev::Jev::open(crate::decide_jev::JevConfig {
            base: config.base,
            model_name: config.model,
            key,
            ..Default::default()
        })
    })
}

/// Read the remote API-key value for a preset, in disclosure order: the
/// key env var (`PIXEL_REMOTE_KEY_ENV` names one, else the preset's own),
/// then `pixel config remote-key <preset>`, then a configured Infisical
/// project (see `decide_infisical`) — a configured Infisical failure is
/// loud, an unconfigured one just falls through. The value is consumed
/// here and held only inside the engine adapter — never logged or written
/// to a document.
#[cfg_attr(test, mutants::skip)] // reads the real env and ~/.pixel; the name rule is key_env_name
fn remote_key_value(preset: crate::decide_remote::Preset) -> Result<Option<String>, String> {
    let explicit = std::env::var("PIXEL_REMOTE_KEY_ENV").ok();
    let from_env = crate::decide_remote::key_env_name(preset, explicit)
        .and_then(|name| std::env::var(name).ok().filter(|v| !v.is_empty()));
    if from_env.is_some() {
        return Ok(from_env);
    }
    // `pixel config remote-key <preset>` is the fallback so a key need not
    // live in every shell's environment.
    if let Some(key) = crate::config_cmd::remote_key(preset) {
        return Ok(Some(key));
    }
    // Infisical is the third source, off unless configured; its own
    // contract decides what counts as absent.
    crate::decide_infisical::lookup_key(preset)
}

/// Parse one JSONL spec line (serve mode and tests share this path).
fn parse_spec_line(line: &str) -> Result<Spec, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("invalid spec JSON: {e}"))?;
    let text = v
        .get("text")
        .and_then(Value::as_str)
        .ok_or("spec needs a \"text\" string")?
        .to_string();
    let context = match v.get("context") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(context)) => context.clone(),
        Some(other) => return Err(format!("\"context\" must be a string or null, got {other}")),
    };
    // Dropping a non-string silently would classify a request the caller
    // never sent: `["a", 7, "b"]` would pass validation as two labels, and a
    // numeric criterion would silently fall back to the label's own name.
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .ok_or("spec needs a \"labels\" array")?
        .iter()
        .map(|l| {
            l.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("every label must be a string, got {l}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let criteria = match v.get("criteria") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| format!("criterion {k:?} must be a string, got {v}"))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        Some(other) => return Err(format!("\"criteria\" must be an object, got {other}")),
    };
    Spec::checked(text, context, labels, criteria)
}

/// One input line to zero or one output lines. `None` is a blank line, which
/// carries no decision and so produces no result; a bad line answers `{"ok":
/// false}` rather than ending the stream, because one malformed item must not
/// abort a run of several hundred.
fn serve_line(engine: &mut dyn DecisionEngine, line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let out = match parse_spec_line(line).and_then(|s| engine.decide(&s).map(|p| (s, p))) {
        Ok((spec, probs)) => {
            let mut doc = document(engine, &spec, &probs);
            doc["ok"] = json!(true);
            doc
        }
        Err(e) => json!({"ok": false, "error": e}),
    };
    // A `Value` built here is always encodable; a failure would still have to
    // answer on this line rather than take the stream down with it.
    Some(serde_json::to_string(&out).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"result encode: {e}\"}}").replace('\n', " ")
    }))
}

trait ClassifyOutput {
    fn write_text(&mut self, text: &str) -> Result<(), String>;
    fn print_document(&mut self, document: &Value) -> Result<(), String>;
}

struct ProductionOutput;

impl ClassifyOutput for ProductionOutput {
    #[cfg_attr(test, mutants::skip)] // thin adapter preserving stdout accounting
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        crate::write_stdout(text)
    }

    #[cfg_attr(test, mutants::skip)] // thin adapter preserving JSON output caps
    fn print_document(&mut self, document: &Value) -> Result<(), String> {
        crate::print_data(document, true)
    }
}

/// Stream stdin lines through one resident model and preserve line framing.
fn serve_jsonl(
    reader: impl BufRead,
    engine: &mut dyn DecisionEngine,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    for line in reader.lines() {
        let line = line.map_err(|error| format!("stdin read: {error}"))?;
        if let Some(line_output) = serve_line(engine, &line) {
            output.write_text(&line_output)?;
            output.write_text("\n")?;
        }
    }
    Ok(())
}

#[derive(clap::Args, Debug)]
#[group(multiple = true)]
pub struct ClassifyOptions {
    /// The state text to judge — the part that varies (omit with --jsonl).
    pub text: Option<String>,
    /// Framing every candidate shares (the question and rubric preamble).
    /// It is replicated into each candidate rather than added to the state.
    #[arg(long, conflicts_with = "jsonl")]
    pub context: Option<String>,
    /// Candidate labels (repeatable or comma-separated). Omit to answer the
    /// default question battery over the text instead — that path needs the
    /// local Ollaya engine (`--engine ollaya` or stored `local`).
    #[arg(long = "label", value_delimiter = ',')]
    pub labels: Vec<String>,
    /// Criterion text per label: --criterion label="description".
    #[arg(long = "criterion", requires = "labels")]
    pub criteria: Vec<String>,
    /// Remote provider preset (overrides the stored choice; defaults to `openrouter`).
    /// Selects the base URL and the API-key env var; see `PIXEL_REMOTE_*`.
    #[arg(long, value_enum)]
    pub remote_preset: Option<crate::decide_remote::Preset>,
    /// Remote model id (overrides the preset default and `PIXEL_REMOTE_MODEL`).
    #[arg(long)]
    pub remote_model: Option<String>,
    /// Decision engine override: `remote` (an OpenAI-compatible chat
    /// completion) or `ollaya` (a local Ollaya server's native typed-choice
    /// readout). Without a flag the engine comes from `pixel install`'s
    /// stored preference, probing the local server and falling back to
    /// remote.
    #[arg(long, value_enum)]
    pub engine: Option<EngineChoice>,
    /// Base URL of the local Ollaya server (with `--engine ollaya`).
    #[arg(long, default_value_t = crate::decide_ollaya::DEFAULT_BASE.to_string())]
    pub ollaya_url: String,
    /// Serve mode: JSONL spec lines on stdin, one result per line.
    #[arg(long)]
    pub jsonl: bool,
    /// Answer only from a local engine that is already listening: never
    /// auto-start it, never fall back to remote. When it is not warm the
    /// command prints nothing on stdout and fails with a message.
    #[arg(long, conflicts_with = "remote_preset")]
    pub if_warm: bool,
    /// Judge the text as a coding-agent prompt with the built-in intent
    /// labels (bugfix, feature, refactor, investigate, question, review, ops)
    /// and name the pixel ops that fit the verdict.
    #[arg(long, conflicts_with_all = ["labels", "context", "jsonl"])]
    pub task_intent: bool,
    #[arg(long)]
    pub json: bool,
}

/// Which decision engine answers `pixel classify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum EngineChoice {
    /// The OpenAI-compatible remote adapter (default).
    #[default]
    Remote,
    /// A local Ollaya server's native typed-choice readout.
    Ollaya,
}

/// Parse `--criterion label=description` pairs, keeping the strict contract
/// but naming the declared labels and a valid invocation in the error so the
/// fix is one copy-paste away.
fn parse_criteria(pairs: &[String], labels: &[String]) -> Result<BTreeMap<String, String>, String> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| criterion_error(p, labels))
        })
        .collect()
}

fn criterion_error(got: &str, labels: &[String]) -> String {
    // Quoting the whole `label=description` pair keeps a spaced label one shell
    // argument, copy-pasteable as is.
    let example = labels.first().map_or_else(
        || r#"--criterion "<label>=<description>""#.to_string(),
        |l| format!(r#"--criterion "{l}=one bounded edit""#),
    );
    format!(
        "--criterion needs <label>=<description>, got {got:?}; declared labels: {}; example: {example}",
        labels.join(", ")
    )
}

/// The `Spec` a one-shot invocation asks for. Separate from `run` so the
/// argument contract is checked before anything touches the network: a
/// caller who forgot `--label` should be told that, not made to wait for a
/// remote call that cannot answer the question they meant to ask.
fn one_shot_spec(opts: &ClassifyOptions) -> Result<Spec, String> {
    let text = opts
        .text
        .as_deref()
        .ok_or("classify needs a text argument (or --jsonl)")?;
    if opts.task_intent {
        return crate::prompt_intent::spec(text);
    }
    Spec::checked(
        text.to_string(),
        opts.context.clone().unwrap_or_default(),
        opts.labels.clone(),
        parse_criteria(&opts.criteria, &opts.labels)?,
    )
}

/// One line per battery answer, `name: answer (evidence)`: a choice names
/// the argmax with its probability, a score names value/levels plus the
/// legend of the rounded level, a noul names yes/no with its probability.
/// Unknown answer shapes print the raw JSON; `--json` always has it anyway.
fn render_battery(answers: &Value) -> String {
    let mut out = String::new();
    let Some(map) = answers.as_object() else {
        return out;
    };
    for (name, answer) in map {
        let ty = answer.get("type").and_then(Value::as_str).unwrap_or("");
        let line = match ty {
            "choice" => {
                let choice = answer.get("choice").and_then(Value::as_str).unwrap_or("?");
                let p = answer
                    .get("probabilities")
                    .and_then(|m| m.get(choice))
                    .and_then(Value::as_f64);
                match p {
                    Some(p) => format!("{name}: {choice} ({p:.3})"),
                    None => format!("{name}: {choice}"),
                }
            }
            "score" => {
                let score = answer.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                let legend = answer.get("legend").and_then(Value::as_object);
                let max = legend.map_or(0, |l| l.len().saturating_sub(1));
                let level = score.round().to_string();
                let label = legend
                    .and_then(|l| l.get(&level))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("{name}: {score:.2}/{max} {label}")
            }
            "noul" => {
                let p = answer.get("noul").and_then(Value::as_f64).unwrap_or(0.0);
                format!("{name}: {} ({p:.3})", if p >= 0.5 { "yes" } else { "no" })
            }
            _ => format!("{name}: {answer}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The JSON document for a battery answer — same disclosure envelope as a
/// labeled decision, but `answers` carries the per-question typed answers.
fn battery_document(engine: &dyn DecisionEngine, answers: &Value, clipped: bool) -> Value {
    let marker = if clipped { "capped" } else { "complete" };
    json!({
        "marker": marker,
        "answers": answers,
        "epistemics": {
            "closed_world": false,
            "lower_bound": false,
            "basis": engine.basis(),
            "confidence": marker,
        },
        "snapshot": {
            "model": engine.model_id(),
            "provider": engine.provider(),
            "deterministic": engine.deterministic(),
        },
    })
}

/// Render probabilities and disclose component clipping only when it occurred.
fn render_probs(probs: &BTreeMap<String, f64>, spec: &Spec) -> String {
    let mut out = String::new();
    for (label, probability) in probs {
        out.push_str(&format!("{label}: {probability:.3}\n"));
    }
    let top = predicted(probs, &spec.labels);
    out.push_str(&format!("predicted: {top}\n"));
    if spec.was_clipped() {
        out.push_str("marker: capped\nconfidence: capped\n");
        out.push_str(&format!(
            "warning: input capped at {TEXT_CAP_CHARS} characters per component; affected fields: {}\n",
            spec.clipped_fields.join(", ")
        ));
    }
    out
}

fn run_with(
    opts: ClassifyOptions,
    resolve: impl FnOnce(&ClassifyOptions) -> Result<crate::classify_setup::ResolvedEngine, String>,
    opener: impl FnOnce(
        crate::classify_setup::ResolvedEngine,
    ) -> Result<Box<dyn DecisionEngine>, String>,
    reader: impl BufRead,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    if opts.jsonl {
        let mut engine = opener(resolve(&opts)?)? as Box<dyn DecisionEngine>;
        return serve_jsonl(reader, engine.as_mut(), output);
    }

    // No labels, no bounded decision: answer the default question battery
    // instead (what `ollaya run` does with no --questions). Context folds
    // into the state — the battery's questions carry their own rubric.
    if opts.labels.is_empty() && !opts.task_intent {
        let text = opts
            .text
            .as_deref()
            .ok_or("classify needs a text argument (or --jsonl)")?;
        let state = match opts.context.as_deref().filter(|c| !c.is_empty()) {
            Some(context) => format!("{context}\n\n{text}"),
            None => text.to_string(),
        };
        let (state, clipped) = clip_text(&state);
        let resolved = resolve(&opts)?;
        if !matches!(
            resolved,
            crate::classify_setup::ResolvedEngine::Local { .. }
        ) {
            return Err(
                "no --label given: bare classify needs the local Ollaya engine \
                 (`--engine ollaya` or `pixel config classify-engine local`)"
                    .to_string(),
            );
        }
        let mut engine = opener(resolved)?;
        let answers = engine.decide_battery(&state)?;
        if opts.json {
            return output.print_document(&battery_document(engine.as_ref(), &answers, clipped));
        }
        let mut out = render_battery(&answers);
        if clipped {
            out.push_str(&format!(
                "warning: input capped at {TEXT_CAP_CHARS} characters; affected fields: state\n"
            ));
        }
        return output.write_text(&out);
    }

    let spec = one_shot_spec(&opts)?;
    let mut engine = opener(resolve(&opts)?)?;
    let probs = engine.decide(&spec)?;
    let ops = opts
        .task_intent
        .then(|| crate::prompt_intent::ops_for(predicted(&probs, &spec.labels)))
        .flatten();
    if opts.json {
        let mut doc = document(engine.as_ref(), &spec, &probs);
        if let Some(ops) = ops {
            doc["next_ops"] = json!(ops);
        }
        output.print_document(&doc)
    } else {
        let mut text = render_probs(&probs, &spec);
        if let Some(ops) = ops {
            text.push_str(&format!("next: {}\n", ops.join(", ")));
        }
        output.write_text(&text)
    }
}

/// The engine for this invocation: the explicit flag wins, then the stored
/// preference, then a reachability probe of the local daemon. A resolved
/// local engine that is not yet answering gets one auto-start chance.
#[cfg_attr(test, mutants::skip)] // Environment adapter; resolution and startup are tested with injected I/O.
fn resolve_engine_for(
    opts: &ClassifyOptions,
) -> Result<crate::classify_setup::ResolvedEngine, String> {
    if opts.if_warm {
        return resolve_if_warm(
            opts,
            crate::config_cmd::classify_engine(),
            crate::classify_setup::local_base(),
            crate::classify_setup::server_reachable,
        );
    }
    resolve_engine_with(
        opts,
        crate::config_cmd::classify_engine(),
        crate::classify_setup::server_reachable,
        crate::classify_setup::ensure_local,
    )
}

fn resolve_engine_with(
    opts: &ClassifyOptions,
    stored: Option<String>,
    mut reachable: impl FnMut(&str) -> bool,
    ensure: impl FnOnce(&str) -> Result<(), String>,
) -> Result<crate::classify_setup::ResolvedEngine, String> {
    let needs_probe =
        opts.engine.is_none() && !matches!(stored.as_deref(), Some("local") | Some("remote"));
    let available = if needs_probe {
        reachable(&crate::classify_setup::local_base())
    } else {
        false
    };
    let resolved = crate::classify_setup::resolve_engine(
        opts.engine,
        opts.ollaya_url.clone(),
        stored,
        available,
    );
    if let crate::classify_setup::ResolvedEngine::Local { base } = &resolved
        && !reachable(base)
    {
        ensure(base)?;
    }
    Ok(resolved)
}

/// The engine `--if-warm` may use: the local daemon, and only when it
/// already answers. Nothing is started and nothing falls back to remote.
fn resolve_if_warm(
    opts: &ClassifyOptions,
    stored: Option<String>,
    local_base: String,
    reachable: impl FnOnce(&str) -> bool,
) -> Result<crate::classify_setup::ResolvedEngine, String> {
    let base = match opts.engine {
        Some(EngineChoice::Remote) => {
            return Err(
                "--if-warm answers only from the local engine; drop --engine remote".into(),
            );
        }
        Some(EngineChoice::Ollaya) => opts.ollaya_url.clone(),
        None if !crate::classify_setup::local_permitted(stored.as_deref()) => {
            return Err(
                "not warm: the stored classify engine is remote, and --if-warm answers only from the local engine".into(),
            );
        }
        None => local_base,
    };
    if !reachable(&base) {
        return Err(format!(
            "not warm: no local classify engine is listening at {base}; --if-warm never starts it (`pixel classify` without --if-warm does)"
        ));
    }
    Ok(crate::classify_setup::ResolvedEngine::Local { base })
}

fn resolve_remote_preset(
    flag: Option<crate::decide_remote::Preset>,
    stored: Option<crate::decide_remote::Preset>,
) -> crate::decide_remote::Preset {
    flag.or(stored).unwrap_or_default()
}

/// The Ollaya engine config for one classify call. `--if-warm` gets the
/// prompt hook's short whole-request cap: its warm check is a TCP connect
/// only, so a daemon that accepts while it is still loading the model would
/// otherwise hold the command for the 120 s default instead of leaving the
/// documented exit 1.
fn ollaya_config(base: String, if_warm: bool) -> crate::decide_ollaya::OllayaConfig {
    let mut config = crate::decide_ollaya::OllayaConfig {
        base,
        ..Default::default()
    };
    if if_warm {
        config.timeout = crate::prompt_intent::HOOK_CALL_TIMEOUT;
    }
    config
}

pub fn run(opts: ClassifyOptions) -> Result<(), String> {
    if !crate::config_cmd::classify_enabled()? {
        return Err("classify is disabled; enable it with `pixel config classify on` or `pixel config setup`".into());
    }
    let remote_preset = resolve_remote_preset(
        opts.remote_preset,
        crate::config_cmd::classify_remote_preset(),
    );
    let remote_model = opts.remote_model.clone();
    // `opts` moves into `run_with`; the engine opener still needs the flag.
    let if_warm = opts.if_warm;
    let stdin = std::io::stdin();
    let mut output = ProductionOutput;
    run_with(
        opts,
        resolve_engine_for,
        move |resolved| open_resolved(resolved, remote_preset, remote_model, if_warm),
        stdin.lock(),
        &mut output,
    )
}

/// The engine a resolved choice maps to. The one place the remote/local
/// split becomes an adapter, so `pixel classify` and a caller holding an
/// engine open (`pixel ultraflow`) cannot drift apart on which one answers.
pub(crate) fn open_resolved(
    resolved: crate::classify_setup::ResolvedEngine,
    preset: crate::decide_remote::Preset,
    model: Option<String>,
    if_warm: bool,
) -> Result<Box<dyn DecisionEngine>, String> {
    match resolved {
        crate::classify_setup::ResolvedEngine::Remote => match preset {
            // Jev speaks TypeSafe's decision wire, not `/chat/completions`;
            // it resolves its base/model/key through the same config.
            crate::decide_remote::Preset::Jev => {
                open_jev_engine(model).map(|engine| Box::new(engine) as _)
            }
            _ => open_engine(preset, model).map(|engine| Box::new(engine) as _),
        },
        crate::classify_setup::ResolvedEngine::Local { base } => Ok(Box::new(
            crate::decide_ollaya::Ollaya::open(ollaya_config(base, if_warm)),
        ) as _),
    }
}

/// A decision engine held open across many questions: the same resolution
/// `pixel classify` does for one invocation, with the same `--engine`,
/// `--remote-preset` and `--remote-model` inputs, for a caller that asks one
/// decision per cycle and pays the resolution and the connection once.
pub(crate) fn open_session(
    engine: Option<EngineChoice>,
    ollaya_url: String,
    preset: Option<crate::decide_remote::Preset>,
    model: Option<String>,
) -> Result<Box<dyn DecisionEngine>, String> {
    if !crate::config_cmd::classify_enabled()? {
        return Err("classify is disabled; enable it with `pixel config classify on` or `pixel config setup`".into());
    }
    let opts = ClassifyOptions {
        text: None,
        context: None,
        labels: Vec::new(),
        criteria: Vec::new(),
        remote_preset: preset,
        remote_model: model.clone(),
        engine,
        ollaya_url,
        jsonl: false,
        json: false,
        // A held-open session is an interactive caller, not a prompt hook:
        // it takes the interaction cap, not the hook's sub-second one.
        if_warm: false,
        // Every question it asks carries its own labels, so the built-in
        // prompt-intent battery (which `task_intent` selects) is never what
        // it wants.
        task_intent: false,
    };
    let resolved = resolve_engine_for(&opts)?;
    open_resolved(
        resolved,
        resolve_remote_preset(preset, crate::config_cmd::classify_remote_preset()),
        model,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::{self, Cursor, Read};
    use std::sync::{Arc, Mutex};

    /// Deterministic fake engine: "yes" wins when the spec mentions "alpha",
    /// "no" wins on "beta"; `fail_next` turns the next decide into an error.
    struct FakeEngine {
        calls: Arc<Mutex<Vec<Spec>>>,
        battery_calls: Arc<Mutex<Vec<String>>>,
        battery_answer: Option<Value>,
        fail_next: bool,
        snapshot_extra: Option<Value>,
        custom_basis: Option<String>,
    }

    impl FakeEngine {
        fn new(calls: Arc<Mutex<Vec<Spec>>>) -> Self {
            FakeEngine {
                calls,
                battery_calls: Arc::new(Mutex::new(Vec::new())),
                battery_answer: None,
                fail_next: false,
                snapshot_extra: None,
                custom_basis: None,
            }
        }
    }

    impl DecisionEngine for FakeEngine {
        fn model_id(&self) -> String {
            "fake".to_string()
        }

        fn provider(&self) -> Option<&'static str> {
            None
        }

        fn deterministic(&self) -> bool {
            true
        }

        fn basis(&self) -> String {
            self.custom_basis
                .clone()
                .unwrap_or_else(|| REMOTE_BASIS.to_string())
        }

        fn extra_snapshot(&self) -> Option<Value> {
            self.snapshot_extra.clone()
        }

        fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
            self.calls.lock().unwrap().push(spec.clone());
            if self.fail_next {
                self.fail_next = false;
                return Err("decision failed".to_string());
            }
            let top = spec.text.contains("alpha");
            Ok(spec
                .labels
                .iter()
                .map(|l| {
                    let p = match (l.as_str(), top) {
                        ("yes", true) => 0.9,
                        ("yes", false) => 0.1,
                        (_, true) => 0.1 / (spec.labels.len() - 1) as f64,
                        (_, false) if l.as_str() == "no" => 0.9,
                        (_, false) => 0.1 / (spec.labels.len() - 1) as f64,
                    };
                    (l.clone(), p)
                })
                .collect())
        }

        fn decide_battery(&mut self, state: &str) -> Result<Value, String> {
            self.battery_calls.lock().unwrap().push(state.to_string());
            self.battery_answer
                .clone()
                .ok_or_else(|| "no battery answer scripted".to_string())
        }
    }

    #[derive(Default)]
    struct RecordingOutput {
        text: String,
        documents: Vec<Value>,
        fail_text: bool,
        fail_document: bool,
    }

    impl ClassifyOutput for RecordingOutput {
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            if self.fail_text {
                return Err("text output failed".to_string());
            }
            self.text.push_str(text);
            Ok(())
        }

        fn print_document(&mut self, document: &Value) -> Result<(), String> {
            if self.fail_document {
                return Err("document output failed".to_string());
            }
            self.documents.push(document.clone());
            Ok(())
        }
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("reader failed"))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("reader failed"))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    fn fake_engine(calls: Arc<Mutex<Vec<Spec>>>) -> Result<Box<dyn DecisionEngine>, String> {
        Ok(Box::new(FakeEngine::new(calls)))
    }

    fn test_resolve(
        opts: &ClassifyOptions,
    ) -> Result<crate::classify_setup::ResolvedEngine, String> {
        Ok(crate::classify_setup::resolve_engine(
            opts.engine,
            opts.ollaya_url.clone(),
            None,
            false,
        ))
    }

    fn parse_classify(args: &[&str]) -> ClassifyOptions {
        const PARSER_TEST_STACK: usize = 16_777_216;
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        std::thread::Builder::new()
            .stack_size(PARSER_TEST_STACK)
            .spawn(move || {
                let cli = crate::Cli::try_parse_from(args).unwrap();
                let crate::Command::Classify(options) = cli.command else {
                    panic!("classify argv parsed as another command");
                };
                options
            })
            .unwrap()
            .join()
            .unwrap()
    }

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

    #[test]
    fn spec_rejects_bad_label_sets() {
        let labels = |v: &[&str]| v.iter().map(ToString::to_string).collect();
        let checked = |l| Spec::checked("t".into(), String::new(), l, BTreeMap::new());
        assert!(checked(labels(&["a"])).is_err());
        assert!(checked(labels(&["a", "a"])).is_err());
        assert!(checked(labels(&["a", ""])).is_err());
        assert!(checked(labels(&["a", "b"])).is_ok());
        let mut stray = BTreeMap::new();
        stray.insert("zz".to_string(), "desc".to_string());
        assert!(Spec::checked("t".into(), String::new(), labels(&["a", "b"]), stray).is_err());
    }

    /// The cap bounds worst-case request size, and it counts characters rather
    /// than bytes so a multi-byte character is never cut in half.
    #[test]
    fn clip_text_bounds_long_input_and_reports_only_removed_characters() {
        assert_eq!(clip_text("hello"), ("hello".to_string(), false));
        assert_eq!(clip_text(""), (String::new(), false));
        let exact = "é".repeat(TEXT_CAP_CHARS);
        assert_eq!(clip_text(&exact), (exact, false));
        let long = "é".repeat(TEXT_CAP_CHARS + 1);
        let (clipped, was_clipped) = clip_text(&long);
        assert!(was_clipped);
        assert_eq!(clipped.chars().count(), TEXT_CAP_CHARS);
        assert!(clipped.chars().all(|character| character == 'é'));
    }

    #[test]
    fn predicted_resolves_ties_to_first_label_in_order() {
        let probs = BTreeMap::from([("b".to_string(), 0.5f64), ("a".to_string(), 0.5f64)]);
        let labels = vec!["b".to_string(), "a".to_string()];
        assert_eq!(predicted(&probs, &labels), "b");
        let labels = vec!["a".to_string(), "b".to_string()];
        assert_eq!(predicted(&probs, &labels), "a");
    }

    #[test]
    fn parse_spec_line_validates_and_defaults() {
        let s = parse_spec_line(r#"{"text":"hello","labels":["a","b"],"criteria":{"a":"desc a"}}"#)
            .unwrap();
        assert_eq!(s.labels, vec!["a", "b"]);
        assert_eq!(s.criteria["a"], "desc a");
        // `context` is optional and defaults to empty — never to the text.
        assert_eq!(s.context, "");
        let s = parse_spec_line(r#"{"text":"hello","context":"the rubric","labels":["a","b"]}"#)
            .unwrap();
        assert_eq!(s.context, "the rubric");
        assert_eq!(s.text, "hello");
        assert_eq!(
            parse_spec_line(r#"{"text":"hello","context":"","labels":["a","b"]}"#)
                .unwrap()
                .context,
            ""
        );
        assert_eq!(
            parse_spec_line(r#"{"text":"hello","context":null,"labels":["a","b"]}"#)
                .unwrap()
                .context,
            ""
        );
        for context in ["7", "true", "[]", "{}"] {
            let line = format!(r#"{{"text":"hello","context":{context},"labels":["a","b"]}}"#);
            let error = parse_spec_line(&line).unwrap_err();
            assert!(error.contains("must be a string or null"), "{error}");
        }
        assert!(parse_spec_line("not json").is_err());
        assert!(parse_spec_line(r#"{"text":"t"}"#).is_err());
        assert!(parse_spec_line(r#"{"labels":["a","b"]}"#).is_err());
        // One label is a checked error, not a panic.
        assert!(parse_spec_line(r#"{"text":"t","labels":["a"]}"#).is_err());
    }

    #[test]
    fn parse_spec_line_rejects_non_string_labels_and_criteria() {
        // Silently dropping the 7 would classify a two-label request the
        // caller never sent.
        let e = parse_spec_line(r#"{"text":"t","labels":["a",7,"b"]}"#).unwrap_err();
        assert!(e.contains("every label must be a string"), "{e}");
        let e =
            parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":{"a":7}}"#).unwrap_err();
        assert!(e.contains("must be a string"), "{e}");
        let e = parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":[]}"#).unwrap_err();
        assert!(e.contains("must be an object"), "{e}");
        // An absent or null `criteria` is still the documented default.
        assert!(parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":null}"#).is_ok());
    }

    #[test]
    fn document_carries_probs_argmax_and_remote_envelope() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine::new(calls);
        let s = spec("t", "", &["a", "b"], &[]);
        let probs = BTreeMap::from([("a".to_string(), 0.9f64), ("b".to_string(), 0.1f64)]);
        let doc = document(&engine, &s, &probs);
        assert_eq!(doc["marker"], "complete");
        assert_eq!(doc["predicted"], "a");
        assert_eq!(doc["probs"]["a"], 0.9);
        assert!(
            doc["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .contains("remote LLM")
        );
        assert_eq!(doc["snapshot"]["model"], "fake");
        assert_eq!(doc["snapshot"]["temperature"], 0.0);
        assert_eq!(doc["snapshot"]["deterministic"], true);
        assert_eq!(doc["snapshot"]["labels"], json!(["a", "b"]));
    }

    #[test]
    fn remote_provider_should_use_stored_choice_unless_explicitly_overridden() {
        use crate::decide_remote::Preset;
        assert_eq!(resolve_remote_preset(None, None), Preset::Openrouter);
        assert_eq!(
            resolve_remote_preset(None, Some(Preset::Deepseek)),
            Preset::Deepseek
        );
        let opts = parse_classify(&[
            "pixel",
            "classify",
            "state",
            "--remote-preset",
            "openrouter",
        ]);
        assert_eq!(
            resolve_remote_preset(opts.remote_preset, Some(Preset::Deepseek)),
            Preset::Openrouter
        );
        let opts = parse_classify(&["pixel", "classify", "state"]);
        assert_eq!(
            resolve_remote_preset(opts.remote_preset, Some(Preset::OpencodeGo)),
            Preset::OpencodeGo
        );
    }

    #[test]
    fn ollaya_document_should_disclose_confidence_after_a_real_adapter_decision() {
        let mut engine = crate::decide_ollaya::Ollaya::with_post(
            crate::decide_ollaya::OllayaConfig::default(),
            |_, _| {
                Ok(json!({"answers": {"q1": {
                    "probabilities": {"yes": 0.8, "no": 0.2}, "confidence": 0.6
                }}}))
            },
        );
        assert_eq!(engine.extra_snapshot(), None);
        let spec = spec("state", "", &["yes", "no"], &[]);
        let probabilities = DecisionEngine::decide(&mut engine, &spec).unwrap();
        let doc = document(&engine, &spec, &probabilities);
        assert_eq!(doc["snapshot"]["confidence"], 0.6);
        assert_eq!(doc["probs"]["yes"], 0.8);
    }

    #[test]
    fn resolution_should_probe_only_when_selection_or_startup_needs_it() {
        use crate::classify_setup::ResolvedEngine;
        for (flag, stored, live, local, expected_probes, expected_starts) in [
            (Some(EngineChoice::Remote), None, true, false, 0, 0),
            (Some(EngineChoice::Remote), Some("local"), true, false, 0, 0),
            (None, Some("remote"), true, false, 0, 0),
            (None, Some("local"), true, true, 1, 0),
            (None, Some("local"), false, true, 1, 1),
            (None, None, false, false, 1, 0),
            (None, Some("auto"), true, true, 2, 0),
            (Some(EngineChoice::Ollaya), Some("remote"), true, true, 1, 0),
            (Some(EngineChoice::Ollaya), None, false, true, 1, 1),
        ] {
            let mut opts = parse_classify(&["pixel", "classify", "state"]);
            opts.engine = flag;
            let mut probes = 0;
            let mut starts = 0;
            let resolved = resolve_engine_with(
                &opts,
                stored.map(str::to_string),
                |_| {
                    probes += 1;
                    live
                },
                |_| {
                    starts += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(matches!(resolved, ResolvedEngine::Local { .. }), local);
            assert_eq!(probes, expected_probes, "flag={flag:?}, stored={stored:?}");
            assert_eq!(starts, expected_starts, "flag={flag:?}, stored={stored:?}");
        }
    }

    #[test]
    fn classify_should_return_startup_failure_before_opening_the_engine() {
        for args in [
            &["pixel", "classify", "state", "--engine", "ollaya"][..],
            &[
                "pixel", "classify", "state", "--engine", "ollaya", "--label", "yes,no",
            ],
            &["pixel", "classify", "--jsonl", "--engine", "ollaya"],
        ] {
            let opts = parse_classify(args);
            let mut output = RecordingOutput::default();
            let error = run_with(
                opts,
                |opts| {
                    resolve_engine_with(
                        opts,
                        None,
                        |_| false,
                        |_| Err("cannot open server log".to_string()),
                    )
                },
                |_| panic!("startup failure must prevent opening the engine"),
                Cursor::new(Vec::<u8>::new()),
                &mut output,
            )
            .unwrap_err();
            assert_eq!(error, "cannot open server log");
            assert!(output.text.is_empty());
        }
    }

    #[test]
    fn production_engine_adapters_disclose_their_real_metadata() {
        let remote = crate::decide_remote::Remote::open(
            crate::decide_remote::resolve_config(
                crate::decide_remote::Preset::Local,
                Some("test-remote".to_string()),
                None,
            )
            .unwrap(),
        );
        let spec = spec("state", "", &["yes", "no"], &[]);
        let probabilities = BTreeMap::from([("yes".to_string(), 0.8), ("no".to_string(), 0.2)]);
        let remote_document = document(&remote, &spec, &probabilities);
        assert_eq!(remote.extra_snapshot(), None);
        assert_eq!(remote_document["snapshot"]["model"], "test-remote");
        assert_eq!(remote_document["snapshot"]["provider"], "local");
        assert_eq!(remote_document["snapshot"]["deterministic"], false);
        assert_eq!(remote_document["epistemics"]["basis"], REMOTE_BASIS);

        let mut remote = remote;
        assert!(remote.decide_battery("state").is_err());

        let mut ollaya = crate::decide_ollaya::Ollaya::open(crate::decide_ollaya::OllayaConfig {
            base: "http://127.0.0.1:9".to_string(),
            model_name: "test-ollaya".to_string(),
            ..Default::default()
        });
        let ollaya_document = battery_document(&ollaya, &json!({}), false);
        assert_eq!(ollaya_document["snapshot"]["model"], "test-ollaya");
        assert_eq!(ollaya_document["snapshot"]["provider"], "ollaya");
        assert_eq!(ollaya_document["snapshot"]["deterministic"], false);
        assert_eq!(
            ollaya_document["epistemics"]["basis"],
            crate::decide_ollaya::OLLAYA_BASIS
        );
        assert!(ollaya.decide_battery("state").is_err());
    }

    /// A criterion missing its `label=` prefix is rejected with the declared
    /// labels and a complete valid invocation; a well-formed pair parses and
    /// keeps everything after the first `=` in the description.
    #[test]
    fn parse_criteria_requires_key_value_pairs() {
        let labels = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(parse_criteria(&["a=desc".to_string()], &labels(&["a"])).is_ok());
        let ok = parse_criteria(&["a=one bounded = edit".to_string()], &labels(&["a"])).unwrap();
        assert_eq!(ok["a"], "one bounded = edit");
        let err =
            parse_criteria(&["missing-eq".to_string()], &labels(&["trivial", "deep"])).unwrap_err();
        assert_eq!(
            err,
            "--criterion needs <label>=<description>, got \"missing-eq\"; \
             declared labels: trivial, deep; \
             example: --criterion \"trivial=one bounded edit\""
        );
        // A spaced label stays one shell argument inside the example's quotes.
        let spaced = parse_criteria(&["missing-eq".to_string()], &labels(&["one edit", "deep"]))
            .unwrap_err();
        assert!(
            spaced.contains(r#"example: --criterion "one edit=one bounded edit""#),
            "{spaced}"
        );
    }

    #[test]
    fn serve_line_skips_blanks_and_isolates_a_bad_line() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut e = FakeEngine::new(calls);
        assert_eq!(serve_line(&mut e, ""), None);
        assert_eq!(serve_line(&mut e, "   \t "), None);

        // A bad line answers on that line; the caller keeps serving.
        let out = serve_line(&mut e, "not json").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("invalid spec JSON"));

        // A good line carries the full envelope plus `ok`.
        let line =
            r#"{"text":"alpha","labels":["no","yes"],"criteria":{"yes":"alpha","no":"beta"}}"#;
        let out = serve_line(&mut e, line).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["predicted"], json!("yes"));
        assert_eq!(v["marker"], json!("complete"));
        assert_eq!(v["snapshot"]["model"], json!("fake"));
        assert!(out.lines().count() == 1, "one result line per input line");
    }

    #[test]
    fn one_shot_spec_validates_before_any_engine_is_opened() {
        let opts = |text: Option<&str>, labels: &[&str], criteria: &[&str]| ClassifyOptions {
            text: text.map(str::to_string),
            context: Some("the rubric".to_string()),
            labels: labels.iter().map(ToString::to_string).collect(),
            criteria: criteria.iter().map(ToString::to_string).collect(),
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Remote),
            ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            jsonl: false,
            if_warm: false,
            task_intent: false,
            json: false,
        };
        let e = one_shot_spec(&opts(None, &["a", "b"], &[])).unwrap_err();
        assert!(e.contains("needs a text argument"), "{e}");
        let e = one_shot_spec(&opts(Some("t"), &["a"], &[])).unwrap_err();
        assert!(e.contains("at least two labels"), "{e}");
        let e = one_shot_spec(&opts(Some("t"), &["a", "b"], &["no-equals"])).unwrap_err();
        assert!(e.contains("needs <label>=<description>"), "{e}");
        assert!(e.contains("declared labels: a, b"), "{e}");

        let s = one_shot_spec(&opts(Some("t"), &["a", "b"], &["a=desc"])).unwrap();
        assert_eq!(s.text, "t");
        assert_eq!(s.context, "the rubric");
        assert_eq!(s.criteria["a"], "desc");
    }

    #[test]
    fn checked_caps_components_and_preserves_label_and_key_identity() {
        let exact = "é".repeat(TEXT_CAP_CHARS);
        let over = "é".repeat(TEXT_CAP_CHARS + 1);
        let labels = vec!["yes".to_string(), "no".to_string()];
        let exact_spec = Spec::checked(
            exact.clone(),
            exact.clone(),
            labels.clone(),
            BTreeMap::from([("yes".to_string(), exact.clone())]),
        )
        .unwrap();
        assert!(!exact_spec.was_clipped());
        assert_eq!(exact_spec.text, exact);
        assert_eq!(exact_spec.criteria["yes"].chars().count(), TEXT_CAP_CHARS);

        let capped = Spec::checked(
            over.clone(),
            over.clone(),
            labels.clone(),
            BTreeMap::from([("yes".to_string(), over), ("no".to_string(), String::new())]),
        )
        .unwrap();
        assert_eq!(capped.labels, labels);
        assert_eq!(
            capped.criteria.keys().cloned().collect::<Vec<_>>(),
            ["no", "yes"]
        );
        assert_eq!(capped.criteria["no"], "");
        assert_eq!(capped.text.chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.context.chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.criteria["yes"].chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.clipped_fields, ["text", "context", "criteria.yes"]);
    }

    #[test]
    fn checked_bounds_omitted_label_fallback_without_changing_label_identity() {
        let long_label = "x".repeat(TEXT_CAP_CHARS + 1);
        let capped = Spec::checked(
            "state".to_string(),
            String::new(),
            vec![long_label.clone(), "short".to_string()],
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(capped.labels[0], long_label);
        assert_eq!(capped.criteria[&capped.labels[0]].len(), TEXT_CAP_CHARS);
        assert_eq!(
            capped.clipped_fields,
            [format!("label_fallback.{}", capped.labels[0])]
        );
    }

    #[test]
    fn disclosure_changes_only_capped_json_jsonl_and_human_output() {
        let probs = BTreeMap::from([("no".to_string(), 0.25), ("yes".to_string(), 0.75)]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine::new(Arc::clone(&calls));
        let uncapped = spec("t", "", &["no", "yes"], &[]);
        let uncapped_doc = document(&engine, &uncapped, &probs);
        assert_eq!(uncapped_doc["marker"], "complete");
        assert!(uncapped_doc.get("caps").is_none());
        assert!(uncapped_doc.get("warnings").is_none());
        assert_eq!(
            render_probs(&probs, &uncapped),
            "no: 0.250\nyes: 0.750\npredicted: yes\n"
        );

        let capped = spec(&"x".repeat(TEXT_CAP_CHARS + 1), "", &["no", "yes"], &[]);
        let capped_doc = document(&engine, &capped, &probs);
        assert_eq!(capped_doc["marker"], "capped");
        assert_eq!(capped_doc["epistemics"]["confidence"], "capped");
        assert_eq!(capped_doc["caps"][0]["limit"], TEXT_CAP_CHARS);
        assert_eq!(capped_doc["caps"][0]["affected_fields"], json!(["text"]));
        assert_eq!(capped_doc["warnings"][0]["code"], "INPUT_CAPPED");
        assert!(
            capped_doc["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .contains("affected fields: text")
        );
        let human = render_probs(&probs, &capped);
        assert!(human.contains("marker: capped\nconfidence: capped\n"));
        assert!(human.contains("affected fields: text"));

        let mut engine = FakeEngine::new(calls);
        let line = format!(
            r#"{{"text":"{}","labels":["no","yes"],"criteria":{{"yes":"alpha","no":"beta"}}}}"#,
            "x".repeat(TEXT_CAP_CHARS + 1)
        );
        let jsonl: Value = serde_json::from_str(&serve_line(&mut engine, &line).unwrap()).unwrap();
        assert_eq!(jsonl["ok"], true);
        assert_eq!(jsonl["marker"], "capped");
        assert_eq!(jsonl["caps"][0]["affected_fields"], json!(["text"]));
    }

    #[test]
    fn parsed_one_shot_payload_runs_offline() {
        let options = parse_classify(&[
            "pixel",
            "classify",
            "state alpha",
            "--context",
            "the rubric",
            "--label",
            "yes,no",
            "--criterion",
            "yes=alpha",
            "--criterion",
            "no=beta",
        ]);
        assert_eq!(options.text.as_deref(), Some("state alpha"));
        assert_eq!(options.context.as_deref(), Some("the rubric"));
        assert_eq!(options.labels, ["yes", "no"]);
        assert_eq!(options.criteria, ["yes=alpha", "no=beta"]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let mut output = RecordingOutput::default();
        run_with(
            options,
            test_resolve,
            move |_resolved| fake_engine(recorded),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.contains("predicted: yes"));
        assert!(output.documents.is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].text, "state alpha");
        assert_eq!(calls[0].context, "the rubric");
    }

    #[test]
    fn parser_preserves_all_one_shot_arguments_and_json_flag() {
        let options = parse_classify(&[
            "pixel",
            "classify",
            "the state",
            "--context",
            "the rubric",
            "--label",
            "yes,no",
            "--label",
            "maybe",
            "--criterion",
            "yes=allowed",
            "--criterion",
            "maybe=unknown",
            "--json",
        ]);
        assert_eq!(options.text.as_deref(), Some("the state"));
        assert_eq!(options.context.as_deref(), Some("the rubric"));
        assert_eq!(options.labels, ["yes", "no", "maybe"]);
        assert_eq!(options.criteria, ["yes=allowed", "maybe=unknown"]);
        assert!(options.json);
        assert!(!options.jsonl);
    }

    #[test]
    fn parser_accepts_jsonl_with_omitted_text_and_labels() {
        let options = parse_classify(&["pixel", "classify", "--jsonl"]);
        assert!(options.text.is_none());
        assert!(options.labels.is_empty());
        assert!(options.jsonl);
    }

    #[test]
    fn parser_rejects_the_removed_backend_flag() {
        const PARSER_TEST_STACK: usize = 16_777_216;
        std::thread::Builder::new()
            .stack_size(PARSER_TEST_STACK)
            .spawn(move || {
                assert!(
                    crate::Cli::try_parse_from([
                        "pixel",
                        "classify",
                        "--backend",
                        "remote",
                        "--jsonl"
                    ])
                    .is_err()
                );
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn run_with_validates_before_open_and_selects_json_output() {
        let opens = Arc::new(Mutex::new(0usize));
        let opened = Arc::clone(&opens);
        let invalid = ClassifyOptions {
            text: None,
            context: None,
            labels: vec!["yes".to_string(), "no".to_string()],
            criteria: Vec::new(),
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Remote),
            ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            jsonl: false,
            if_warm: false,
            task_intent: false,
            json: false,
        };
        let mut output = RecordingOutput::default();
        let error = run_with(
            invalid,
            test_resolve,
            move |_resolved| {
                *opened.lock().unwrap() += 1;
                fake_engine(Arc::new(Mutex::new(Vec::new())))
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "classify needs a text argument (or --jsonl)");
        assert_eq!(*opens.lock().unwrap(), 0);
        assert!(output.text.is_empty());
        assert!(output.documents.is_empty());

        let options = ClassifyOptions {
            text: Some("alpha".to_string()),
            context: None,
            labels: vec!["yes".to_string(), "no".to_string()],
            criteria: vec!["yes=alpha".to_string(), "no=beta".to_string()],
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Remote),
            ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            jsonl: false,
            if_warm: false,
            task_intent: false,
            json: true,
        };
        run_with(
            options,
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.is_empty());
        assert_eq!(output.documents.len(), 1);
        assert_eq!(output.documents[0]["predicted"], "yes");
        assert!(
            output.documents[0].get("next_ops").is_none(),
            "ops are named only for --task-intent"
        );
    }

    #[test]
    fn task_intent_should_judge_the_built_in_labels_and_name_the_ops_of_the_verdict() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let mut output = RecordingOutput::default();
        run_with(
            parse_classify(&["pixel", "classify", "the login broke", "--task-intent"]),
            test_resolve,
            move |_resolved| fake_engine(recorded),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        let spec = calls.lock().unwrap().remove(0);
        assert_eq!(spec, crate::prompt_intent::spec("the login broke").unwrap());
        // The fake ties every intent label: the first, bugfix, is predicted.
        assert!(
            output.text.contains("predicted: bugfix\n"),
            "{}",
            output.text
        );
        assert!(
            output.text.ends_with(
                "next: pixel plan-rollback \"<problem>\", pixel dig-history --phrase \"<text>\", pixel impact \"<symbol>\"\n"
            ),
            "{}",
            output.text
        );

        let mut output = RecordingOutput::default();
        run_with(
            parse_classify(&[
                "pixel",
                "classify",
                "the login broke",
                "--task-intent",
                "--json",
            ]),
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert_eq!(output.documents[0]["predicted"], "bugfix");
        assert_eq!(
            output.documents[0]["next_ops"],
            json!(crate::prompt_intent::ops_for("bugfix").unwrap())
        );

        let mut output = RecordingOutput::default();
        run_with(
            parse_classify(&["pixel", "classify", "beta", "--label", "yes,no"]),
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.contains("predicted: no\n"), "{}", output.text);
        assert!(!output.text.contains("next:"), "{}", output.text);
    }

    #[test]
    fn resolve_if_warm_should_use_only_an_already_listening_local_engine() {
        use crate::classify_setup::ResolvedEngine;
        let base = "http://127.0.0.1:7777".to_string();
        let resolve = |args: &[&str], stored: Option<&str>, live: bool| {
            let probed = std::cell::RefCell::new(Vec::new());
            let result = resolve_if_warm(
                &parse_classify(args),
                stored.map(str::to_string),
                base.clone(),
                |url| {
                    probed.borrow_mut().push(url.to_string());
                    live
                },
            );
            (result, probed.into_inner())
        };

        let (result, probed) = resolve(
            &["pixel", "classify", "t", "--if-warm"],
            Some("local"),
            true,
        );
        assert!(matches!(result, Ok(ResolvedEngine::Local { base: b }) if b == base));
        assert_eq!(probed, std::slice::from_ref(&base));

        let (result, probed) = resolve(&["pixel", "classify", "t", "--if-warm"], None, false);
        let Err(error) = result else {
            panic!("a cold engine is refused")
        };
        assert_eq!(
            error,
            "not warm: no local classify engine is listening at http://127.0.0.1:7777; --if-warm never starts it (`pixel classify` without --if-warm does)"
        );
        assert_eq!(probed, std::slice::from_ref(&base));

        let (result, probed) = resolve(
            &["pixel", "classify", "t", "--if-warm"],
            Some("remote"),
            true,
        );
        let Err(error) = result else {
            panic!("a stored remote engine is refused")
        };
        assert!(
            error.starts_with("not warm: the stored classify engine is remote"),
            "{error}"
        );
        assert!(probed.is_empty());

        let args = [
            "pixel",
            "classify",
            "t",
            "--if-warm",
            "--engine",
            "ollaya",
            "--ollaya-url",
            "http://127.0.0.1:8888",
        ];
        let (result, probed) = resolve(&args, Some("remote"), true);
        assert!(
            matches!(result, Ok(ResolvedEngine::Local { base: b }) if b == "http://127.0.0.1:8888")
        );
        assert_eq!(probed, ["http://127.0.0.1:8888"]);

        let args = ["pixel", "classify", "t", "--if-warm", "--engine", "remote"];
        let (result, probed) = resolve(&args, Some("local"), true);
        let Err(error) = result else {
            panic!("an explicit remote engine is refused")
        };
        assert_eq!(
            error,
            "--if-warm answers only from the local engine; drop --engine remote"
        );
        assert!(probed.is_empty());
    }

    #[test]
    fn ollaya_config_should_cap_only_the_if_warm_call_at_the_hook_timeout() {
        let base = "http://127.0.0.1:7777".to_string();
        let warm = ollaya_config(base.clone(), true);
        assert_eq!(warm.base, base);
        assert_eq!(warm.timeout, crate::prompt_intent::HOOK_CALL_TIMEOUT);
        assert_eq!(
            ollaya_config(base, false).timeout,
            crate::decide_ollaya::OllayaConfig::default().timeout,
            "a plain classify keeps the interactive cap"
        );
    }

    #[test]
    fn run_with_jsonl_opens_once_and_continues_across_line_errors() {
        let input = [
            "",
            "not json",
            r#"{"text":"alpha","context":"rubric","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
            r#"{"text":"beta","context":null,"labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
        ]
        .join("\n");
        let opens = Arc::new(Mutex::new(0usize));
        let opened = Arc::clone(&opens);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let options = ClassifyOptions {
            text: None,
            context: None,
            labels: Vec::new(),
            criteria: Vec::new(),
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Remote),
            ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            jsonl: true,
            if_warm: false,
            task_intent: false,
            json: false,
        };
        let mut output = RecordingOutput::default();
        run_with(
            options,
            test_resolve,
            move |_resolved| {
                *opened.lock().unwrap() += 1;
                fake_engine(recorded)
            },
            Cursor::new(input),
            &mut output,
        )
        .unwrap();
        assert_eq!(*opens.lock().unwrap(), 1);
        assert!(output.text.ends_with('\n'));
        let lines: Vec<Value> = output
            .text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["predicted"], "yes");
        assert_eq!(lines[2]["ok"], true);
        assert_eq!(lines[2]["predicted"], "no");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].context, "rubric");
    }

    #[test]
    fn bare_classify_runs_the_default_battery_on_the_local_engine() {
        let battery_calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&battery_calls);
        let answers = json!({
            "intent": {
                "type": "choice",
                "choice": "other",
                "confidence": 0.8,
                "probabilities": {"refund": 0.1, "other": 0.9}
            },
            "is_urgent": {"type": "noul", "noul": 0.9},
            "frustration": {
                "type": "score",
                "score": 2.4,
                "confidence": 0.7,
                "legend": {"0": "calm", "1": "civil", "2": "annoyed", "3": "angry"},
                "probabilities": {"0": 0.1, "1": 0.1, "2": 0.3, "3": 0.5}
            }
        });
        let mut output = RecordingOutput::default();
        run_with(
            ClassifyOptions {
                text: Some("refund me now".to_string()),
                context: Some("customer message".to_string()),
                labels: Vec::new(),
                criteria: Vec::new(),
                remote_preset: Some(crate::decide_remote::Preset::Openrouter),
                remote_model: None,
                engine: Some(EngineChoice::Ollaya),
                ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
                jsonl: false,
                if_warm: false,
                task_intent: false,
                json: false,
            },
            test_resolve,
            move |_resolved| {
                let mut engine = FakeEngine::new(Arc::new(Mutex::new(Vec::new())));
                engine.battery_calls = Arc::clone(&recorded);
                engine.battery_answer = Some(answers.clone());
                Ok(Box::new(engine) as _)
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert_eq!(
            battery_calls.lock().unwrap().as_slice(),
            &["customer message\n\nrefund me now"]
        );
        assert!(
            output.text.contains("intent: other (0.900)"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("is_urgent: yes (0.900)"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("frustration: 2.40/3 annoyed"),
            "{}",
            output.text
        );
    }

    #[test]
    fn run_with_uses_the_injected_resolver_for_the_bare_battery() {
        let mut output = RecordingOutput::default();
        run_with(
            ClassifyOptions {
                text: Some("refund me now".to_string()),
                context: None,
                labels: Vec::new(),
                criteria: Vec::new(),
                remote_preset: Some(crate::decide_remote::Preset::Openrouter),
                remote_model: None,
                engine: Some(EngineChoice::Remote),
                ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
                jsonl: false,
                if_warm: false,
                task_intent: false,
                json: false,
            },
            |_| {
                Ok(crate::classify_setup::ResolvedEngine::Local {
                    base: "http://127.0.0.1:11435".to_string(),
                })
            },
            |_resolved| {
                let mut engine = FakeEngine::new(Arc::new(Mutex::new(Vec::new())));
                engine.battery_answer = Some(json!({
                    "intent": {"type": "choice", "choice": "other", "confidence": 0.9}
                }));
                Ok(Box::new(engine) as _)
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.contains("intent: other"));
    }

    #[test]
    fn bare_classify_on_the_remote_engine_errors_before_opening_it() {
        let opens = Arc::new(Mutex::new(0usize));
        let opened = Arc::clone(&opens);
        let mut output = RecordingOutput::default();
        let error = run_with(
            ClassifyOptions {
                text: Some("alpha".to_string()),
                context: None,
                labels: Vec::new(),
                criteria: Vec::new(),
                remote_preset: Some(crate::decide_remote::Preset::Openrouter),
                remote_model: None,
                engine: Some(EngineChoice::Remote),
                ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
                jsonl: false,
                if_warm: false,
                task_intent: false,
                json: false,
            },
            test_resolve,
            move |_resolved| {
                *opened.lock().unwrap() += 1;
                fake_engine(Arc::new(Mutex::new(Vec::new())))
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert!(
            error.contains("--label") && error.contains("Ollaya"),
            "{error}"
        );
        assert_eq!(*opens.lock().unwrap(), 0);
    }

    #[test]
    fn jsonl_decide_error_is_a_line_error_and_next_request_continues() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let input = [
            r#"{"text":"alpha","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
            r#"{"text":"beta","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
        ]
        .join("\n");
        let mut output = RecordingOutput::default();
        let mut engine = FakeEngine {
            calls,
            battery_calls: Arc::new(Mutex::new(Vec::new())),
            battery_answer: None,
            fail_next: true,
            snapshot_extra: None,
            custom_basis: None,
        };
        serve_jsonl(Cursor::new(input), &mut engine, &mut output).unwrap();
        let lines: Vec<Value> = output
            .text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[0]["error"], "decision failed");
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["predicted"], "no");
    }

    #[test]
    fn run_with_propagates_opener_reader_and_output_failures() {
        let jsonl_options = || ClassifyOptions {
            text: None,
            context: None,
            labels: Vec::new(),
            criteria: Vec::new(),
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Remote),
            ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
            jsonl: true,
            if_warm: false,
            task_intent: false,
            json: false,
        };
        let mut output = RecordingOutput::default();
        let error = run_with(
            jsonl_options(),
            test_resolve,
            |_resolved| Err("open failed".to_string()),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "open failed");

        let error = run_with(
            jsonl_options(),
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            FailingReader,
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "stdin read: reader failed");

        output.fail_text = true;
        let line =
            r#"{"text":"alpha","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#;
        let error = run_with(
            jsonl_options(),
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            Cursor::new(line),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "text output failed");

        let mut output = RecordingOutput {
            fail_document: true,
            ..RecordingOutput::default()
        };
        let error = run_with(
            ClassifyOptions {
                text: Some("alpha".to_string()),
                context: None,
                labels: vec!["yes".to_string(), "no".to_string()],
                criteria: vec!["yes=alpha".to_string(), "no=beta".to_string()],
                remote_preset: Some(crate::decide_remote::Preset::Openrouter),
                remote_model: None,
                engine: Some(EngineChoice::Remote),
                ollaya_url: crate::decide_ollaya::DEFAULT_BASE.to_string(),
                jsonl: false,
                if_warm: false,
                task_intent: false,
                json: true,
            },
            test_resolve,
            |_resolved| fake_engine(Arc::new(Mutex::new(Vec::new()))),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "document output failed");
    }

    #[test]
    fn engine_flag_defaults_to_remote_and_selects_ollaya_when_asked() {
        const PARSER_TEST_STACK: usize = 16_777_216;
        std::thread::Builder::new()
            .stack_size(PARSER_TEST_STACK)
            .spawn(move || {
                let default =
                    crate::Cli::try_parse_from(["pixel", "classify", "state", "--label", "a,b"])
                        .unwrap();
                let crate::Command::Classify(options) = default.command else {
                    panic!("not classify");
                };
                // No flag = None: the engine then comes from the stored
                // preference (auto: probe local, fall back to remote).
                assert_eq!(options.engine, None);
                assert_eq!(options.ollaya_url, crate::decide_ollaya::DEFAULT_BASE);
                let ollaya = crate::Cli::try_parse_from([
                    "pixel",
                    "classify",
                    "state",
                    "--label",
                    "a,b",
                    "--engine",
                    "ollaya",
                    "--ollaya-url",
                    "http://127.0.0.1:9999",
                ])
                .unwrap();
                let crate::Command::Classify(options) = ollaya.command else {
                    panic!("not classify");
                };
                assert_eq!(options.engine, Some(EngineChoice::Ollaya));
                assert_eq!(options.ollaya_url, "http://127.0.0.1:9999");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn run_with_ollaya_engine_opens_the_local_server_adapter() {
        let options = ClassifyOptions {
            text: Some("alpha".to_string()),
            context: None,
            labels: vec!["yes".to_string(), "no".to_string()],
            criteria: Vec::new(),
            remote_preset: Some(crate::decide_remote::Preset::Openrouter),
            remote_model: None,
            engine: Some(EngineChoice::Ollaya),
            ollaya_url: "http://127.0.0.1:9".to_string(),
            jsonl: false,
            if_warm: false,
            task_intent: false,
            json: false,
        };
        // A dead server address fails the decision with a transport error —
        // proving the ollaya engine was opened and consulted. The opened
        // config pins the dead base: the test must not depend on whether a
        // real daemon happens to answer the default address.
        let error = run_with(
            options,
            test_resolve,
            |_resolved| {
                Ok(Box::new(crate::decide_ollaya::Ollaya::open(
                    crate::decide_ollaya::OllayaConfig {
                        base: "http://127.0.0.1:9".to_string(),
                        ..Default::default()
                    },
                )) as _)
            },
            Cursor::new(Vec::<u8>::new()),
            &mut RecordingOutput::default(),
        )
        .unwrap_err();
        assert!(error.contains("ollaya"), "{error}");
    }

    #[test]
    fn document_merges_engine_snapshot_extra_and_basis() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut engine = FakeEngine::new(Arc::clone(&calls));
        engine.snapshot_extra = Some(json!({
            "unknown_probability": 0.05,
            "abstained": false,
            "confidence": 0.8,
        }));
        engine.custom_basis = Some("ollaya native typed-choice probabilities".to_string());
        let s = spec("t", "", &["yes", "no"], &[]);
        let probs = BTreeMap::from([("yes".to_string(), 0.9f64), ("no".to_string(), 0.1f64)]);
        let doc = document(&engine, &s, &probs);
        assert_eq!(doc["snapshot"]["unknown_probability"], json!(0.05));
        assert_eq!(doc["snapshot"]["abstained"], json!(false));
        assert_eq!(doc["snapshot"]["confidence"], json!(0.8));
        assert!(
            doc["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .starts_with("ollaya")
        );
    }

    #[test]
    fn render_probs_lists_every_label_then_the_argmax() {
        let probs = BTreeMap::from([("no".to_string(), 0.25f64), ("yes".to_string(), 0.75f64)]);
        let spec = spec("t", "", &["no", "yes"], &[]);
        assert_eq!(
            render_probs(&probs, &spec),
            "no: 0.250\nyes: 0.750\npredicted: yes\n"
        );
    }
}
