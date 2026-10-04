// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Engine 3 ranking signals — activity (churn) + session (recent working
//! set) + live error-sink join.
//!
//! These are *rerankers*, never candidate channels: they reorder within a
//! tier but never promote across tiers (protects the closed-world claim).
//!
//! P1-2 structural verification — RANK-BY-IN-DEGREE. The `fan_in` signal
//! (in-degree / number of dependents per candidate) is complete AND
//! wired end-to-end, but deliberately stays a *reranker*, not a candidate
//! channel: it only multiplies the RRF score of files that ALREADY made the
//! candidate set (see `rerank::rerank`: `1 + 0.20*fan_in_norm`). It
//! never introduces a file that has only dependents but no lexical/graph
//! evidence, and it never promotes across tiers — so it cannot break the
//! closed-world claim by surfacing a structural hit outside the announced
//! tier set. It is deterministic and already calculable from impact edges
//! (`graph.db` `calls` edges — see `pixel-daemon::api::fan_in_counts`), so
//! the structural claim is verified at this surface: complete as a reranker,
//! and correctly closed-world as a candidate channel (it is not one).
//! Everything here is deterministic — same repo state + session journal +
//! error sink ⇒ byte-identical output.
//!
//! The pure scorer [`score_signals`] takes caller-supplied raw inputs so it
//! is unit-testable without a git repo or a session store. The I/O
//! convenience [`compute_signals`] gathers the activity map (from
//! `history.db` when the caller supplies it, else a one-shot `git log
//! --since=90.days --name-only` fallback) and takes the session events and the
//! error-sink store from its caller: a caller that passes `session_store:
//! None` and no events (as `pixel-daemon` does today) leaves both channels
//! empty by construction — only the activity channel is live in production.

use std::collections::{HashMap, HashSet};

use pixel_git::GitRunner;
use pixel_session::store::Store;
use pixel_session::types::ErrorRecord;

/// The kind of a session event, mirroring PLAN.md's
/// `session_events(ts, session_id, kind: read|edit|resolve_hit|targets_hit|error, path, detail)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventKind {
    Read,
    Edit,
    ResolveHit,
    TargetsHit,
    Error,
}

impl SessionEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionEventKind::Read => "read",
            SessionEventKind::Edit => "edit",
            SessionEventKind::ResolveHit => "resolve_hit",
            SessionEventKind::TargetsHit => "targets_hit",
            SessionEventKind::Error => "error",
        }
    }
}

/// One session event fed by PostToolUse hooks and pixel's own
/// resolve/targets hits. `ts_ms` is milliseconds since epoch.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub ts_ms: i64,
    pub kind: SessionEventKind,
    pub path: String,
    pub detail: Option<String>,
}

/// Tunable weights for the rerank formula. Defaults match PLAN.md:
/// `final = rrf * (1 + 0.15*activity_norm + 0.35*session_norm) * test_penalty`.
#[derive(Debug, Clone)]
pub struct SignalOptions {
    /// Reference "now" in ms since epoch. Determinism requires the caller
    /// to pin this (e.g. once per query) rather than calling the clock per
    /// candidate.
    pub now_ms: i64,
    /// Bumped whenever the signal weights change; part of `inputs_digest`.
    pub weights_version: u32,
    /// Activity half-life in days (14).
    pub activity_half_life_days: f64,
    /// Session half-life in minutes (30).
    pub session_half_life_minutes: f64,
    /// Session events older than this (ms) are ignored (24h).
    pub session_window_ms: i64,
    /// `activity_norm` coefficient (0.15).
    pub activity_weight: f64,
    /// `session_norm` coefficient (0.35).
    pub session_weight: f64,
    /// `fan_in_norm` coefficient (0.20) — symbols with more dependents rank
    /// higher.
    ///
    /// Deterministic, already calculable from impact edges (graph `calls`
    /// in-degree per candidate file: `pixel-daemon::api::fan_in_counts`),
    /// and — like every signal herein — applies ONLY as a rerank multiplier,
    /// never as a candidate channel: a file with dependents but no
    /// candidate-set membership is never introduced,and tier membership is
    /// never changed (P0/P1/P2 are fixed before reranking).
    pub fan_in_weight: f64,
    /// Multiplier applied when the query mentions test/spec (0.7).
    pub test_penalty: f64,
}

