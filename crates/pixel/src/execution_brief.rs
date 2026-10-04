//! Deterministic, bounded projection of `scope-task` evidence for agents.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

const MAX_CAPS: usize = 32;
const MAX_EVIDENCE_PER_TARGET: usize = 8;
const MAX_SYMBOLS_PER_TARGET: usize = 32;
const MAX_TARGETS: usize = 100;
const MAX_TEXT_CHARS: usize = 320;
const MAX_TASK_CHARS: usize = 4096;
const MAX_WORKSTREAMS: usize = 64;
const MAX_ROUTE_TASK_CHARS: usize = 240;

#[derive(Default)]
struct Workstream {
    area: String,
    tier: String,
    targets: Vec<Value>,
}

/// Convert the existing `Request::Targets` response into the versioned brief
/// consumed by orchestration callers. Dependency edges are deliberately not
/// inferred: this command has no complete static dependency graph.
pub fn from_scope_task(task: &str, data: &Value) -> Value {
    let mut caps = BTreeSet::new();
    collect_source_caps(data, &mut caps);
    let bounded_task = bounded_text(task, "task", MAX_TASK_CHARS, &mut caps);

    let mut targets: Vec<&Value> = match data.get("targets").and_then(Value::as_array) {
        Some(targets) => targets
            .iter()
            .filter(|target| {
                matches!(
                    target.get("tier").and_then(Value::as_str),
                    Some("P0" | "P1")
                )
            })
            .collect(),
        None => {
            caps.insert("scope-task response did not contain a targets array".to_string());
            Vec::new()
        }
    };

    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        caps.insert("P2 targets omitted: the brief contract supports only P0 and P1".to_string());
    }

    targets.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| target_path(left).cmp(target_path(right)))
    });
    if targets.len() > MAX_TARGETS {
        targets.truncate(MAX_TARGETS);
        caps.insert(format!("execution brief targets capped at {MAX_TARGETS}"));
    }

    let mut groups: BTreeMap<(String, String), Workstream> = BTreeMap::new();
    for target in targets {
        let Some(path) = target_path_value(target) else {
            caps.insert("target without a path omitted".to_string());
            continue;
        };
        let tier = target["tier"].as_str().unwrap_or("P1").to_string();
        let area = repository_area(path);
        let key = (area.clone(), tier.clone());
        let entry = groups.entry(key).or_insert_with(|| Workstream {
            area,
            tier,
            targets: Vec::new(),
        });
        entry.targets.push(target_projection(target, &mut caps));
    }

    let mut workstreams = Vec::new();
    for (_, mut group) in groups {
        group
            .targets
            .sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        let paths: Vec<Value> = group
            .targets
            .iter()
            .filter_map(|target| target["path"].as_str().map(Value::from))
            .collect();
        workstreams.push(json!({
            "id": format!("workstream:{}:{}", group.area, group.tier),
            "paths": paths,
            "tier": group.tier,
            "depends_on": [],
            "ownership": if group.tier == "P0" { "write" } else { "read" },
            "targets": group.targets,
        }));
    }
    // The groups come out ordered by area; order them P0 first before the
    // cap so a truncation drops read-only context, never a write workstream.
    workstreams.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    if workstreams.len() > MAX_WORKSTREAMS {
        workstreams.truncate(MAX_WORKSTREAMS);
        caps.insert(format!(
            "execution brief workstreams capped at {MAX_WORKSTREAMS}"
        ));
    }

    let mut caps: Vec<String> = caps.into_iter().collect();
    if caps.len() > MAX_CAPS {
        caps.truncate(MAX_CAPS);
        caps.push(format!("execution brief caps capped at {MAX_CAPS}"));
    }
    let mut validation = Vec::new();
    let has_p0 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P0");
    let has_p1 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P1");
    if has_p0 {
        validation
            .push("Run focused tests covering changed P0 behavior before integration.".to_string());
    }
    if has_p1 {
        validation.push(
            "Review P1 workstreams for callers and contracts, then run regression tests."
                .to_string(),
        );
    }
    if !has_p0 && has_p1 {
        validation
            .push("No P0 target was returned; validate the P1 context before editing.".to_string());
    }
    if workstreams.is_empty() {
        validation.push(
            "No P0/P1 targets were returned; inspect scope-task caps before acting.".to_string(),
        );
    }
    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        validation.push(
            "P2 targets were omitted; validate any peripheral files separately if touched."
                .to_string(),
        );
    }
    if !caps.is_empty() {
        validation.push("Review uncertainty.caps before relying on this brief.".to_string());
    }

    json!({
        "version": 1,
        "task": bounded_task,
        "retrieval_route": retrieval_route(task),
        "workstreams": workstreams,
        "uncertainty": {
            "closed_world": false,
            "lower_bound": true,
            "caps": caps,
            "unknown_dependencies": [
                "Static dependency edges are unavailable; every workstream depends_on list is intentionally empty."
            ],
        },
        "validation": validation,
    })
}

