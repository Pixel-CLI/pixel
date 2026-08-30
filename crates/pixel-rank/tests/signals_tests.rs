//! Fixture-based correctness tests for Engine 3 (activity + session +
//! error-sink rerank signals). These exercise real inputs — a real git repo
//! with backdated commits, realistic session-event timelines, realistic
//! `ErrorRecord` shapes, and adversarial candidate sets — rather than
//! asserting only that the functions run without panicking.

use std::collections::HashMap;
use std::process::Command;

use pixel_git::GitRunner;
use pixel_rank::rerank::{rerank, rerank_targets, RankedCandidate};
use pixel_rank::signals::{
    activity_from_git_log, inputs_digest, score_signals, test_penalty_for, SessionEvent,
    SessionEventKind, SignalOptions,
};
use pixel_rank::TargetFile;
use pixel_session::types::{ErrorRecord, Surface};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway git repo with three files committed at controlled dates
/// (via `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`), so `activity_from_git_log`'s
/// recency decay can be checked against real, varying ages instead of
/// synthetic numbers.
struct GitFixture {
    dir: tempfile::TempDir,
}

impl GitFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        run(dir.path(), &["init", "-q", "-b", "main"]);
        run(dir.path(), &["config", "user.email", "test@example.com"]);
        run(dir.path(), &["config", "user.name", "Test"]);
        GitFixture { dir }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Commit `file` with content `content`, dated `days_ago` days before
    /// now (both author and committer date, so `git log --format=%ct` and
    /// `--since` agree).
    fn commit_file_days_ago(&self, file: &str, content: &str, days_ago: i64) {
        let full = self.dir.path().join(file);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        run(self.dir.path(), &["add", file]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let commit_ts = now - days_ago * 86_400;
        // git accepts "@<unix-ts> <tz>" for author/committer dates.
        let date = format!("@{commit_ts} +0000");

        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(self.dir.path())
            .args(["commit", "-q", "-m", &format!("touch {file}")])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
        let status = cmd.status().expect("git commit");
        assert!(status.success(), "git commit failed for {file}");
    }
}

fn run(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn error(id: i64, last_ts: i64, message: &str, http_url: Option<&str>) -> ErrorRecord {
    ErrorRecord {
        id,
        first_ts: last_ts,
        last_ts,
        count: 1,
        run_id: None,
        surface: Surface::Http5xx,
        kind: Some("Error".to_string()),
        message: message.to_string(),
        stack_raw: None,
        frames: None,
        values: None,
        http: http_url.map(|u| serde_json::json!({ "url": u, "status": 503 })),
        extra: None,
        dedup_hash: format!("hash-{id}"),
    }
}

// ---------------------------------------------------------------------------
// activity_from_git_log — real recency scoring against a real repo
// ---------------------------------------------------------------------------

#[test]
fn activity_from_git_log_scores_recent_files_higher_than_stale_ones() {
    let repo = GitFixture::new();
    // hot.txt: touched today. warm.txt: touched 30 days ago. cold.txt:
    // touched 85 days ago (inside the 90-day --since window, but far past
    // the 14-day half-life). ancient.txt: touched 120 days ago — outside
    // the `--since=90.days` window entirely, must score exactly 0.
    //
    // Commits are created oldest-first (monotonically increasing commit
    // dates as the DAG walks from root to HEAD), matching how real repo
    // history is actually produced. `git log --since` uses a chronological
    // traversal heuristic that assumes non-decreasing dates walking from
    // HEAD to root; backdating in the opposite order (child older than
    // parent) breaks that heuristic and makes `--since` drop commits that
    // are genuinely inside the window — a fixture-ordering pitfall, not a
    // production bug (real commits are never non-monotonic like that).
    repo.commit_file_days_ago("ancient.txt", "ancient", 120);
    repo.commit_file_days_ago("cold.txt", "cold", 85);
    repo.commit_file_days_ago("warm.txt", "warm", 30);
    repo.commit_file_days_ago("hot.txt", "hot", 0);

    let runner = GitRunner::new(repo.path());
    let now = now_ms();
    let activity = activity_from_git_log(&runner, now, 14.0);

    let hot = *activity.get("hot.txt").unwrap_or(&0.0);
    let warm = *activity.get("warm.txt").unwrap_or(&0.0);
    let cold = *activity.get("cold.txt").unwrap_or(&0.0);

    // Real numbers, not just an ordering check: exp(-0/14)=1.0,
    // exp(-30/14)=~0.1173, exp(-85/14)=~0.00219. `hot` is a hair under 1.0
    // (not exactly 1.0) because a few milliseconds of real wall-clock time
    // elapse between the commit and the `now_ms()` call above.
    assert!(
        (hot - 1.0).abs() < 1e-3,
        "hot.txt (touched today) should score ~1.0, got {hot}"
    );
    assert!(
        (warm - 0.1173).abs() < 0.001,
        "warm.txt (30d old) should score ~0.1173, got {warm}"
    );
    assert!(
        (cold - 0.00219).abs() < 0.0005,
        "cold.txt (85d old) should score ~0.00219, got {cold}"
    );
    assert!(hot > warm && warm > cold, "recency ordering must hold: hot={hot} warm={warm} cold={cold}");

    // ancient.txt is outside `--since=90.days` entirely: must not appear at
    // all (not merely "low score" — genuinely absent from the git log
    // output pixel-rank scanned).
    assert!(
        !activity.contains_key("ancient.txt"),
        "ancient.txt (120d old) must be excluded by --since=90.days, got {:?}",
        activity.get("ancient.txt")
    );
}

#[test]
fn activity_from_git_log_degrades_gracefully_outside_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let runner = GitRunner::new(dir.path());
    let activity = activity_from_git_log(&runner, now_ms(), 14.0);
    assert!(activity.is_empty(), "non-repo dir must yield empty activity map, got {activity:?}");
}