impl Default for SignalOptions {
    fn default() -> Self {
        SignalOptions {
            now_ms: 0,
            weights_version: 1,
            activity_half_life_days: 14.0,
            session_half_life_minutes: 30.0,
            session_window_ms: 24 * 60 * 60 * 1000,
            activity_weight: 0.15,
            session_weight: 0.35,
            fan_in_weight: 0.20,
            test_penalty: 0.7,
        }
    }
}

/// The per-path signal bundle consumed by [`crate::rerank::rerank`].
/// `activity` and `session` are already normalized over the candidate set
/// (max = 1.0), so the reranker can multiply them directly.
#[derive(Debug, Clone, Default)]
pub struct SignalBundle {
    /// Normalized activity (churn) per path.
    pub activity: HashMap<String, f64>,
    /// Normalized session + error-sink weight per path.
    pub session: HashMap<String, f64>,
    /// Normalized fan-in (in-degree / dependents) per path.
    pub fan_in: HashMap<String, f64>,
    /// Human-readable session reasons ("edited 12m ago").
    pub session_reasons: Vec<String>,
    /// Human-readable error-sink reasons ("matches live error #42").
    pub error_reasons: Vec<String>,
    /// Named reason the git-log activity scan produced no map (git failure,
    /// timeout, output cap), `None` when a scan ran or no scan reported one.
    ///
    /// "Unavailable" is not "clean repository": the caller must not read an
    /// empty `activity` map as "no recent churn" without checking this.
    pub activity_unavailable: Option<String>,
}

/// Errors from gathering signals.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("git activity scan failed: {0}")]
    Git(#[from] pixel_git::GitError),
    #[error("session store error: {0}")]
    Session(String),
}

impl From<pixel_session::StoreError> for SignalError {
    fn from(e: pixel_session::StoreError) -> Self {
        SignalError::Session(e.to_string())
    }
}

/// True when `task` mentions test/spec.
pub fn mentions_tests(task: &str) -> bool {
    task.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "test" | "tests" | "spec" | "specs"
        )
    })
}

/// `0.7` when `task` does NOT mention test/spec (a test file is a worse
/// target for a non-test task), else `1.0` (task is about tests → no penalty).
pub fn test_penalty_for(task: &str) -> f64 {
    if mentions_tests(task) { 1.0 } else { 0.7 }
}

/// True when `path` looks like a test file: a `test`/`tests`/`__tests__`
/// directory component, a `*_test.*` filename, or a `*.spec.*` / `*.test.*`
/// filename. Case-insensitive.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let comps: Vec<&str> = lower.split('/').collect();
    // Directory component: /tests?/ or __tests__.
    if comps
        .iter()
        .any(|c| *c == "test" || *c == "tests" || *c == "__tests__")
    {
        return true;
    }
    let file = comps.last().copied().unwrap_or("");
    let parts: Vec<&str> = file.split('.').collect();
    // *_test.*
    if parts.first().is_some_and(|stem| stem.ends_with("_test")) {
        return true;
    }
    // *.spec.* / *.test.*
    if parts.len() >= 2 {
        let penultimate = parts[parts.len() - 2];
        if penultimate == "spec" || penultimate == "test" {
            return true;
        }
    }
    false
}

/// Build a per-candidate penalty closure for [`crate::rerank::rerank`]:
/// applies `test_penalty` to test paths only when `mentions_tests` is false,
/// else `1.0` for every path. A test file is a *worse* target for a non-test
/// task, so it is demoted only when the task does NOT mention tests/specs;
/// when the task is about tests, the penalty is gated off. The
/// task-mentions-tests gate lives here.
pub fn test_penalty_fn(mentions_tests: bool, test_penalty: f64) -> impl Fn(&str) -> f64 {
    move |path: &str| {
        if !mentions_tests && is_test_path(path) {
            test_penalty
        } else {
            1.0
        }
    }
}

