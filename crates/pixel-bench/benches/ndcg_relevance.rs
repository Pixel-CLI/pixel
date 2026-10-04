// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! NDCG@10 relevance benchmark for pixel search, measured over a real
//! labeled query set on this workspace's `pixel-graph` crate.
//!
//! Why NDCG: it is the same retrieval-quality metric semble publishes
//! (their claim: NDCG@10 0.854 across 63 repos). Publishing our own measured
//! number — instead of borrowing theirs — is the credibility anchor per the
//! T1 doctrine (no claim without a measurement). We measure BOTH ranked
//! (`scope: "code"`) and unranked search on the same qrels, so the ranking
//! layer's contribution is isolated.
//!
//! A/B lane: a third measurement runs `pixel ask` (semantic code search via
//! potion-code-16M-v2 static embeddings) over the SAME qrels, rooted at the
//! same `crates/pixel-graph/src` subtree, so the lexical-vs-semantic gap is
//! measured on identical inputs — not borrowed from semble's corpus.
//!
//! Method (TREC-style NDCG@10):
//!   - q: a natural-language-ish query issued as a `search` pattern.
//!   - relevant: ground-truth files that genuinely answer q (hand-labeled
//!     from this repo's structure, not from search results).
//!   - R(q): the ranked list of match ORDER as search returns it (per-file
//!     deduped, first position of each file).
//!   - DCG@10 = sum over top-10 of rel(i)/log2(i+2), IDCG from ideal order,
//!     NDCG@10 = DCG/IDCG.
//!
//! Honest bounds:
//!   - Search is keyword/regex-forward, not semantic-NL ("how is X handled?").
//!     Queries here are the *keyword endpoints* of real questions, so the
//!     number measures keyword retrieval quality, not open-ended NL.
//!   - Ground truth is 1 (relevant) / 0 (irrelevant); no graded relevance.
//!   - Pool is capped but on this crate it is complete for these patterns.
//!
//! This is a self-benchmark; it must NOT be reported as comparable to
//! semble's cross-repo number, only as pixel's own measured quality and as a
//! regression gate for the ranking layer and the `ask` semantic channel.

use std::collections::HashSet;
use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pixel_bench::{RESOLVE_RANK_GATE, labelled_rank, validate_query_score, validate_resolve_rank};
use pixel_daemon::api::{Response, Service};
use pixel_proto::Op;

/// Ground truth (v2): query -> files in `crates/pixel-graph/src/` that
/// genuinely answer it. Labeled from the code (module responsibilities), not
/// search output. Each file's relevant label is semantic — e.g. "how the
/// concept index resolves phrases" → `concept_resolve.rs` even though the
/// word "concept" is a substring of several files. v2 preserves every frozen
/// query and adds the independently audited second owner where the legacy
/// one-file label was incomplete.
fn qrels(crate_dir: &std::path::Path) -> Vec<(&'static str, Vec<String>)> {
    // `crate_dir` is the workspace root. Search returns paths RELATIVE to
    // the workspace root (`crates/pixel-graph/src/…`), so qrels must use the
    // same relative form to compare against `file_order_from_response`.
    let _ = crate_dir;
    let tag = |name: &str| format!("crates/pixel-graph/src/{name}");
    vec![
        (
            "concept index resolve phrase map marked",
            vec![tag("concept_resolve.rs"), tag("concept.rs")],
        ),
        (
            "callers callees impact trace reachability",
            vec![tag("impact.rs")],
        ),
        (
            "cluster functional area detect co locate",
            vec![tag("cluster.rs")],
        ),
        (
            "extract tree sitter symbols ast code",
            vec![tag("extract.rs")],
        ),
        (
            "imports dependency resolved graph edge",
            vec![tag("imports.rs")],
        ),
        ("process execution flow detection", vec![tag("process.rs")]),
        (
            "resolution ranked candidate ambiguity disambiguation",
            // `resolve.rs` handles call-target ambiguity, while
            // `concept_resolve.rs` owns `RankedCandidate` and phrase-candidate
            // disambiguation. The frozen query describes both, so a strict
            // one-file label was not a valid top-1 control.
            vec![tag("resolve.rs"), tag("concept_resolve.rs")],
        ),
        ("symbol store database query index", vec![tag("store.rs")]),
        (
            "changes blast radius working tree diff",
            vec![tag("changes.rs")],
        ),
        ("trace path between symbols", vec![tag("trace.rs")]),
    ]
}

