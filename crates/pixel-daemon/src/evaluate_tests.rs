// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel evaluate`: the snapshot contract, the resolution policy and the
//! statuses they produce.
//!
//! A child module of `api` rather than an integration test, because the two
//! races below need the seam between the before-check and the after-check,
//! which is private on purpose: widening the daemon's public surface to
//! reach it would make the test a worse reader of the contract, not a
//! better one.
//!
//! The races are the tests that matter. Without an after-check, a negative
//! answer can be produced from a tree that had already grown the very edge
//! it denies; and the second race edits a file no witness names, which is
//! exactly where an unseen edge hides, so it fails the moment the check is
//! narrowed from the whole tree to the witness's own files.

use super::*;
use pixel_proto::evaluate as wire;
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pixel-evaluate-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Drive the fixture's git through [`pixel_git::GitRunner`], the bounded
/// primitive the whole workspace goes through.
///
/// Spawning the binary directly would be the obvious thing here, and the
/// boundary test in `pixel-git` would reject it: this file lives under
/// `src/` (it is `#[path]`-included into `api`'s test module), and that
/// scanner only exempts a test region opened in the same file — it also
/// matches on text, so the spawn it forbids must not appear even in a
/// comment. Going through the runner is the right answer anyway: the
/// fixture inherits the timeout and the output cap instead of being one
/// more place a stuck `git` can hang the suite.
///
/// `GIT_CONFIG_GLOBAL` is pinned away from the developer's own config so a
/// local `commit.gpgsign` or hook cannot fail the fixture.
fn git(dir: &Path, args: &[&str]) {
    let out = pixel_git::GitRunner::new(dir)
        .run_output(
            args,
            &[
                ("GIT_AUTHOR_NAME", "t"),
                ("GIT_AUTHOR_EMAIL", "t@t"),
                ("GIT_COMMITTER_NAME", "t"),
                ("GIT_COMMITTER_EMAIL", "t@t"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ],
        )
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(out.success(), "git {args:?}: {}", out.stderr);
}

/// `work` calls `helper`; `lonely` calls nothing. `spare.ts` is there so a
/// test can edit a file that appears in no witness.
fn fixture(tag: &str) -> PathBuf {
    let dir = tmpdir(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/worker.ts"),
        "import { helper } from \"./util\";\n\
         export function work(n: number): number {\n  return helper(n)\n}\n\
         export function lonely(): number {\n  return 0\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/spare.ts"),
        "export function spare(): number { return 2 }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);
    build_graph(&dir);
    dir
}

/// Build the graph the way anything else in the repository does.
///
/// `evaluate` deliberately never builds one: a full build can take minutes
/// and is a decision the caller makes with a command of its own, so asking
/// a question about a repository that has none answers
/// `graph_unavailable` instead of silently spending the time.
fn build_graph(dir: &Path) {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Symbol {
        name: "work".into(),
    });
    assert!(resp.ok, "graph build failed: {:?}", resp.error);
}

/// A second `helper`, outside `src/`, so a bare name stops being unique
/// while `--in src` can still tell the two apart — the shape of the real
/// case, where a vendored or generated copy shadows the one meant.
fn add_second_helper(dir: &Path) {
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(
        dir.join("lib/other.ts"),
        "export function helper(y: number): number { return y }\n",
    )
    .unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "second helper"]);
    build_graph(dir);
}

fn request(from: &str, to: &str) -> EvaluateRequest {
    EvaluateRequest {
        from: from.to_string(),
        to: to.to_string(),
        traversal: None,
        tiers: None,
        max_depth: None,
        time_budget_ms: None,
        scope: None,
        at_snapshot: false,
    }
}

fn evaluate(svc: &mut Service, req: EvaluateRequest) -> wire::EvaluationEnvelope {
    evaluate_probed(svc, req, &mut || {})
}

fn evaluate_probed(
    svc: &mut Service,
    req: EvaluateRequest,
    probe: &mut dyn FnMut(),
) -> wire::EvaluationEnvelope {
    let value = svc.op_evaluate_probed(req, probe).expect("op failed");
    match serde_json::from_value::<wire::Output>(value).expect("output shape") {
        wire::Output::Evaluation(envelope) => *envelope,
        wire::Output::Error(error) => panic!("expected an evaluation, got {error:?}"),
    }
}