/// Pure scorer: normalize activity, fold session events + error-sink weights
/// into one session channel, and produce human-readable reasons. No I/O.
pub fn score_signals(
    activity_raw: &HashMap<String, f64>,
    dirty_paths: &[String],
    session_events: &[SessionEvent],
    error_records: &[ErrorRecord],
    fan_in_raw: &HashMap<String, u64>,
    candidates: &[String],
    opts: &SignalOptions,
) -> SignalBundle {
    let mut activity = activity_raw.clone();
    for p in dirty_paths {
        *activity.entry(p.clone()).or_default() += 1.0;
    }

    let session = session_score(
        session_events,
        opts.now_ms,
        opts.session_half_life_minutes,
        opts.session_window_ms,
    );
    let (error_weights, error_reasons) = error_sink_join(
        error_records,
        candidates,
        opts.now_ms,
        opts.session_half_life_minutes,
    );

    let mut combined = session;
    for (p, w) in &error_weights {
        *combined.entry(p.clone()).or_default() += w;
    }

    // PLAN.md and SignalBundle's own doc comment require activity/session to
    // be "normalized over the candidate set" — the max used as the 1.0
    // denominator must come from a candidate, not from some unrelated file
    // that happens to have high raw churn/session weight (e.g. a hot file
    // outside the current tier/candidate set from the 90-day git scan or a
    // stale session event on a path nobody is looking at). Without this
    // filter, `normalize` divides by that unrelated file's value and every
    // real candidate's normalized signal collapses toward 0, silently
    // neutering the reranker.
    let candidate_set: HashSet<&str> = candidates.iter().map(String::as_str).collect();
    let activity: HashMap<String, f64> = activity
        .into_iter()
        .filter(|(p, _)| candidate_set.contains(p.as_str()))
        .collect();
    let combined: HashMap<String, f64> = combined
        .into_iter()
        .filter(|(p, _)| candidate_set.contains(p.as_str()))
        .collect();

    // Fan-in (in-degree): restrict to the candidate set, then normalize by
    // the max so the top candidate is 1.0 — same shape as `activity`.
    //
    // Restricting to `candidate_set` (same filter as `activity`/`combined`
    // above) is what keeps fan-in a RERANKER and not a candidate channel:
    // a high-in-degree file that is NOT a candidate never dilutes the
    // 1.0 denominator, so no unseen file's structural weight can silently
    // bend the ranking toward or against a candidate — the candidate set is
    // fixed here and the structural signal only reorders within it. The
    // closed-world claim (\"restrict reads/edits to the listed files\") therefore
    // holds: fan-in introduces no new fileand promotes no path across tiers.
    let fan_in: HashMap<String, f64> = fan_in_raw
        .iter()
        .filter(|(p, _)| candidate_set.contains(p.as_str()))
        .map(|(p, v)| (p.clone(), *v as f64))
        .collect();

    SignalBundle {
        activity: normalize(&activity),
        session: normalize(&combined),
        fan_in: normalize(&fan_in),
        session_reasons: session_reasons(session_events, opts.now_ms, opts.session_window_ms),
        error_reasons,
        // `score_signals` is pure and runs no scan: only `compute_signals`
        // can report an unavailable activity channel.
        activity_unavailable: None,
    }
}

/// I/O convenience: gather activity (facts map when supplied, else a one-shot
/// `git log --since=90.days --name-only` fallback), read the error sink from
/// the session store when one is supplied, then delegate to [`score_signals`].
///
/// `activity_from_facts` is the `history.db.file_changes`-derived map once
/// pixel-facts lands its API; until then pass `None` to use the git fallback.
/// A failed or capped git-log scan still degrades to an empty activity map
/// (no error), with the reason carried in
/// [`SignalBundle::activity_unavailable`].
#[allow(clippy::too_many_arguments)] // Coordinates independent signal sources; changing this public API would ripple to callers.
pub fn compute_signals(
    runner: &GitRunner,
    session_store: Option<&Store>,
    session_events: &[SessionEvent],
    activity_from_facts: Option<&HashMap<String, f64>>,
    dirty_paths: &[String],
    fan_in_raw: &HashMap<String, u64>,
    candidates: &[String],
    opts: &SignalOptions,
) -> Result<SignalBundle, SignalError> {
    let (activity_raw, activity_unavailable) = match activity_from_facts {
        Some(m) => (m.clone(), None),
        None => match activity_from_git_log(runner, opts.now_ms, opts.activity_half_life_days) {
            Ok(m) => (m, None),
            Err(e) => (HashMap::new(), Some(e.to_string())),
        },
    };
    let error_records = match session_store {
        Some(store) => store.errors_since_ts(opts.now_ms - opts.session_window_ms, 200)?,
        None => Vec::new(),
    };
    let mut bundle = score_signals(
        &activity_raw,
        dirty_paths,
        session_events,
        &error_records,
        fan_in_raw,
        candidates,
        opts,
    );
    // Named, never silent: the reranker must not read a failed or capped
    // scan as "this repository has no recent commits".
    bundle.activity_unavailable = activity_unavailable;
    Ok(bundle)
}

