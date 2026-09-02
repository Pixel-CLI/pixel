//! Flow document types — the on-disk schema for a saved browser flow.

use serde::{Deserialize, Serialize};

/// A variable that can be substituted into replay commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowVar {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// One step in a flow. `action` is the agent-browser verb
/// (`open`, `snapshot`, `click`, `fill`, `type`, `select`, `press`,
/// `wait`, `conditional`, `switch_tab`). Fields are optional except
/// `action` and `rationale` — only the relevant ones are set per action type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowStep {
    pub action: String,
    /// Why this step exists — the human/agent rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// URL for `open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// CSS-selector-ish description of the element (NOT a real @eN ref —
    /// refs are ephemeral). The agent re-snapshots and resolves the actual
    /// ref at replay time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_hint: Option<String>,
    /// Literal value to fill/type. Mutually exclusive with `value_var`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Name of a flow var whose value is substituted at replay time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_var: Option<String>,
    /// Key to press (e.g. "Enter", "Tab").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Wait condition: "load" | "selector" | "text" | "timeout".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<String>,
    /// Wait target (selector, text, or ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_target: Option<String>,
    /// Human-readable condition for `conditional` steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Steps to run if the condition is true.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub then: Vec<FlowStep>,
    /// Steps to run if the condition is false.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub otherwise: Vec<FlowStep>,
    /// Which tab to operate on — a URL substring or title substring that
    /// identifies the tab this step targets. When set, the replay output
    /// includes a `switch_tab` command before the step's action so the
    /// agent focuses the correct tab even if another tab was active.
    /// Example: "github.com/login/device" matches any tab whose URL
    /// contains that substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// Self-fix strategy when this step fails. A human-readable instruction
    /// the agent follows if the step's action doesn't produce the expected
    /// result (element not found, page didn't load, wrong state).
    /// Example: "re-snapshot and look for a 'Continue' button instead".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<String>,
    /// Retry count for this step before applying `on_failure`. Default 1.
    #[serde(default = "default_retries")]
    pub max_retries: u32,
}

fn default_retries() -> u32 {
    1
}

/// A saved browser flow — a deterministic, replayable path through a web
/// interaction (auth, config, OAuth, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub name: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Default tab for all steps in this flow — a URL substring or title
    /// substring. Steps with their own `tab` field override this. When set,
    /// every replayed step is prefixed with a `switch_tab` command so the
    /// agent always operates on the correct tab even if another tab was
    /// opened or focused mid-flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// URL substrings that indicate success (e.g. "claude.ai/new"). The
    /// agent checks the active tab URL after the flow completes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_url_contains: Vec<String>,
    /// URL substrings that indicate failure even if success_url_contains
    /// matches (e.g. "/login" means not actually logged in).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_url_excludes: Vec<String>,
    /// Keywords that indicate an MFA/passkey/2FA prompt appeared — the
    /// agent should hand off to the user (cannot be automated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mfa_keywords: Vec<String>,
    /// URL substrings of stale tabs to close before starting the flow
    /// (cleanup, prevents confusion with leftover auth tabs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_tab_cleanup: Vec<String>,
    /// Preconditions to check before starting (file existence, etc.).
    /// Each entry is a human-readable instruction for the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<FlowVar>,
    pub steps: Vec<FlowStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_signal: Option<String>,
    pub created_unix: i64,
    pub revised_unix: i64,
    #[serde(default = "default_revision")]
    pub revision: u32,
    #[serde(default)]
    pub proven: bool,
}

fn default_revision() -> u32 {
    1
}

impl Flow {
    /// Validate the flow document. Returns an error message for invalid
    /// flows so bad data is caught at load/save time, not at replay time.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("flow name must not be empty".into());
        }
        if self.steps.is_empty() {
            return Err("flow must have at least one step".into());
        }
        // Check var references in value_var fields exist.
        let var_names: Vec<&str> = self.vars.iter().map(|v| v.name.as_str()).collect();
        for step in &self.steps {
            check_var_refs(step, &var_names)?;
        }
        Ok(())
    }
}

fn check_var_refs(step: &FlowStep, var_names: &[&str]) -> Result<(), String> {
    if let Some(ref v) = step.value_var
        && !var_names.contains(&v.as_str())
    {
        return Err(format!(
            "step references value_var '{}' which is not declared in vars",
            v
        ));
    }
    for sub in &step.then {
        check_var_refs(sub, var_names)?;
    }
    for sub in &step.otherwise {
        check_var_refs(sub, var_names)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_flow(name: &str, steps: Vec<FlowStep>) -> Flow {
        Flow {
            name: name.into(),
            title: "Test".into(),
            description: "Test flow".into(),
            tags: vec![],
            url: None,
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps,
            success_signal: None,
            created_unix: 1000,
            revised_unix: 1000,
            revision: 1,
            proven: false,
        }
    }

    #[test]
    fn empty_name_rejected() {
        let flow = make_flow("", vec![FlowStep {
            action: "open".into(),
            ..Default::default()
        }]);
        assert!(flow.validate().is_err());
    }

    #[test]
    fn empty_steps_rejected() {
        let flow = make_flow("test", vec![]);
        assert!(flow.validate().is_err());
    }

    #[test]
    fn valid_flow_passes() {
        let flow = make_flow(
            "test",
            vec![
                FlowStep {
                    action: "open".into(),
                    url: Some("https://example.com".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "snapshot".into(),
                    ..Default::default()
                },
            ],
        );
        assert!(flow.validate().is_ok());
    }

    #[test]
    fn undeclared_var_ref_rejected() {
        let flow = make_flow(
            "test",
            vec![FlowStep {
                action: "fill".into(),
                value_var: Some("code".into()),
                ..Default::default()
            }],
        );
        assert!(flow.validate().is_err());
    }

    #[test]
    fn declared_var_ref_passes() {
        let mut flow = make_flow(
            "test",
            vec![FlowStep {
                action: "fill".into(),
                value_var: Some("code".into()),
                ..Default::default()
            }],
        );
        flow.vars.push(FlowVar {
            name: "code".into(),
            description: "The code".into(),
            required: true,
            default: None,
        });
        assert!(flow.validate().is_ok());
    }

    #[test]
    fn flow_round_trips_json() {
        let flow = make_flow(
            "github-auth",
            vec![
                FlowStep {
                    action: "open".into(),
                    url: Some("https://github.com/login/device".into()),
                    rationale: Some("Device code entry".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "fill".into(),
                    ref_hint: Some("input[type=text]".into()),
                    value_var: Some("user_code".into()),
                    rationale: Some("Paste the device code".into()),
                    ..Default::default()
                },
            ],
        );
        let json = serde_json::to_string_pretty(&flow).unwrap();
        let back: Flow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "github-auth");
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.steps[1].value_var.as_deref(), Some("user_code"));
    }
}
