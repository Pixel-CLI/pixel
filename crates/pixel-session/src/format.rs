// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Compact human formatting for the CLI (the `--json` path bypasses this and
//! serializes the query-layer structs directly).

use crate::query::{CursorResult, EnvFingerprint, ErrorList, HmrStatus, ShowResult, TestStatus};
use crate::store::GcOutcome;
use crate::types::{ErrorRecord, EventRecord, Frame, RunRecord};

/// `2s ago`, `5m ago`, `3h ago`, `2d ago`.
pub fn age(now_ms: i64, ts_ms: i64) -> String {
    let delta = (now_ms - ts_ms).max(0) / 1000;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

fn best_frame(record: &ErrorRecord) -> Option<&Frame> {
    let frames = record.frames.as_deref()?;
    frames
        .iter()
        .find(|f| {
            f.best_location().is_some_and(|(file, _, _)| {
                !file.contains("node_modules") && !file.starts_with("node:")
            })
        })
        .or_else(|| frames.iter().find(|f| f.best_location().is_some()))
}

/// The two-line compact rendering:
/// ```text
/// #412  2s ago   ×3  [browser-rejection] TypeError: undefined is not an object …
///       @ src/routes/chat.tsx:88:14  ← via @tanstack/react-router@1.130.2  [!] 2 physical copies
/// ```
pub fn error_lines(record: &ErrorRecord, now_ms: i64) -> String {
    let mut headline = record.message.replace('\n', " ");
    if let Some(kind) = &record.kind
        && !headline.starts_with(kind.as_str())
    {
        headline = format!("{kind}: {headline}");
    }
    let count = if record.count > 1 {
        format!("\u{d7}{}", record.count)
    } else {
        "\u{d7}1".to_owned()
    };
    let mut out = format!(
        "#{id}  {age:<8} {count:<4} [{surface}] {headline}",
        id = record.id,
        age = age(now_ms, record.last_ts),
        count = count,
        surface = record.surface.as_str(),
    );
    if let Some(frame) = best_frame(record)
        && let Some((file, line, column)) = frame.best_location()
    {
        let mut loc = format!("\n      @ {file}:{line}");
        if let Some(column) = column {
            loc.push_str(&format!(":{column}"));
        }
        if let Some(pkg) = &frame.pkg {
            loc.push_str(&format!(
                "  \u{2190} via {}{}",
                pkg.name,
                pkg.version
                    .as_deref()
                    .map(|v| format!("@{v}"))
                    .unwrap_or_default()
            ));
            if pkg.dup_paths.len() > 1 {
                loc.push_str(&format!("  [!] {} physical copies", pkg.dup_paths.len()));
            }
        }
        out.push_str(&loc);
    }
    out
}

pub fn render_error_list(list: &ErrorList, now_ms: i64) -> String {
    let mut out = String::new();
    if list.errors.is_empty() {
        out.push_str("no errors\n");
    } else {
        for record in &list.errors {
            out.push_str(&error_lines(record, now_ms));
            out.push('\n');
        }
    }
    out.push_str(&format!("cursor: {}\n", list.cursor));
    out
}

fn render_event(event: &EventRecord, now_ms: i64) -> String {
    let detail = event
        .data
        .as_ref()
        .map(|d| format!("  {d}"))
        .unwrap_or_default();
    format!(
        "{:<9} [{}]{}",
        age(now_ms, event.ts),
        event.kind.as_str(),
        detail
    )
}

fn render_run(run: &RunRecord) -> String {
    let mut out = format!("run {}", run.run_id);
    if let Some(pid) = run.pid {
        out.push_str(&format!("  pid {pid}"));
    }
    if let Some(port) = run.port {
        out.push_str(&format!("  port {port}"));
    }
    if let Some(head) = &run.git_head {
        out.push_str(&format!("  head {}", &head[..head.len().min(12)]));
    }
    if let Some(hash) = &run.lockfile_hash {
        out.push_str(&format!("  lockfile {}", &hash[..hash.len().min(12)]));
    }
    if let Some(hash) = &run.vite_dep_hash {
        out.push_str(&format!("  vite-deps {}", &hash[..hash.len().min(12)]));
    }
    if let Some(changed) = &run.changed_since_last_run
        && !changed.is_empty()
    {
        out.push_str(&format!("  changed-since-last-run: {}", changed.join(", ")));
    }
    out
}

pub fn render_show(result: &ShowResult, now_ms: i64) -> String {
    let mut out = error_lines(&result.error, now_ms);
    out.push('\n');
    out.push_str(&format!(
        "first seen {}, seen {} time(s)\n",
        age(now_ms, result.error.first_ts),
        result.error.count
    ));
    if let Some(stack) = &result.error.stack_raw {
        out.push_str("stack:\n");
        for line in stack.lines().take(15) {
            out.push_str(&format!("  {line}\n"));
        }
    }
    if let Some(values) = &result.error.values {
        out.push_str(&format!("values: {values}\n"));
    }
    if let Some(http) = &result.error.http {
        out.push_str(&format!("http: {http}\n"));
    }
    if let Some(extra) = &result.error.extra {
        out.push_str(&format!("extra: {extra}\n"));
    }
    if let Some(run) = &result.run {
        out.push_str(&format!("{}\n", render_run(run)));
    }
    if !result.correlated_events.is_empty() {
        out.push_str("events within \u{b1}30s:\n");
        for event in &result.correlated_events {
            out.push_str(&format!("  {}\n", render_event(event, now_ms)));
        }
    }
    out
}

pub fn render_hmr(status: &HmrStatus, now_ms: i64) -> String {
    let mut out = String::new();
    match &status.last_update {
        Some(event) => {
            out.push_str(&format!("last update: {}\n", render_event(event, now_ms)));
        }
        None => out.push_str("no hmr updates recorded\n"),
    }
    for event in &status.events {
        out.push_str(&format!("  {}\n", render_event(event, now_ms)));
    }
    out
}

pub fn render_env(env: &EnvFingerprint) -> String {
    let mut out = String::new();
    match &env.run {
        Some(run) => out.push_str(&format!("{}\n", render_run(run))),
        None => out.push_str("no runs recorded\n"),
    }
    if let Some(previous) = &env.previous {
        out.push_str(&format!("previous: {}\n", render_run(previous)));
    }
    if let Some(changed) = &env.changed {
        if changed.is_empty() {
            out.push_str("changed: nothing (or no previous run)\n");
        } else {
            out.push_str("changed:\n");
            for change in changed {
                out.push_str(&format!("  {change}\n"));
            }
        }
    }
    out
}

pub fn render_test(status: &TestStatus, now_ms: i64) -> String {
    let mut out = String::new();
    match status.passing {
        Some(true) => out.push_str("passing\n"),
        Some(false) => out.push_str("failing\n"),
        None => out.push_str("no test runs recorded\n"),
    }
    if let Some(failure) = &status.latest_failure {
        out.push_str(&error_lines(failure, now_ms));
        out.push('\n');
        if let Some(extra) = &failure.extra {
            out.push_str(&format!("failures: {extra}\n"));
        }
    }
    if let Some(pass) = &status.latest_pass {
        out.push_str(&format!("last green: {}\n", render_event(pass, now_ms)));
    }
    out
}

pub fn render_cursor(result: &CursorResult) -> String {
    format!("cursor: {}\n", result.cursor)
}

pub fn render_gc(outcome: &GcOutcome) -> String {
    format!(
        "deleted {} error(s), {} event(s){}\n",
        outcome.errors_deleted,
        outcome.events_deleted,
        if outcome.vacuumed { ", vacuumed" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FramePackage, Surface};

    fn fixture_record() -> ErrorRecord {
        ErrorRecord {
            id: 412,
            first_ts: 0,
            last_ts: 98_000,
            count: 3,
            run_id: None,
            surface: Surface::BrowserRejection,
            kind: Some("TypeError".into()),
            message: "undefined is not an object (evaluating 'api.sessions.x')".into(),
            stack_raw: None,
            frames: Some(vec![Frame {
                raw: "at chat".into(),
                file: Some("src/routes/chat.tsx".into()),
                line: Some(88),
                column: Some(14),
                pkg: Some(FramePackage {
                    name: "@tanstack/react-router".into(),
                    version: Some("1.130.2".into()),
                    path: None,
                    dup_paths: vec!["a".into(), "b".into()],
                }),
                ..Frame::default()
            }]),
            values: None,
            http: None,
            extra: None,
            dedup_hash: "x".into(),
        }
    }

    #[test]
    fn matches_plan_example_shape() {
        let rendered = error_lines(&fixture_record(), 100_000);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "#412  2s ago   \u{d7}3   [browser-rejection] TypeError: undefined is not an object (evaluating 'api.sessions.x')"
        );
        assert_eq!(
            lines[1],
            "      @ src/routes/chat.tsx:88:14  \u{2190} via @tanstack/react-router@1.130.2  [!] 2 physical copies"
        );
    }

    #[test]
    fn kind_not_duplicated_when_message_already_prefixed() {
        let mut record = fixture_record();
        record.message = "TypeError: boom".into();
        let rendered = error_lines(&record, 100_000);
        assert!(rendered.contains("] TypeError: boom"));
        assert!(!rendered.contains("TypeError: TypeError"));
    }

    #[test]
    fn ages() {
        assert_eq!(age(100_000, 98_000), "2s ago");
        assert_eq!(age(400_000, 100_000), "5m ago");
        assert_eq!(age(8_000_000, 500_000), "2h ago");
        assert_eq!(age(200_000_000, 10_000_000), "2d ago");
    }

    #[test]
    fn list_footer_prints_cursor() {
        let list = ErrorList {
            errors: vec![fixture_record()],
            cursor: 412,
        };
        let rendered = render_error_list(&list, 100_000);
        assert!(rendered.ends_with("cursor: 412\n"));
    }
}