/// Produce one deterministic retrieval route from a task description. An
/// explicitly backticked identifier starts with exact search; other tasks
/// start with behavior search. A non-converging first call gets one alternate
/// Pixel query, then a bounded native fallback. Search warnings and call-count
/// notices are informational; only the result itself advances the route.
pub fn retrieval_route(task: &str) -> Value {
    let task = truncate_chars(task.trim(), MAX_ROUTE_TASK_CHARS);
    let identifier = explicit_identifier(&task);
    let (subcommand, args) = match identifier.as_deref() {
        Some(identifier) => (
            "search-content",
            vec![
                "-F".to_string(),
                identifier.to_string(),
                "--fallback-query".to_string(),
                task.clone(),
                "--no-daemon".to_string(),
            ],
        ),
        None => ("find-code", vec![task.clone()]),
    };
    let first = route_command(subcommand, &args);
    let fallback_query = identifier.clone().unwrap_or_else(|| bounded_native_query(&task));
    let native_fallback = format!(
        "rtk rg -m 5 -n -F -- {} . | rtk sed -n '1,20p'",
        shell_quote(&fallback_query)
    );
    let alternate = if identifier.is_some() {
        native_fallback.clone()
    } else {
        format!(
            "rtk pixel find-code {}",
            shell_quote(&format!("{task} implementation and callers"))
        )
    };
    json!({
        "first_command": first,
        "first_operation": {"subcommand": subcommand, "args": args},
        "first_on_empty_or_irrelevant": alternate,
        "after_two_nonconverging_calls": native_fallback,
        "automatic_empty_fallback": identifier.is_some(),
        "read": "Read only a path returned by Pixel, in a maximum 40-line window around its line.",
        "validation": "After an edit, run the smallest relevant test for the changed behavior; read-only tasks need no test.",
        "progression": if identifier.is_some() {
            "An empty exact result automatically runs one task-aware find-code fallback in the same command; warnings or prior-call counts alone never trigger it. If that combined lookup is unusable, use the bounded native fallback."
        } else {
            "A warning or prior-call count is informational, not a no-hit. Advance only when this invocation returns no usable match, an unresolved result, or an irrelevant result."
        }
    })
}