fn reason(envelope: &wire::EvaluationEnvelope) -> &wire::Reason {
    match &envelope.outcome {
        wire::Outcome::Unknown { reason, .. } => reason,
        other => panic!("expected unknown, got {other:?}"),
    }
}

#[test]
fn a_reachable_pair_should_be_established_with_the_call_site_as_witness() {
    let dir = fixture("established");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));

    let wire::Outcome::Established { witness } = &envelope.outcome else {
        panic!("expected established, got {:?}", envelope.outcome);
    };
    let wire::Witness::Path { edges, .. } = witness else {
        panic!("expected a path witness, got {witness:?}");
    };
    assert_eq!(edges.len(), 1, "one hop from work to helper: {edges:?}");
    assert!(edges[0].from.uid.contains("work"), "{:?}", edges[0].from);
    assert!(edges[0].to.uid.contains("helper"), "{:?}", edges[0].to);
    assert_eq!(edges[0].edge.site.path, "src/worker.ts");
    assert!(
        !edges[0].from.content_hash.is_empty(),
        "the witness must carry the hash of the bytes the graph parsed"
    );
    assert_eq!(envelope.outcome.answer(), Some(true));
}

#[test]
fn an_unreachable_pair_should_be_absent_in_snapshot_with_an_exhaustive_traversal() {
    let dir = fixture("absent");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("lonely", "helper"));

    assert_eq!(
        envelope.outcome,
        wire::Outcome::AbsentInSnapshot,
        "{:?}",
        envelope.outcome
    );
    assert!(
        envelope.coverage.traversal_exhausted,
        "an absence is only an answer when the traversal exhausted: {:?}",
        envelope.coverage
    );
    assert_eq!(envelope.outcome.answer(), Some(false));
}

/// The point of the after-check: a file rewritten while the traversal runs
/// invalidates the answer, whatever that answer was.
#[test]
fn a_file_rewritten_during_the_traversal_should_yield_snapshot_changed() {
    let dir = fixture("race-witness");
    let mut svc = Service::open(&dir).unwrap();
    // Warm the graph so the edit lands after the before-check, not before it.
    let _ = evaluate(&mut svc, request("work", "helper"));

    let target = dir.join("src/worker.ts");
    let envelope = evaluate_probed(&mut svc, request("work", "helper"), &mut || {
        std::fs::write(
            &target,
            "import { helper } from \"./util\";\n\
             export function work(n: number): number {\n  return n\n}\n",
        )
        .unwrap();
    });

    assert_eq!(
        reason(&envelope),
        &wire::Reason::SnapshotChanged,
        "{:?}",
        envelope.outcome
    );
    assert_eq!(envelope.outcome.answer(), None);
}

/// The check is whole-tree, not witness-only. The file edited here appears
/// in no witness, and it is exactly where an unseen edge would live:
/// narrowing the after-check to the witness's files lets this stale
/// negative through.
#[test]
fn a_file_outside_the_witness_rewritten_during_a_negative_should_yield_snapshot_changed() {
    let dir = fixture("race-outside");
    let mut svc = Service::open(&dir).unwrap();
    let _ = evaluate(&mut svc, request("lonely", "helper"));

    let untouched_by_the_answer = dir.join("src/spare.ts");
    let envelope = evaluate_probed(&mut svc, request("lonely", "helper"), &mut || {
        std::fs::write(
            &untouched_by_the_answer,
            "export function spare(): number { return 3 }\n",
        )
        .unwrap();
    });

    assert_eq!(
        reason(&envelope),
        &wire::Reason::SnapshotChanged,
        "a negative must not survive an edit to a file it never looked at: {:?}",
        envelope.outcome
    );
}

/// `--at-snapshot` is the documented way to answer about the stored
/// generation: it waives the after-check and says so on the wire.
#[test]
fn at_snapshot_should_waive_the_after_check_and_report_before_only() {
    let dir = fixture("at-snapshot");
    let mut svc = Service::open(&dir).unwrap();
    let _ = evaluate(&mut svc, request("work", "helper"));

    let target = dir.join("src/spare.ts");
    let mut req = request("work", "helper");
    req.at_snapshot = true;
    let envelope = evaluate_probed(&mut svc, req, &mut || {
        std::fs::write(&target, "export function spare(): number { return 4 }\n").unwrap();
    });

    assert_eq!(
        envelope.snapshot.working_tree_check,
        wire::WorkingTreeCheck::BeforeOnly
    );
    assert!(
        matches!(envelope.outcome, wire::Outcome::Established { .. }),
        "the stored snapshot still holds the path: {:?}",
        envelope.outcome
    );
}

