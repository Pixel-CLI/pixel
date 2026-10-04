// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for pixel-graph concept extraction + resolve cascade.

use std::fs;

use pixel_graph::concept::{ConceptKind, extract_concepts};
use pixel_graph::concept_resolve::{Confidence, ResolveOptions, Tier, resolve};
use pixel_graph::extract::extract_file;
use pixel_graph::store::{GraphStore, SymbolKind};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A TSX file with JSX text, a form, a route, and a string literal.
const TSX_CONTENT: &str = r#"
import { useForm } from "react-hook-form";

export default function ContactPage() {
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

/// Build a GraphStore in a temp dir, insert one file, extract + store concepts.
fn make_store_with_concepts() -> (TempDir, GraphStore, i64) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("graph.db");
    let mut store = GraphStore::open(&db_path).expect("open graph store");

    // Write the TSX file to disk so we can reference it.
    let file_rel = "src/ContactPage.tsx";
    let abs = dir.path().join(file_rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(&abs, TSX_CONTENT).unwrap();

    // Insert the file row.
    let file_id = store
        .replace_file(file_rel, "fake-oid", "tsx")
        .expect("replace_file");

    // Extract concepts from the file content.
    let concepts = extract_concepts(file_rel, TSX_CONTENT.as_bytes());
    assert!(!concepts.is_empty(), "extraction should produce concepts");

    // Store them.
    store
        .replace_concepts(file_id, &concepts)
        .expect("replace_concepts");

    (dir, store, file_id)
}

// ---------------------------------------------------------------------------
// extraction tests
// ---------------------------------------------------------------------------

#[test]
fn extract_concepts_finds_ui_text() {
    let concepts = extract_concepts("src/ContactPage.tsx", TSX_CONTENT.as_bytes());
    let ui_texts: Vec<_> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::UiText)
        .collect();
    assert!(
        !ui_texts.is_empty(),
        "should extract ui_text concepts from JSX text"
    );
    // "Email Address" and "Submit the form" are JSX text nodes.
    let all_raw: Vec<String> = concepts.iter().map(|c| c.raw.clone()).collect();
    let joined = all_raw.join(" ");
    assert!(
        joined.contains("Email Address") || joined.contains("email address"),
        "should find 'Email Address' JSX text"
    );
}

#[test]
fn extract_concepts_finds_form_concept() {
    let concepts = extract_concepts("src/ContactPage.tsx", TSX_CONTENT.as_bytes());
    let forms: Vec<_> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Form)
        .collect();
    assert!(
        !forms.is_empty(),
        "should extract at least one form concept (<form> + useForm)"
    );
}

#[test]
fn extract_concepts_finds_route_concept() {
    let concepts = extract_concepts("src/ContactPage.tsx", TSX_CONTENT.as_bytes());
    let routes: Vec<_> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Route)
        .collect();
    assert!(
        !routes.is_empty(),
        "should extract a route concept from fetch('/api/contact')"
    );
    // The route raw should mention /api/contact.
    let found = routes.iter().any(|r| r.raw.contains("/api/contact"));
    assert!(found, "route concept should contain /api/contact");
}

#[test]
fn extract_concepts_finds_attr_text() {
    let concepts = extract_concepts("src/ContactPage.tsx", TSX_CONTENT.as_bytes());
    let attrs: Vec<_> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::AttrText)
        .collect();
    assert!(
        !attrs.is_empty(),
        "should extract attr_text from placeholder/aria-label"
    );
    // "Enter your email here" is a placeholder.
    let found = attrs.iter().any(|a| a.raw.contains("Enter your email"));
    assert!(found, "should find placeholder attr_text");
}

#[test]
fn extract_concepts_skips_unsupported_extensions() {
    let concepts = extract_concepts("readme.md", b"# Hello\nWorld");
    assert!(
        concepts.is_empty(),
        "unsupported extensions produce no concepts"
    );
}

#[test]
fn extract_concepts_skips_oversized_files() {
    let huge = vec![b'a'; 2 * 1024 * 1024]; // > 1MB
    let concepts = extract_concepts("src/big.tsx", &huge);
    assert!(
        concepts.is_empty(),
        "files > 1MB should produce no concepts"
    );
}