// ---------------------------------------------------------------------------
// session scoring (via the public score_signals API) — real event timelines
// ---------------------------------------------------------------------------

#[test]
fn session_score_weights_edits_2x_reads_at_equal_recency() {
    let now = now_ms();
    let events = vec![
        SessionEvent {
            ts_ms: now - 5 * 60_000, // 5 minutes ago
            kind: SessionEventKind::Read,
            path: "src/read_only.rs".to_string(),
            detail: None,
        },
        SessionEvent {
            ts_ms: now - 5 * 60_000, // same age
            kind: SessionEventKind::Edit,
            path: "src/edited.rs".to_string(),
            detail: None,
        },
    ];
    let candidates = vec!["src/read_only.rs".to_string(), "src/edited.rs".to_string()];
    let opts = SignalOptions {
        now_ms: now,
        ..SignalOptions::default()
    };
    let bundle = score_signals(&HashMap::new(), &[], &events, &[], &candidates, &opts);

    // Both normalized to the candidate set's max: edited.rs (2x weight) must
    // be the max (1.0), read_only.rs exactly half of it.
    let edited = *bundle.session.get("src/edited.rs").unwrap();
    let read = *bundle.session.get("src/read_only.rs").unwrap();
    assert!((edited - 1.0).abs() < 1e-9, "edited.rs should normalize to 1.0, got {edited}");
    assert!((read - 0.5).abs() < 1e-9, "read_only.rs should be exactly half of edited.rs, got {read}");
}

#[test]
fn session_score_ignores_events_older_than_24h_window() {
    let now = now_ms();
    let events = vec![
        SessionEvent {
            ts_ms: now - 23 * 60 * 60_000, // 23h ago: inside window
            kind: SessionEventKind::Edit,
            path: "src/recent.rs".to_string(),
            detail: None,
        },
        SessionEvent {
            ts_ms: now - 25 * 60 * 60_000, // 25h ago: outside the 24h window
            kind: SessionEventKind::Edit,
            path: "src/stale.rs".to_string(),
            detail: None,
        },
    ];
    let candidates = vec!["src/recent.rs".to_string(), "src/stale.rs".to_string()];
    let opts = SignalOptions {
        now_ms: now,
        ..SignalOptions::default()
    };
    let bundle = score_signals(&HashMap::new(), &[], &events, &[], &candidates, &opts);

    assert!(bundle.session.get("src/recent.rs").copied().unwrap_or(0.0) > 0.0);
    assert_eq!(
        bundle.session.get("src/stale.rs").copied().unwrap_or(0.0),
        0.0,
        "an event 25h old must contribute ~0 (outside the 24h session_window_ms)"
    );
}

#[test]
fn normalization_is_scoped_to_the_candidate_set_not_the_whole_map() {
    // Regression test for the normalize-over-everything bug: a non-candidate
    // file with a huge raw activity score must not suppress the normalized
    // score of the actual (lower-raw-score) candidate.
    let mut activity_raw = HashMap::new();
    activity_raw.insert("not_a_candidate.rs".to_string(), 1000.0);
    activity_raw.insert("src/candidate.rs".to_string(), 0.4);

    let candidates = vec!["src/candidate.rs".to_string()];
    let opts = SignalOptions {
        now_ms: now_ms(),
        ..SignalOptions::default()
    };
    let bundle = score_signals(&activity_raw, &[], &[], &[], &candidates, &opts);

    let candidate_score = *bundle.activity.get("src/candidate.rs").unwrap();
    assert!(
        (candidate_score - 1.0).abs() < 1e-9,
        "candidate.rs is the only candidate, so it must normalize to 1.0 \
         regardless of a huge non-candidate raw score; got {candidate_score}"
    );
    assert!(
        !bundle.activity.contains_key("not_a_candidate.rs"),
        "non-candidate paths must not leak into the normalized bundle"
    );
}