/// Without `--at-snapshot` the same answer names the full check, so the two
/// are distinguishable by a reader and by a machine.
#[test]
fn a_normal_run_should_report_the_full_before_and_after_check() {
    let dir = fixture("full-check");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));

    assert_eq!(
        envelope.snapshot.working_tree_check,
        wire::WorkingTreeCheck::FullBeforeAndAfter
    );
    assert!(envelope.snapshot.working_tree_matches);
    assert!(
        !envelope.snapshot.signature.is_empty(),
        "an answer names the generation it came from"
    );
}

/// A uid that names nothing is a resolution failure, never an absence of
/// path: "I could not ask" and "the answer is no" are different claims, and
/// conflating them turns a typo into a false negative.
#[test]
fn a_uid_that_does_not_exist_should_be_symbol_not_found_never_absent() {
    let dir = fixture("bad-uid");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("src/worker.ts#ghost#function", "helper"));

    assert_eq!(
        reason(&envelope),
        &wire::Reason::SymbolNotFound {
            argument: "--from".to_string()
        },
        "{:?}",
        envelope.outcome
    );
    assert_ne!(envelope.outcome.answer(), Some(false));
}

/// A name shared by two symbols is never picked for the caller. The
/// candidates carry the uids that resolve it, and re-asking with one
/// answers — which is what makes the ambiguity actionable rather than a
/// dead end.
#[test]
fn an_ambiguous_name_should_list_candidates_whose_uid_then_resolves() {
    let dir = fixture("ambiguous");
    add_second_helper(&dir);

    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));

    let wire::Reason::AmbiguousSymbol {
        argument,
        candidates,
    } = reason(&envelope)
    else {
        panic!("expected ambiguity, got {:?}", envelope.outcome);
    };
    assert_eq!(argument, "--to");
    assert_eq!(candidates.len(), 2, "{candidates:?}");

    let uid = candidates
        .iter()
        .find(|c| c.path == "src/util.ts")
        .expect("the helper worker.ts imports")
        .uid
        .clone();
    let resolved = evaluate(&mut svc, request("work", &uid));
    assert!(
        matches!(resolved.outcome, wire::Outcome::Established { .. }),
        "re-asking with a candidate uid must answer: {:?}",
        resolved.outcome
    );
}

/// `--in` narrows resolution instead of forcing the caller to paste a uid.
/// It scopes every name argument, not just the ambiguous one, so the rule
/// stays predictable: a name resolves iff it is unique under the prefix.
#[test]
fn the_in_scope_should_disambiguate_a_shared_name() {
    let dir = fixture("scoped");
    add_second_helper(&dir);

    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.scope = Some("src".to_string());
    let envelope = evaluate(&mut svc, req);

    assert!(
        matches!(envelope.outcome, wire::Outcome::Established { .. }),
        "{:?}",
        envelope.outcome
    );
}

/// No graph database is `graph_unavailable`, and the op does not quietly
/// build one: a rebuild can take minutes and is the caller's decision.
#[test]
fn a_repository_with_no_graph_should_be_graph_unavailable() {
    let dir = fixture("no-graph");
    let mut svc = Service::open(&dir).unwrap();
    let db = svc.graph_db_path();
    svc.graph = None;
    std::fs::remove_file(&db).ok();

    let envelope = evaluate(&mut svc, request("work", "helper"));

    assert_eq!(reason(&envelope), &wire::Reason::GraphUnavailable);
    assert!(
        !db.exists(),
        "asking a question must not build a graph as a side effect"
    );
}

