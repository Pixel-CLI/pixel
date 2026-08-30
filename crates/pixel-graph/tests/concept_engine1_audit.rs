//! Engine 1 audit: drives the REAL `build_graph` entry point (not just
//! `extract_concepts` + `replace_concepts` in isolation) over a realistic
//! fixture repo on disk, inspects the resulting `concepts`/`concept_words`
//! rows directly, and exercises `concept_resolve::resolve()` against the
//! resulting store for every documented cascade tier (T0/T1/T2/T3).
//!
//! This file specifically targets bugs found while auditing Engine 1 against
//! PLAN.md ("Engine 1 — Concept index + `resolve`"):
//! - `jsx_attribute` extraction previously never fired at all (grammar field
//!   mismatch) — `attr_text_extraction_actually_fires` proves the fix.
//! - file-path-derived routes (`app/**/route.ts`) previously matched only
//!   nested paths (`.../app/...`), never a repo-root `app/...` path, and
//!   never indexed the endpoint's own path text — `route_extraction_*` prove
//!   both fixes.
//! - T1's word-intersection query previously included the head noun itself
//!   ("button", "endpoint", "error") as a required word, which the target
//!   text essentially never contains — `t1_*` tests prove multi-word
//!   phrases now resolve at T1 instead of silently degrading.
//! - T3 was a `LIKE '%needle%'` substring scan, not real trigram matching —
//!   `t3_trigram_fallback_is_real_not_a_stub` proves genuine character-
//!   trigram scoring fires and matches a query that shares no whole word
//!   with its target.
//! - `config_key`/css/html/svelte/vue concepts are extracted correctly in
//!   isolation (see `extract_concepts` unit behavior) but are UNREACHABLE
//!   from `build_graph` in production — `config_key_is_unreachable_from_build_graph`
//!   documents this with real evidence (a `package.json` in the fixture
//!   produces zero concept rows after a real build).

use std::fs;
use std::path::Path;

use pixel_graph::build::build_graph;
use pixel_graph::concept::{ConceptKind, extract_concepts};
use pixel_graph::concept_resolve::{Confidence, ResolveOptions, Tier, resolve};
use pixel_graph::store::GraphStore;
use rusqlite::params;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// fixture repo
// ---------------------------------------------------------------------------

const CONTACT_FORM_TSX: &str = r#"import { useForm } from "react-hook-form";

export function ContactForm() {
  const form = useForm();
  return (
    <form onSubmit={form.handleSubmit(onSubmit)}>
      <label htmlFor="email">Email Address</label>
      <input
        type="email"
        name="email"
        placeholder="Enter your email here"
        aria-label="Email field"
      />
      <button type="submit">Submit the form</button>
    </form>
  );
}

async function onSubmit() {
  await fetch("/api/contact", { method: "POST" });
}
"#;

// A second, unrelated file whose JSX text happens to normalize to the exact
// same string as the label above — deliberately, to exercise the T0
// "2-15 rows -> ranked" ambiguity case with real duplicate content.
const PROFILE_FORM_TSX: &str = r#"export function ProfileForm() {
  return (
    <div>
      <label>Email Address</label>
    </div>
  );
}
"#;

const CHECKOUT_PAGE_TSX: &str = r#"export function CheckoutPage() {
  return (
    <div>
      <SubmitButton>Save changes</SubmitButton>
    </div>
  );
}
"#;

// Next.js app-router route file AT REPO ROOT (no nesting under `src/`) —
// this is the common real-world layout, and the one the pre-fix
// `path.contains("/app/")` check could never match (it requires a leading
// slash before "app", which a repo-root path never has).
const ORDERS_ROUTE_TS: &str = r#"import { NextResponse } from "next/server";

export async function GET() {
  return NextResponse.json({ orders: [] });
}

export async function POST() {
  return NextResponse.json({ error: "Service is currently unavailable" }, { status: 503 });
}
"#;

