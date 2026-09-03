//! Replay engine — emit ready-to-run agent-browser commands from a flow.

use std::collections::HashMap;

use crate::types::{Flow, FlowStep};

/// Render a flow as a sequence of agent-browser commands with rationale
/// comments and variable substitution. Returns the text for the agent to
/// read and execute (or for `--dry-run` display).
///
/// `vars` is a map of `key=value` substitutions. Missing required vars
/// produce an error. Missing optional vars fall back to their default, or
/// to a placeholder `{{var}}` if no default.
pub fn replay(flow: &Flow, vars: &HashMap<String, String>) -> Result<String, String> {
    // Validate required vars are present.
    for v in &flow.vars {
        if v.required && !vars.contains_key(&v.name) && v.default.is_none() {
            return Err(format!(
                "missing required variable '{}' for flow '{}'",
                v.name, flow.name
            ));
        }
    }
    let mut out = String::new();
    out.push_str(&format!(
        "# Flow: {} ({})\n",
        flow.name, flow.title
    ));
    if !flow.description.is_empty() {
        out.push_str(&format!("# {}\n", flow.description));
    }
    out.push_str(&format!(
        "# Steps: {} | Revision: {} | Proven: {}\n",
        flow.steps.len(),
        flow.revision,
        if flow.proven { "yes" } else { "no" }
    ));
    out.push('\n');

    // Preconditions — check before starting.
    if !flow.preconditions.is_empty() {
        out.push_str("# Preconditions (verify before starting):\n");
        for pre in &flow.preconditions {
            out.push_str(&format!("#   - {}\n", substitute(pre, vars)));
        }
        out.push('\n');
    }

    // Stale tab cleanup — close leftover auth tabs.
    if !flow.stale_tab_cleanup.is_empty() {
        out.push_str("# Stale tab cleanup — close leftover tabs matching:\n");
        for pattern in &flow.stale_tab_cleanup {
            out.push_str(&format!(
                "#   agent-browser --session comet tab list   # find tabs matching '{}'\n",
                substitute(pattern, vars)
            ));
            out.push_str(&format!(
                "#   agent-browser --session comet tab close <id>   # close each matching tab\n"
            ));
        }
        out.push('\n');
    }

    // Default tab — if set, emit a switch_tab at the start.
    if let Some(ref tab) = flow.tab {
        out.push_str(&format!(
            "# Focus the flow's tab: {}\n",
            substitute(tab, vars)
        ));
        out.push_str(&format!(
            "agent-browser --session comet tab list   # find the tab matching '{}'\n",
            substitute(tab, vars)
        ));
        out.push_str(&format!(
            "agent-browser --session comet tab <id>   # switch to it\n\n"
        ));
    }

    for (i, step) in flow.steps.iter().enumerate() {
        render_step(&mut out, step, i + 1, vars, flow.tab.as_deref(), 0);
    }

    // Success signal (text-based).
    if let Some(ref signal) = flow.success_signal {
        out.push_str(&format!(
            "\n# Success signal: {}\n",
            substitute(signal, vars)
        ));
    }

    // Success URL checks.
    if !flow.success_url_contains.is_empty() {
        out.push_str("# Success URL check — active tab URL should contain one of:\n");
        for u in &flow.success_url_contains {
            out.push_str(&format!("#   - {}\n", substitute(u, vars)));
        }
        out.push_str("agent-browser --session comet snapshot -i   # verify URL\n");
    }
    if !flow.success_url_excludes.is_empty() {
        out.push_str("# Success URL exclusion — URL should NOT contain:\n");
        for u in &flow.success_url_excludes {
            out.push_str(&format!("#   - {}\n", substitute(u, vars)));
        }
    }

    // MFA keywords — hand off to user if detected.
    if !flow.mfa_keywords.is_empty() {
        out.push_str("\n# MFA detection — if the snapshot contains any of these keywords,\n");
        out.push_str("# hand off to the user (cannot be automated):\n");
        for kw in &flow.mfa_keywords {
            out.push_str(&format!("#   - {}\n", kw));
        }
    }

    Ok(out)
}

