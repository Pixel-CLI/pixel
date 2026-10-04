// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Explicit controlled trials backed by the existing `eval/` scorer and gate.
//!
//! The bundled Bun backend uses Docker private storage and a filtering inference
//! gateway. This module never runs trials implicitly while recording or replaying.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EVALUATION_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum EvaluationError {
    #[error("invalid evaluation suite: {0}")]
    Invalid(String),
    #[error("controlled evaluation failed: {0}")]
    Runner(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInput {
    pub path: String,
    pub contents: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRunner {
    pub host: String,
    pub version: String,
    pub version_argv: Vec<String>,
    pub argv: Vec<String>,
    pub model: String,
    pub model_config: BTreeMap<String, Value>,
    pub permissions: BTreeMap<String, Value>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<FileInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldoutVerifier {
    pub argv: Vec<String>,
    pub files: Vec<FileInput>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationCase {
    pub id: String,
    pub prompt: String,
    pub source_files: Vec<FileInput>,
    pub contract: Value,
    /// Same `must`/`never` rubric consumed by the existing arena's `score.py`.
    pub rubric: Value,
    pub verifier: HeldoutVerifier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvaluationNetwork {
    Offline,
    ModelGateway {
        endpoint: String,
        request_path: String,
        model: String,
        credential_env: String,
        auth_header: String,
        auth_prefix: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationSuite {
    pub schema_version: u32,
    pub id: String,
    pub image: String,
    pub gateway_image: String,
    pub output_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_seed: Option<String>,
    pub repetitions: u32,
    pub timeout_ms: u64,
    pub max_interactions: u64,
    pub runners: Vec<EvaluationRunner>,
    pub arms: Vec<String>,
    pub network: EvaluationNetwork,
    pub cases: Vec<EvaluationCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub suite_id: String,
    pub output_dir: PathBuf,
    pub order_seed: String,
    pub order_algorithm: String,
    pub rows: Vec<Value>,
    pub gates: Vec<Value>,
    pub all_passed: bool,
}

/// Execute only an explicitly supplied suite; the runner validates every bound
/// and refuses unpinned images or unrestricted networking before creating trials.
pub fn evaluate(suite: &EvaluationSuite) -> Result<EvaluationReport, EvaluationError> {
    if suite.schema_version != EVALUATION_VERSION
        || suite.timeout_ms == 0
        || suite.max_interactions == 0
    {
        return Err(EvaluationError::Invalid(
            "version and positive timeout/interaction budget are required".into(),
        ));
    }
    let scratch = tempfile::tempdir()?;
    let script = scratch.path().join("controlled.ts");
    std::fs::write(&script, include_str!("../../../eval/controlled.ts"))?;
    let mut command = Command::new("bun");
    command
        .arg(&script)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in [
        "HOME",
        "PATH",
        "TMPDIR",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_CONFIG",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let EvaluationNetwork::ModelGateway { credential_env, .. } = &suite.network {
        if credential_env == "ANTHROPIC_API_KEY"
            || !credential_env
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || credential_env.is_empty()
        {
            return Err(EvaluationError::Invalid(
                "use a named registered OAuth/provider credential".into(),
            ));
        }
        let value = std::env::var_os(credential_env).ok_or_else(|| {
            EvaluationError::Invalid(format!("missing registered credential: {credential_env}"))
        })?;
        command.env(credential_env, value);
    }
    let mut child = command.spawn()?;
    let input = serde_json::json!({"suite": suite, "resources": {
        "scorer": include_str!("../../../eval/score.py"),
        "gate": include_str!("../../../eval/gate.py")
    }});
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&serde_json::to_vec(&input)?)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(EvaluationError::Runner(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn suite() -> EvaluationSuite {
        serde_json::from_value(serde_json::json!({"schema_version":1,"id":"fixture","image":"fixture","gateway_image":"fixture","output_dir":"results","repetitions":1,"timeout_ms":1000,"max_interactions":10,"runners":[],"arms":[],"network":{"kind":"offline"},"cases":[]})).unwrap()
    }

    fn gateway(name: &str) -> EvaluationNetwork {
        EvaluationNetwork::ModelGateway {
            endpoint: "https://model.invalid/v1/messages".into(),
            request_path: "/v1/messages".into(),
            model: "fixture".into(),
            credential_env: name.into(),
            auth_header: "authorization".into(),
            auth_prefix: "Bearer ".into(),
        }
    }

    #[test]
    fn evaluator_rejects_each_invalid_bound_and_credential_name_before_launch() {
        for field in ["version", "timeout", "interactions"] {
            let mut value = suite();
            match field {
                "version" => value.schema_version = 2,
                "timeout" => value.timeout_ms = 0,
                "interactions" => value.max_interactions = 0,
                _ => unreachable!(),
            }
            assert!(
                matches!(evaluate(&value), Err(EvaluationError::Invalid(message)) if message == "version and positive timeout/interaction budget are required"),
                "{field}"
            );
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "",
            "lowercase",
            "BAD-NAME",
            "BAD NAME",
            "É",
        ] {
            let mut value = suite();
            value.network = gateway(name);
            assert!(
                matches!(evaluate(&value), Err(EvaluationError::Invalid(message)) if message == "use a named registered OAuth/provider credential"),
                "{name}"
            );
        }
        let mut value = suite();
        let name = format!("PIXEL_EVALUATION_ABSENT_{}", std::process::id());
        value.network = gateway(&name);
        assert!(
            matches!(evaluate(&value), Err(EvaluationError::Invalid(message)) if message == format!("missing registered credential: {name}"))
        );
    }

    #[test]
    fn evaluator_runs_frozen_subprocess_protocol_and_rejects_failure_or_bad_json() {
        for scenario in ["success", "failure", "bad-json", "gateway"] {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join("bun");
            let response = serde_json::json!({"schema_version":1,"suite_id":"fixture","output_dir":"results","order_seed":"fixture-seed","order_algorithm":"fake-v1","rows":[{"fixture":true}],"gates":[{"passed":false}],"all_passed":false}).to_string();
            let stdout = if scenario == "bad-json" {
                "not-json"
            } else {
                &response
            };
            let exit = if scenario == "failure" { 7 } else { 0 };
            std::fs::write(&binary, format!("#!/bin/sh\n/bin/cat > \"$HOME/input.json\"\n/usr/bin/env > \"$HOME/runner.env\"\nprintf '%s' '{stdout}'\nprintf '%s' 'fixture stderr' >&2\nexit {exit}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "evaluation::tests::evaluation_subprocess_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", directory.path())
                .env("PATH", directory.path())
                .env("PIXEL_TEST_EVAL_SCENARIO", scenario)
                .env("PIXEL_EVAL_TEST_42", "fake-oauth-value")
                .env("PIXEL_TEST_EVAL_UNRELATED_SECRET", "must-not-reach-runner")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{scenario}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let input: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join("input.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(input["suite"]["id"], "fixture");
            assert_eq!(input["suite"]["timeout_ms"], 1000);
            assert_eq!(input["suite"]["max_interactions"], 10);
            assert_eq!(
                input["resources"]["scorer"],
                include_str!("../../../eval/score.py")
            );
            assert_eq!(
                input["resources"]["gate"],
                include_str!("../../../eval/gate.py")
            );
            let environment = std::fs::read_to_string(directory.path().join("runner.env")).unwrap();
            assert!(!environment.contains("must-not-reach-runner"));
            assert!(!environment.contains("PIXEL_TEST_EVAL_SCENARIO"));
            assert_eq!(
                environment.contains("PIXEL_EVAL_TEST_42=fake-oauth-value"),
                scenario == "gateway"
            );
        }
    }

    #[test]
    #[ignore = "invoked by subprocess protocol test with an isolated HOME and fake Bun"]
    fn evaluation_subprocess_child() {
        let scenario = std::env::var("PIXEL_TEST_EVAL_SCENARIO").unwrap();
        let mut value = suite();
        if scenario == "gateway" {
            value.network = gateway("PIXEL_EVAL_TEST_42");
        }
        let result = evaluate(&value);
        match scenario.as_str() {
            "failure" => assert!(
                matches!(result, Err(EvaluationError::Runner(message)) if message == "fixture stderr")
            ),
            "bad-json" => assert!(matches!(result, Err(EvaluationError::Json(_)))),
            "success" | "gateway" => {
                let report = result.unwrap();
                assert_eq!(report.schema_version, 1);
                assert_eq!(report.suite_id, "fixture");
                assert_eq!(report.output_dir, PathBuf::from("results"));
                assert_eq!(report.order_seed, "fixture-seed");
                assert_eq!(report.order_algorithm, "fake-v1");
                assert_eq!(report.rows, vec![serde_json::json!({"fixture":true})]);
                assert_eq!(report.gates, vec![serde_json::json!({"passed":false})]);
                assert!(!report.all_passed);
            }
            _ => panic!("unexpected test scenario"),
        }
    }

    #[test]
    fn suite_requires_explicit_timeout_before_starting_a_process() {
        let suite = EvaluationSuite {
            schema_version: 1,
            id: "test".into(),
            image: String::new(),
            gateway_image: String::new(),
            output_dir: PathBuf::new(),
            order_seed: None,
            repetitions: 1,
            timeout_ms: 0,
            max_interactions: 1,
            runners: Vec::new(),
            arms: Vec::new(),
            network: EvaluationNetwork::Offline,
            cases: Vec::new(),
        };
        assert!(
            matches!(evaluate(&suite), Err(EvaluationError::Invalid(message)) if message.contains("timeout/interaction"))
        );
    }

    #[test]
    fn suite_roundtrip_keeps_network_and_frozen_runtime_inputs() {
        let value = serde_json::json!({"schema_version":1,"id":"suite","image":"image@sha256:abc","gateway_image":"bun@sha256:def","output_dir":"results","repetitions":2,"timeout_ms":1000,"max_interactions":10,"runners":[{"host":"pi","version":"0.87.1","version_argv":["pi","--version"],"argv":["pi","--print","--mode","json","{prompt}"],"model":"fixed","model_config":{"thinking":"low"},"permissions":{"mode":"private-container"},"environment":{},"files":[]}],"arms":["retrieval","gates","gates_classifier"],"network":{"kind":"model_gateway","endpoint":"https://model.example/v1/messages","request_path":"/v1/messages","model":"fixed","credential_env":"CLAUDE_CODE_OAUTH_TOKEN","auth_header":"authorization","auth_prefix":"Bearer "},"cases":[]});
        let suite: EvaluationSuite = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(suite).unwrap(), value);
    }
}