fn route_command(subcommand: &str, args: &[String]) -> String {
    format!(
        "rtk pixel {subcommand} {}",
        args.iter()
            .map(|argument| match argument.as_str() {
                "-F" | "--fallback-query" | "--no-daemon" => argument.clone(),
                _ => shell_quote(argument),
            })
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetrievalOutcome {
    Usable,
    NoUsableResult,
    WarningOnly,
}

/// Resolve the next route step from an invocation's outcome. A warning by
/// itself never advances the route, regardless of the displayed call count.
#[cfg(test)]
pub(crate) fn next_retrieval_command(
    route: &Value,
    completed_pixel_calls: usize,
    outcome: RetrievalOutcome,
) -> Option<&str> {
    if outcome != RetrievalOutcome::NoUsableResult {
        return None;
    }
    match completed_pixel_calls {
        0 => route["first_command"].as_str(),
        1 => route["first_on_empty_or_irrelevant"].as_str(),
        _ => route["after_two_nonconverging_calls"].as_str(),
    }
}

/// Render an executable route without presenting task-context candidates as
/// recommendations or boundaries.
pub fn pretty_retrieval_route(route: &Value) -> String {
    if route["automatic_empty_fallback"].as_bool() == Some(true) {
        return format!(
            "[PIXEL:EXECUTION_ROUTE]\n1. Run: {}\n   An empty exact result automatically runs the task-aware find-code fallback once in the same command.\n2. Read: {}\n3. If the combined Pixel lookup is unusable, use: {}\n4. Validate: {}\n{}\n[/PIXEL:EXECUTION_ROUTE]",
            route["first_command"].as_str().unwrap_or(""),
            route["read"].as_str().unwrap_or(""),
            route["after_two_nonconverging_calls"]
                .as_str()
                .unwrap_or(""),
            route["validation"].as_str().unwrap_or(""),
            route["progression"].as_str().unwrap_or(""),
        );
    }
    format!(
        "[PIXEL:EXECUTION_ROUTE]\n1. Run: {}\n   If it returns no usable or relevant result, run exactly once: {}\n2. Read: {}\n3. If both Pixel calls do not converge, use: {}\n4. Validate: {}\n{}\n[/PIXEL:EXECUTION_ROUTE]",
        route["first_command"].as_str().unwrap_or(""),
        route["first_on_empty_or_irrelevant"].as_str().unwrap_or(""),
        route["read"].as_str().unwrap_or(""),
        route["after_two_nonconverging_calls"]
            .as_str()
            .unwrap_or(""),
        route["validation"].as_str().unwrap_or(""),
        route["progression"].as_str().unwrap_or(""),
    )
}

fn explicit_identifier(task: &str) -> Option<String> {
    let (_, tail) = task.split_once('`')?;
    let (identifier, _) = tail.split_once('`')?;
    let identifier = identifier.trim();
    (!identifier.is_empty()
        && identifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_:-./".contains(ch)))
    .then(|| identifier.to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Derive a bounded literal term for a native search fallback so the whole
/// task sentence is never handed to a fixed-string `rg`. Pick the longest
/// punctuation-free keyword; if the task has none, fall back to the task text
/// so the fixed-string search still targets a term that can match.
fn bounded_native_query(task: &str) -> String {
    task.split_whitespace()
        .map(|word| word.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .max_by_key(|word| word.chars().count())
        .and_then(|word| (!word.is_empty()).then(|| word.to_string()))
        .unwrap_or_else(|| task.to_string())
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Human-readable rendering that preserves the same bounded target details as
/// the JSON contract without making callers parse a second evidence shape.
pub fn pretty(brief: &Value) -> String {
    let mut output = String::new();
    output.push_str("execution brief v1\n");
    output.push_str(&format!(
        "task: {}\n",
        brief.get("task").and_then(Value::as_str).unwrap_or("")
    ));
    if let Some(workstreams) = brief.get("workstreams").and_then(Value::as_array) {
        for workstream in workstreams {
            output.push_str(&format!(
                "\n{} [{} / {}]\n",
                workstream.get("id").and_then(Value::as_str).unwrap_or("?"),
                workstream
                    .get("tier")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                workstream
                    .get("ownership")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ));
            if let Some(targets) = workstream.get("targets").and_then(Value::as_array) {
                for target in targets {
                    output.push_str(&format!(
                        "  {}\n",
                        target.get("path").and_then(Value::as_str).unwrap_or("?")
                    ));
                    if let Some(symbols) = target.get("symbols").and_then(Value::as_array) {
                        for symbol in symbols {
                            output.push_str(&format!(
                                "    symbol: {} {}\n",
                                symbol.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                symbol.get("name").and_then(Value::as_str).unwrap_or("?")
                            ));
                        }
                    }
                    if let Some(evidence) = target.get("evidence").and_then(Value::as_array) {
                        for item in evidence {
                            output.push_str(&format!(
                                "    evidence [{}:{}]: {}\n",
                                item.get("keyword").and_then(Value::as_str).unwrap_or("?"),
                                item.get("line").and_then(Value::as_u64).unwrap_or(0),
                                item.get("text").and_then(Value::as_str).unwrap_or("")
                            ));
                        }
                    }
                    if let Some(reasons) = target.get("reasons").and_then(Value::as_array) {
                        for reason in reasons.iter().filter_map(Value::as_str) {
                            output.push_str(&format!("    reason: {reason}\n"));
                        }
                    }
                }
            }
        }
    }
    if let Some(uncertainty) = brief.get("uncertainty") {
        output.push_str("\nuncertainty:\n");
        output.push_str("  closed_world: false\n  lower_bound: true\n");
        if let Some(caps) = uncertainty.get("caps").and_then(Value::as_array) {
            for cap in caps.iter().filter_map(Value::as_str) {
                output.push_str(&format!("  cap: {cap}\n"));
            }
        }
        output.push_str("  dependencies: unknown (depends_on is empty)\n");
    }
    if let Some(validation) = brief.get("validation").and_then(Value::as_array) {
        output.push_str("\nvalidation:\n");
        for item in validation.iter().filter_map(Value::as_str) {
            output.push_str(&format!("  - {item}\n"));
        }
    }
    output
}

fn collect_source_caps(data: &Value, caps: &mut BTreeSet<String>) {
    if let Some(source_caps) = data["envelope"]["caps"].as_array() {
        for cap in source_caps.iter().filter_map(Value::as_str) {
            let bounded = bounded_source_cap(cap, caps);
            caps.insert(bounded);
        }
    }
    if let Some(warnings) = data["warnings"].as_array() {
        for warning in warnings {
            if let Some(message) = warning["message"].as_str() {
                let bounded = bounded_source_cap(message, caps);
                caps.insert(bounded);
            }
        }
    }
    if let Some(closed_world) = data["closed_world"].as_str() {
        let bounded = bounded_source_cap(closed_world, caps);
        caps.insert(bounded);
    }
}

fn bounded_source_cap(text: &str, caps: &mut BTreeSet<String>) -> String {
    bounded_text(text, "source cap", MAX_TEXT_CHARS, caps)
}

fn target_projection(target: &Value, caps: &mut BTreeSet<String>) -> Value {
    let symbols = target["symbols"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_SYMBOLS_PER_TARGET, "symbols", caps);
            bounded_items(items, MAX_SYMBOLS_PER_TARGET, |symbol| {
                json!({
                    "kind": symbol["kind"].as_str().unwrap_or("?"),
                    "name": symbol["name"].as_str().unwrap_or("?"),
                    "uid": symbol["uid"].as_str().unwrap_or("?"),
                    "line": symbol["line"].as_u64().unwrap_or(0),
                })
            })
        })
        .unwrap_or_default();
    let evidence = target["evidence"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "evidence", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |item| {
                json!({
                    "keyword": item["keyword"].as_str().unwrap_or("?"),
                    "line": item["line"].as_u64().unwrap_or(0),
                    "text": bounded_text(
                        item["text"].as_str().unwrap_or(""),
                        "evidence text",
                        MAX_TEXT_CHARS,
                        caps,
                    ),
                })
            })
        })
        .unwrap_or_default();
    let reasons = target["reasons"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "reasons", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |reason| {
                Value::from(bounded_text(
                    reason.as_str().unwrap_or(""),
                    "reason",
                    MAX_TEXT_CHARS,
                    caps,
                ))
            })
        })
        .unwrap_or_default();
    json!({
        "path": target_path_value(target).unwrap_or("?"),
        "symbols": symbols,
        "evidence": evidence,
        "reasons": reasons,
    })
}

fn bounded_items<T>(items: &[Value], limit: usize, map: impl FnMut(&Value) -> T) -> Vec<T> {
    items.iter().take(limit).map(map).collect()
}

fn cap_items(items: &[Value], limit: usize, label: &str, caps: &mut BTreeSet<String>) {
    if items.len() > limit {
        caps.insert(format!("{label} per target capped at {limit}"));
    }
}

fn bounded_text(text: &str, label: &str, limit: usize, caps: &mut BTreeSet<String>) -> String {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        caps.insert(format!("{label} capped at {limit} characters"));
    }
    bounded
}