fn render_step(
    out: &mut String,
    step: &FlowStep,
    num: usize,
    vars: &HashMap<String, String>,
    flow_tab: Option<&str>,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    if let Some(r) = &step.rationale {
        out.push_str(&format!("{}# Step {}: {}\n", indent, num, substitute(r, vars)));
    } else {
        out.push_str(&format!("{}# Step {}\n", indent, num));
    }

    // Per-step tab switching — if this step has a `tab` field, emit a
    // switch_tab command before the action. Falls back to the flow-level
    // default tab. Skip if the action itself is `switch_tab` (it emits its
    // own tab commands).
    let effective_tab = step.tab.as_deref().or(flow_tab);
    if step.action != "switch_tab" {
        if let Some(tab) = effective_tab {
            if step.tab.is_some() {
                out.push_str(&format!(
                    "{}agent-browser --session comet tab list   # switch to tab matching '{}'\n",
                    indent,
                    substitute(tab, vars)
                ));
                out.push_str(&format!(
                    "{}agent-browser --session comet tab <id>\n",
                    indent
                ));
            }
        }
    }

    match step.action.as_str() {
        "open" => {
            if let Some(ref url) = step.url {
                out.push_str(&format!(
                    "{}agent-browser --session comet open \"{}\"\n",
                    indent,
                    substitute(url, vars)
                ));
            }
        }
        "snapshot" => {
            out.push_str(&format!("{}agent-browser --session comet snapshot -i\n", indent));
        }
        "click" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("element"),
                vars,
            );
            out.push_str(&format!(
                "{}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {}\n",
                indent, target
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet click @eN      # @eN = {}\n",
                indent, target
            ));
        }
        "fill" | "type" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("input"),
                vars,
            );
            let value = resolve_value(step, vars);
            out.push_str(&format!(
                "{}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {}\n",
                indent, target
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet {} @eN \"{}\"   # @eN = {}\n",
                indent, step.action, value, target
            ));
        }
        "select" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("select"),
                vars,
            );
            let value = resolve_value(step, vars);
            out.push_str(&format!(
                "{}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {}\n",
                indent, target
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet select @eN \"{}\"   # @eN = {}\n",
                indent, value, target
            ));
        }
        "press" => {
            let key = step
                .key
                .as_deref()
                .unwrap_or("Enter");
            out.push_str(&format!(
                "{}agent-browser --session comet press {}\n",
                indent,
                substitute(key, vars)
            ));
        }
        "wait" => {
            let wait = step.wait.as_deref().unwrap_or("load");
            let target = step.wait_target.as_deref().unwrap_or("");
            let arg = if target.is_empty() {
                format!("--{}", wait)
            } else {
                format!("--{} \"{}\"", wait, substitute(target, vars))
            };
            out.push_str(&format!(
                "{}agent-browser --session comet wait {}\n",
                indent, arg
            ));
        }
        "conditional" => {
            let cond = substitute(
                step.condition.as_deref().unwrap_or("condition"),
                vars,
            );
            out.push_str(&format!("{}# CONDITIONAL: if {}\n", indent, cond));
            if !step.then.is_empty() {
                out.push_str(&format!("{}# → THEN:\n", indent));
                for (i, sub) in step.then.iter().enumerate() {
                    render_step(out, sub, i + 1, vars, flow_tab, depth + 1);
                }
            }
            if !step.otherwise.is_empty() {
                out.push_str(&format!("{}# → ELSE:\n", indent));
                for (i, sub) in step.otherwise.iter().enumerate() {
                    render_step(out, sub, i + 1, vars, flow_tab, depth + 1);
                }
            }
        }
        "switch_tab" => {
            let tab = substitute(
                step.tab.as_deref().unwrap_or("tab"),
                vars,
            );
            out.push_str(&format!(
                "{}agent-browser --session comet tab list   # find tab matching '{}'\n",
                indent, tab
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet tab <id>   # switch to it\n",
                indent
            ));
        }
        _ => {
            // Unknown action — emit as a comment for the agent to interpret.
            out.push_str(&format!(
                "{}# action: {} (unknown — interpret manually)\n",
                indent, step.action
            ));
        }
    }

    // On-failure self-fix instruction.
    if let Some(ref fix) = step.on_failure {
        out.push_str(&format!(
            "{}# ON FAILURE (max {} retries): {}\n",
            indent, step.max_retries, substitute(fix, vars)
        ));
    }

    out.push('\n');
}