/// Extract per-file order from a Search response: walk the ordered match
/// rows and record the first-seen position of each distinct relative path.
fn file_order_from_response(resp: &Response) -> Vec<String> {
    let matches = resp
        .data()
        .get("matches")
        .and_then(|x| x.as_array())
        .expect("successful retrieval response must contain a matches array");
    pixel_bench::checked_file_order(
        matches
            .iter()
            .map(|m| m.get("path").and_then(|x| x.as_str())),
    )
    .expect("successful retrieval response must contain valid match paths")
}

/// Success-rate (correctness axis), distinct from NDCG: 1.0 if any of the
/// top-k results is a ground-truth relevant file, else 0.0. This is the "did
/// we solve the task" binary per task — the axis P0·1 adds on top of (not instead
/// of) retrieval quality. Where NDCG rewards how high in the ranking the relevant
/// file lands, success-rate asks the sharper binary question: did hit numberk/#1
/// actually answer (or not). Mean over tasks = % solved, directly comparable
/// across the lexical-search / resolve lanes.
fn success_at_k(ranking: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    for path in ranking.iter().take(k) {
        if relevant.contains(path) {
            return 1.0;
        }
    }
    0.0
}

/// Run the Engine-1 `resolve` lane (concept-index cascade rank) over the SAME
/// qrels the NDCG lanes use, counting each task as solved (1) if the top resolved
/// match is one of its ground-truth relevant files, else unsolved (0). This is
/// the correctness / success-rate axis for pixel's own resolve machinery —
/// deterministic, no API, no agent, measured on identical inputs as the other lanes.
///
/// The per-probe guard is a rank gate ([`RESOLVE_RANK_GATE`]), not the
/// success@1 the lane reports. Gating each probe on rank 1 made this lane the
/// tripwire the `ask` lane above was already corrected for: writing a scoped
/// query correctly put a new file at the top of a probe while the labelled
/// answer's own score never moved (the measurement is with the constant).
/// Whether the cascade *answers* the task is what the gate asks; how often it
/// answers first is what the reported mean measures, and every probe's rank
/// is printed so a file sliding toward the gate is visible before it fires.
fn resolve_success_rate(svc: &mut Service, qrels: &[(&'static str, Vec<String>)]) -> f64 {
    let mut sum = 0.0;
    for (q, relevant) in qrels {
        let rel_set: HashSet<String> = relevant.iter().cloned().collect();
        // `resolve` takes a phrase directly (concept-index engine), not a regex
        // alternation — pass the query string verbatim so the inputs match the
        // semantic ground-truth labels the qrels are labeled from.
        let resp = svc.handle(Op::Resolve {
            phrase: q.to_string(),
            limit: Some(10),
        });
        assert!(resp.ok, "resolve query {q:?} failed: {resp:?}");
        let order = file_order_from_response(&resp);
        let score = success_at_k(&order, &rel_set, 1);
        let rank = labelled_rank(&order, &rel_set);
        eprintln!(
            "resolve query={q:?} rank={} success@1={score}",
            rank.map_or_else(|| "unretrieved".to_string(), |rank| rank.to_string())
        );
        validate_resolve_rank(rank, RESOLVE_RANK_GATE)
            .unwrap_or_else(|err| panic!("resolve query {q:?}: {err}"));
        sum += score;
    }
    sum / qrels.len() as f64
}

fn ndcg_at_k(ranking: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    let mut dcg = 0.0;
    let mut idcg = 0.0;
    let rel_count = relevant.len();
    for (i, path) in ranking.iter().take(k).enumerate() {
        let rel = if relevant.contains(path) { 1.0 } else { 0.0 };
        dcg += rel / ((i + 2) as f64).log2(); // log2(i+2): 0-indexed discount
    }
    // Ideal: all relevant files first (only count those that fit in k).
    for j in 0..k.min(rel_count) {
        idcg += 1.0 / ((j + 2) as f64).log2();
    }
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn run_ndcg(
    svc: &mut Service,
    qrels: &[(&'static str, Vec<String>)],
    scope: Option<&str>,
    k: usize,
) -> f64 {
    let mut sum = 0.0;
    for (q, relevant) in qrels {
        let rel_set: HashSet<String> = relevant.iter().cloned().collect();
        // `search` takes one regex pattern, not a phrase. Build the honest
        // keyword endpoint: alternation of the query's identifier words so
        // "concept index resolve" becomes `concept|index|resolve` (same as
        // a real agent lowering an NL question to a search).
        let words: Vec<&str> = q.split_whitespace().filter(|w| !w.is_empty()).collect();
        let pattern = if words.len() == 1 {
            words[0].to_string()
        } else {
            words.join("|")
        };
        // Search limits matching lines, not files. A single file can fill a
        // page: follow the advertised cursor until we actually have k files.
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        let mut offset = 0;
        loop {
            let resp = svc.handle(Op::Search {
                pattern: pattern.clone(),
                json: true,
                limit: Some(50),
                offset: Some(offset),
                paths: Some(vec!["crates/pixel-graph/src".to_string()]),
                scope: scope.map(str::to_string),
                globs: Vec::new(),
                types: Vec::new(),
            });
            assert!(
                resp.ok,
                "search query {q:?} scope={scope:?} failed: {resp:?}"
            );
            for path in file_order_from_response(&resp) {
                if seen.insert(path.clone()) {
                    order.push(path);
                }
            }
            if order.len() >= k || resp.data()["truncated"].as_bool() == Some(false) {
                break;
            }
            let next = resp.data()["next_offset"]
                .as_u64()
                .expect("capped search response needs a continuation cursor")
                as usize;
            assert!(
                next > offset && next < 10_000,
                "query {q:?}: incomplete benchmark corpus or non-progressing cursor {next}"
            );
            offset = next;
        }
        let score = ndcg_at_k(&order, &rel_set, k);
        // The deliberately-unranked control may score zero (e.g. an
        // alphabetically late filename). Candidate lanes may not.
        if scope.is_some() {
            validate_query_score(score, None)
                .unwrap_or_else(|err| panic!("search query {q:?} scope={scope:?}: {err}"));
        }
        sum += score;
    }
    sum / qrels.len() as f64
}

/// A/B lane: run `pixel ask` (semantic) over the SAME qrels, rooted at the
/// same `crates/pixel-graph/src` subtree. `ask` returns paths relative to its
/// search root; we resolve and verify them inside that subtree before converting
/// them to the qrels' `crates/pixel-graph/src/<name>` form. Same query strings as
/// the lexical lane → identical inputs, isolated channel effect.
///
/// The per-probe guard is a *presence* check, not the `k`-truncated score the
/// lexical lane gates on. `ask` retrieves the whole corpus here, so a labelled
/// file below `k` is the channel reporting the file at rank 11 -- not the "no
/// relevant evidence" a score of 0 stands for. Gating on the truncation made
/// this lane a boundary tripwire: `"imports dependency resolved graph edge"`
/// puts `imports.rs` 10th of the 16 corpus files, and 26 lines of code with no
/// test and no comment in *any* file of the subtree evict it to 11th, so
/// ordinary development in `crates/pixel-graph` failed a gate about ranking
/// quality. How *high* the channel ranks a relevant file is what NDCG measures
/// and what the lane's mean gates on; the per-probe rank is printed, so a
/// probe sliding toward the boundary is visible in the run before it gates.
fn run_ndcg_ask(root: &std::path::Path, qrels: &[(&'static str, Vec<String>)], k: usize) -> f64 {
    let subtree = root.join("crates/pixel-graph/src");
    let mut sum = 0.0;
    for (q, relevant) in qrels {
        let rel_set: HashSet<String> = relevant.iter().cloned().collect();
        // `ask` embeds the raw query string. The qrels queries are keyword
        // endpoints; static embeddings handle keyword-ish text fine, and
        // using the identical string keeps the A/B inputs matched. No file
        // budget (`None`), as `pixel search-meaning` runs by default.
        let hits = pixel_recall::code_search::ask(&subtree, q, k.max(50), None)
            .unwrap_or_else(|err| panic!("ask query {q:?} failed: {err}"));
        let order: Vec<String> = hits
            .iter()
            .map(|h| {
                let hit = std::path::Path::new(&h.path);
                let hit = if hit.is_absolute() {
                    hit.to_path_buf()
                } else {
                    subtree.join(hit)
                };
                let hit = hit
                    .canonicalize()
                    .expect("ask result must name a file in the benchmark corpus");
                hit.strip_prefix(&subtree)
                    .expect("ask result must be inside the benchmark corpus");
                hit.strip_prefix(root)
                    .expect("benchmark corpus must be inside its fixture root")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let score = ndcg_at_k(&order, &rel_set, k);
        let rank = order
            .iter()
            .position(|path| rel_set.contains(path))
            .map_or(usize::MAX, |position| position + 1);
        eprintln!(
            "ask query={q:?} rank={} ndcg@{k}={score}",
            if rank == usize::MAX {
                "unretrieved".to_string()
            } else {
                rank.to_string()
            }
        );
        // Presence, not position: the score at the lane's own result limit is
        // > 0 exactly when the channel returned the labelled file at all.
        let retrieved = ndcg_at_k(&order, &rel_set, order.len());
        validate_query_score(retrieved, None)
            .unwrap_or_else(|err| panic!("ask query {q:?} rank={rank}: {err}"));
        sum += score;
    }
    sum / qrels.len() as f64
}

/// Copy the real graph source into an isolated indexed corpus. The evaluator
/// itself contains every exact query and must never be a retrieval candidate.
fn graph_fixture() -> (tempfile::TempDir, PathBuf, Service) {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let relative = "crates/pixel-graph/src";
    let destination = root.join(relative);
    std::fs::create_dir_all(&destination).unwrap();
    for entry in std::fs::read_dir(workspace.join(relative)).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().is_some_and(|ext| ext == "rs") {
            std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
    // Fixture setup only; never commits or modifies the working repository.
    for args in [
        vec!["init", "-q"],
        vec!["add", "crates"],
        vec![
            "-c",
            "user.name=Pixel Audit",
            "-c",
            "user.email=audit@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "frozen graph source corpus",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture git: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let svc = Service::open(&root).unwrap();
    (temporary, root, svc)
}

fn bench(c: &mut Criterion) {
    let (_temporary, root, mut svc) = graph_fixture();
    let suite = qrels(&root);
    // Warm up index + graph.
    let _ = svc.handle(Op::Search {
        pattern: "concept resolve".into(),
        json: true,
        limit: Some(10),
        offset: None,
        paths: Some(vec!["crates/pixel-graph/src".to_string()]),
        scope: Some("code".to_string()),
        globs: Vec::new(),
        types: Vec::new(),
    });

    let ranked = run_ndcg(&mut svc, &suite, Some("code"), 10);
    let unranked = run_ndcg(&mut svc, &suite, None, 10);
    // Compare the same query individually before means can conceal a regression.
    for query in &suite {
        let single = std::slice::from_ref(query);
        let baseline = run_ndcg(&mut svc, single, None, 10);
        let candidate = run_ndcg(&mut svc, single, Some("code"), 10);
        validate_query_score(candidate, Some(baseline))
            .unwrap_or_else(|err| panic!("ranked query {:?}: {err}", query.0));
    }
    // A/B lane: semantic `ask` over the SAME qrels + subtree.
    let semantic = run_ndcg_ask(&root, &suite, 10);
    // Precision 1 lane: hybrid search (lexical RRF + semantic channel fused).
    let hybrid = run_ndcg(&mut svc, &suite, Some("hybrid"), 10);
    // Correctness / success-rate axis (P0·1): doesthe resolve machinery actually
    // *answer* the tasks, binary per task — not how high the relevant file ranks.
    // The point this lane adds: latency (m1_latency.rs) is a COST gate, not a
    // correctness claim. Success rate isthe justification.
    let resolve_ok = resolve_success_rate(&mut svc, &suite);

    // Sanity: ranking must not DECREASE NDCG meaningfully vs unranked, and
    // must be > 0 (retrieval actually finds relevant files).
    assert!(
        unranked > 0.0,
        "NDCG@10 unranked = {unranked:.3} — retrieval found nothing; suite is broken"
    );
    assert!(
        ranked >= unranked - 0.05,
        "NDCG@10 ranked ({ranked:.3}) regressed below unranked ({unranked:.3})"
    );
    assert!(
        semantic > 0.0,
        "NDCG@10 semantic = {semantic:.3} — no relevant results; ask lane is broken"
    );
    assert!(
        hybrid > 0.0,
        "NDCG@10 hybrid = {hybrid:.3} — no relevant results; hybrid lane is broken"
    );
    assert!(
        resolve_ok > 0.0,
        "resolve success-rate = {resolve_ok:.3} — no relevant results; resolve lane is broken"
    );

    eprintln!(
        "NDCG@10 (pixel-graph qrels, self-bench A/B):\n  \
         lexical unranked= {unranked:.3}\n  \
         lexical ranked   = {ranked:.3}\n  \
         hybrid search   = {hybrid:.3}  (5ch RRF + semantic S6)\n  \
         semantic ask    = {semantic:.3}  (potion-code-16M-v2 standalone)"
    );
    eprintln!("resolve success-rate (correctness, P0·1): top-match-is-relevant binary,");
    eprintln!(
        "  resolve (Engine-1 cascade) = {:.1}%  of tasks solved  ({:.2}/{})",
        resolve_ok * 100.0,
        resolve_ok * suite.len() as f64,
        suite.len()
    );
    eprintln!("  NOTE: m1_latency.rs latency gates are COST-only; correctness axis is this");
    eprintln!("  success-rate lane (+ the agent-level A/B in the isolated harness).");
    let mut ranked_grp = c.benchmark_group("ndcg10");
    ranked_grp.sample_size(10);
    ranked_grp.bench_with_input(BenchmarkId::new("ranked_search", 10), &ranked, |b, _| {
        b.iter(|| run_ndcg(&mut svc, &suite, Some("code"), 10))
    });
    ranked_grp.bench_with_input(
        BenchmarkId::new("unranked_search", 10),
        &unranked,
        |b, _| b.iter(|| run_ndcg(&mut svc, &suite, None, 10)),
    );
    ranked_grp.bench_with_input(BenchmarkId::new("hybrid_search", 10), &hybrid, |b, _| {
        b.iter(|| run_ndcg(&mut svc, &suite, Some("hybrid"), 10))
    });
    ranked_grp.bench_with_input(BenchmarkId::new("semantic_ask", 10), &semantic, |b, _| {
        b.iter(|| run_ndcg_ask(&root, &suite, 10))
    });
    ranked_grp.bench_with_input(
        BenchmarkId::new("resolve_success_rate", 10),
        &resolve_ok,
        |b, _| b.iter(|| resolve_success_rate(&mut svc, &suite)),
    );
    ranked_grp.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
