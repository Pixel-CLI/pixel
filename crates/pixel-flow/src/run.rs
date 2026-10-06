// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Run engine — emit ready-to-run agent-browser commands from a flow.

use std::collections::HashMap;

use crate::types::{Flow, FlowStep};
use crate::vars::{resolve_value, substitute};

/// Render a flow as a sequence of agent-browser commands with rationale
/// comments and variable substitution. Returns the text for the agent to
/// read and execute (or for `--dry-run` display).
///
/// `vars` is a map of `key=value` substitutions. Missing required vars
/// produce an error. A step `value_var` the caller did not pass falls back
/// to the variable's declared default, then to a placeholder `{{var}}`.
pub fn run(flow: &Flow, vars: &HashMap<String, String>) -> Result<String, String> {
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
        comment_text(&flow.name),
        comment_text(&flow.title)
    ));
    if !flow.description.is_empty() {
        out.push_str(&format!("# {}\n", comment_text(&flow.description)));
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
            out.push_str(&format!("#   - {}\n", comment_text(&substitute(pre, vars))));
        }
        out.push('\n');
    }

    // Stale tab cleanup — close leftover auth tabs.
    if !flow.stale_tab_cleanup.is_empty() {
        out.push_str("# Stale tab cleanup — close leftover tabs matching:\n");
        for pattern in &flow.stale_tab_cleanup {
            out.push_str(&format!(
                "#   agent-browser --session comet tab list   # find tabs matching '{}'\n",
                comment_text(&substitute(pattern, vars))
            ));
            out.push_str(
                "#   agent-browser --session comet tab close <id>   # close each matching tab\n",
            );
        }
        out.push('\n');
    }

    // Default tab — if set, emit a switch_tab at the start.
    if let Some(ref tab) = flow.tab {
        out.push_str(&format!(
            "# Focus the flow's tab: {}\n",
            comment_text(&substitute(tab, vars))
        ));
        out.push_str(&format!(
            "agent-browser --session comet tab list   # find the tab matching '{}'\n",
            comment_text(&substitute(tab, vars))
        ));
        out.push_str("agent-browser --session comet tab <id>   # switch to it\n\n");
    }

    for (i, step) in flow.steps.iter().enumerate() {
        render_step(&mut out, step, i + 1, vars, flow, 0);
    }

    // Success signal (text-based).
    if let Some(ref signal) = flow.success_signal {
        out.push_str(&format!(
            "\n# Success signal: {}\n",
            comment_text(&substitute(signal, vars))
        ));
    }

    // Success URL checks.
    if !flow.success_url_contains.is_empty() {
        out.push_str("# Success URL check — active tab URL should contain one of:\n");
        for u in &flow.success_url_contains {
            out.push_str(&format!("#   - {}\n", comment_text(&substitute(u, vars))));
        }
        out.push_str("agent-browser --session comet snapshot -i   # verify URL\n");
    }
    if !flow.success_url_excludes.is_empty() {
        out.push_str("# Success URL exclusion — URL should NOT contain:\n");
        for u in &flow.success_url_excludes {
            out.push_str(&format!("#   - {}\n", comment_text(&substitute(u, vars))));
        }
    }

    // MFA keywords — hand off to user if detected.
    if !flow.mfa_keywords.is_empty() {
        out.push_str("\n# MFA detection — if the snapshot contains any of these keywords,\n");
        out.push_str("# hand off to the user (cannot be automated):\n");
        for kw in &flow.mfa_keywords {
            out.push_str(&format!("#   - {}\n", comment_text(kw)));
        }
    }

    Ok(out)
}