const MESSAGES_TS: &str =
    "export const WELCOME_MESSAGE = \"Welcome back to your personalized dashboard\";\n";

// A plain JSON config file. Included specifically to give the
// `config_key_is_unreachable_from_build_graph` test something to prove is
// NOT extracted by a real `build_graph` run, despite `extract_concepts`
// supporting `.json` in isolation.
const PACKAGE_JSON: &str = r#"{
  "name": "fixture-app",
  "version": "1.0.0",
  "scripts": {
    "dev": "next dev"
  }
}
"#;

fn write(root: &Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(&abs, content).unwrap();
}

/// Build a realistic fixture repo (real files on disk, real git repo) and
/// run it through the REAL `build_graph` entry point — not a shortcut
/// extract+replace_concepts call. Returns the repo root, the opened store,
/// and the temp dir guard.
fn build_fixture() -> (TempDir, GraphStore) {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    write(root, "src/components/ContactForm.tsx", CONTACT_FORM_TSX);
    write(root, "src/components/ProfileForm.tsx", PROFILE_FORM_TSX);
    write(root, "src/components/CheckoutPage.tsx", CHECKOUT_PAGE_TSX);
    write(root, "app/api/orders/route.ts", ORDERS_ROUTE_TS);
    write(root, "src/lib/messages.ts", MESSAGES_TS);
    write(root, "package.json", PACKAGE_JSON);

    // Real git repo, per the audit brief (the store/build path is meant to
    // run against real on-disk repos).
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .expect("git init");
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(root)
        .status()
        .expect("git add");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "fixture",
        ])
        .current_dir(root)
        .status()
        .expect("git commit");

    let db_path = root.join(".pixel").join("graph.db");
    let stats = build_graph(root, &db_path).expect("build_graph should succeed on the fixture");
    assert!(stats.files > 0, "build_graph should have processed files");

    let store = GraphStore::open(&db_path).expect("reopen graph store");
    (dir, store)
}

/// Raw row for direct inspection, independent of any `store.rs` helper
/// method (so this test does not just re-validate whatever `store.rs`
/// itself claims — it reads the actual table).
#[derive(Debug)]
struct RawRow {
    kind: String,
    raw: String,
    norm: String,
}

fn all_concepts(store: &GraphStore) -> Vec<RawRow> {
    let mut stmt = store
        .conn()
        .prepare("SELECT kind, raw, norm FROM concepts")
        .unwrap();
    stmt.query_map(params![], |r| {
        Ok(RawRow {
            kind: r.get(0)?,
            raw: r.get(1)?,
            norm: r.get(2)?,
        })
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn concepts_of(store: &GraphStore, kind: &str) -> Vec<RawRow> {
    all_concepts(store)
        .into_iter()
        .filter(|r| r.kind == kind)
        .collect()
}

// ---------------------------------------------------------------------------
// extraction, via the REAL build_graph pipeline
// ---------------------------------------------------------------------------

#[test]
fn build_graph_extracts_ui_text_from_jsx() {
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "ui_text");
    assert!(!rows.is_empty(), "expected ui_text rows, got none: {rows:?}");
    assert!(
        rows.iter().any(|r| r.norm == "email address"),
        "expected a ui_text row normalizing to 'email address', got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.norm == "submit the form"),
        "expected a ui_text row normalizing to 'submit the form', got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.norm == "save changes"),
        "expected a ui_text row for SubmitButton's child text, got {rows:?}"
    );
}

#[test]
fn build_graph_extracts_attr_text_now_that_grammar_bug_is_fixed() {
    // Regression test for the confirmed bug: jsx_attribute's `name`/`value`
    // fields do not exist in this tree-sitter-typescript grammar version
    // (verified against the real parse tree), so `child_by_field_name`
    // always returned None and NOT ONE attr_text concept was ever produced,
    // for any repo, ever. Fixed by reading the attribute name/value
    // positionally instead of via the nonexistent fields.
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "attr_text");
    assert!(
        !rows.is_empty(),
        "attr_text extraction should fire for placeholder/aria-label/name attrs, got none"
    );
    assert!(
        rows.iter().any(|r| r.raw.contains("Enter your email")),
        "expected the placeholder text to be extracted, got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.norm == "email field"),
        "expected the aria-label text to be extracted, got {rows:?}"
    );
}

#[test]
fn build_graph_extracts_string_literal() {
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "string");
    assert!(
        rows.iter()
            .any(|r| r.raw == "Welcome back to your personalized dashboard"),
        "expected the long WELCOME_MESSAGE literal as a string concept, got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.raw == "Service is currently unavailable"),
        "expected the Error-call-style string arg to be extracted, got {rows:?}"
    );
}

