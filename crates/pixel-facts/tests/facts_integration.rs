//! Integration tests for pixel-facts: store, ingest, search, lifecycle,
//! poison detection, and index_state progress reporting.

use std::fs;
use std::path::Path;
use std::process::Command;

use pixel_facts::ingest::{IngestOptions, ingest_tick, ingest_until_fresh};
use pixel_facts::lifecycle::Lifecycle;
use pixel_facts::poison::{ContentKind, classify_content, skip_path};
use pixel_facts::search::{SearchFacet, search};
use pixel_facts::store::FactsStore;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Create a temp git repo with a few commits. Returns the TempDir (keep alive).
fn make_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Commit 1: add a source file with a distinctive string.
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello world\");\n}\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "Add main with hello world greeting"]);

    // Commit 2: modify the file (diff content has a new function).
    fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello world\");\n}\n\nfn helper() {\n    let secret_token = 42;\n}\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "-q", "-m", "Add helper with secret_token variable"],
    );

    // Commit 3: add a generated/lock file (should be poison-skipped).
    fs::write(
        root.join("package-lock.json"),
        "{\"name\": \"app\", \"lockfileVersion\": 3}",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "Add package-lock.json"]);

    dir
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git command");
    if !out.status.success() {
        panic!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn ingest_phase_a_completes_and_index_state_reports_progress() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    // Before ingest: empty index state.
    let state = store.index_state();
    assert_eq!(state.total_commits, 0, "fresh store should have 0 commits");

    // Run ingest to completion (repo is tiny — one tick is enough).
    let opts = IngestOptions::default();
    let report = ingest_until_fresh(&mut store, &opts).expect("ingest until fresh");

    // Phase A must have completed: commits indexed.
    assert!(
        report.total_commits >= 3,
        "should have indexed >= 3 commits, got {}",
        report.total_commits
    );
    assert_eq!(
        report.commits_indexed, report.total_commits,
        "all commits should be indexed"
    );
    assert!(report.fresh, "report should be fresh after full ingest");

    // index_state should now reflect the ingested commits.
    let state = store.index_state();
    assert!(state.total_commits >= 3);
    assert!(state.fresh, "index_state should be fresh");
}

#[test]
fn search_finds_term_in_commit_messages() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    let opts = IngestOptions::default();
    ingest_until_fresh(&mut store, &opts).expect("ingest");

    // Search for "helper" which appears in a commit subject.
    let result = search(&store, "helper", SearchFacet::Message, 50).expect("message search");

    assert!(
        !result.candidates.is_empty(),
        "should find 'helper' in commit messages"
    );
    // At least one hit should mention helper in the subject.
    let found = result
        .candidates
        .iter()
        .any(|h| h.subject.to_lowercase().contains("helper"));
    assert!(found, "a hit subject should contain 'helper'");
}

#[test]
fn search_finds_term_in_diff_content() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    let opts = IngestOptions::default();
    ingest_until_fresh(&mut store, &opts).expect("ingest");

    // Search for "secret_token" which only appears in diff content, not messages.
    let result = search(&store, "secret_token", SearchFacet::Diff, 50).expect("diff search");

    assert!(
        !result.candidates.is_empty(),
        "should find 'secret_token' in diff content"
    );
    // The hit should be a diff-kind hit.
    let diff_hit = result.candidates.iter().any(|h| h.kind == "diff");
    assert!(diff_hit, "should have a diff-kind hit");
}

#[test]
fn path_lifecycle_reports_first_seen_and_last_changed() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    let opts = IngestOptions::default();
    ingest_until_fresh(&mut store, &opts).expect("ingest");

    let lifecycle: Lifecycle = store
        .path_lifecycle("src/main.rs")
        .expect("lifecycle query")
        .expect("should have lifecycle for src/main.rs");

    assert_eq!(lifecycle.what, "src/main.rs");
    assert!(lifecycle.first_seen.is_some(), "should have first_seen");
    assert!(lifecycle.last_changed.is_some(), "should have last_changed");
    // The file was touched in commits 1 and 2.
    assert!(
        lifecycle.total_touches >= 2,
        "should have >= 2 touches, got {}",
        lifecycle.total_touches
    );
    // File exists at HEAD.
    assert!(
        lifecycle.present_at_head,
        "src/main.rs should be present at HEAD"
    );
    // Not removed.
    assert!(lifecycle.removed_in.is_none(), "file should not be removed");
}