fn repository_area(path: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || ".".to_string(),
        |(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        },
    )
}

fn target_path(target: &Value) -> &str {
    target["path"].as_str().unwrap_or("?")
}

fn target_path_value(target: &Value) -> Option<&str> {
    target["path"].as_str().filter(|path| !path.is_empty())
}

fn tier_rank(target: &Value) -> u8 {
    match target["tier"].as_str() {
        Some("P0") => 0,
        Some("P1") => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tier: &str, path: &str) -> Value {
        json!({"tier": tier, "path": path})
    }

    fn brief_of(targets: Vec<Value>) -> Value {
        from_scope_task("task", &json!({"targets": targets}))
    }

    fn caps_of(brief: &Value) -> Vec<String> {
        brief["uncertainty"]["caps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| cap.as_str().unwrap().to_string())
            .collect()
    }

    fn target_count(brief: &Value) -> usize {
        brief["workstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workstream| workstream["targets"].as_array().unwrap().len())
            .sum()
    }

    #[test]
    fn targets_are_capped_at_one_hundred_and_the_cap_is_disclosed() {
        let make = |n: usize| {
            (0..n)
                .map(|i| target("P1", &format!("src/f{i:03}.rs")))
                .collect()
        };
        let exact = brief_of(make(MAX_TARGETS));
        assert_eq!(target_count(&exact), 100);
        assert!(caps_of(&exact).is_empty(), "{:?}", caps_of(&exact));
        let over = brief_of(make(MAX_TARGETS + 1));
        assert_eq!(target_count(&over), 100);
        assert_eq!(caps_of(&over), ["execution brief targets capped at 100"]);
    }

    #[test]
    fn a_workstream_cap_drops_read_context_before_any_write_workstream() {
        // 64 P1 areas named before the one P0 area: an area-ordered cap
        // would keep them all and lose the only write workstream.
        let mut targets: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        targets.push(target("P0", "zz/edit.rs"));
        let brief = brief_of(targets);
        let workstreams = brief["workstreams"].as_array().unwrap();
        assert_eq!(workstreams.len(), 64);
        assert_eq!(workstreams[0]["id"], "workstream:zz:P0");
        assert_eq!(workstreams[0]["ownership"], "write");
        assert_eq!(
            caps_of(&brief),
            ["execution brief workstreams capped at 64"]
        );
        assert!(
            !brief["validation"].to_string().contains("No P0 target"),
            "{}",
            brief["validation"]
        );

        let exact: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        let brief = brief_of(exact);
        assert_eq!(brief["workstreams"].as_array().unwrap().len(), 64);
        assert!(caps_of(&brief).is_empty());
    }

    #[test]
    fn per_target_lists_and_texts_are_cut_exactly_at_their_limits() {
        let symbols = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"kind": "fn", "name": format!("s{i}")}))
                .collect()
        };
        let at_limit = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS)}],
        })]);
        let projected = &at_limit["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert!(caps_of(&at_limit).is_empty(), "{:?}", caps_of(&at_limit));

        let over = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET + 1),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS + 1)}],
        })]);
        let projected = &over["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert_eq!(
            caps_of(&over),
            [
                "evidence text capped at 320 characters",
                "symbols per target capped at 32",
            ]
        );
    }

    #[test]
    fn more_than_thirty_two_caps_keep_thirty_two_plus_a_marker() {
        let source = |n: usize| -> Value {
            json!({
                "targets": [],
                "envelope": {"caps": (0..n).map(|i| format!("cap {i:02}")).collect::<Vec<_>>()},
            })
        };
        // The empty targets array adds no cap of its own.
        let exact = from_scope_task("t", &source(MAX_CAPS));
        assert_eq!(caps_of(&exact).len(), 32);
        let over = from_scope_task("t", &source(MAX_CAPS + 1));
        let caps = caps_of(&over);
        assert_eq!(caps.len(), 33);
        assert_eq!(caps[32], "execution brief caps capped at 32");
    }

    #[test]
    fn the_pretty_brief_carries_each_targets_reasons() {
        let brief = brief_of(vec![json!({
            "tier": "P0", "path": "src/login.rs", "reasons": ["defines login_user"],
        })]);
        let text = pretty(&brief);
        assert!(
            text.contains("  src/login.rs\n    reason: defines login_user\n"),
            "{text}"
        );
    }

    #[test]
    fn explicit_identifier_starts_exact_and_empty_result_routes_to_find_code() {
        let route = retrieval_route("Trace callers of `Foo::bar`");
        assert_eq!(
            route["first_command"],
            "rtk pixel search-content -F 'Foo::bar' --fallback-query 'Trace callers of `Foo::bar`' --no-daemon"
        );
        assert_eq!(
            route["first_operation"],
            json!({
                "subcommand": "search-content",
                "args": ["-F", "Foo::bar", "--fallback-query", "Trace callers of `Foo::bar`", "--no-daemon"]
            })
        );
        assert_eq!(route["automatic_empty_fallback"], true);
        assert_eq!(
            route["first_on_empty_or_irrelevant"],
            "rtk rg -m 5 -n -F -- 'Foo::bar' . | rtk sed -n '1,20p'"
        );
        let rendered = pretty_retrieval_route(&route);
        assert!(
            rendered.contains("runs the task-aware find-code fallback once in the same command")
        );
        assert!(rendered.contains("maximum 40-line window"));
        assert!(rendered.contains("warnings or prior-call counts alone never trigger it"));
        assert!(!rendered.contains("run exactly once: rtk pixel find-code"));
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::NoUsableResult),
            route["first_on_empty_or_irrelevant"].as_str()
        );
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::Usable),
            None
        );
        assert_eq!(
            next_retrieval_command(&route, 2, RetrievalOutcome::NoUsableResult),
            route["after_two_nonconverging_calls"].as_str()
        );
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::WarningOnly),
            None
        );
    }

    #[test]
    fn behavior_route_has_a_distinct_second_query_then_bounded_fallback() {
        let route = retrieval_route("How does task preparation refresh stale source evidence?");
        assert_eq!(route["automatic_empty_fallback"], false);
        assert_eq!(
            route["first_command"],
            "rtk pixel find-code 'How does task preparation refresh stale source evidence?'"
        );
        assert_eq!(
            route["first_operation"],
            json!({
                "subcommand": "find-code",
                "args": ["How does task preparation refresh stale source evidence?"]
            })
        );
        assert_eq!(
            route["first_on_empty_or_irrelevant"],
            "rtk pixel find-code 'How does task preparation refresh stale source evidence? implementation and callers'"
        );
        assert!(
            route["after_two_nonconverging_calls"]
                .as_str()
                .unwrap()
                .starts_with("rtk rg -m 5 -n -F -- '")
        );
        assert!(
            route["after_two_nonconverging_calls"]
                .as_str()
                .unwrap()
                .ends_with("| rtk sed -n '1,20p'")
        );
        assert_eq!(
            route["progression"],
            "A warning or prior-call count is informational, not a no-hit. Advance only when this invocation returns no usable match, an unresolved result, or an irrelevant result."
        );
    }

    #[test]
    fn identifier_shell_quoting_is_safe_and_invalid_backticks_do_not_select_exact_search() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        let route = retrieval_route("Find `two words` in code");
        assert!(
            route["first_command"]
                .as_str()
                .unwrap()
                .starts_with("rtk pixel find-code ")
        );
    }

    #[test]
    fn route_command_should_leave_known_flags_bare_and_quote_query_values() {
        let route = retrieval_route("Trace callers of `Foo::bar` in Livio's code");
        assert_eq!(
            route["first_command"],
            "rtk pixel search-content -F 'Foo::bar' --fallback-query 'Trace callers of `Foo::bar` in Livio'\\''s code' --no-daemon"
        );
    }
}