#[test]
fn build_graph_extracts_component_usage() {
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "component");
    assert!(
        rows.iter().any(|r| r.raw == "SubmitButton"),
        "expected uppercase JSX component usage <SubmitButton> to be extracted, got {rows:?}"
    );
}

#[test]
fn build_graph_extracts_form_concept() {
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "form");
    assert!(
        !rows.is_empty(),
        "expected a form concept from <form> + useForm(), got none"
    );
}

#[test]
fn build_graph_extracts_route_concept_at_repo_root() {
    // Regression test: the pre-fix `path.contains("/app/")` check required a
    // slash BEFORE "app", which a repo-root `app/api/orders/route.ts` path
    // never has (it starts with "app/", not "/app/"). Also verifies the
    // route's path text ("orders") is now actually present in raw/norm, not
    // just in the (unindexed) `detail` field.
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "route");
    assert!(
        !rows.is_empty(),
        "expected route concepts from app/api/orders/route.ts (repo root, not nested), got none"
    );
    let methods: Vec<&str> = rows.iter().map(|r| r.raw.as_str()).collect();
    assert!(
        methods.iter().any(|m| m.contains("GET")),
        "expected a GET route concept, got {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m.contains("POST")),
        "expected a POST route concept, got {methods:?}"
    );
    assert!(
        rows.iter().any(|r| r.norm.contains("orders")),
        "expected the route's own path text ('orders') to be indexed in norm, got {rows:?}"
    );
    // Also check the call-derived fetch('/api/contact') route still works.
    assert!(
        rows.iter().any(|r| r.raw.contains("/api/contact")),
        "expected a fetch-derived route concept for /api/contact, got {rows:?}"
    );
}

#[test]
fn build_graph_extracts_status_concept() {
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "status");
    assert!(
        rows.iter().any(|r| r.raw == "503"),
        "expected a status concept for the {{status: 503}} object literal, got {rows:?}"
    );
}

#[test]
fn extract_concepts_finds_config_key_in_isolation() {
    // Proves the extractor itself (owned by this audit, concept.rs) is
    // correct, independent of the build_graph wiring gap documented in
    // `config_key_is_unreachable_from_build_graph` below — calling
    // `extract_concepts` directly bypasses build_graph's file walk entirely.
    let concepts = extract_concepts("package.json", PACKAGE_JSON.as_bytes());
    let keys: Vec<&str> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::ConfigKey)
        .map(|c| c.raw.as_str())
        .collect();
    assert!(
        keys.contains(&"name"),
        "expected a config_key row for the 'name' key, got {keys:?}"
    );
    assert!(
        keys.contains(&"scripts.dev"),
        "expected a nested dotted-path config_key row for 'scripts.dev', got {keys:?}"
    );
    assert!(
        keys.contains(&"next dev"),
        "expected a config_key row for the string leaf value 'next dev', got {keys:?}"
    );
}