fn render_step(
    out: &mut String,
    step: &FlowStep,
    num: usize,
    vars: &HashMap<String, String>,
    flow: &Flow,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    if let Some(r) = &step.rationale {
        out.push_str(&format!(
            "{}# Step {}: {}\n",
            indent,
            num,
            comment_text(&substitute(r, vars))
        ));
    } else {
        out.push_str(&format!("{indent}# Step {num}\n"));
    }

    // Per-step tab switching — if this step has a `tab` field, emit a
    // switch_tab command before the action. Falls back to the flow-level
    // default tab. Skip if the action itself is `switch_tab` (it emits its
    // own tab commands).
    let effective_tab = step.tab.as_deref().or(flow.tab.as_deref());
    if step.action != "switch_tab"
        && let Some(tab) = effective_tab
        && step.tab.is_some()
    {
        out.push_str(&format!(
            "{}agent-browser --session comet tab list   # switch to tab matching '{}'\n",
            indent,
            comment_text(&substitute(tab, vars))
        ));
        out.push_str(&format!("{indent}agent-browser --session comet tab <id>\n"));
    }

    match step.action.as_str() {
        "open" => {
            if let Some(ref url) = step.url {
                out.push_str(&format!(
                    "{}agent-browser --session comet open \"{}\"\n",
                    indent,
                    shell_content(&substitute(url, vars))
                ));
            }
        }
        "snapshot" => {
            out.push_str(&format!(
                "{indent}agent-browser --session comet snapshot -i\n"
            ));
        }
        "click" => {
            let target = comment_text(&substitute(
                step.ref_hint.as_deref().unwrap_or("element"),
                vars,
            ));
            out.push_str(&format!(
                "{indent}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {target}\n"
            ));
            out.push_str(&format!(
                "{indent}agent-browser --session comet click @eN      # @eN = {target}\n"
            ));
        }
        "fill" | "type" => {
            let target = comment_text(&substitute(
                step.ref_hint.as_deref().unwrap_or("input"),
                vars,
            ));
            let value = resolve_value(step, vars, &flow.vars);
            out.push_str(&format!(
                "{indent}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {target}\n"
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet {} @eN \"{}\"   # @eN = {}\n",
                indent,
                step.action,
                shell_content(&value),
                target
            ));
        }
        "select" => {
            let target = comment_text(&substitute(
                step.ref_hint.as_deref().unwrap_or("select"),
                vars,
            ));
            let value = resolve_value(step, vars, &flow.vars);
            out.push_str(&format!(
                "{indent}agent-browser --session comet snapshot -i   # find the actual @eN ref for: {target}\n"
            ));
            out.push_str(&format!(
                "{}agent-browser --session comet select @eN \"{}\"   # @eN = {}\n",
                indent,
                shell_content(&value),
                target
            ));
        }
        "press" => {
            let key = step.key.as_deref().unwrap_or("Enter");
            out.push_str(&format!(
                "{}agent-browser --session comet press \"{}\"\n",
                indent,
                shell_content(&substitute(key, vars))
            ));
        }
        "wait" => {
            let wait = step.wait.as_deref().unwrap_or("load");
            let target = step.wait_target.as_deref().unwrap_or("");
            let arg = if target.is_empty() {
                format!("\"--{}\"", shell_content(wait))
            } else {
                format!(
                    "\"--{}\" \"{}\"",
                    shell_content(wait),
                    shell_content(&substitute(target, vars))
                )
            };
            out.push_str(&format!(
                "{indent}agent-browser --session comet wait {arg}\n"
            ));
        }
        "conditional" => {
            let cond = comment_text(&substitute(
                step.condition.as_deref().unwrap_or("condition"),
                vars,
            ));
            out.push_str(&format!("{indent}# CONDITIONAL: if {cond}\n"));
            if !step.then.is_empty() {
                out.push_str(&format!("{indent}# → THEN:\n"));
                for (i, sub) in step.then.iter().enumerate() {
                    render_step(out, sub, i + 1, vars, flow, depth + 1);
                }
            }
            if !step.otherwise.is_empty() {
                out.push_str(&format!("{indent}# → ELSE:\n"));
                for (i, sub) in step.otherwise.iter().enumerate() {
                    render_step(out, sub, i + 1, vars, flow, depth + 1);
                }
            }
        }
        "switch_tab" => {
            let tab = comment_text(&substitute(step.tab.as_deref().unwrap_or("tab"), vars));
            out.push_str(&format!(
                "{indent}agent-browser --session comet tab list   # find tab matching '{tab}'\n"
            ));
            out.push_str(&format!(
                "{indent}agent-browser --session comet tab <id>   # switch to it\n"
            ));
        }
        _ => {
            // Unknown action — emit as a comment for the agent to interpret.
            out.push_str(&format!(
                "{}# action: {} (unknown — interpret manually)\n",
                indent,
                comment_text(&step.action)
            ));
        }
    }

    // On-failure self-fix instruction.
    if let Some(ref fix) = step.on_failure {
        out.push_str(&format!(
            "{}# ON FAILURE (max {} retries): {}\n",
            indent,
            step.max_retries,
            comment_text(&substitute(fix, vars))
        ));
    }

    out.push('\n');
}