/// `Σ exp(-age_days/14)` per file over commits in the last 90 days, from a
/// one-shot `git log --since=90.days --name-only --format=%x00%ct`.
///
/// `Err` with the failing git call named (timeout, output cap, non-zero exit)
/// when the scan could not run — a caller degrading to an empty activity map
/// must report that reason rather than pass it off as a clean repository.
///
/// The format string is prefixed with a NUL byte (`%x00`) as an unambiguous
/// per-commit record separator. A naive `"\n\n"` split (matching git's
/// visual "timestamp, blank line, file list" layout) is wrong: git does
/// *not* insert a blank line between one commit's file list and the next
/// commit's timestamp line, only between a commit's own timestamp and its
/// file list. Splitting on `"\n\n"` therefore misaligns every block after
/// the first — the previous commit's last filename ends up as the next
/// block's "timestamp" line, fails to parse as an integer, and the whole
/// block (including the real timestamp on the following line) is silently
/// dropped. That bug previously made this function return an empty map for
/// every real multi-commit (or even single-commit) repository. NUL never
/// appears in a commit timestamp or a tracked file path, so splitting on it
/// is unambiguous regardless of git's blank-line formatting.
pub fn activity_from_git_log(
    runner: &GitRunner,
    now_ms: i64,
    half_life_days: f64,
) -> Result<HashMap<String, f64>, SignalError> {
    let mut activity: HashMap<String, f64> = HashMap::new();
    let out = runner.run(&["log", "--since=90.days", "--name-only", "--format=%x00%ct"])?;
    let text = String::from_utf8_lossy(&out);
    for block in text.split('\0') {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        let Ok(ts_secs) = first.trim().parse::<i64>() else {
            continue;
        };
        // `%ct` is git's commit time in *seconds* since epoch; every other
        // timestamp in this module (`now_ms`, `SessionEvent::ts_ms`,
        // `ErrorRecord::last_ts`) is milliseconds. Without this conversion,
        // `now_ms - ts_secs` is off by a factor of ~1000 (git's seconds
        // value is ~1e9 while `now_ms` is ~1e12), which inflates every
        // commit's apparent age to tens of thousands of days regardless of
        // how recent it actually was — `exp(-age_days/14)` then underflows
        // to 0.0 for every file, silently zeroing out the entire git-log
        // activity fallback in production.
        let ts_ms = ts_secs * 1000;
        for file in lines {
            let file = file.trim();
            if file.is_empty() {
                continue;
            }
            let age_days = (now_ms as f64 - ts_ms as f64) / 86_400_000.0;
            if age_days >= 0.0 {
                *activity.entry(file.to_string()).or_default() +=
                    (-age_days / half_life_days).exp();
            }
        }
    }
    Ok(activity)
}

/// `Σ exp(-age_minutes/30)` over events ≤ `window_ms`, edits 2× reads.
fn session_score(
    events: &[SessionEvent],
    now_ms: i64,
    half_life_minutes: f64,
    window_ms: i64,
) -> HashMap<String, f64> {
    let mut score: HashMap<String, f64> = HashMap::new();
    for e in events {
        let age_ms = now_ms - e.ts_ms;
        if age_ms < 0 || age_ms > window_ms {
            continue;
        }
        let age_min = age_ms as f64 / 60_000.0;
        let base = (-age_min / half_life_minutes).exp();
        let mult = if e.kind == SessionEventKind::Edit {
            2.0
        } else {
            1.0
        };
        *score.entry(e.path.clone()).or_default() += base * mult;
    }
    score
}

