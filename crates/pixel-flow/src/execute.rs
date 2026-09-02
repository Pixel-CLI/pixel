//! Flow executor — actually runs agent-browser commands from a flow.
//!
//! Unlike `replay` (which only emits text), this module shells out to
//! `agent-browser --session comet`, parses snapshot output to resolve
//! `@eN` refs from `ref_hint` descriptions, evaluates conditionals by
//! inspecting the page, and handles tab switching.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use crate::types::{Flow, FlowStep};

/// Result of executing a flow.
#[derive(Debug)]
pub struct ExecResult {
    pub steps_executed: usize,
    pub steps_skipped: usize,
    pub log: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Execute a flow by running agent-browser commands.
///
/// `vars` is a map of `key=value` substitutions. Missing required vars
/// produce an error.
pub fn execute(flow: &Flow, vars: &HashMap<String, String>) -> ExecResult {
    let mut log = String::new();
    let mut steps_executed = 0usize;
    let mut steps_skipped = 0usize;

    // Validate required vars.
    for v in &flow.vars {
        if v.required && !vars.contains_key(&v.name) && v.default.is_none() {
            return ExecResult {
                steps_executed: 0,
                steps_skipped: 0,
                log: String::new(),
                success: false,
                error: Some(format!(
                    "missing required variable '{}' for flow '{}'",
                    v.name, flow.name
                )),
            };
        }
    }

    log.push_str(&format!(
        "# Executing flow: {} ({})\n# Steps: {} | Revision: {}\n\n",
        flow.name,
        flow.title,
        flow.steps.len(),
        flow.revision
    ));

    // Preconditions — print as warnings, don't block.
    if !flow.preconditions.is_empty() {
        log.push_str("# Preconditions (verify before starting):\n");
        for pre in &flow.preconditions {
            log.push_str(&format!("#   - {}\n", substitute(pre, vars)));
        }
        log.push('\n');
    }

    // Stale tab cleanup.
    if !flow.stale_tab_cleanup.is_empty() {
        log.push_str("# Stale tab cleanup:\n");
        for pattern in &flow.stale_tab_cleanup {
            let pat = substitute(pattern, vars);
            log.push_str(&format!("#   closing tabs matching '{}'\n", pat));
            if let Err(e) = close_stale_tabs(&pat, &mut log) {
                log.push_str(&format!("#   WARN: stale cleanup failed: {}\n", e));
            }
        }
        log.push('\n');
    }

    // Focus the flow's default tab.
    if let Some(ref tab) = flow.tab {
        let tab = substitute(tab, vars);
        log.push_str(&format!("# Focusing flow tab: {}\n", tab));
        if let Err(e) = switch_to_tab(&tab, &mut log) {
            log.push_str(&format!("#   WARN: tab focus failed: {}\n", e));
        }
        log.push('\n');
    }

    // Execute steps.
    for (i, step) in flow.steps.iter().enumerate() {
        match exec_step(step, i + 1, vars, flow.tab.as_deref(), &mut log, 0) {
            Ok(executed) => {
                if executed {
                    steps_executed += 1;
                } else {
                    steps_skipped += 1;
                }
            }
            Err(e) => {
                log.push_str(&format!("# ERROR at step {}: {}\n", i + 1, e));
                return ExecResult {
                    steps_executed,
                    steps_skipped,
                    log,
                    success: false,
                    error: Some(format!("step {} failed: {}", i + 1, e)),
                };
            }
        }
    }

    // Success URL check — use `get url` to check the actual page URL,
    // and also check the snapshot for success signals.
    let mut url_ok = true;
    let final_snapshot = run_agent_browser(&["snapshot", "-i"]).unwrap_or_default();
    let final_url = run_agent_browser(&["get", "url"]).unwrap_or_default();
    let combined = format!("{}\n{}", final_url, final_snapshot);

    if !flow.success_url_contains.is_empty() {
        for u in &flow.success_url_contains {
            let u = substitute(u, vars);
            if !combined.contains(&u) {
                log.push_str(&format!(
                    "# WARN: success URL check — '{}' not found in URL or snapshot\n",
                    u
                ));
                url_ok = false;
            }
        }
    }

    // Success signal check — if the success_signal text is found in the
    // snapshot, override url_ok to true (the signal is authoritative).
    if let Some(ref signal) = flow.success_signal {
        let signal = substitute(signal, vars);
        // Check if any quoted term from the signal appears in the snapshot.
        let terms = extract_quoted_strings(&signal);
        let signal_met = if terms.is_empty() {
            // No quoted terms — check the whole signal as a keyword.
            final_snapshot.to_lowercase().contains(&signal.to_lowercase())
        } else {
            terms.iter().any(|t| final_snapshot.to_lowercase().contains(&t.to_lowercase()))
        };
        if signal_met {
            log.push_str(&format!("# ✓ Success signal detected: {}\n", signal));
            url_ok = true;
        }
    }

    // MFA detection.
    if !flow.mfa_keywords.is_empty() {
        if let Ok(snapshot) = run_agent_browser(&["snapshot", "-i"]) {
            for kw in &flow.mfa_keywords {
                if snapshot.contains(kw) {
                    log.push_str(&format!(
                        "# MFA DETECTED: keyword '{}' found in snapshot.\n",
                        kw
                    ));
                    log.push_str("# → Hand off to user — MFA cannot be automated.\n");
                    return ExecResult {
                        steps_executed,
                        steps_skipped,
                        log,
                        success: false,
                        error: Some(format!(
                            "MFA gate detected (keyword: '{}') — user intervention required",
                            kw
                        )),
                    };
                }
            }
        }
    }

    log.push_str(&format!(
        "\n# Flow complete: {} steps executed, {} skipped\n",
        steps_executed, steps_skipped
    ));

    if let Some(ref signal) = flow.success_signal {
        log.push_str(&format!("# Success signal: {}\n", substitute(signal, vars)));
    }

    ExecResult {
        steps_executed,
        steps_skipped,
        log,
        success: url_ok,
        error: if url_ok { None } else { Some("success URL check failed".into()) },
    }
}

/// Execute a single step. Returns Ok(true) if executed, Ok(false) if skipped
/// (e.g. conditional branch not taken).
fn exec_step(
    step: &FlowStep,
    num: usize,
    vars: &HashMap<String, String>,
    flow_tab: Option<&str>,
    log: &mut String,
    depth: usize,
) -> Result<bool, String> {
    let indent = "  ".repeat(depth);
    if let Some(r) = &step.rationale {
        log.push_str(&format!("{}# Step {}: {}\n", indent, num, substitute(r, vars)));
    }

    // Per-step tab switching.
    let effective_tab = step.tab.as_deref().or(flow_tab);
    if step.action != "switch_tab" && step.tab.is_some() {
        if let Some(tab) = effective_tab {
            let tab = substitute(tab, vars);
            log.push_str(&format!("{}# Switching to tab: {}\n", indent, tab));
            switch_to_tab(&tab, log)?;
        }
    }

    match step.action.as_str() {
        "open" => {
            if let Some(ref url) = step.url {
                let url = substitute(url, vars);
                log.push_str(&format!("{}agent-browser open \"{}\"\n", indent, url));
                // Try open first; if the bound tab is gone, use `tab new`.
                match run_agent_browser(&["open", &url]) {
                    Ok(_) => {}
                    Err(e) if e.contains("tab_gone") || e.contains("no tab") => {
                        log.push_str(&format!("{}# bound tab gone — opening new tab\n", indent));
                        run_agent_browser(&["tab", "new", &url])
                            .map_err(|e2| format!("open/tab new failed: {e2}"))?;
                    }
                    Err(e) => return Err(format!("open failed: {e}")),
                }
                // Give the page time to load (websites open in <5s).
                std::thread::sleep(Duration::from_secs(3));
            }
            Ok(true)
        }
        "snapshot" => {
            log.push_str(&format!("{}agent-browser snapshot -i\n", indent));
            run_agent_browser(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot failed: {e}"))?;
            Ok(true)
        }
        "click" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("element"),
                vars,
            );
            log.push_str(&format!("{}# Finding ref for: {}\n", indent, target));
            let snapshot = run_agent_browser(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before click failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{}' found in snapshot", target))?;
            log.push_str(&format!("{}agent-browser click @{}\n", indent, ref_id));
            run_agent_browser(&["click", &format!("@{}", ref_id)])
                .map_err(|e| format!("click @{} failed: {e}", ref_id))?;
            // Wait for potential navigation.
            std::thread::sleep(Duration::from_secs(2));

            // Check if the click actually navigated (snapshot changed).
            // If on_failure mentions JS click/eval, try that as a fallback.
            if let Some(ref fix) = step.on_failure {
                let fix = substitute(fix, vars);
                let fix_lower = fix.to_lowercase();
                if fix_lower.contains("eval") || fix_lower.contains("js click") || fix_lower.contains("queryselector") {
                    // Check if the page is still on the same URL (click didn't work).
                    let post_snapshot = run_agent_browser(&["snapshot", "-i"])
                        .unwrap_or_default();
                    // If the target is still visible, the click didn't navigate.
                    if post_snapshot.contains(&target) {
                        log.push_str(&format!("{}# click didn't navigate — trying JS click fallback\n", indent));
                        // Try eval with querySelector.
                        let js = format!(
                            "document.querySelector('button')?.click()"
                        );
                        run_agent_browser(&["eval", &js])
                            .map_err(|e| format!("JS click fallback failed: {e}"))?;
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
            Ok(true)
        }
        "fill" | "type" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("input"),
                vars,
            );
            let value = resolve_value(step, vars);

            // Special case: if the ref_hint contains "Code character N of",
            // extract N and use the Nth character of user_code (without dash).
            let value = if target.contains("Code character") && value.is_empty() {
                if let Some(user_code) = vars.get("user_code") {
                    let code_clean = user_code.replace('-', "");
                    // Extract N from "Code character N of 9"
                    if let Some(n) = extract_char_number(&target) {
                        if n > 0 && n <= code_clean.len() {
                            code_clean.chars().nth(n - 1).map(|c| c.to_string()).unwrap_or_default()
                        } else {
                            value
                        }
                    } else {
                        value
                    }
                } else {
                    value
                }
            } else {
                value
            };

            log.push_str(&format!("{}# Finding ref for: {}\n", indent, target));
            let snapshot = run_agent_browser(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before fill failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{}' found in snapshot", target))?;
            log.push_str(&format!(
                "{}agent-browser {} @{} \"{}\"\n",
                indent, step.action, ref_id, value
            ));
            run_agent_browser(&[&step.action, &format!("@{}", ref_id), &value])
                .map_err(|e| format!("{} @{} failed: {e}", step.action, ref_id))?;
            Ok(true)
        }
        "select" => {
            let target = substitute(
                step.ref_hint.as_deref().unwrap_or("select"),
                vars,
            );
            let value = resolve_value(step, vars);
            let snapshot = run_agent_browser(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before select failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{}' found in snapshot", target))?;
            log.push_str(&format!(
                "{}agent-browser select @{} \"{}\"\n",
                indent, ref_id, value
            ));
            run_agent_browser(&["select", &format!("@{}", ref_id), &value])
                .map_err(|e| format!("select @{} failed: {e}", ref_id))?;
            Ok(true)
        }
        "press" => {
            let key = substitute(step.key.as_deref().unwrap_or("Enter"), vars);
            log.push_str(&format!("{}agent-browser press {}\n", indent, key));
            run_agent_browser(&["press", &key])
                .map_err(|e| format!("press {} failed: {e}", key))?;
            Ok(true)
        }
        "wait" => {
            let wait = step.wait.as_deref().unwrap_or("load");
            let dur = parse_wait_duration(wait);
            log.push_str(&format!("{}# Waiting {}s\n", indent, dur.as_secs()));
            std::thread::sleep(dur);
            Ok(true)
        }
        "conditional" => {
            let cond = substitute(
                step.condition.as_deref().unwrap_or("condition"),
                vars,
            );
            log.push_str(&format!("{}# CONDITIONAL: if {}\n", indent, cond));

            // Take a snapshot to evaluate the condition.
            let snapshot = run_agent_browser(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot for conditional failed: {e}"))?;

            let condition_met = evaluate_condition(&cond, &snapshot);
            log.push_str(&format!(
                "{}# Condition {} — taking {} branch\n",
                indent,
                if condition_met { "MET" } else { "NOT MET" },
                if condition_met { "THEN" } else { "ELSE" }
            ));

            let branch = if condition_met { &step.then } else { &step.otherwise };
            let mut any_executed = false;
            for (i, sub) in branch.iter().enumerate() {
                match exec_step(sub, i + 1, vars, flow_tab, log, depth + 1) {
                    Ok(true) => any_executed = true,
                    Ok(false) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(any_executed)
        }
        "switch_tab" => {
            let tab = substitute(
                step.tab.as_deref().unwrap_or("tab"),
                vars,
            );
            log.push_str(&format!("{}# Switch to tab: {}\n", indent, tab));
            switch_to_tab(&tab, log)?;
            Ok(true)
        }
        "eval" => {
            // Evaluate JS — for cases where regular click doesn't work.
            let js = substitute(
                step.value.as_deref().unwrap_or(""),
                vars,
            );
            if !js.is_empty() {
                log.push_str(&format!("{}agent-browser eval \"{}\"\n", indent, js));
                run_agent_browser(&["eval", &js])
                    .map_err(|e| format!("eval failed: {e}"))?;
            }
            Ok(true)
        }
        _ => {
            log.push_str(&format!(
                "{}# Unknown action: {} — skipping\n",
                indent, step.action
            ));
            Ok(false)
        }
    }
}

/// Run an agent-browser command and return its stdout.
fn run_agent_browser(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("agent-browser");
    cmd.args(["--session", "comet"]);
    cmd.args(args);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn agent-browser: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "agent-browser {} exited with {}: stderr={stderr} stdout={stdout}",
            args.join(" "),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a snapshot and find the accessibility ref (`@eN`) for an element
/// matching the `ref_hint` description.
///
/// Snapshot lines look like:
///   - button "Continue with Google" [ref=e5]
///   - link "Log in to another account" [ref=e5]
///   - textbox "Code character 1 of 9" [ref=e5]
///   - heading "Welcome back" [level=1, ref=e1]
///
/// The `ref_hint` is a natural language description like:
///   "button containing 'Continue with Google'"
///   "account matching livio.gamassia@gmail.com"
///   "input[type=email] or textbox matching 'Email'"
fn find_ref_in_snapshot(snapshot: &str, ref_hint: &str) -> Option<String> {
    // Extract quoted strings from the ref_hint — these are the search terms.
    // e.g. "button containing 'Continue with Google'" → ["Continue with Google"]
    let search_terms: Vec<&str> = extract_quoted_strings(ref_hint);

    // If no quoted strings, try to match the whole hint as a fuzzy term.
    let fallback_term = ref_hint
        .replace("button containing", "")
        .replace("link containing", "")
        .replace("account matching", "")
        .replace("textbox matching", "")
        .replace("input", "")
        .replace("or", "")
        .trim()
        .to_lowercase();
    let fallback_term = fallback_term.trim_matches(|c: char| !c.is_alphanumeric()).to_string();

    for line in snapshot.lines() {
        // Look for [ref=eN] pattern.
        if let Some(ref_start) = line.find("ref=e") {
            // Extract the ref id: "e" followed by digits.
            let after_e = &line[ref_start + 4..]; // skip "ref=", now at "eN...]"
            // after_e starts with "e", then digits
            let digits_start = 1; // skip the "e"
            let rest = &after_e[digits_start..];
            let ref_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let ref_id = &rest[..ref_end];

            // Check if this line matches any search term.
            let line_lower = line.to_lowercase();

            let matched = if !search_terms.is_empty() {
                search_terms.iter().all(|term| line_lower.contains(&term.to_lowercase()))
            } else if !fallback_term.is_empty() && fallback_term.len() > 2 {
                line_lower.contains(&fallback_term)
            } else {
                // No search term — match the first element with a ref.
                true
            };

            if matched {
                return Some(format!("e{}", ref_id));
            }
        }
    }
    None
}

/// Extract the character number from a ref_hint like "Code character 3 of 9".
fn extract_char_number(hint: &str) -> Option<usize> {
    // Look for "character N of" pattern.
    let parts: Vec<&str> = hint.split_whitespace().collect();
    for i in 0..parts.len().saturating_sub(1) {
        if parts[i] == "character" {
            if let Ok(n) = parts[i + 1].parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

/// Extract single-quoted strings from a hint string.
/// e.g. "button containing 'Continue'" → ["Continue"]
fn extract_quoted_strings(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\'' {
            if in_quote {
                result.push(&s[start..i]);
                in_quote = false;
            } else {
                start = i + 1;
                in_quote = true;
            }
        }
    }
    result
}

/// Evaluate a condition string against a snapshot.
///
/// Conditions look like:
///   "page shows 'Continue with Google' or 'Continue with Google'"
///   "page shows 'Welcome back' heading with account buttons"
///   "the desired account livio.gamassia@gmail.com is visible"
///   "page contains 'hCaptcha' or 'Drag'"
///   "page shows password input field"
///   "URL contains 'code=' parameter"
fn evaluate_condition(condition: &str, snapshot: &str) -> bool {
    let cond_lower = condition.to_lowercase();
    let snap_lower = snapshot.to_lowercase();

    // Special case: URL check — need to get the URL.
    if cond_lower.contains("url contains") {
        // Extract the quoted term.
        if let Some(term) = extract_quoted_strings(condition).first() {
            // Try to get the URL from the snapshot or a separate command.
            if let Ok(url_output) = run_agent_browser(&["get", "url"]) {
                return url_output.to_lowercase().contains(&term.to_lowercase());
            }
            return false;
        }
    }

    // Extract quoted terms and check if any/all appear in the snapshot.
    let terms = extract_quoted_strings(condition);

    if terms.is_empty() {
        // No quoted terms — try keyword matching.
        // Check for common condition keywords.
        let keywords: Vec<&str> = cond_lower
            .split_whitespace()
            .filter(|w| w.len() > 3 && !["page", "shows", "contains", "visible", "button", "field", "input"].contains(w))
            .collect();
        if keywords.is_empty() {
            return false;
        }
        // If any keyword matches, condition is met.
        return keywords.iter().any(|kw| snap_lower.contains(kw));
    }

    // "or" in the condition means any term matches.
    // Otherwise all terms must match.
    if cond_lower.contains(" or ") {
        terms.iter().any(|t| snap_lower.contains(&t.to_lowercase()))
    } else {
        terms.iter().all(|t| snap_lower.contains(&t.to_lowercase()))
    }
}

/// Switch to a tab matching the given pattern.
fn switch_to_tab(pattern: &str, log: &mut String) -> Result<(), String> {
    let output = run_agent_browser(&["tab", "list"])
        .map_err(|e| format!("tab list failed: {e}"))?;

    // Parse tab list output to find a tab matching the pattern.
    // Output looks like:
    //   → [t24] Welcome back - OpenAI - https://auth.openai.com/...
    //   [t12] Claude - https://claude.ai/...
    let pat_lower = pattern.to_lowercase();
    for line in output.lines() {
        if line.to_lowercase().contains(&pat_lower) {
            // Extract tab id (tNN).
            if let Some(t_start) = line.find("t") {
                let after = &line[t_start..];
                if let Some(end) = after.find(|c: char| !c.is_ascii_digit() && c != 't') {
                    let tab_id = &after[..end];
                    log.push_str(&format!("#   switching to tab {}\n", tab_id));
                    run_agent_browser(&["tab", tab_id])
                        .map_err(|e| format!("tab switch failed: {e}"))?;
                    return Ok(());
                }
            }
        }
    }
    // Tab not found — not an error, the flow may be on the right tab already.
    log.push_str(&format!("#   WARN: no tab matching '{}' found\n", pattern));
    Ok(())
}

/// Close stale tabs matching a pattern.
fn close_stale_tabs(pattern: &str, log: &mut String) -> Result<(), String> {
    let output = run_agent_browser(&["tab", "list"])
        .map_err(|e| format!("tab list failed: {e}"))?;

    let pat_lower = pattern.to_lowercase();
    for line in output.lines() {
        if line.to_lowercase().contains(&pat_lower) {
            if let Some(t_start) = line.find("t") {
                let after = &line[t_start..];
                if let Some(end) = after.find(|c: char| !c.is_ascii_digit() && c != 't') {
                    let tab_id = &after[..end];
                    log.push_str(&format!("#   closing tab {}\n", tab_id));
                    let _ = run_agent_browser(&["tab", "close", tab_id]);
                }
            }
        }
    }
    Ok(())
}

/// Parse a wait duration string like "120s", "10s", "load", "500ms".
fn parse_wait_duration(s: &str) -> Duration {
    let s = s.trim();
    if s.ends_with("ms") {
        let n: u64 = s.trim_end_matches("ms").parse().unwrap_or(500);
        return Duration::from_millis(n);
    }
    if s.ends_with('s') {
        let n: u64 = s.trim_end_matches('s').parse().unwrap_or(5);
        return Duration::from_secs(n);
    }
    if s == "load" {
        return Duration::from_secs(2);
    }
    // Try to parse as seconds.
    s.parse::<u64>().map(Duration::from_secs).unwrap_or(Duration::from_secs(2))
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

    #[test]
    fn extract_quoted_simple() {
        let terms = extract_quoted_strings("button containing 'Continue with Google'");
        assert_eq!(terms, vec!["Continue with Google"]);
    }

    #[test]
    fn extract_quoted_multiple() {
        let terms = extract_quoted_strings("page contains 'hCaptcha' or 'Drag'");
        assert_eq!(terms, vec!["hCaptcha", "Drag"]);
    }

    #[test]
    fn extract_quoted_none() {
        let terms = extract_quoted_strings("page shows password input field");
        assert!(terms.is_empty());
    }

    #[test]
    fn find_ref_button() {
        let snapshot = "- heading \"Welcome back\" [level=1, ref=e1]\n- button \"Continue with Google\" [ref=e5]";
        let ref_id = find_ref_in_snapshot(snapshot, "button containing 'Continue with Google'");
        assert_eq!(ref_id, Some("e5".into()));
    }

    #[test]
    fn find_ref_account() {
        let snapshot = "- button \"Select account Livio livio.gamassia@gmail.com\" [ref=e7]";
        let ref_id = find_ref_in_snapshot(snapshot, "account matching 'livio.gamassia@gmail.com'");
        assert_eq!(ref_id, Some("e7".into()));
    }

    #[test]
    fn find_ref_not_found() {
        let snapshot = "- heading \"Welcome\" [ref=e1]";
        let ref_id = find_ref_in_snapshot(snapshot, "button containing 'Submit'");
        assert_eq!(ref_id, None);
    }

    #[test]
    fn evaluate_condition_or() {
        let snapshot = "- heading \"hCaptcha\" [ref=e1]";
        assert!(evaluate_condition("page contains 'hCaptcha' or 'Drag'", snapshot));
    }

    #[test]
    fn evaluate_condition_and() {
        let snapshot = "- button \"Continue\" [ref=e5]\n- text \"signing back in\"";
        assert!(evaluate_condition("page shows 'Continue' and 'signing back in'", snapshot));
    }

    #[test]
    fn evaluate_condition_not_met() {
        let snapshot = "- heading \"Welcome\" [ref=e1]";
        assert!(!evaluate_condition("page shows 'Continue with Google'", snapshot));
    }

    #[test]
    fn evaluate_condition_keyword() {
        let snapshot = "- button \"Continue\" [ref=e5]";
        assert!(evaluate_condition("page shows Continue button", snapshot));
    }

    #[test]
    fn parse_wait_seconds() {
        assert_eq!(parse_wait_duration("120s"), Duration::from_secs(120));
        assert_eq!(parse_wait_duration("10s"), Duration::from_secs(10));
    }

    #[test]
    fn parse_wait_millis() {
        assert_eq!(parse_wait_duration("500ms"), Duration::from_millis(500));
    }

    #[test]
    fn parse_wait_load() {
        assert_eq!(parse_wait_duration("load"), Duration::from_secs(2));
    }
}
