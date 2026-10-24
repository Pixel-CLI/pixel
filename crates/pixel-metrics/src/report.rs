// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The numbers `pixel metrics` prints, derived from the journal's typed lines
//! (`specs/metrics.md`). Pure: lines in, a JSON value out, deterministic for a
//! fixed journal and window. The CLI's own estimated savings come from
//! [`crate::summarize_metrics`]; nothing here estimates anything, and what is
//! derived rather than counted says so.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

/// A line of kind `kind`.
fn is(line: &Value, kind: &str) -> bool {
    line.get("t").and_then(Value::as_str) == Some(kind)
}

/// `numerator / denominator`, `None` when there is nothing to divide by.
fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn count_by(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_owned()).or_default() += 1;
}

fn strings(line: &Value, key: &str) -> BTreeSet<String> {
    line.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The briefs: how many were delivered and declined, and by what.
fn briefs(lines: &[&Value]) -> Value {
    let (mut delivered, mut declined) = (0_u64, 0_u64);
    let (mut tiers, mut kinds, mut reasons) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for line in lines.iter().filter(|line| is(line, "brief")) {
        count_by(&mut tiers, line["tier"].as_str().unwrap_or("none"));
        if line["bytes"].as_u64().unwrap_or(0) > 0 {
            delivered += 1;
            count_by(&mut kinds, line["kind"].as_str().unwrap_or("none"));
        } else {
            declined += 1;
            count_by(&mut reasons, line["reason"].as_str().unwrap_or("none"));
        }
    }
    json!({
        "delivered": delivered, "declined": declined,
        "by_tier": tiers, "delivered_by_kind": kinds, "declined_by_reason": reasons,
    })
}

/// The turns: calls before the first answer, with and without a brief, and how
/// much of what the brief named the agent read or edited anyway.
fn turns(lines: &[&Value]) -> Value {
    let (mut briefed, mut zero_call, mut briefed_calls) = (0_u64, 0_u64, 0_u64);
    let (mut plain, mut plain_calls) = (0_u64, 0_u64);
    let (mut injected, mut used, mut hits) = (0_u64, 0_u64, 0_u64);
    for line in lines.iter().filter(|line| is(line, "turn")) {
        let calls = line["calls"].as_u64().unwrap_or(0);
        if line["briefed"].as_bool() == Some(true) {
            briefed += 1;
            briefed_calls += calls;
            zero_call += u64::from(calls == 0);
            let named = strings(line, "injected");
            let touched: BTreeSet<String> = strings(line, "read")
                .union(&strings(line, "edited"))
                .cloned()
                .collect();
            injected += named.len() as u64;
            used += touched.len() as u64;
            hits += named.intersection(&touched).count() as u64;
        } else {
            plain += 1;
            plain_calls += calls;
        }
    }
    let mean = |calls: u64, turns: u64| ratio(calls, turns);
    json!({
        "briefed": briefed,
        "zero_call": zero_call,
        "zero_call_rate": ratio(zero_call, briefed),
        "mean_calls_briefed": mean(briefed_calls, briefed),
        "unbriefed": plain,
        "mean_calls_unbriefed": mean(plain_calls, plain),
        "injected_paths": injected,
        "touched_paths": used,
        "injected_and_touched": hits,
        "precision": ratio(hits, injected),
        "recall": ratio(hits, used),
    })
}

/// The edit hooks: cards, stale sites, Stop checks and blocks.
fn edits(lines: &[&Value]) -> Value {
    let injected =
        |line: &Value| line["silent"].is_null() && line["bytes"].as_u64().unwrap_or(0) > 0;
    let (mut cards, mut stale_events, mut stale_sites) = (0_u64, 0_u64, 0_u64);
    let (mut checks, mut blocks, mut blocked_sites) = (0_u64, 0_u64, 0_u64);
    let mut silent = BTreeMap::new();
    for line in lines {
        let Some(kind @ ("edit" | "stale" | "guard")) = line["t"].as_str() else {
            continue;
        };
        if injected(line) {
            let count = line["count"].as_u64().unwrap_or(0);
            match kind {
                "edit" => cards += 1,
                "stale" => {
                    stale_events += 1;
                    stale_sites += count;
                }
                _ => {
                    blocks += 1;
                    blocked_sites += count;
                }
            }
        } else {
            count_by(
                &mut silent,
                &format!("{kind}:{}", line["silent"].as_str().unwrap_or("none")),
            );
        }
        checks += u64::from(kind == "guard");
    }
    json!({
        "cards": cards,
        "stale_events": stale_events,
        "stale_sites": stale_sites,
        "stop_checks": checks,
        "stop_blocks": blocks,
        "blocked_sites": blocked_sites,
        // Derived: sites reported stale less the sites Stop still found stale.
        "stale_sites_fixed_before_stop": stale_sites.saturating_sub(blocked_sites),
        "silent": silent,
    })
}

/// The journal report over the lines at or after `since_ms`.
#[must_use]
pub fn journal_report(lines: &[Value], since_ms: u64) -> Value {
    let window: Vec<&Value> = lines
        .iter()
        .filter(|line| line["ts"].as_u64().unwrap_or(0) >= since_ms)
        .collect();
    json!({
        "since_ms": since_ms,
        "lines": window.len(),
        "briefs": briefs(&window),
        "turns": turns(&window),
        "edit_hooks": edits(&window),
    })
}

#[cfg(test)]
mod tests;
