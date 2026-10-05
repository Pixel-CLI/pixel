// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the ingest skip rules: which paths and which blob
//! contents never reach the history index, and that every skip is recorded
//! with its reason.

use super::*;
use crate::testutil::init_repo;

/// Machine-noise paths are skipped wherever the noisy segment sits.
#[test]
fn skip_path_should_skip_noise_paths_by_segment_name_and_suffix() {
    for path in [
        "a/vendor/lib.rb",
        "web/node_modules/x.js",
        "out/dist/app.js",
        "Gemfile.lock",
        "uv.lock",
        "assets/app.MIN.css",
        "assets/app.bundle.min.js",
        "src/app.js.map",
        "tests/__snapshots__/view.snap",
        "proto/generated/api.rs",
        "x/codegen_out/y.ts",
        &format!("icons/{}.svg", "a".repeat(80)),
    ] {
        assert!(skip_path(path), "{path} should be skipped");
    }
}

/// Ordinary source stays indexed, including names that merely resemble a
/// skip rule.
#[test]
fn skip_path_should_keep_ordinary_source_and_near_misses() {
    for path in [
        "src/main.rs",
        "src/admin.config.js",
        "src/vendoring.rs",
        "docs/distance.md",
        "icons/logo.svg",
        "src/sourcemap.rs",
        "Cargo.toml",
    ] {
        assert!(!skip_path(path), "{path} should be indexed");
    }
}

/// An SVG is a "large asset" only past 80 characters of name.
#[test]
fn skip_path_should_skip_an_svg_only_past_eighty_characters() {
    let at_limit = format!("{}.svg", "b".repeat(76));
    assert_eq!(at_limit.len(), 80);
    assert!(!skip_path(&at_limit));
    let over = format!("{}.svg", "b".repeat(77));
    assert!(skip_path(&over));
}

/// NUL bytes or bytes that are not UTF-8 make a blob binary; an empty probe
/// is text.
#[test]
fn classify_content_should_flag_nul_and_invalid_utf8_as_binary() {
    assert_eq!(classify_content(b"a\0b", "x.rs"), ContentKind::Binary);
    assert_eq!(
        classify_content(&[0xff, 0xfe, b'a'], "x.rs"),
        ContentKind::Binary
    );
    assert_eq!(classify_content(b"", "x.rs"), ContentKind::Text);
}

/// A mean line over 400 bytes, or one line over 2000, is minified; exactly
/// at either limit is still text.
#[test]
fn classify_content_should_flag_long_lines_as_minified_past_the_limits() {
    let mean_400 = format!("{}\n{}\n", "a".repeat(400), "b".repeat(400));
    assert_eq!(
        classify_content(mean_400.as_bytes(), "x.js"),
        ContentKind::Text
    );
    let mean_401 = format!("{}\n{}\n", "a".repeat(401), "b".repeat(401));
    assert_eq!(
        classify_content(mean_401.as_bytes(), "x.js"),
        ContentKind::Minified
    );

    let mut one_long = "a".repeat(2000);
    one_long.push('\n');
    for _ in 0..10 {
        one_long.push_str("x\n");
    }
    assert_eq!(
        classify_content(one_long.as_bytes(), "x.css"),
        ContentKind::Text
    );
    let mut longer = "a".repeat(2001);
    longer.push('\n');
    for _ in 0..10 {
        longer.push_str("x\n");
    }
    assert_eq!(
        classify_content(longer.as_bytes(), "x.css"),
        ContentKind::Minified
    );
}

/// Heavy non-ASCII marks generated JSON/JS only; the same bytes in a source
/// file are text.
#[test]
fn classify_content_should_flag_non_ascii_heavy_json_and_js_as_generated() {
    // 4 ASCII bytes + 3 x 2-byte chars = 6 non-ASCII of 10 bytes (60 %).
    let heavy = "abcd\u{e9}\u{e9}\u{e9}";
    for path in ["data.JSON", "glyphs.js", "m.mjs"] {
        assert_eq!(
            classify_content(heavy.as_bytes(), path),
            ContentKind::Generated,
            "{path}"
        );
    }
    assert_eq!(
        classify_content(heavy.as_bytes(), "notes.rs"),
        ContentKind::Text
    );
}

/// Exactly 30 % non-ASCII is not over the threshold.
#[test]
fn classify_content_should_keep_json_at_exactly_thirty_percent_non_ascii() {
    // 14 ASCII bytes + 3 x 2-byte chars = 6 of 20 bytes (30 %).
    let edge = format!("{}{}", "a".repeat(14), "\u{e9}".repeat(3));
    assert_eq!(edge.len(), 20);
    assert_eq!(
        classify_content(edge.as_bytes(), "x.json"),
        ContentKind::Text
    );
    let over = format!("{}{}", "a".repeat(13), "\u{e9}".repeat(3));
    assert_eq!(
        classify_content(over.as_bytes(), "x.json"),
        ContentKind::Generated
    );
}

/// A learned poison path is excluded with the reason it was learned under,
/// structural skips with theirs; the first reason learned for a path stays.
#[test]
fn decide_skips_should_exclude_learned_and_structural_paths_with_their_reasons() {
    let repo = init_repo();
    let mut store = FactsStore::open(repo.path()).unwrap();
    store
        .learn_poison("assets/huge.bin", "blob over cap")
        .unwrap();
    store
        .learn_poison("assets/huge.bin", "second reason")
        .unwrap();
    assert_eq!(store.poison_paths().unwrap(), vec!["assets/huge.bin"]);

    let touched = vec![
        "src/main.rs".to_string(),
        "yarn.lock".to_string(),
        "assets/huge.bin".to_string(),
    ];
    let plan = decide_skips(&store, &touched);
    assert_eq!(
        plan.excludes,
        vec![":(exclude)assets/huge.bin", ":(exclude)yarn.lock"]
    );
    assert_eq!(
        plan.skipped,
        vec![
            ("yarn.lock".to_string(), "structural-skip".to_string()),
            ("assets/huge.bin".to_string(), "blob over cap".to_string()),
        ]
    );
    assert_eq!(store.skip_reason("src/main.rs"), None);
}