/// Resolve a step's value: `value_var` takes precedence, then `value`,
/// then empty string.
fn resolve_value(step: &FlowStep, vars: &HashMap<String, String>) -> String {
    if let Some(ref var_name) = step.value_var {
        if let Some(v) = vars.get(var_name) {
            return v.clone();
        }
        // Fall back to default or placeholder.
        return format!("{{{{{}}}}}", var_name);
    }
    if let Some(ref v) = step.value {
        return substitute(v, vars);
    }
    String::new()
}

/// Substitute `{{var}}` templates in a string.
fn substitute(s: &str, vars: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (k, v) in vars {
        let placeholder = format!("{{{{{}}}}}", k);
        result = result.replace(&placeholder, v);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlowStep, FlowVar};

    fn make_flow(steps: Vec<FlowStep>, vars: Vec<FlowVar>) -> Flow {
        Flow {
            name: "test".into(),
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
            vars,
            steps,
            success_signal: None,
            created_unix: 1000,
            revised_unix: 1000,
            revision: 1,
            proven: true,
        }
    }

    #[test]
    fn replay_open_and_snapshot() {
        let flow = make_flow(
            vec![
                FlowStep {
                    action: "open".into(),
                    url: Some("https://example.com".into()),
                    rationale: Some("Open the page".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "snapshot".into(),
                    rationale: Some("See what's there".into()),
                    ..Default::default()
                },
            ],
            vec![],
        );
        let out = replay(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("agent-browser --session comet open \"https://example.com\""));
        assert!(out.contains("agent-browser --session comet snapshot -i"));
        assert!(out.contains("# Step 1: Open the page"));
    }

    #[test]
    fn replay_var_substitution() {
        let flow = make_flow(
            vec![FlowStep {
                action: "fill".into(),
                ref_hint: Some("input[type=text]".into()),
                value_var: Some("code".into()),
                rationale: Some("Paste the code".into()),
                ..Default::default()
            }],
            vec![FlowVar {
                name: "code".into(),
                description: "The code".into(),
                required: true,
                default: None,
            }],
        );
        let mut vars = HashMap::new();
        vars.insert("code".into(), "ABCD-1234".into());
        let out = replay(&flow, &vars).unwrap();
        assert!(out.contains("\"ABCD-1234\""));
    }

    #[test]
    fn replay_missing_required_var_errors() {
        let flow = make_flow(
            vec![FlowStep {
                action: "fill".into(),
                value_var: Some("code".into()),
                ..Default::default()
            }],
            vec![FlowVar {
                name: "code".into(),
                description: "The code".into(),
                required: true,
                default: None,
            }],
        );
        assert!(replay(&flow, &HashMap::new()).is_err());
    }

    #[test]
    fn replay_optional_var_uses_placeholder() {
        let flow = make_flow(
            vec![FlowStep {
                action: "fill".into(),
                value_var: Some("account".into()),
                ..Default::default()
            }],
            vec![FlowVar {
                name: "account".into(),
                description: "Account".into(),
                required: false,
                default: None,
            }],
        );
        let out = replay(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("{{account}}"));
    }

    #[test]
    fn replay_template_substitution_in_url() {
        let flow = make_flow(
            vec![FlowStep {
                action: "open".into(),
                url: Some("https://example.com/auth?code={{user_code}}".into()),
                ..Default::default()
            }],
            vec![FlowVar {
                name: "user_code".into(),
                description: "Code".into(),
                required: true,
                default: None,
            }],
        );
        let mut vars = HashMap::new();
        vars.insert("user_code".into(), "XYZ-999".into());
        let out = replay(&flow, &vars).unwrap();
        assert!(out.contains("https://example.com/auth?code=XYZ-999"));
    }

    #[test]
    fn replay_conditional_steps() {
        let flow = make_flow(
            vec![FlowStep {
                action: "conditional".into(),
                condition: Some("multiple accounts visible".into()),
                rationale: Some("Only when multiple accounts".into()),
                then: vec![FlowStep {
                    action: "click".into(),
                    ref_hint: Some("account matching primary".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![],
        );
        let out = replay(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("CONDITIONAL: if multiple accounts visible"));
        assert!(out.contains("THEN:"));
        assert!(out.contains("account matching primary"));
    }

    #[test]
    fn replay_success_signal() {
        let mut flow = make_flow(vec![FlowStep {
            action: "snapshot".into(),
            ..Default::default()
        }], vec![]);
        flow.success_signal = Some("page contains 'authorized'".into());
        let out = replay(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("Success signal: page contains 'authorized'"));
    }
}