#[test]
fn string_concepts_skipped_in_test_files() {
    // A long string literal that WOULD be indexed in a normal file.
    let content = r#"const MSG = "hello world this is a test";"#;
    // Same content, test-path vs non-test-path.
    let normal = extract_concepts("src/lib/messages.ts", content.as_bytes());
    let strings_normal: Vec<_> = normal
        .iter()
        .filter(|c| c.kind == ConceptKind::String)
        .collect();
    assert!(
        !strings_normal.is_empty(),
        "a long literal in a normal file should be indexed as a String concept"
    );
    for test_path in [
        "src/__tests__/messages.test.ts",
        "src/messages.spec.ts",
        "src/messages.test.ts",
        "src/messages_test.ts",
        "tests/messages.ts",
        "test/messages.ts",
    ] {
        let concepts = extract_concepts(test_path, content.as_bytes());
        let strings: Vec<_> = concepts
            .iter()
            .filter(|c| c.kind == ConceptKind::String)
            .collect();
        assert!(
            strings.is_empty(),
            "String concepts should be skipped in test file {test_path}, got {strings:?}"
        );
    }
}

#[test]
fn string_worth_indexing_requires_both_words_and_chars() {
    // ≥3 words AND ≥12 chars — a literal must satisfy BOTH thresholds.
    let content = r#"
const BOTH = "hello world this is a test";
const FEW_WORDS = "abcdefghijkl";
const SHORT = "a b c";
"#;
    let concepts = extract_concepts("src/lib/strings.ts", content.as_bytes());
    let strings: Vec<&str> = concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::String)
        .map(|c| c.raw.as_str())
        .collect();
    assert!(
        strings.contains(&"hello world this is a test"),
        "a literal with ≥3 words AND ≥12 chars should be indexed, got {strings:?}"
    );
    assert!(
        !strings.contains(&"abcdefghijkl"),
        "a literal with ≥12 chars but <3 words should NOT be indexed, got {strings:?}"
    );
    assert!(
        !strings.contains(&"a b c"),
        "a literal with ≥3 words but <12 chars should NOT be indexed, got {strings:?}"
    );
}

// ---------------------------------------------------------------------------
// resolve cascade tests
// ---------------------------------------------------------------------------

#[test]
fn resolve_exact_match_t0() {
    let (_dir, store, _file_id) = make_store_with_concepts();

    // "submit the form" is a JSX text node — its normalized form should be
    // an exact T0 match.
    let outcome = resolve(&store, "submit the form", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.confidence != Confidence::Unresolved,
        "should resolve 'submit the form'"
    );
    assert!(outcome.tier.is_some(), "should have a tier");
    assert!(
        !outcome.matches.is_empty(),
        "should have at least one match"
    );
}

#[test]
fn resolve_the_form_finds_form_concept() {
    let (_dir, store, _file_id) = make_store_with_concepts();

    // "the form" — article stripped, head noun "form" maps to ConceptKind::Form.
    let outcome = resolve(&store, "the form", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.confidence != Confidence::Unresolved,
        "'the form' should resolve, got {:?}",
        outcome.confidence
    );
    // T1 kind-directed should fire (head noun "form" → ConceptKind::Form).
    if outcome.tier == Some(Tier::T1) {
        // At least one match should be a form concept.
        let has_form = outcome.matches.iter().any(|m| m.kind == ConceptKind::Form);
        assert!(has_form, "T1 'the form' should match a form concept");
    }
    // Even if it fell to T0 or T2, we should have matches.
    assert!(
        !outcome.matches.is_empty(),
        "'the form' should produce matches"
    );
}

#[test]
fn resolve_word_intersection_t2() {
    let (_dir, store, _file_id) = make_store_with_concepts();

    // "email field" — "email" and "field" as words. The head noun "field"
    // maps to UiText+AttrText (T1). If T1 finds nothing, T2 word intersection
    // should find concepts containing "email".
    let outcome = resolve(&store, "email field", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.confidence != Confidence::Unresolved,
        "'email field' should resolve via word intersection"
    );
    assert!(
        !outcome.matches.is_empty(),
        "should have matches for 'email field'"
    );
}

