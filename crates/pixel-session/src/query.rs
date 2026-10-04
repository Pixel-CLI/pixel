// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! THE shared query layer — `pixel list-errors` is a thin wrapper over these
//! functions. Every result type is `Serialize`, and the CLI `--json` output
//! is that serialization.

use serde::Serialize;

use crate::store::{GcOutcome, Result, Store};
use crate::types::{ErrorRecord, EventKind, EventRecord, RunRecord, Surface};

/// ±window for events correlated with an error in `show`.
const CORRELATION_WINDOW_MS: i64 = 30_000;
const SINCE_LIMIT: i64 = 200;

#[derive(Debug, Serialize)]
pub struct ErrorList {
    pub errors: Vec<ErrorRecord>,
    /// Highest error id in the store — quote it back to `since`.
    pub cursor: i64,
}

#[derive(Debug, Serialize)]
pub struct ShowResult {
    pub error: ErrorRecord,
    /// Lifecycle events within ±30 s of the error's last occurrence.
    pub correlated_events: Vec<EventRecord>,
    /// The run the error belongs to (or the latest run), with its
    /// changed-since-last-run diff — the "bun install under a stale server"
    /// smoking gun rides here.
    pub run: Option<RunRecord>,
}

#[derive(Debug, Serialize)]
pub struct HmrStatus {
    /// Most recent hmr-update (or full-reload) matching the filter.
    pub last_update: Option<EventRecord>,
    pub events: Vec<EventRecord>,
}

#[derive(Debug, Serialize)]
pub struct EnvFingerprint {
    pub run: Option<RunRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<RunRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct TestStatus {
    pub latest_failure: Option<ErrorRecord>,
    pub latest_pass: Option<EventRecord>,
    /// true = last signal green, false = last signal red, null = no signal.
    pub passing: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct CursorResult {
    pub cursor: i64,
}

pub fn last(store: &Store, n: i64, surface: Option<Surface>) -> Result<ErrorList> {
    Ok(ErrorList {
        errors: store.last_errors(n, surface)?,
        cursor: store.max_cursor()?,
    })
}

pub fn since(store: &Store, cursor: i64) -> Result<ErrorList> {
    Ok(ErrorList {
        errors: store.errors_since(cursor, SINCE_LIMIT)?,
        cursor: store.max_cursor()?,
    })
}

pub fn since_ts(store: &Store, ts: i64) -> Result<ErrorList> {
    Ok(ErrorList {
        errors: store.errors_since_ts(ts, SINCE_LIMIT)?,
        cursor: store.max_cursor()?,
    })
}

pub fn show(store: &Store, id: i64) -> Result<Option<ShowResult>> {
    let Some(error) = store.get_error(id)? else {
        return Ok(None);
    };
    let correlated_events = store.events_between(
        error.last_ts - CORRELATION_WINDOW_MS,
        error.last_ts + CORRELATION_WINDOW_MS,
    )?;
    let runs = store.latest_runs(2)?;
    let run = match &error.run_id {
        Some(run_id) => runs
            .iter()
            .find(|r| &r.run_id == run_id)
            .cloned()
            .or_else(|| runs.first().cloned()),
        None => runs.first().cloned(),
    };
    Ok(Some(ShowResult {
        error,
        correlated_events,
        run,
    }))
}

pub fn search(store: &Store, text: &str, limit: i64) -> Result<ErrorList> {
    Ok(ErrorList {
        errors: store.search_errors(text, limit)?,
        cursor: store.max_cursor()?,
    })
}

/// "Was my edit applied?" — recent HMR-related lifecycle events, optionally
/// filtered to those touching `file`.
pub fn hmr(store: &Store, file: Option<&str>) -> Result<HmrStatus> {
    let kinds = [
        EventKind::HmrUpdate,
        EventKind::FullReload,
        EventKind::DepOptimized,
        EventKind::ServerStart,
    ];
    let mut events = store.latest_events(&kinds, 50)?;
    if let Some(file) = file {
        // Keep non-HMR lifecycle context; filter hmr-update events to the file.
        events.retain(|e| e.kind != EventKind::HmrUpdate || event_touches_file(e, file));
    }
    events.truncate(20);
    let last_update = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::HmrUpdate | EventKind::FullReload))
        .cloned();
    Ok(HmrStatus {
        last_update,
        events,
    })
}

fn event_touches_file(event: &EventRecord, file: &str) -> bool {
    event
        .data
        .as_ref()
        .and_then(|d| d.get("files"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|f| f.contains(file))
        })
}