/// Join the error sink against candidate paths: an active error whose
/// http url / frames / message match a candidate's path concepts adds a
/// recency-weighted boost plus a reason string (the "live 503" path).
fn error_sink_join(
    errors: &[ErrorRecord],
    candidates: &[String],
    now_ms: i64,
    half_life_minutes: f64,
) -> (HashMap<String, f64>, Vec<String>) {
    let mut weights: HashMap<String, f64> = HashMap::new();
    let mut reasons: Vec<String> = Vec::new();
    for path in candidates {
        for err in errors {
            if let Some(reason) = error_matches_path(err, path) {
                let age_min = (now_ms - err.last_ts) as f64 / 60_000.0;
                let recency = if age_min >= 0.0 {
                    (-age_min / half_life_minutes).exp()
                } else {
                    0.0
                };
                *weights.entry(path.clone()).or_default() += recency;
                reasons.push(reason);
            }
        }
    }
    (weights, reasons)
}

/// Does `err` reference `path`'s concepts? Returns a human-readable reason.
fn error_matches_path(err: &ErrorRecord, path: &str) -> Option<String> {
    let concepts = path_concepts(path);
    if concepts.is_empty() {
        return None;
    }
    if let Some(http) = &err.http
        && let Some(url) = http.get("url").and_then(serde_json::Value::as_str)
    {
        let url_lower = url.to_lowercase();
        if concepts.iter().any(|c| url_lower.contains(c.as_str())) {
            let status = http
                .get("status")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            return Some(format!(
                "matches live error #{} ({} on {})",
                err.id, status, url
            ));
        }
    }
    if let Some(frames) = &err.frames {
        for f in frames {
            for loc in [f.file.as_deref(), f.mapped_file.as_deref()]
                .into_iter()
                .flatten()
            {
                let loc_lower = loc.to_lowercase();
                if concepts.iter().any(|c| loc_lower.contains(c.as_str())) {
                    return Some(format!("matches live error #{} frame {}", err.id, loc));
                }
            }
        }
    }
    let msg = format!("{} {}", err.message, err.kind.as_deref().unwrap_or("")).to_lowercase();
    if concepts.iter().any(|c| msg.contains(c.as_str())) {
        return Some(format!("matches live error #{} message", err.id));
    }
    None
}

/// Non-trivial path components used to match a candidate against an error.
fn path_concepts(path: &str) -> Vec<String> {
    const TRIVIAL: &[&str] = &[
        "app",
        "src",
        "api",
        "route",
        "index",
        "page",
        "pages",
        "lib",
        "components",
        "component",
        "ts",
        "tsx",
        "js",
        "jsx",
        "rs",
        "py",
        "go",
        "json",
        "yaml",
        "yml",
        "css",
        "html",
        "vue",
        "svelte",
        "test",
        "spec",
        "tests",
        "specs",
        "node_modules",
        "public",
        "static",
        "server",
        "client",
    ];
    path.split(['/', '.', '-', '_'])
        .filter(|s| s.len() >= 3)
        .filter(|s| !TRIVIAL.contains(s))
        .map(str::to_lowercase)
        .collect()
}

/// Normalize a raw per-path map over the candidate set: divide by the max so
/// the top candidate is 1.0. Empty when there is no positive signal.
fn normalize(map: &HashMap<String, f64>) -> HashMap<String, f64> {
    let max = map.values().copied().fold(0.0, f64::max);
    if max <= 0.0 {
        return HashMap::new();
    }
    map.iter().map(|(k, v)| (k.clone(), v / max)).collect()
}