/// A withheld signature is a writer saying "I could not vouch for the tree
/// I just wrote". There is no generation to attribute an answer to, so the
/// op reports staleness rather than naming a signature that means nothing.
#[test]
fn a_withheld_signature_should_be_graph_stale() {
    let dir = fixture("withheld");
    let mut svc = Service::open(&dir).unwrap();
    let _ = evaluate(&mut svc, request("work", "helper"));

    let db = svc.graph_db_path();
    svc.graph = None;
    {
        let store = GraphStore::open(&db).unwrap();
        store
            .meta_set(
                pixel_graph::build::FRESHNESS_KEY,
                pixel_graph::build::FRESHNESS_WITHHELD,
            )
            .unwrap();
    }

    let envelope = evaluate(&mut svc, request("work", "helper"));
    assert_eq!(
        reason(&envelope),
        &wire::Reason::GraphStale,
        "{:?}",
        envelope.outcome
    );
}

/// Every `unknown` carries a way out; a reason with no action is a dead end
/// the caller cannot act on.
#[test]
fn every_unknown_should_carry_next_actions() {
    let dir = fixture("actions");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("ghost_name", "helper"));

    let wire::Outcome::Unknown { next_actions, .. } = &envelope.outcome else {
        panic!("expected unknown, got {:?}", envelope.outcome);
    };
    assert!(!next_actions.is_empty(), "{next_actions:?}");
}

/// The tier selection reaches the wire as the relation that was walked, not
/// as a confidence label.
#[test]
fn the_domain_should_report_the_relation_that_was_walked() {
    let dir = fixture("domain");
    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.tiers = Some("exact,probable".to_string());
    req.traversal = Some("callers".to_string());
    let envelope = evaluate(&mut svc, req);

    assert_eq!(envelope.domain.traversal, wire::Traversal::Callers);
    assert_eq!(
        envelope.domain.tiers,
        vec![wire::Tier::Exact, wire::Tier::Probable]
    );
    assert_eq!(envelope.domain.relation, wire::RELATION_INDEXED_CALL_GRAPH);
}

/// `callers` answers a different proposition from `callees`, so the two
/// must not agree on an asymmetric pair. A direction bug that swapped them
/// would otherwise pass every single-direction test.
#[test]
fn the_traversal_direction_should_change_the_answer_on_an_asymmetric_pair() {
    let dir = fixture("direction");
    let mut svc = Service::open(&dir).unwrap();

    let forward = evaluate(&mut svc, request("work", "helper"));
    let mut backward = request("work", "helper");
    backward.traversal = Some("callers".to_string());
    let backward = evaluate(&mut svc, backward);

    assert!(matches!(forward.outcome, wire::Outcome::Established { .. }));
    assert_eq!(backward.outcome, wire::Outcome::AbsentInSnapshot);
}

/// An unknown flag value is a usage error, never a silent fallback to a
/// relation the caller did not ask for.
#[test]
fn an_unknown_tiers_value_should_be_refused_rather_than_widened() {
    let dir = fixture("bad-tiers");
    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.tiers = Some("probable".to_string());

    let error = svc.op_evaluate(req).expect_err("must be refused");
    assert!(error.contains("--tiers"), "{error}");
}

#[test]
fn an_unknown_traversal_value_should_be_refused() {
    let dir = fixture("bad-traversal");
    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.traversal = Some("sideways".to_string());

    let error = svc.op_evaluate(req).expect_err("must be refused");
    assert!(error.contains("--traversal"), "{error}");
}

/// A budget that cut the traversal is reported as such, with the flag that
/// raises it — never as an absence.
#[test]
fn a_depth_cap_that_drops_a_frontier_should_be_unknown_with_the_flag_to_raise() {
    let dir = fixture("budget");
    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.max_depth = Some(0);
    let envelope = evaluate(&mut svc, req);

    let wire::Reason::TraversalBudgetExhausted { parameter, current } = reason(&envelope) else {
        panic!("expected a budget reason, got {:?}", envelope.outcome);
    };
    assert_eq!(*parameter, wire::BudgetParameter::MaxDepth);
    assert_eq!(*current, 0);
    assert!(
        !envelope.coverage.traversal_exhausted,
        "a cut traversal is not exhaustive"
    );
}