#[test]
fn poison_detection_skips_lock_and_generated_files() {
    // Structural skip rules.
    assert!(skip_path("package-lock.json"), "package-lock.json should be skipped");
    assert!(skip_path("Cargo.lock"), "Cargo.lock should be skipped");
    assert!(skip_path("yarn.lock"), "yarn.lock should be skipped");
    assert!(
        skip_path("node_modules/react/index.js"),
        "node_modules paths should be skipped"
    );
    assert!(
        skip_path("dist/bundle.min.js"),
        "minified bundle should be skipped"
    );
    assert!(
        skip_path("src/__generated__/schema.ts"),
        "generated paths should be skipped"
    );

    // Normal source files should NOT be skipped.
    assert!(!skip_path("src/main.rs"), "normal source should not be skipped");
    assert!(!skip_path("src/components/Form.tsx"), "normal tsx should not be skipped");

    // Content classification: binary (NUL byte).
    let binary = b"hello\x00world";
    assert_eq!(classify_content(binary, "file.bin"), ContentKind::Binary);

    // Content classification: minified (very long lines).
    let minified = b"a=1;".repeat(600);
    assert_eq!(
        classify_content(&minified, "app.min.js"),
        ContentKind::Minified
    );

    // Content classification: normal text.
    let text = b"fn main() {\n    println!(\"hi\");\n}\n";
    assert_eq!(classify_content(text, "main.rs"), ContentKind::Text);
}

#[test]
fn poison_paths_excluded_from_diff_ingest() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    let opts = IngestOptions::default();
    let report = ingest_until_fresh(&mut store, &opts).expect("ingest");

    // The package-lock.json commit's diff for that file should be skipped
    // (structural skip). The skip count or poisoned count should be >= 1
    // because the lock file was touched.
    assert!(
        report.skipped_this_tick >= 1 || report.poisoned_this_tick >= 1,
        "lock file should trigger a skip or poison, got skipped={} poisoned={}",
        report.skipped_this_tick,
        report.poisoned_this_tick
    );

    // Searching for "lockfileVersion" in diff should return nothing — the
    // lock file's diff was never ingested.
    let result = search(&store, "lockfileVersion", SearchFacet::Diff, 50).expect("diff search");
    assert!(
        result.candidates.is_empty(),
        "lock file diff content should not be indexed"
    );
}

#[test]
fn excavate_finds_phrase_in_history() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    let opts = IngestOptions::default();
    ingest_until_fresh(&mut store, &opts).expect("ingest");

    // Excavate for "secret_token" — should find the commit that added it.
    let result = store
        .excavate(Some("secret_token"), None, None, None, 50)
        .expect("excavate");

    assert!(
        !result.candidates.is_empty(),
        "excavate should find 'secret_token' candidates"
    );
    assert!(
        result.candidates.iter().any(|c| c.phrase_present),
        "at least one candidate should have phrase_present=true"
    );
}

#[test]
fn excavate_result_carries_index_state() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    // Before any ingest: a miss must be distinguishable from "not indexed yet"
    // — the result carries index_state.fresh == false.
    let unfetched = store
        .excavate(Some("secret_token"), None, None, None, 50)
        .expect("excavate on empty db");
    assert!(
        !unfetched.index_state.fresh,
        "an un-ingested db must report fresh=false on its excavate result"
    );

    ingest_until_fresh(&mut store, &IngestOptions::default()).expect("ingest");
    let result = store
        .excavate(Some("secret_token"), None, None, None, 50)
        .expect("excavate");
    assert!(
        result.index_state.fresh,
        "after ingest_until_fresh the excavate result must report fresh=true"
    );
    assert!(
        result.index_state.commits_indexed > 0,
        "index_state should reflect the indexed commits"
    );
}

// ---------------------------------------------------------------------------
// Invariant tests: this crate's whole reason to exist over usable-git's
// synchronous ingest is (1) a query never blocks on / waits for ingest, and
// (2) a byte/size budget genuinely bounds what a writer stores, enforced
// inside the writer itself rather than only checked at a loop-top. Both
// invariants were actually broken in this crate at various points during
// this pass (a livelock that made `fresh` unreachable, and a per-file cap
// check that double-counted one side's length) — these tests pin the fixed
// behavior down so a regression is caught here, not by a hung CI job.
// ---------------------------------------------------------------------------