/// Human-readable session reasons, newest first, within `window_ms`.
fn session_reasons(events: &[SessionEvent], now_ms: i64, window_ms: i64) -> Vec<String> {
    let mut reasons: Vec<(i64, String)> = Vec::new();
    for e in events {
        let age_ms = now_ms - e.ts_ms;
        if age_ms < 0 || age_ms > window_ms {
            continue;
        }
        let age_min = age_ms as f64 / 60_000.0;
        let kind = match e.kind {
            SessionEventKind::Read => "read",
            SessionEventKind::Edit => "edited",
            SessionEventKind::ResolveHit => "resolve hit",
            SessionEventKind::TargetsHit => "targets hit",
            SessionEventKind::Error => "error",
        };
        reasons.push((e.ts_ms, format!("{kind} {} {:.0}m ago", e.path, age_min)));
    }
    reasons.sort_by_key(|&(ts, _)| std::cmp::Reverse(ts));
    reasons.into_iter().map(|(_, r)| r).collect()
}

/// `xxh3(head_oid ‖ dirty_set ‖ session_high_water ‖ error_sink_high_water ‖
/// weights_version)` — the digest every ranked response carries so a caller
/// can detect when the underlying signals changed.
pub fn inputs_digest(
    head_oid: &str,
    dirty_set: &[String],
    session_high_water: i64,
    error_sink_high_water: i64,
    weights_version: u32,
) -> u64 {
    use xxhash_rust::xxh3::xxh3_64;
    let mut buf = Vec::new();
    buf.extend_from_slice(head_oid.as_bytes());
    buf.push(0);
    for d in dirty_set {
        buf.extend_from_slice(d.as_bytes());
        buf.push(0);
    }
    buf.push(0);
    buf.extend_from_slice(&session_high_water.to_le_bytes());
    buf.extend_from_slice(&error_sink_high_water.to_le_bytes());
    buf.extend_from_slice(&weights_version.to_le_bytes());
    xxh3_64(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_test_path_matches_all_forms() {
        // /tests?/ directory component
        assert!(is_test_path("src/test/foo.rs"));
        assert!(is_test_path("src/tests/foo.rs"));
        // __tests__ directory
        assert!(is_test_path("src/__tests__/foo.js"));
        // *_test.*
        assert!(is_test_path("src/foo_test.rs"));
        assert!(is_test_path("foo_test.py"));
        // *.spec.* / *.test.*
        assert!(is_test_path("src/foo.spec.ts"));
        assert!(is_test_path("src/foo.test.ts"));
        // case-insensitive
        assert!(is_test_path("src/FOO_Test.RS"));
    }

    #[test]
    fn is_test_path_rejects_non_tests() {
        assert!(!is_test_path("src/foo.rs"));
        assert!(!is_test_path("src/test_utils.rs")); // starts with test, not a test file
        assert!(!is_test_path("src/contest.rs"));
        assert!(!is_test_path("src/foo.testing.rs"));
        assert!(!is_test_path(""));
    }

    #[test]
    fn test_penalty_fn_gates_on_mentions() {
        // Task mentions tests → penalty gated OFF for every path.
        let off = test_penalty_fn(true, 0.7);
        assert!((off("src/foo_test.rs") - 1.0).abs() < 1e-9);
        assert!((off("src/foo.rs") - 1.0).abs() < 1e-9);

        // Task does NOT mention tests → test paths get 0.7, others 1.0.
        let p = test_penalty_fn(false, 0.7);
        assert!((p("src/foo_test.rs") - 0.7).abs() < 1e-9);
        assert!((p("src/foo.rs") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mentions_tests_detects_task_language() {
        assert!(mentions_tests("add tests for login"));
        assert!(mentions_tests("fix the spec"));
        assert!(!mentions_tests("refactor the auth service"));
        // Task mentions tests → no penalty.
        assert_eq!(test_penalty_for("add tests"), 1.0);
        // Task does NOT mention tests → penalty applies.
        assert_eq!(test_penalty_for("refactor auth"), 0.7);
    }

    #[test]
    fn fan_in_normalizes_over_candidate_set_and_ignores_external_hubs() {
        // P1-2 structural verification — rank-by-in-degree. The fan_in
        // signal is a RERANKER, not a candidate channel: it normalizes
        // ONLY over the candidate set, so a high-in-degree file that is NOT a
        // candidate (an external hub) can neither become a candidate nor
        // dilute the 1.0 denominator of real candidates (the closed-world
        // claim held), and it never changes tier membership (tiers are fixed
        // before reranking, in `rerank::rerank`).
        let candidates = vec!["src/auth.rs".to_string(), "src/api.rs".to_string()];
        // `src/hot_lib.rs` is NOT in the candidate set but has a huge
        // in-degree.
        let fan_in_raw: HashMap<String, u64> = [
            ("src/auth.rs".into(), 3),
            ("src/api.rs".into(), 1),
            ("src/hot_lib.rs".into(), 9000),
        ]
        .into_iter()
        .collect();
        let opts = SignalOptions {
            now_ms: 1_000_000_000_000,
            ..Default::default()
        };
        let bundle = score_signals(
            &HashMap::new(),
            &[],
            &[],
            &[],
            &fan_in_raw,
            &candidates,
            &opts,
        );
        // Only candidates survive the filter; the external hub is gone entirely.
        assert!(!bundle.fan_in.contains_key("src/hot_lib.rs"));
        // Normalization is within-candidate-set: max(3)=1.0, 1/3≈0.333…
        let auth = bundle.fan_in.get("src/auth.rs").copied().unwrap_or(0.0);
        let api = bundle.fan_in.get("src/api.rs").copied().unwrap_or(0.0);
        assert!((auth - 1.0).abs() < 1e-9);
        assert!((api - (1.0 / 3.0)).abs() < 1e-9);
        // Without the candidate-set filter, the hub's 9000 would have
        // crushed every real candidate toward 0 (auth would be ≈0.0003).
    }

    #[test]
    fn fan_in_is_wired_into_rerank_and_never_promotes_across_tiers() {
        // P1-2 structural verification — RANK-BY-IN-DEGREE, wiring check.
        // The `score_signals` test above proves fan_in is normalized over the
        // candidate set; this test closes the OTHER half of the verification:
        // that the reranker actually CONSUMES `SignalBundle.fan_in` (so the
        // structural signal reaches the final score), and that consuming it
        // cannot turn fan-in into a candidate channel (tiers stay fixed).
        use crate::rerank::RankedCandidate;

        let mk = |path: &str, rrf: f64, tier: &str| RankedCandidate {
            id: 0,
            path: path.to_string(),
            rrf_score: rrf,
            tier: tier.to_string(),
        };
        let candidates = vec![mk("src/auth.rs", 10.0, "P1"), mk("src/api.rs", 1.0, "P1")];
        let opts = SignalOptions {
            now_ms: 1_000_000_000_000,
            ..Default::default()
        };
        // auth has the most dependents (in-degree 3), api 1, hot_lib 9000
        // but is NOT a candidate and must stay out (closed-world).
        let fan_in_raw: HashMap<String, u64> = [
            ("src/auth.rs".into(), 3),
            ("src/api.rs".into(), 1),
            ("src/hot_lib.rs".into(), 9000),
        ]
        .into_iter()
        .collect();
        let signals = score_signals(
            &HashMap::new(),
            &[],
            &[],
            &[],
            &fan_in_raw,
            &["src/auth.rs".to_string(), "src/api.rs".to_string()],
            &opts,
        );

        let out = crate::rerank::rerank(
            candidates,
            &signals,
            &crate::rerank::RerankWeights::from(&opts),
            |_| 1.0,
        );
        // auth (fan_in_norm 1.0) must outrank api (fan_in_norm ≈0.333):
        // auth's 10.0 * (1 + 0.2*1.0) beats api's 1.0 * (1 + 0.2*(1/3)).
        assert_eq!(out[0].path, "src/auth.rs");
        // The structural multiplier is really in the score: auth keeps
        // rrf_score 10.0, and after rerank it must be 10.0 * (1 + 0.2 * 1.0).
        assert!((out[0].rrf_score - 10.0 * (1.0 + 0.2)).abs() < 1e-9);
        // Both are still P1 — fan-in never promoted one across a tier.
        for c in &out {
            assert_eq!(c.tier, "P1");
        }
        // The external hub never surfaces as candidate or as a bonus.
        assert!(!signals.fan_in.contains_key("src/hot_lib.rs"));
    }
}