/// The summary is what a screenshot shows, so the scope travels in its
/// first sentence rather than in a field a reader may never open.
#[test]
fn the_summary_should_name_the_snapshot_and_the_relation() {
    let dir = fixture("summary");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("lonely", "helper"));

    let first = envelope
        .summary
        .split(". ")
        .next()
        .expect("a summary has a first sentence");
    assert!(first.contains("snapshot"), "{first}");
    assert!(first.contains("tiers exact"), "{first}");
    assert!(first.contains("traversal callees"), "{first}");
    assert!(
        first.contains("indexed call graph"),
        "the verdict must name the relation it is about: {first}"
    );
}

/// Deleting the zero-row arm of the name lookup turns "no such symbol"
/// into "ambiguous, here are the candidates" with an empty candidate list:
/// a reason that tells the caller to disambiguate between nothing. The uid
/// path returns `SymbolNotFound` from its own branch, so only a *name* that
/// matches no row exercises this one.
#[test]
fn a_name_that_matches_no_symbol_should_be_symbol_not_found_never_ambiguous() {
    let dir = fixture("name-absent");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "noSuchFunctionAnywhere"));

    assert_eq!(
        reason(&envelope),
        &wire::Reason::SymbolNotFound {
            argument: "--to".to_string()
        },
        "a name with no rows is not an ambiguity: {:?}",
        envelope.outcome
    );
}

/// The same arm, reached the other way: `--in` filters every row out. The
/// name exists in the store, so only the post-filter count can produce the
/// right reason.
#[test]
fn a_scope_that_excludes_every_match_should_be_symbol_not_found() {
    let dir = fixture("scope-empty");
    let mut svc = Service::open(&dir).unwrap();
    let mut req = request("work", "helper");
    req.scope = Some("no/such/dir".to_string());
    let envelope = evaluate(&mut svc, req);

    assert_eq!(
        reason(&envelope),
        &wire::Reason::SymbolNotFound {
            argument: "--from".to_string()
        },
        "{:?}",
        envelope.outcome
    );
}

/// The witness carries the content hash so a reader can re-read the bytes
/// the graph parsed and check the claim. That only works if it is the hash
/// the store actually holds for that file: any other string — a constant, a
/// digest of something else — still looks like a hash and silently fails
/// every verification done against it.
#[test]
fn a_witness_hash_should_be_the_stored_hash_of_the_file_it_names() {
    let dir = fixture("witness-hash");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));

    let wire::Outcome::Established { witness } = &envelope.outcome else {
        panic!("expected established, got {:?}", envelope.outcome);
    };
    let wire::Witness::Path { edges, .. } = witness else {
        panic!("expected a path witness, got {witness:?}");
    };

    let store = svc.graph.as_ref().expect("the evaluation opened the graph");
    for end in [&edges[0].from, &edges[0].to] {
        let stored = store
            .file_by_path(&end.path)
            .expect("store readable")
            .unwrap_or_else(|| panic!("the witness names a file the store has: {}", end.path))
            .blob_oid;
        assert_eq!(
            end.content_hash, stored,
            "the witness hash for {} must be the hash the graph parsed",
            end.path
        );
        assert!(
            !stored.is_empty(),
            "the fixture must give the store a real hash to compare against"
        );
    }
}

/// `coverage.graph_file_cap_hit` is a published claim about how the graph
/// was built, and the three cases have to stay apart: no cap in force is
/// not "the cap was hit", and a graph that stopped exactly at the cap is
/// the case the flag exists for — a walk that ends at the cap is
/// indistinguishable from one that ended at the cap with more files left.
#[test]
fn the_file_cap_flag_should_track_the_cap_the_build_actually_enforced() {
    let dir = fixture("file-cap");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));
    assert!(
        !envelope.coverage.graph_file_cap_hit,
        "a three-file fixture under the default 50 000-file cap did not hit it"
    );

    let files = svc
        .graph
        .as_ref()
        .expect("the evaluation opened the graph")
        .counts()
        .expect("counts readable")
        .0;
    assert!(files > 1, "the fixture has several files: {files}");

    assert!(
        !svc.graph_file_cap_hit(None),
        "no cap in force is never a cap that was hit"
    );
    assert!(
        svc.graph_file_cap_hit(Some(files as usize)),
        "a graph holding exactly the cap is where the walk stopping early hides"
    );
    assert!(
        svc.graph_file_cap_hit(Some(1)),
        "more files than the cap admits means the cap was hit"
    );
    assert!(
        !svc.graph_file_cap_hit(Some(files as usize + 1)),
        "one file short of the cap is a walk that reached the end of the tree"
    );
}

