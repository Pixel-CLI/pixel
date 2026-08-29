//! Engine 3 ranking signals — activity (churn) + session (recent working
//! set) + live error-sink join.
//!
//! These are *rerankers*, never candidate channels: they reorder within a
//! tier but never promote across tiers (protects the closed-world claim).
//! Everything here is deterministic — same repo state + session journal +
//! error sink ⇒ byte-identical output.
//!
//! The pure scorer [`score_signals`] takes caller-supplied raw inputs so it
//! is unit-testable without a git repo or a session store. The I/O
//! convenience [`compute_signals`] gathers activity (from `history.db` when
//! the caller supplies it, else a one-shot `git log --since=90.days
//! --name-only` fallback), session events, and the error sink, then calls
//! the pure scorer.

use std::collections::HashMap;

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
    /// Human-readable session reasons ("edited 12m ago").
    pub session_reasons: Vec<String>,
    /// Human-readable error-sink reasons ("matches live error #42").
    pub error_reasons: Vec<String>,
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

/// `0.7` when `task` mentions test/spec, else `1.0`.
pub fn test_penalty_for(task: &str) -> f64 {
    let mentions = task.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        matches!(t.to_ascii_lowercase().as_str(), "test" | "tests" | "spec" | "specs")
    });
    if mentions {
        0.7
    } else {
        1.0
    }
}

/// Pure scorer: normalize activity, fold session events + error-sink weights
/// into one session channel, and produce human-readable reasons. No I/O.
pub fn score_signals(
    activity_raw: &HashMap<String, f64>,
    dirty_paths: &[String],
    session_events: &[SessionEvent],
    error_records: &[ErrorRecord],
    candidates: &[String],
    opts: &SignalOptions,
) -> SignalBundle {
    let mut activity = activity_raw.clone();
    for p in dirty_paths {
        *activity.entry(p.clone()).or_default() += 1.0;
    }

    let session = session_score(session_events, opts.now_ms, opts.session_half_life_minutes, opts.session_window_ms);
    let (error_weights, error_reasons) =
        error_sink_join(error_records, candidates, opts.now_ms, opts.session_half_life_minutes);

    let mut combined = session;
    for (p, w) in &error_weights {
        *combined.entry(p.clone()).or_default() += w;
    }

    SignalBundle {
        activity: normalize(&activity),
        session: normalize(&combined),
        session_reasons: session_reasons(session_events, opts.now_ms),
        error_reasons,
    }
}

/// I/O convenience: gather activity (facts map when supplied, else a one-shot
/// `git log --since=90.days --name-only` fallback), read the error sink from
/// the session store, then delegate to [`score_signals`].
///
/// `activity_from_facts` is the `history.db.file_changes`-derived map once
/// pixel-facts lands its API; until then pass `None` to use the git fallback.
pub fn compute_signals(
    runner: &GitRunner,
    session_store: Option<&Store>,
    session_events: &[SessionEvent],
    activity_from_facts: Option<&HashMap<String, f64>>,
    dirty_paths: &[String],
    candidates: &[String],
    opts: &SignalOptions,
) -> Result<SignalBundle, SignalError> {
    let activity_raw = match activity_from_facts {
        Some(m) => m.clone(),
        None => activity_from_git_log(runner, opts.now_ms, opts.activity_half_life_days),
    };
    let error_records = match session_store {
        Some(store) => store.errors_since_ts(opts.now_ms - opts.session_window_ms, 200)?,
        None => Vec::new(),
    };
    Ok(score_signals(
        &activity_raw,
        dirty_paths,
        session_events,
        &error_records,
        candidates,
        opts,
    ))
}

/// `Σ exp(-age_days/14)` per file over commits in the last 90 days, from a
/// one-shot `git log --since=90.days --name-only --format=%ct`. Empty on any
/// git failure (graceful degradation outside a repo).
pub fn activity_from_git_log(
    runner: &GitRunner,
    now_ms: i64,
    half_life_days: f64,
) -> HashMap<String, f64> {
    let mut activity: HashMap<String, f64> = HashMap::new();
    let Some(out) = runner.run_opt(&["log", "--since=90.days", "--name-only", "--format=%ct"]) else {
        return activity;
    };
    let text = String::from_utf8_lossy(&out);
    for block in text.split("\n\n") {
        let mut lines = block.lines();
        let Some(first) = lines.next() else { continue };
        let Ok(ts) = first.trim().parse::<i64>() else { continue };
        for file in lines {
            let file = file.trim();
            if file.is_empty() {
                continue;
            }
            let age_days = (now_ms as f64 - ts as f64) / 86_400_000.0;
            if age_days >= 0.0 {
                *activity.entry(file.to_string()).or_default() += (-age_days / half_life_days).exp();
            }
        }
    }
    activity
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
        let mult = if e.kind == SessionEventKind::Edit { 2.0 } else { 1.0 };
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
                let recency = if age_min >= 0.0 { (-age_min / half_life_minutes).exp() } else { 0.0 };
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
    if let Some(http) = &err.http {
        if let Some(url) = http.get("url").and_then(serde_json::Value::as_str) {
            let url_lower = url.to_lowercase();
            if concepts.iter().any(|c| url_lower.contains(c.as_str())) {
                let status = http.get("status").and_then(serde_json::Value::as_u64).unwrap_or(0);
                return Some(format!("matches live error #{} ({} on {})", err.id, status, url));
            }
        }
    }
    if let Some(frames) = &err.frames {
        for f in frames {
            for loc in [f.file.as_deref(), f.mapped_file.as_deref()].into_iter().flatten() {
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
        "app", "src", "api", "route", "index", "page", "pages", "lib", "components", "component",
        "ts", "tsx", "js", "jsx", "rs", "py", "go", "json", "yaml", "yml", "css", "html", "vue",
        "svelte", "test", "spec", "tests", "specs", "node_modules", "public", "static", "server",
        "client",
    ];
    path.split(['/', '.', '-', '_'])
        .filter(|s| s.len() >= 3)
        .filter(|s| !TRIVIAL.contains(s))
        .map(|s| s.to_lowercase())
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

/// Human-readable session reasons, newest first, within the 24h window.
fn session_reasons(events: &[SessionEvent], now_ms: i64) -> Vec<String> {
    let mut reasons: Vec<(i64, String)> = Vec::new();
    for e in events {
        let age_min = (now_ms - e.ts_ms) as f64 / 60_000.0;
        if age_min < 0.0 || age_min > 24.0 * 60.0 {
            continue;
        }
        let kind = match e.kind {
            SessionEventKind::Read => "read",
            SessionEventKind::Edit => "edited",
            SessionEventKind::ResolveHit => "resolve hit",
            SessionEventKind::TargetsHit => "targets hit",
            SessionEventKind::Error => "error",
        };
        reasons.push((e.ts_ms, format!("{kind} {} {:.0}m ago", e.path, age_min)));
    }
    reasons.sort_by(|a, b| b.0.cmp(&a.0));
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