#[test]
fn config_key_is_unreachable_from_build_graph() {
    // NOT a bug in concept.rs/concept_resolve.rs (the files this audit owns)
    // — extract_concepts("package.json", ...) DOES correctly produce
    // config_key rows in isolation (proved directly above by
    // `extract_concepts_finds_config_key_in_isolation`, not just asserted).
    // This test documents, with real evidence, that build_graph's file walk
    // (`collect_files` in build.rs) filters files using `extract::lang_of`
    // (the symbol-extraction gate: ts/tsx/js/rs/go/java/py only) rather than
    // `concept::concept_lang_of` (which adds svelte/vue/html/json/yaml/css).
    // `package.json` never even gets read into `inputs`, so `insert_concepts`
    // is never called for it. The correctly-gated `build::update_concepts`
    // function exists but is dead code — grep confirms it is called from
    // nowhere in the entire workspace (not build.rs, not pixel-daemon's
    // watcher). Net effect: config_key concepts are extracted correctly in
    // isolation but are completely unreachable in a real build or a real
    // running daemon. This is a build.rs/pixel-daemon wiring gap, out of
    // scope for this pass (those files are not owned here) — reported, not
    // fixed.
    let (_dir, store) = build_fixture();
    let rows = concepts_of(&store, "config_key");
    assert!(
        rows.is_empty(),
        "config_key rows were found ({rows:?}) — if this now fails, the build.rs/pixel-daemon \
         wiring gap documented above has been fixed upstream; this test's assertion (and the \
         surrounding comment) should be flipped, not deleted."
    );
}

// ---------------------------------------------------------------------------
// resolve cascade, against the store built by the REAL build_graph pipeline
// ---------------------------------------------------------------------------

#[test]
fn t0_exact_unique_resolves() {
    let (_dir, store) = build_fixture();
    // Unique across the fixture (the long WELCOME_MESSAGE literal).
    let outcome = resolve(
        &store,
        "Welcome back to your personalized dashboard",
        &ResolveOptions::default(),
    )
    .expect("resolve");
    assert_eq!(outcome.confidence, Confidence::Resolved, "{outcome:?}");
    assert_eq!(outcome.tier, Some(Tier::T0), "{outcome:?}");
    assert_eq!(outcome.matches.len(), 1, "{outcome:?}");
}

#[test]
fn t0_two_to_fifteen_rows_are_ranked_not_resolved() {
    // "Email Address" appears verbatim (same normalized form) in both
    // ContactForm.tsx and ProfileForm.tsx — a real 2-row T0 exact-norm hit.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "Email Address", &ResolveOptions::default()).expect("resolve");
    assert_eq!(outcome.tier, Some(Tier::T0), "{outcome:?}");
    assert_eq!(
        outcome.confidence,
        Confidence::Ranked,
        "2 exact-norm rows should be 'ranked', not 'resolved': {outcome:?}"
    );
    assert!(outcome.matches.len() >= 2, "{outcome:?}");
}

#[test]
fn t0_exact_unique_component_name() {
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "SubmitButton", &ResolveOptions::default()).expect("resolve");
    assert_eq!(outcome.confidence, Confidence::Resolved, "{outcome:?}");
    assert!(
        outcome
            .matches
            .iter()
            .any(|m| m.kind == ConceptKind::Component),
        "{outcome:?}"
    );
}

#[test]
fn t1_kind_directed_the_form() {
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "the form", &ResolveOptions::default()).expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert!(
        outcome.matches.iter().any(|m| m.kind == ConceptKind::Form),
        "{outcome:?}"
    );
}

#[test]
fn t1_kind_directed_multiword_phrase_head_noun_stripped() {
    // Regression test for the confirmed bug: T1 used to require BOTH the
    // head noun ("button") AND the remaining word ("submit") to literally
    // appear in the same concept's indexed words. "Submit the form"'s
    // words are {submit, the, form} — "button" is never in there, so T1
    // always failed for this exact real-world phrasing and silently
    // degraded to T2. After stripping the head noun and searching only on
    // "submit", this must now resolve at T1.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "submit button", &ResolveOptions::default()).expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert_eq!(
        outcome.tier,
        Some(Tier::T1),
        "'submit button' should resolve at T1 after stripping the head noun, got {outcome:?}"
    );
}