#[test]
fn resolve_unresolved_for_nonexistent_phrase() {
    let (_dir, store, _file_id) = make_store_with_concepts();

    let outcome = resolve(
        &store,
        "nonexistent xyzzy phrase",
        &ResolveOptions::default(),
    )
    .expect("resolve");

    assert_eq!(
        outcome.confidence,
        Confidence::Unresolved,
        "nonexistent phrase should be unresolved"
    );
    assert!(outcome.matches.is_empty(), "no matches for unresolved");
    assert!(
        !outcome.tiers_attempted.is_empty(),
        "should record tiers attempted"
    );
}

#[test]
fn resolve_carries_index_state() {
    let (_dir, store, _file_id) = make_store_with_concepts();

    let outcome = resolve(&store, "form", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.index_state.concepts > 0,
        "should report concept count"
    );
    assert!(
        outcome.index_state.concepts_version.is_some(),
        "should report concepts_version"
    );
    assert!(outcome.index_state.fresh, "should be fresh with concepts");
    assert!(
        outcome.inputs_digest != 0,
        "should carry a non-zero inputs_digest"
    );
}

// ---------------------------------------------------------------------------
// inputs_digest: the "index changed" signal must survive a reindex that
// leaves the concept count alone
// ---------------------------------------------------------------------------

/// `inputs_digest` is what a caller caches on to know the index moved, so a
/// reindex that swaps the scanned content without changing the concept count
/// must move it. `replace_file` deletes and reinserts a file's concepts, and
/// the reinsert takes the rowid the delete just freed: neither the count nor
/// the highest id changes, which is exactly what made the old digest blind.
#[test]
fn inputs_digest_moves_when_a_reindex_swaps_concepts_without_changing_the_count() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = GraphStore::open(&dir.path().join("graph.db")).expect("open graph store");
    let a = store.replace_file("src/a.rs", "oid-a", "rs").unwrap();
    let b = store.replace_file("src/b.rs", "oid-b", "rs").unwrap();
    for (file, norm) in [(a, "submit form"), (b, "cancel order")] {
        store
            .insert_concept(file, ConceptKind::UiText, norm, norm, "", 1, 1, None)
            .unwrap();
    }

    let before = resolve(&store, "submit form", &ResolveOptions::default()).expect("resolve");
    let repeat = resolve(&store, "submit form", &ResolveOptions::default()).expect("resolve");
    assert_eq!(
        repeat.inputs_digest, before.inputs_digest,
        "an untouched index must keep the digest stable"
    );

    store
        .replace_file("src/b.rs", "oid-b2", "rs")
        .expect("replace_file");
    store
        .insert_concept(
            b,
            ConceptKind::UiText,
            "cancel invoice",
            "cancel invoice",
            "",
            1,
            1,
            None,
        )
        .unwrap();
    let after = resolve(&store, "submit form", &ResolveOptions::default()).expect("resolve");

    let rowids: Vec<(i64, String)> = store
        .conn()
        .prepare("SELECT id, norm FROM concepts ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rowids,
        vec![(1, "submit form".into()), (2, "cancel invoice".into())],
        "the fixture must reuse the rowid so count and highest id stay put"
    );
    assert_eq!(
        after.index_state.concepts, before.index_state.concepts,
        "the reindex kept the concept count — the old digest could not tell"
    );
    assert_ne!(
        after.inputs_digest, before.inputs_digest,
        "a digest a caller caches on must move with the scanned content"
    );
}