/// The blind spots ride on every answer, not just the ones that reach the
/// unit that builds them: `closed_world` is false because of this list, so
/// an envelope that lost it would publish a completeness it cannot back.
#[test]
fn every_envelope_should_publish_the_extraction_blind_spots() {
    let dir = fixture("limits-on-wire");
    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));

    assert_eq!(
        envelope.coverage.extraction_limits.len(),
        4,
        "{:?}",
        envelope.coverage.extraction_limits
    );
    assert!(
        envelope
            .coverage
            .extraction_limits
            .iter()
            .any(|l| l.contains("dynamic dispatch")),
        "{:?}",
        envelope.coverage.extraction_limits
    );
}

/// The database path of a fixture, for the tests that must reach past the
/// service to the store it reads.
fn graph_db(dir: &Path) -> PathBuf {
    Service::open(dir).unwrap().graph_db_path()
}

/// Replace a table with one of an unusable shape, through a connection of
/// its own, so an already-open store's next read fails the way a corrupted
/// or half-migrated database makes it fail.
fn break_table(db: &Path, table: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(&format!("DROP TABLE {table}"), []).unwrap();
    conn.execute(&format!("CREATE TABLE {table} (unusable INTEGER)"), [])
        .unwrap();
}

fn path_of_row(store: &GraphStore, row: &pixel_graph::store::SymbolRow) -> String {
    store
        .file_by_id(row.file_id)
        .unwrap()
        .expect("the resolved symbol must belong to a file")
        .path
}

/// A store that cannot answer is not a symbol that does not exist.
///
/// Both are "no row came back", and collapsing them publishes
/// `symbol_not_found` — a statement about the code — on behalf of a
/// database that never answered. The caller would take it as fact and stop
/// looking. A technical failure leaves as an error and exits 3.
#[test]
fn a_store_that_cannot_answer_should_fail_rather_than_report_a_missing_symbol() {
    let dir = fixture("store-error");
    let db = graph_db(&dir);
    // Opened while the database is healthy, then broken underneath it:
    // dropping the table outright would only have the store recreate an
    // empty one on open, which answers "no such symbol" truthfully, and a
    // store opened on a mangled schema fails at open rather than at the
    // read this test is about.
    let store = GraphStore::open(&db).unwrap();
    break_table(&db, "symbols");

    // Both resolution paths: a uid is looked up verbatim, a bare name is
    // ranked, and each has its own query to fail.
    for value in ["helper", "src/util.ts#helper#function"] {
        match evaluate::resolve_argument(&store, "--from", value, None) {
            Err(evaluate::Failure::Store(_)) => {}
            other => panic!("a broken store must not answer `{value}` with an absence: {other:?}"),
        }
    }
}

/// The same for the snapshot identity: a failed read is not a stale graph.
///
/// `graph_stale` says the writer withheld its signature, which is a
/// deliberate statement. A database that could not be read made no
/// statement at all.
#[test]
fn an_unreadable_identity_should_fail_rather_than_report_a_stale_graph() {
    let dir = fixture("identity-error");
    let db = graph_db(&dir);
    let store = GraphStore::open(&db).unwrap();
    break_table(&db, "meta");

    match evaluate::identity(&store) {
        Err(evaluate::Failure::Store(_)) => {}
        other => panic!("an unreadable store must not be published as stale: {other:?}"),
    }
}

/// The scope must be part of the question, not a filter over its answer.
///
/// With the scope applied after the store's cap, a name with more homonyms
/// than the cap resolves from a page that may hold none of the scoped ones:
/// the answer becomes `symbol_not_found` for a symbol that plainly exists,
/// and — worse — a single scoped row inside the page looks unique while
/// others sit beyond it, so the evaluation answers about the wrong symbol
/// while claiming exactness.
#[test]
fn a_name_with_more_homonyms_than_the_cap_should_still_resolve_inside_its_scope() {
    let dir = fixture("scope-before-cap");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    // `lib/` sorts before `src/`, so these fill the capped page and push
    // the one that matters out of it.
    for i in 0..(evaluate::CANDIDATE_CAP + 5) {
        std::fs::write(
            dir.join(format!("lib/h{i:03}.ts")),
            "export function helper(y: number): number { return y }\n",
        )
        .unwrap();
    }
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "many helpers"]);
    build_graph(&dir);

    let store = GraphStore::open(&graph_db(&dir)).unwrap();
    let row = evaluate::resolve_argument(&store, "--to", "helper", Some("src"))
        .expect("the single `helper` under src/ must resolve however many exist elsewhere");
    assert_eq!(
        path_of_row(&store, &row),
        "src/util.ts",
        "the scope must select the symbol, not whichever ones the cap let through"
    );
}