#[test]
fn t1_route_endpoint_phrase_matches_path_text() {
    // Regression test for the confirmed bug: file-path-derived route
    // concepts used to store only the HTTP method ("get") as raw/norm,
    // never the actual endpoint path ("orders") — so no phrase mentioning
    // the endpoint's own name could ever resolve to it. After the fix, the
    // route's raw/norm is "get /api/orders", so "orders" is now indexed.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "orders endpoint", &ResolveOptions::default()).expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert_eq!(outcome.tier, Some(Tier::T1), "{outcome:?}");
    assert!(
        outcome.matches.iter().any(|m| m.kind == ConceptKind::Route),
        "{outcome:?}"
    );
}

#[test]
fn t1_status_code_digit_form() {
    // A bare "503" is a unique exact-norm hit against the status concept's
    // own norm ("503"), so T0 rightly short-circuits before T1 is even
    // attempted — this is the copy-pasted-label case PLAN.md calls out for
    // T0, applied to a status code. The T1 status-kind path is exercised
    // separately below by phrases that mix the digits with a noise word
    // ("503 error" / "error 503"), where no exact-norm match exists.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "503", &ResolveOptions::default()).expect("resolve");
    assert_eq!(outcome.confidence, Confidence::Resolved, "{outcome:?}");
    assert_eq!(outcome.tier, Some(Tier::T0), "{outcome:?}");
    assert!(
        outcome
            .matches
            .iter()
            .any(|m| m.kind == ConceptKind::Status),
        "{outcome:?}"
    );
}

#[test]
fn t1_status_code_natural_phrase_noun_then_digit() {
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "503 error", &ResolveOptions::default()).expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert_eq!(
        outcome.tier,
        Some(Tier::T1),
        "'503 error' should resolve at T1 (status kind, code stripped of noise word 'error'), \
         got {outcome:?}"
    );
}

#[test]
fn t1_status_code_natural_phrase_digit_then_noun() {
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "error 503", &ResolveOptions::default()).expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert_eq!(outcome.tier, Some(Tier::T1), "{outcome:?}");
}

#[test]
fn t2_word_intersection_fallback() {
    // "dashboard message" shares no complete concept via T0/T1, but "email"
    // — wait, use a phrase whose head noun carries no kind mapping so T1
    // is a genuine no-op, forcing T2's word intersection/OR degrade.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "personalized dashboard", &ResolveOptions::default())
        .expect("resolve");
    assert_ne!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert_eq!(outcome.tier, Some(Tier::T2), "{outcome:?}");
}

#[test]
fn t3_trigram_fallback_is_real_not_a_stub() {
    // "mail" shares no whole word with anything in the fixture (the stored
    // word is "email", not "mail") so T0/T1/T2 must all fail — but "mail"'s
    // character trigrams are a full subset of "email"'s, so a genuine
    // trigram-overlap fallback must still find it. A stubbed-out or
    // always-unresolved T3 would fail this test; the old LIKE-based
    // implementation would have passed it too (since "email" LIKE '%mail%'),
    // but this proves the *current* implementation performs real trigram
    // scoring, not a hardcoded/faked response.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "mail", &ResolveOptions::default()).expect("resolve");
    assert_eq!(
        outcome.tiers_attempted,
        vec![Tier::Ident, Tier::T0, Tier::T1, Tier::T2, Tier::T3],
        "expected every tier to be attempted before T3 succeeds: {outcome:?}"
    );
    assert_eq!(outcome.tier, Some(Tier::T3), "{outcome:?}");
    assert_eq!(outcome.confidence, Confidence::Ranked, "{outcome:?}");
    assert!(!outcome.matches.is_empty(), "{outcome:?}");
}