/// The symbol fallback scan is the digest's other bounded window: swapping
/// the indexed symbols without changing their count must move the digest too,
/// and the answer that a stale cache would have served must really differ.
#[test]
fn inputs_digest_moves_when_a_reindex_swaps_symbols_without_changing_the_count() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = GraphStore::open(&dir.path().join("graph.db")).expect("open graph store");
    let a = store.replace_file("src/a.rs", "oid-a", "rs").unwrap();
    let b = store.replace_file("src/b.rs", "oid-b", "rs").unwrap();
    for (file, uid, name) in [
        (a, "src/a.rs#alpha", "alpha"),
        (b, "src/b.rs#checkoutPage", "checkoutPage"),
    ] {
        store
            .insert_symbol(file, uid, name, name, SymbolKind::Function, 1, 1, "()")
            .unwrap();
    }

    let before = resolve(&store, "checkout page", &ResolveOptions::default()).expect("resolve");
    assert_eq!(before.tier, Some(Tier::Symbol), "{before:?}");

    store
        .replace_file("src/b.rs", "oid-b2", "rs")
        .expect("replace_file");
    store
        .insert_symbol(
            b,
            "src/b.rs#checkoutForm",
            "checkoutForm",
            "checkoutForm",
            SymbolKind::Function,
            1,
            1,
            "()",
        )
        .unwrap();
    let symbol_count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap();
    assert_eq!(symbol_count, 2, "the reindex kept the symbol count");
    let after = resolve(&store, "checkout page", &ResolveOptions::default()).expect("resolve");

    assert_eq!(after.tier, Some(Tier::Symbol), "{after:?}");
    assert_ne!(
        after.matches[0].raw, before.matches[0].raw,
        "the answer itself changed; a cache must not serve the old one"
    );
    assert_ne!(
        after.inputs_digest, before.inputs_digest,
        "the symbol fallback window is part of the digest"
    );
}

// ---------------------------------------------------------------------------
// identifier-shaped query → symbol preference (the GUARD_MATCHER regression)
// ---------------------------------------------------------------------------

/// Build a store that mirrors the real-world bug: a string concept in a
/// test-like file contains the identifier text (e.g. a command string
/// `"pixel search 'GUARD_MATCHER' ..."`), while the actual definition lives
/// in a `pub const GUARD_MATCHER` symbol in a different file. Without the
/// identifier-tier fix, `resolve("GUARD_MATCHER")` returns the string concept
/// (the test fixture) and never reaches the symbol.
fn make_store_with_symbol_and_string_concept() -> (TempDir, GraphStore) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("graph.db");
    let mut store = GraphStore::open(&db_path).expect("open graph store");

    // File 1: the definition file (config.rs equivalent).
    let file1 = "crates/pixel-install/src/config.rs";
    let abs1 = dir.path().join(file1);
    fs::create_dir_all(abs1.parent().unwrap()).unwrap();
    fs::write(
        &abs1,
        r#"pub const GUARD_MATCHER: &str = "Bash|Read|Grep";
"#,
    )
    .unwrap();
    let fid1 = store
        .replace_file(file1, "oid1", "rs")
        .expect("replace_file");
    // Insert the const symbol — this is the real definition.
    store
        .insert_symbol(
            fid1,
            "crates/pixel-install/src/config.rs#GUARD_MATCHER#const",
            "GUARD_MATCHER",
            "GUARD_MATCHER",
            SymbolKind::Const,
            1,
            1,
            "pub const GUARD_MATCHER: &str",
        )
        .expect("insert_symbol");

    // File 2: a guard.rs equivalent with string concepts that mention
    // GUARD_MATCHER in command strings (test fixtures / rewrite examples).
    let file2 = "crates/pixel/src/guard.rs";
    let abs2 = dir.path().join(file2);
    fs::create_dir_all(abs2.parent().unwrap()).unwrap();
    fs::write(
        &abs2,
        r#"fn rewrite() {
    let cmd = "pixel search 'GUARD_MATCHER' /repo --context 5";
    let cmd2 = "rg -l GUARD_MATCHER";
}
"#,
    )
    .unwrap();
    let fid2 = store
        .replace_file(file2, "oid2", "rs")
        .expect("replace_file");
    // Extract concepts from file2 — the string literals become String concepts.
    let concepts = extract_concepts(file2, fs::read(&abs2).unwrap().as_slice());
    store
        .replace_concepts(fid2, &concepts)
        .expect("replace_concepts");

    (dir, store)
}