#[test]
fn query_never_blocks_on_in_progress_ingest() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");

    // A single tick with a zero-millisecond budget: phase A still lands its
    // one guaranteed batch (all 3 commits' metadata, since 3 < the 200-commit
    // batch size), but phase B's per-commit loop checks the deadline AFTER
    // measuring one commit's blobs — with a budget of 0ms that deadline is
    // already in the past, so phase B stops after exactly one commit. This
    // deterministically produces a genuine "ingest is still in progress"
    // state (phase_b, not fresh, zero diff text indexed) without any timing
    // race: everything here runs synchronously on one thread.
    let opts = IngestOptions { tick_budget_ms: 0 };
    let report = ingest_tick(&mut store, &opts).expect("one tick");
    assert!(
        !report.fresh,
        "a single zero-budget tick over 3 commits should leave phase B \
         incomplete, got a fully fresh report: {report:?}"
    );
    assert_eq!(
        report.phase, "phase_b",
        "expected to be mid phase-B, got phase={:?}",
        report.phase
    );

    // Queries must be read-only over whatever the index currently holds —
    // they must never themselves invoke ingest, and so must return promptly
    // regardless of how much ingest work remains. 2s is a very generous
    // bound for CPU-loaded CI; the actual invariant is architectural (search/
    // excavate/index_state touch no ingest code path at all), not a tight
    // timing assertion.
    let start = Instant::now();
    let search_result = search(&store, "helper", SearchFacet::Message, 50).expect("search must not error mid-ingest");
    let excavate_result = store
        .excavate(Some("secret_token"), None, None, None, 50)
        .expect("excavate must not error mid-ingest");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "search+excavate took {elapsed:?} while ingest was mid-flight — \
         looks like a query is blocking on (or itself performing) ingest work"
    );

    // The commit message search can already find results (phase A metadata
    // landed), but the diff facet has nothing yet — index_state must report
    // that honestly rather than claiming fresh.
    assert!(
        search_result.candidates.iter().any(|h| h.subject.to_lowercase().contains("helper")),
        "message search should still work from phase-A-only metadata"
    );
    let _ = excavate_result; // must not panic/error; content presence isn't the point here.

    let state = store.index_state();
    assert!(
        !state.fresh,
        "index_state must honestly report partial progress, not fake freshness, \
         while phase B/C work remains"
    );
}

#[test]
fn file_text_cap_genuinely_bounds_a_single_files_stored_diff_text() {
    use pixel_facts::poison::FILE_TEXT_CAP_BYTES;

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // A big-but-ordinary multi-line source file: well under the minified/
    // generated content heuristics (short, regular lines; low non-ASCII
    // ratio), so it is never structurally skipped — the ONLY thing that
    // should bound its stored size is the FILE_TEXT_CAP_BYTES writer cap
    // itself, exactly the property this test exists to prove.
    let mut content = String::new();
    for i in 0..3000 {
        content.push_str(&format!("let line_{i}_value = {i}; // padding text to add real bytes\n"));
    }
    assert!(
        content.len() > FILE_TEXT_CAP_BYTES * 2,
        "fixture file ({} bytes) must comfortably exceed the {} byte cap for this test to mean anything",
        content.len(),
        FILE_TEXT_CAP_BYTES
    );
    fs::write(root.join("big.rs"), &content).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "Add an oversized generated-looking source file"]);

    let mut store = FactsStore::open(root).expect("open store");
    let opts = IngestOptions::default();
    let report = ingest_until_fresh(&mut store, &opts).expect("ingest until fresh");
    assert!(report.fresh, "ingest should still converge to fresh even with an over-cap file");

    let (added_len, truncated): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT length(added), truncated FROM hunks WHERE path = 'big.rs'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("big.rs should have an indexed hunk (truncated, not silently dropped)");

    assert!(
        (added_len as usize) < FILE_TEXT_CAP_BYTES,
        "stored added-text length {added_len} must be bounded below the \
         {FILE_TEXT_CAP_BYTES}-byte cap — the writer must enforce the budget \
         DURING the write, not just check-and-ignore it"
    );
    // Prove the cap is the real ~32KB documented budget, not some much
    // smaller accidental bound (this pins the double-counting bug fix: the
    // check used to fire at effectively half of FILE_TEXT_CAP_BYTES for a
    // pure-addition file like this one).
    assert!(
        (added_len as usize) > FILE_TEXT_CAP_BYTES / 2,
        "stored added-text length {added_len} is suspiciously small relative \
         to the {FILE_TEXT_CAP_BYTES}-byte cap — the per-file budget check may \
         be double-counting and truncating far earlier than intended"
    );
    assert_eq!(truncated, 1, "the oversized file's hunk must be flagged truncated, never silently clipped");
}