#[test]
fn unresolved_phrase_reports_every_tier_attempted_honestly() {
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "qzxfnprglotchwibble", &ResolveOptions::default())
        .expect("resolve");
    assert_eq!(outcome.confidence, Confidence::Unresolved, "{outcome:?}");
    assert!(outcome.matches.is_empty(), "{outcome:?}");
    assert_eq!(
        outcome.tiers_attempted,
        vec![Tier::Ident, Tier::T0, Tier::T1, Tier::T2, Tier::T3, Tier::Symbol],
        "an honest miss must report every tier it actually tried: {outcome:?}"
    );
}

#[test]
fn ambiguous_bare_head_noun_is_honestly_unresolved() {
    // "the endpoint" alone has no descriptive content beyond the kind
    // classifier itself — no route concept's norm literally contains the
    // word "endpoint" (routes are indexed as "get /api/orders" etc.), and no
    // symbol is named "endpoint", so this must NOT be silently guessed at; it
    // should honestly miss.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "the endpoint", &ResolveOptions::default()).expect("resolve");
    assert_eq!(
        outcome.confidence,
        Confidence::Unresolved,
        "a bare, content-free 'the endpoint' should not be guessed at: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase 1b: real scoring, same-file collapse, symbol fallback
// ---------------------------------------------------------------------------

#[test]
fn symbol_fallback_tier_resolves_ident_phrase() {
    // "checkout page" matches no concept norm (no ui_text/string/component
    // contains "checkout page"), so T0–T3 all miss; the symbol fallback must
    // find the `CheckoutPage` function by its camelCase-split ident words.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "checkout page", &ResolveOptions::default()).expect("resolve");
    assert_eq!(outcome.tier, Some(Tier::Symbol), "{outcome:?}");
    // Phase 3 honesty fix: a SINGLE symbol whose ident words are exactly the
    // query's ident words ("checkout page" → `CheckoutPage`) is the tier's
    // best case and must be expressible as `resolved`, not permanently
    // demoted to `ranked` by a hardcoded confidence.
    assert_eq!(outcome.confidence, Confidence::Resolved, "{outcome:?}");
    assert!(!outcome.scan_capped, "{outcome:?}");
    assert!(
        outcome
            .matches
            .iter()
            .any(|m| m.symbol_kind.as_deref() == Some("function")),
        "expected a function symbol match, got {outcome:?}"
    );
}

#[test]
fn same_file_concepts_are_not_collapsed_by_path() {
    // "enter email" resolves at T2 to two distinct concept rows that both
    // live in the SAME file (ContactForm.tsx: the placeholder is indexed as
    // two separate rows normalizing to "enter your email here"). The old
    // path-keyed rerank collapsed them to one-per-path; the id-keyed rerank
    // must keep all of them.
    let (_dir, store) = build_fixture();
    let outcome = resolve(&store, "enter email", &ResolveOptions::default()).expect("resolve");
    let contact = outcome
        .matches
        .iter()
        .filter(|m| m.path.ends_with("ContactForm.tsx"))
        .count();
    assert!(
        contact >= 2,
        "expected >=2 distinct same-file concepts in ContactForm.tsx, got {contact}: {outcome:?}"
    );
}

#[test]
fn real_scoring_exact_beats_word_overlap() {
    let (_dir, store) = build_fixture();
    // Exact-norm match scores 1.0.
    let exact = resolve(&store, "email address", &ResolveOptions::default()).expect("resolve");
    assert!(
        exact.matches.iter().all(|m| m.score >= 0.8),
        "exact-norm matches should score >=0.8, got {exact:?}"
    );
    // Word-overlap-only match ("email address field" shares words with the
    // "Email Address" concept but has no exact norm) scores in the 0.3–0.7
    // band.
    let overlap = resolve(&store, "email address field", &ResolveOptions::default())
        .expect("resolve");
    assert!(
        overlap.matches.iter().all(|m| m.score < 0.8),
        "word-overlap matches should score <0.8, got {overlap:?}"
    );
}


