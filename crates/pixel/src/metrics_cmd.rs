// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel metrics`: the journal's and the action log's numbers over a window
//! (`specs/metrics.md`). Reads the two files, writes nothing, and labels what
//! it counted apart from what it estimated.

use std::io::Read;
use std::path::Path;

use pixel_metrics::report::journal_report;
use pixel_metrics::{ActionEvent, ActionLog, summarize_metrics};
use serde_json::{Value, json};

use crate::client;
use crate::commands::Done;

/// A file is read up to this many bytes.
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

/// What `pixel metrics` was asked.
pub(crate) struct Request<'a> {
    pub since: &'a str,
    pub path: &'a Path,
    pub json: bool,
}

/// The milliseconds a window such as `30m`, `24h`, `7d` or `all` spans;
/// `None` for `all`.
fn window_ms(since: &str) -> Result<Option<u64>, String> {
    let text = since.trim();
    if text == "all" {
        return Ok(None);
    }
    let bad = || format!("--since takes 30m, 24h, 7d or all, not {since:?}");
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let unit_ms: u64 = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(bad()),
    };
    count.checked_mul(unit_ms).map(Some).ok_or_else(bad)
}

/// The lines of `path`, empty when there is no file.
fn lines(path: &Path) -> Vec<String> {
    let Ok(file) = pixel_io::nofollow::open_read(path) else {
        return Vec::new();
    };
    let mut text = String::new();
    let _ = file.take(MAX_READ_BYTES).read_to_string(&mut text);
    text.lines().map(str::to_owned).collect()
}

/// A count rendered for a person.
fn show(value: &Value) -> String {
    match value {
        Value::Null => "n/a".to_owned(),
        Value::Number(number) => number
            .as_f64()
            .filter(|_| !number.is_u64() && !number.is_i64())
            .map_or_else(|| number.to_string(), |float| format!("{float:.2}")),
        other => other.to_string(),
    }
}

fn map_line(map: &Value) -> String {
    let entries: Vec<String> = map
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, count)| format!("{key} {}", show(count)))
        .collect();
    if entries.is_empty() {
        "none".to_owned()
    } else {
        entries.join(", ")
    }
}

fn render(report: &Value, window: &str) -> String {
    let (briefs, turns, edit) = (&report["briefs"], &report["turns"], &report["edit_hooks"]);
    let mut out = format!("pixel metrics: {window}\n\nCounted (journal)\n");
    let rows = [
        ("briefs delivered", show(&briefs["delivered"])),
        ("briefs declined", show(&briefs["declined"])),
        ("  by tier", map_line(&briefs["by_tier"])),
        (
            "  delivered by kind",
            map_line(&briefs["delivered_by_kind"]),
        ),
        (
            "  declined because",
            map_line(&briefs["declined_by_reason"]),
        ),
        ("briefed turns", show(&turns["briefed"])),
        (
            "  zero-call turns",
            format!(
                "{} (rate {})",
                show(&turns["zero_call"]),
                show(&turns["zero_call_rate"])
            ),
        ),
        ("  calls per turn", show(&turns["mean_calls_briefed"])),
        ("turns without a brief", show(&turns["unbriefed"])),
        ("  calls per turn", show(&turns["mean_calls_unbriefed"])),
        (
            "injected files read or edited",
            format!(
                "precision {}, recall {}",
                show(&turns["precision"]),
                show(&turns["recall"])
            ),
        ),
        ("edit cards", show(&edit["cards"])),
        (
            "stale-caller notes",
            format!(
                "{} ({} sites)",
                show(&edit["stale_events"]),
                show(&edit["stale_sites"])
            ),
        ),
        (
            "stop checks / blocks",
            format!(
                "{} / {}",
                show(&edit["stop_checks"]),
                show(&edit["stop_blocks"])
            ),
        ),
        ("  silent because", map_line(&edit["silent"])),
    ];
    for (label, value) in rows {
        out.push_str(&format!("  {label:<30} {value}\n"));
    }
    out.push_str(&format!(
        "\nDerived\n  stale sites fixed before Stop  {}\n",
        show(&edit["stale_sites_fixed_before_stop"])
    ));
    out.push_str("\nCommands (action log; tokens and time are estimates, never measurements)\n");
    let commands = &report["commands"];
    out.push_str(&format!(
        "  records                        {} ({} duplicate, {} without metrics)\n",
        show(&commands["unique_records"]),
        show(&commands["duplicate_records"]),
        show(&commands["legacy_records"]),
    ));
    for (version, groups) in commands["versions"].as_object().into_iter().flatten() {
        for (coverage, totals) in groups.as_object().into_iter().flatten() {
            let estimated = &totals["estimated"];
            let saved = if estimated["saved_tokens"].is_null() {
                "unavailable: no comparable baseline".to_owned()
            } else {
                format!(
                    "{} tokens saved (estimate)",
                    show(&estimated["saved_tokens"])
                )
            };
            out.push_str(&format!(
                "  {version} / {coverage:<11}  {} commands, {} B answered (measured), {saved}\n",
                show(&totals["operations"]),
                show(&totals["measured"]["output_bytes"]),
            ));
        }
    }
    for group in commands["time_estimates"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} / {:<11}    {} commands, {} ms saved (sequential estimate at {} ms a round trip)\n",
            group["time_estimator_version"].as_str().unwrap_or("?"),
            group["coverage"].as_str().unwrap_or("?"),
            show(&group["operations"]),
            show(&group["estimated"]["saved_ms"]),
            show(&group["round_trip_ms"]),
        ));
    }
    out
}

pub(crate) fn run(request: &Request) -> Result<Done, String> {
    let window = window_ms(request.since)?;
    let root = client::discover_root(request.path)?;
    let now = pixel_metrics::now_ms().unsigned_abs();
    let since = window.map_or(0, |span| now.saturating_sub(span));
    let journal: Vec<Value> = lines(&pixel_metrics::journal::path_in(&root))
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let events: Vec<ActionEvent> = lines(&ActionLog::path_for_root(&root))
        .iter()
        .filter_map(|line| serde_json::from_str::<ActionEvent>(line).ok())
        .filter(|event| event.ts_ms.unsigned_abs() >= since)
        .collect();
    let mut report = journal_report(&journal, since);
    report["commands"] = summarize_metrics(&events);
    report["window"] = json!(request.since);
    let text = render(&report, &format!("since {}", request.since));
    Ok(Done::new(report, text, request.json))
}

#[cfg(test)]
mod tests;