/// A scope is a directory, not a string prefix.
///
/// `--in src/foo` must not reach into `src/foobar.ts`: that sibling is a
/// different directory that merely starts with the same letters, and
/// pulling it in turns a unique match into a spurious ambiguity — or, in
/// the other direction, silently widens what an answer is about.
#[test]
fn a_scope_should_not_capture_a_sibling_whose_name_merely_starts_the_same() {
    let dir = fixture("scope-boundary");
    std::fs::create_dir_all(dir.join("src/foo")).unwrap();
    std::fs::write(
        dir.join("src/foo/x.ts"),
        "export function scoped(): number { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/foobar.ts"),
        "export function scoped(): number { return 2 }\n",
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "sibling directory"]);
    build_graph(&dir);

    let store = GraphStore::open(&graph_db(&dir)).unwrap();
    // Both spellings, because the doc comment promises they are the same
    // scope; the directory itself and what is under it, nothing beside it.
    for scope in ["src/foo", "src/foo/"] {
        let row = evaluate::resolve_argument(&store, "--from", "scoped", Some(scope))
            .unwrap_or_else(|e| panic!("`--in {scope}` must resolve to the one under it: {e:?}"));
        assert_eq!(
            path_of_row(&store, &row),
            "src/foo/x.ts",
            "`--in {scope}` must not reach into src/foobar.ts"
        );
    }

    // The sibling is still findable, so the boundary narrowed the scope
    // rather than losing the file.
    match evaluate::resolve_argument(&store, "--from", "scoped", Some("src")) {
        Err(evaluate::Failure::Halt(evaluate::Halt(wire::Reason::AmbiguousSymbol {
            candidates,
            ..
        }))) => assert_eq!(candidates.len(), 2, "both copies live under src/"),
        other => panic!("`--in src` covers both copies and must be ambiguous: {other:?}"),
    }
}

/// The cap an evaluation reports is the one the graph was built under.
///
/// The cap lives in the environment and the graph outlives the process that
/// built it, so a daemon restarted with a different `PIXEL_GRAPH_MAX_FILES`
/// would otherwise describe a stored graph with a ceiling that never
/// applied to it — and a cap raised after a truncated build would report
/// `false` for a walk that did stop at one, narrowing a published limit
/// instead of widening it.
#[test]
fn the_reported_file_cap_should_be_the_one_the_graph_was_built_under() {
    let dir = fixture("stored-cap");
    let db = graph_db(&dir);
    // What a build under a ceiling this tree certainly reached would have
    // recorded. Nothing in this process's environment says so.
    GraphStore::open(&db)
        .unwrap()
        .meta_set(pixel_graph::build::GRAPH_FILE_CAP_KEY, "1")
        .unwrap();

    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));
    assert!(
        envelope.coverage.graph_file_cap_hit,
        "the stored cap decides the flag, not the environment: {:?}",
        envelope.coverage
    );
}

/// And a graph built without a cap never claims one was hit.
#[test]
fn a_graph_built_without_a_cap_should_never_report_one_hit() {
    let dir = fixture("stored-cap-none");
    GraphStore::open(&graph_db(&dir))
        .unwrap()
        .meta_set(pixel_graph::build::GRAPH_FILE_CAP_KEY, "none")
        .unwrap();

    let mut svc = Service::open(&dir).unwrap();
    let envelope = evaluate(&mut svc, request("work", "helper"));
    assert!(
        !envelope.coverage.graph_file_cap_hit,
        "an unbounded walk has no ceiling to hit: {:?}",
        envelope.coverage
    );
}