// ---------------------------------------------------------------------------
// error_sink_join (via score_signals) — realistic ErrorRecord matching
// ---------------------------------------------------------------------------

#[test]
fn error_sink_join_boosts_only_the_matching_candidate() {
    let now = now_ms();
    // A live 503 on /api/checkout should boost the checkout route file, and
    // must not touch an unrelated candidate.
    let errors = vec![error(
        42,
        now - 2 * 60_000, // 2 minutes ago
        "upstream timeout",
        Some("https://app.example.com/api/checkout"),
    )];
    let candidates = vec![
        "src/routes/checkout.ts".to_string(),
        "src/routes/profile.ts".to_string(),
    ];
    let opts = SignalOptions {
        now_ms: now,
        ..SignalOptions::default()
    };
    let bundle = score_signals(&HashMap::new(), &[], &[], &errors, &candidates, &opts);

    let checkout = bundle.session.get("src/routes/checkout.ts").copied().unwrap_or(0.0);
    let profile = bundle.session.get("src/routes/profile.ts").copied().unwrap_or(0.0);
    assert!(checkout > 0.0, "checkout.ts must be boosted by the matching live error");
    assert_eq!(profile, 0.0, "profile.ts must get no boost from an unrelated error");

    assert!(
        bundle.error_reasons.iter().any(|r| r.contains("#42")),
        "reasons must cite the matching error id; got {:?}",
        bundle.error_reasons
    );
    assert!(
        bundle.error_reasons.iter().all(|r| !r.contains("profile")),
        "no reason should be generated for the non-matching candidate; got {:?}",
        bundle.error_reasons
    );
}

// ---------------------------------------------------------------------------
// rerank / rerank_targets — the closed-world / tier-non-promotion invariant
// ---------------------------------------------------------------------------

#[test]
fn rerank_never_lets_a_p2_candidate_outrank_any_p0_candidate() {
    // Adversarial setup: give the P2 candidate a massive synthetic
    // activity+session score (so its multiplier is huge) and the P0
    // candidate a score of 0 for both signals — the worst case for the
    // closed-world guarantee.
    let candidates = vec![
        RankedCandidate {
            path: "src/p0_weak.rs".to_string(),
            id: 0,
            rrf_score: 0.01, // deliberately tiny RRF score
            tier: "P0".to_string(),
        },
        RankedCandidate {
            path: "src/p1_mid.rs".to_string(),
            id: 1,
            rrf_score: 0.5,
            tier: "P1".to_string(),
        },
        RankedCandidate {
            path: "src/p2_hot.rs".to_string(),
            id: 2,
            rrf_score: 100.0, // deliberately huge RRF score
            tier: "P2".to_string(),
        },
    ];

    let mut activity = HashMap::new();
    activity.insert("src/p2_hot.rs".to_string(), 1.0); // max normalized signal
    let mut session = HashMap::new();
    session.insert("src/p2_hot.rs".to_string(), 1.0); // max normalized signal
    let signals = pixel_rank::signals::SignalBundle {
        activity,
        session,
        session_reasons: vec![],
        error_reasons: vec![],
    };

    let out = rerank(candidates, &signals, |_| 1.0);

    // Even though p2_hot's final score (100 * (1 + 0.15 + 0.35) = 150) is
    // enormously larger than p0_weak's (0.01), the output must still place
    // every P0 before every P1 before every P2.
    let tiers: Vec<&str> = out.iter().map(|c| c.tier.as_str()).collect();
    assert_eq!(tiers, vec!["P0", "P1", "P2"], "tier order must never be violated: {tiers:?}");

    let p0_index = out.iter().position(|c| c.tier == "P0").unwrap();
    let p2_index = out.iter().position(|c| c.tier == "P2").unwrap();
    assert!(p0_index < p2_index, "P0 candidate must rank ahead of the P2 candidate");

    // Sanity: the P2 candidate's score really was amplified far past the P0
    // candidate's score, proving this isn't a vacuous pass because the
    // multiplier had no effect.
    let p0_final = out.iter().find(|c| c.tier == "P0").unwrap().rrf_score;
    let p2_final = out.iter().find(|c| c.tier == "P2").unwrap().rrf_score;
    assert!(
        p2_final > p0_final * 100.0,
        "expected the P2 candidate's amplified score ({p2_final}) to dwarf the P0 \
         candidate's ({p0_final}), proving the invariant was tested under real pressure"
    );
}