pub fn env(store: &Store, diff: bool) -> Result<EnvFingerprint> {
    let mut runs = store.latest_runs(2)?;
    let run = if runs.is_empty() {
        None
    } else {
        Some(runs.remove(0))
    };
    let previous = if runs.is_empty() {
        None
    } else {
        Some(runs.remove(0))
    };
    if !diff {
        return Ok(EnvFingerprint {
            run,
            previous: None,
            changed: None,
        });
    }
    let changed = match (&run, &previous) {
        (Some(current), Some(prior)) => {
            let mut changes = Vec::new();
            let mut field = |name: &str, a: &Option<String>, b: &Option<String>| {
                if a != b {
                    changes.push(format!(
                        "{name}: {} -> {}",
                        b.as_deref().unwrap_or("-"),
                        a.as_deref().unwrap_or("-"),
                    ));
                }
            };
            field("git_head", &current.git_head, &prior.git_head);
            field(
                "lockfile_hash",
                &current.lockfile_hash,
                &prior.lockfile_hash,
            );
            field(
                "vite_dep_hash",
                &current.vite_dep_hash,
                &prior.vite_dep_hash,
            );
            if current.port != prior.port {
                changes.push(format!(
                    "port: {} -> {}",
                    prior.port.map_or("-".into(), |p| p.to_string()),
                    current.port.map_or("-".into(), |p| p.to_string()),
                ));
            }
            Some(changes)
        }
        _ => Some(Vec::new()),
    };
    Ok(EnvFingerprint {
        run,
        previous,
        changed,
    })
}

/// The surfaces whose failure records are test runs (`sniper test`).
pub const TEST_SURFACES: [Surface; 3] = [Surface::Vitest, Surface::Minitest, Surface::Rspec];

pub fn test_status(store: &Store) -> Result<TestStatus> {
    let mut candidates: Vec<ErrorRecord> = Vec::new();
    for surface in TEST_SURFACES {
        candidates.extend(store.latest_error_by_surface(surface)?);
    }
    // Newest row across the test surfaces; ids are unique so there is no tie.
    let latest_failure = candidates.into_iter().max_by_key(|record| record.id);
    let latest_pass = store.latest_event_by_kind(EventKind::TestPass)?;
    let passing = match (&latest_failure, &latest_pass) {
        (Some(failure), Some(pass)) => Some(pass.ts >= failure.last_ts),
        (Some(_), None) => Some(false),
        (None, Some(_)) => Some(true),
        (None, None) => None,
    };
    Ok(TestStatus {
        latest_failure,
        latest_pass,
        passing,
    })
}

pub fn cursor(store: &Store) -> Result<CursorResult> {
    Ok(CursorResult {
        cursor: store.max_cursor()?,
    })
}

pub fn gc(store: &Store, vacuum: bool) -> Result<GcOutcome> {
    store.gc(vacuum)
}

/// Parse a human duration like `5m`, `30s`, `2h`, `1d` into milliseconds.
///
/// Returns `None` for anything else, including a window that cannot be
/// represented: the text comes from `--ts`, so an invalid unit (a trailing
/// multi-byte character) or an overflowing value is an error to report, never
/// a panic or a wrapped window.
pub fn parse_duration_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // `--ts` comes from the command line: cut on a char boundary so a trailing
    // multi-byte character is an invalid unit (`None`), not a panic.
    let (digits, unit) = text.split_at(text.floor_char_boundary(text.len() - 1));
    let (digits, multiplier) = match unit {
        "s" => (digits, 1_000),
        "m" => (digits, 60_000),
        "h" => (digits, 3_600_000),
        "d" => (digits, 86_400_000),
        _ if unit.chars().all(|c| c.is_ascii_digit()) => (text, 1_000),
        _ => return None,
    };
    let value: i64 = digits.parse().ok()?;
    value.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("5m"), Some(300_000));
        assert_eq!(parse_duration_ms("30s"), Some(30_000));
        assert_eq!(parse_duration_ms("2h"), Some(7_200_000));
        assert_eq!(parse_duration_ms("1d"), Some(86_400_000));
        assert_eq!(parse_duration_ms("45"), Some(45_000));
        assert_eq!(parse_duration_ms("nope"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn parse_duration_ms_should_refuse_a_non_ascii_unit() {
        assert_eq!(parse_duration_ms("é"), None);
        assert_eq!(parse_duration_ms("5é"), None);
    }

    #[test]
    fn parse_duration_ms_should_refuse_a_window_that_overflows() {
        assert_eq!(parse_duration_ms("9223372036854775807d"), None);
        assert_eq!(parse_duration_ms("9223372036854775807"), None);
        // Just below the boundary: 9_223_372_036_854_775 s still fits in ms.
        assert_eq!(
            parse_duration_ms("9223372036854775s"),
            Some(9_223_372_036_854_775_000)
        );
    }
}