#[test]
fn resolve_identifier_prefers_symbol_over_string_concept() {
    let (_dir, store) = make_store_with_symbol_and_string_concept();

    // "GUARD_MATCHER" is an identifier-shaped query (no spaces, UPPER_SNAKE).
    // The bug: T2 word-intersection matches the string concepts in guard.rs
    // (which literally contain "guard" and "matcher" as words), so the symbol
    // fallback never runs, and the const definition in config.rs is missed.
    let outcome = resolve(&store, "GUARD_MATCHER", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.confidence != Confidence::Unresolved,
        "should resolve 'GUARD_MATCHER'"
    );
    assert!(
        !outcome.matches.is_empty(),
        "should have matches for 'GUARD_MATCHER'"
    );
    // The top match should be the const definition in config.rs, not a string
    // concept in guard.rs.
    let top = &outcome.matches[0];
    assert!(
        top.path.ends_with("config.rs"),
        "top match should be the definition file (config.rs), got {}",
        top.path
    );
    assert!(
        top.symbol_kind.as_deref() == Some("const"),
        "top match should be a const symbol, got symbol_kind={:?}",
        top.symbol_kind
    );
}

/// The real extractor emits `SymbolKind::Variant` rows, so an exact
/// identifier-shaped query for an enum variant resolves through the ident
/// tier to the definition line instead of the string concepts that mention
/// the identifier (the `SelfUpdate` regression).
#[test]
fn resolve_identifier_finds_enum_variant_definitions() {
    const RUST_ENUM: &str = r#"
enum Command {
    SelfUpdate { force: bool },
    RepoState,
    DryRun,
    ListErrors(usize),
}
"#;
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("graph.db");
    let mut store = GraphStore::open(&db_path).expect("open graph store");

    let definition = "crates/pixel/src/main.rs";
    let abs = dir.path().join(definition);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(&abs, RUST_ENUM).unwrap();
    let file_id = store.replace_file(definition, "oid-def", "rust").unwrap();
    let extraction = extract_file(definition, RUST_ENUM.as_bytes()).expect("extract");
    for s in &extraction.symbols {
        let uid = format!("{}#{}#{}", definition, s.qualified, s.kind.as_str());
        store
            .insert_symbol(
                file_id,
                &uid,
                &s.name,
                &s.qualified,
                s.kind,
                s.start_line,
                s.end_line,
                &s.sig,
            )
            .unwrap();
    }

    // The noise that used to win: a string concept in another file that
    // literally contains two of the variant identifiers.
    let noisy = "crates/pixel/src/cli.rs";
    let noisy_abs = dir.path().join(noisy);
    fs::create_dir_all(noisy_abs.parent().unwrap()).unwrap();
    let noisy_src = "fn f() { let _ = \"run SelfUpdate and RepoState\"; }\n";
    fs::write(&noisy_abs, noisy_src).unwrap();
    let noisy_id = store.replace_file(noisy, "oid-noise", "rust").unwrap();
    let concepts = extract_concepts(noisy, noisy_src.as_bytes());
    store.replace_concepts(noisy_id, &concepts).unwrap();

    for variant in ["SelfUpdate", "RepoState", "DryRun", "ListErrors"] {
        let out = resolve(&store, variant, &ResolveOptions::default()).expect("resolve");
        assert_eq!(out.tier, Some(Tier::Ident), "{variant}");
        assert_eq!(out.confidence, Confidence::Resolved, "{variant}");
        let top = &out.matches[0];
        assert_eq!(top.path, definition, "{variant}");
        assert_eq!(top.raw, variant, "{variant}");
        assert_eq!(top.symbol_kind.as_deref(), Some("variant"), "{variant}");
    }
}

#[test]
fn resolve_natural_language_phrase_still_uses_concepts() {
    // A natural-language phrase like "submit the form" should still go through
    // the concept cascade, not the identifier tier. This guards against the
    // fix being too aggressive.
    let (_dir, store, _file_id) = make_store_with_concepts();

    let outcome = resolve(&store, "submit the form", &ResolveOptions::default()).expect("resolve");

    assert!(
        outcome.confidence != Confidence::Unresolved,
        "should resolve 'submit the form' via concepts"
    );
    assert!(
        !outcome.matches.is_empty(),
        "a non-symbol phrase keeps its concept results, never collapses to empty"
    );
    // Should NOT be the symbol tier.
    assert!(
        outcome.tier != Some(Tier::Symbol),
        "natural-language phrase should not use symbol fallback, got tier={:?}",
        outcome.tier
    );
    assert_ne!(
        outcome.tier,
        Some(Tier::Ident),
        "natural-language phrase must not take the identifier tier"
    );
}