#[test]
fn rerank_reorders_within_a_tier_by_amplified_score() {
    let candidates = vec![
        RankedCandidate {
            path: "src/a.rs".to_string(),
            id: 0,
            rrf_score: 1.0,
            tier: "P1".to_string(),
        },
        RankedCandidate {
            path: "src/b.rs".to_string(),
            id: 1,
            rrf_score: 1.0, // tied RRF score with a.rs
            tier: "P1".to_string(),
        },
    ];
    let mut activity = HashMap::new();
    activity.insert("src/b.rs".to_string(), 1.0); // only b.rs gets a boost
    let signals = pixel_rank::signals::SignalBundle {
        activity,
        session: HashMap::new(),
        session_reasons: vec![],
        error_reasons: vec![],
    };
    let out = rerank(candidates, &signals, |_| 1.0);
    assert_eq!(out[0].path, "src/b.rs", "b.rs's activity boost should move it ahead of tied a.rs");
    assert_eq!(out[1].path, "src/a.rs");
}

#[test]
fn rerank_targets_preserves_tier_non_promotion_on_target_file_shape() {
    let targets = vec![
        TargetFile {
            path: "src/p0.rs".to_string(),
            tier: "P0".to_string(),
            score: 0.001,
            reasons: vec![],
            symbols: vec![],
        },
        TargetFile {
            path: "src/p2.rs".to_string(),
            tier: "P2".to_string(),
            score: 50.0,
            reasons: vec![],
            symbols: vec![],
        },
    ];
    let mut activity = HashMap::new();
    activity.insert("src/p2.rs".to_string(), 1.0);
    let mut session = HashMap::new();
    session.insert("src/p2.rs".to_string(), 1.0);
    let signals = pixel_rank::signals::SignalBundle {
        activity,
        session,
        session_reasons: vec![],
        error_reasons: vec![],
    };
    let out = rerank_targets(targets, &signals, |_| 1.0);
    assert_eq!(out[0].tier, "P0", "P0 must still lead even though P2 has an amplified score");
    assert_eq!(out[1].tier, "P2");
}

// ---------------------------------------------------------------------------
// test_penalty_for — real query-string detection
// ---------------------------------------------------------------------------

#[test]
fn test_penalty_detects_test_and_spec_mentions() {
    // Task does NOT mention tests → penalty (0.7) applies.
    assert_eq!(test_penalty_for("fix the login flow"), 0.7);
    // Task mentions tests → penalty gated off (1.0).
    assert_eq!(test_penalty_for("fix the failing test for login"), 1.0);
    assert_eq!(test_penalty_for("update auth.spec.ts"), 1.0);
    // must not false-positive on substrings that merely contain "test"/"spec"
    // as part of a longer identifier token.
    assert_eq!(test_penalty_for("update the latest contest results"), 0.7);
}

// ---------------------------------------------------------------------------
// inputs_digest — real determinism / sensitivity to every stated input
// ---------------------------------------------------------------------------

#[test]
fn inputs_digest_is_deterministic_and_sensitive_to_every_input() {
    let base = inputs_digest("oid-a", &["dirty1.rs".to_string()], 100, 200, 1);
    let same = inputs_digest("oid-a", &["dirty1.rs".to_string()], 100, 200, 1);
    assert_eq!(base, same, "identical inputs must produce a byte-identical digest");

    let diff_head = inputs_digest("oid-b", &["dirty1.rs".to_string()], 100, 200, 1);
    let diff_dirty = inputs_digest("oid-a", &["dirty2.rs".to_string()], 100, 200, 1);
    let diff_session_hw = inputs_digest("oid-a", &["dirty1.rs".to_string()], 999, 200, 1);
    let diff_error_hw = inputs_digest("oid-a", &["dirty1.rs".to_string()], 100, 999, 1);
    let diff_weights = inputs_digest("oid-a", &["dirty1.rs".to_string()], 100, 200, 2);

    for (name, other) in [
        ("head_oid", diff_head),
        ("dirty_set", diff_dirty),
        ("session_high_water", diff_session_hw),
        ("error_sink_high_water", diff_error_hw),
        ("weights_version", diff_weights),
    ] {
        assert_ne!(base, other, "changing {name} must change the digest (not a placeholder constant)");
    }
}