// ---------------------------------------------------------------------------
// Phase 2: schema version stamp, phase-A re-run on ref change, lazy ingest
// ---------------------------------------------------------------------------

#[test]
fn index_state_reports_schema_version() {
    let dir = make_repo();
    let root = dir.path();
    let store = FactsStore::open(root).expect("open store");
    let state = store.index_state();
    assert_eq!(
        state.schema_version,
        pixel_facts::store::FACTS_SCHEMA_VERSION,
        "index_state must report the on-disk schema version"
    );
}

#[test]
fn phase_a_reruns_when_refs_change() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");
    let opts = IngestOptions::default();
    ingest_until_fresh(&mut store, &opts).expect("ingest until fresh");
    assert!(store.index_state().fresh, "should be fresh after ingest");

    // Add a new commit: refs moved, so the index is stale again.
    fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"v2\");\n}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "bump to v2"]);

    let state = store.index_state();
    assert!(
        !state.fresh,
        "a ref move must make the index stale, got {state:?}"
    );
    assert_eq!(
        state.phase, "phase_a",
        "a ref move should put us back in phase_a, got {state:?}"
    );

    // Re-ingest converges to fresh again.
    ingest_until_fresh(&mut store, &opts).expect("re-ingest until fresh");
    assert!(
        store.index_state().fresh,
        "re-ingest should restore freshness after a ref move"
    );
}

#[test]
fn pre_versioned_db_with_rows_self_heals_on_open() {
    let dir = make_repo();
    let root = dir.path();
    // Build a store and ingest so the db has rows.
    {
        let mut store = FactsStore::open(root).expect("open store");
        ingest_until_fresh(&mut store, &IngestOptions::default()).expect("ingest");
        // Simulate a pre-versioned (poisoned) db: reset user_version to 0
        // while rows remain. On next open it must be rebuilt (self-healed).
        store
            .conn()
            .pragma_update(None, "user_version", 0)
            .expect("reset version");
    }
    let store = FactsStore::open(root).expect("reopen");
    let state = store.index_state();
    assert_eq!(
        state.schema_version,
        pixel_facts::store::FACTS_SCHEMA_VERSION,
        "reopened store must be stamped with the current schema version"
    );
    // The poisoned db was rebuilt, so it's empty again (derived data).
    assert_eq!(state.total_commits, 0, "rebuilt db should start empty");
}

#[test]
fn concurrent_open_on_poisoned_db_never_ioerrors() {
    // Regression: two processes both deciding a rebuild is needed and racing
    // remove_db() against another's live WAL connection used to surface as
    // "rusqlite: disk I/O error". FactsStore::open now serializes the
    // rebuild-decision + delete + recreate critical section behind an
    // advisory file lock, so N concurrent openers on the same poisoned db
    // must all succeed cleanly instead of racing.
    let dir = make_repo();
    let root = dir.path();
    {
        let mut store = FactsStore::open(root).expect("open store");
        ingest_until_fresh(&mut store, &IngestOptions::default()).expect("ingest");
        store
            .conn()
            .pragma_update(None, "user_version", 0)
            .expect("reset version");
    }

    let root = root.to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || FactsStore::open(&root).map(|s| s.index_state()))
        })
        .collect();

    for h in handles {
        let state = h.join().expect("thread panicked").expect("concurrent open must not error");
        assert_eq!(state.schema_version, pixel_facts::store::FACTS_SCHEMA_VERSION);
    }
}

#[test]
fn lazy_ingest_is_bounded_and_converges() {
    let dir = make_repo();
    let root = dir.path();
    let mut store = FactsStore::open(root).expect("open store");
    // A generous budget still converges for a tiny 3-commit repo.
    let report = pixel_facts::ingest::ingest_until_fresh_bounded(&mut store, 5000)
        .expect("bounded lazy ingest");
    assert!(report.fresh, "bounded lazy ingest should converge on a tiny repo");
    assert!(store.index_state().fresh, "index_state should be fresh after lazy ingest");
}