/// Escape data inside a double-quoted POSIX shell argument without expansion.
fn shell_content(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Keep explanatory data on its comment line, never as shell instructions.
fn comment_text(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
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

    #[cfg(unix)]
    #[test]
    fn replay_shell_preserves_arguments_and_never_executes_data() {
        use std::os::unix::fs::PermissionsExt;
        let payload = "quote\" $HOME $(printf EXPANDED) `printf BACKTICK` \\ end";
        let flow = make_flow(
            vec![FlowStep {
                action: "open".into(),
                url: Some(payload.into()),
                rationale: Some("note\nprintf COMMENT_INJECTION\n#".into()),
                ..Default::default()
            }],
            vec![],
        );
        let rendered = run(&flow, &HashMap::new()).unwrap();
        let dir = std::env::temp_dir().join(format!("pixel-replay-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("agent-browser");
        std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(rendered)
            .env("PATH", &dir)
            .output()
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = format!("--session\0comet\0open\0{payload}\0");
        assert_eq!(output.stdout, expected.as_bytes());
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
        let out = run(&flow, &HashMap::new()).unwrap();
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
        let out = run(&flow, &vars).unwrap();
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
        assert!(run(&flow, &HashMap::new()).is_err());
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
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("{{account}}"));
    }

    #[test]
    fn replay_required_var_with_default_uses_the_default() {
        let flow = make_flow(
            vec![FlowStep {
                action: "fill".into(),
                ref_hint: Some("input[type=text]".into()),
                value_var: Some("account".into()),
                ..Default::default()
            }],
            vec![FlowVar {
                name: "account".into(),
                description: "Account".into(),
                required: true,
                default: Some("west".into()),
            }],
        );
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("\"west\""), "{out}");
        assert!(!out.contains("{{account}}"), "{out}");

        // An explicit --var still wins over the declared default.
        let vars = HashMap::from([("account".to_string(), "east".to_string())]);
        let out = run(&flow, &vars).unwrap();
        assert!(out.contains("\"east\""), "{out}");
        assert!(!out.contains("west"), "{out}");
    }

    #[test]
    fn replay_conditional_numbers_and_indents_sub_steps() {
        let flow = make_flow(
            vec![FlowStep {
                action: "conditional".into(),
                rationale: Some("decide".into()),
                condition: Some("ready".into()),
                then: vec![
                    FlowStep {
                        action: "snapshot".into(),
                        rationale: Some("then first".into()),
                        ..Default::default()
                    },
                    FlowStep {
                        action: "snapshot".into(),
                        rationale: Some("then second".into()),
                        ..Default::default()
                    },
                ],
                otherwise: vec![FlowStep {
                    action: "snapshot".into(),
                    rationale: Some("else first".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![],
        );
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("# Step 1: decide"), "{out}");
        // Sub-steps are numbered from 1 and indented two spaces per depth.
        assert!(out.contains("  # Step 1: then first"), "{out}");
        assert!(out.contains("  # Step 2: then second"), "{out}");
        assert!(out.contains("  # Step 1: else first"), "{out}");
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
        let out = run(&flow, &vars).unwrap();
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
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("CONDITIONAL: if multiple accounts visible"));
        assert!(out.contains("THEN:"));
        assert!(out.contains("account matching primary"));
    }

    #[test]
    fn replay_success_signal() {
        let mut flow = make_flow(
            vec![FlowStep {
                action: "snapshot".into(),
                ..Default::default()
            }],
            vec![],
        );
        flow.success_signal = Some("page contains 'authorized'".into());
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("Success signal: page contains 'authorized'"));
    }

    #[test]
    fn replay_select_wait_and_switch_tab_render_their_commands() {
        let flow = make_flow(
            vec![
                FlowStep {
                    action: "select".into(),
                    ref_hint: Some("combobox matching 'Country'".into()),
                    value: Some("FR".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "wait".into(),
                    wait: Some("10s".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "wait".into(),
                    wait: Some("text".into()),
                    wait_target: Some("Welcome {{who}}".into()),
                    ..Default::default()
                },
                FlowStep {
                    action: "switch_tab".into(),
                    tab: Some("claude".into()),
                    ..Default::default()
                },
            ],
            vec![],
        );
        let vars = HashMap::from([("who".to_string(), "alice".to_string())]);
        let out = run(&flow, &vars).unwrap();
        assert!(
            out.contains("agent-browser --session comet select @eN \"FR\"   # @eN = combobox matching 'Country'"),
            "{out}"
        );
        assert!(
            out.contains("agent-browser --session comet wait \"--10s\"\n"),
            "{out}"
        );
        assert!(
            out.contains("agent-browser --session comet wait \"--text\" \"Welcome alice\"\n"),
            "{out}"
        );
        assert!(
            out.contains("agent-browser --session comet tab list   # find tab matching 'claude'"),
            "{out}"
        );
    }

    #[test]
    fn replay_conditional_prints_only_the_branches_that_exist() {
        let then_only = make_flow(
            vec![FlowStep {
                action: "conditional".into(),
                condition: Some("page shows 'A'".into()),
                then: vec![FlowStep {
                    action: "snapshot".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![],
        );
        let out = run(&then_only, &HashMap::new()).unwrap();
        assert!(out.contains("# → THEN:"), "{out}");
        assert!(!out.contains("# → ELSE:"), "{out}");

        let else_only = make_flow(
            vec![FlowStep {
                action: "conditional".into(),
                condition: Some("page shows 'A'".into()),
                otherwise: vec![FlowStep {
                    action: "snapshot".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![],
        );
        let out = run(&else_only, &HashMap::new()).unwrap();
        assert!(!out.contains("# → THEN:"), "{out}");
        assert!(out.contains("# → ELSE:"), "{out}");
    }

    #[test]
    fn replay_prints_preconditions_only_when_present() {
        let out = run(&make_flow(vec![], vec![]), &HashMap::new()).unwrap();
        assert!(!out.contains("# Preconditions"), "{out}");
        let mut flow = make_flow(vec![], vec![]);
        flow.preconditions = vec!["signed out".into()];
        let out = run(&flow, &HashMap::new()).unwrap();
        assert!(out.contains("#   - signed out"), "{out}");
    }
}
