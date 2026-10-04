// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for `changes::detect`'s coverage report:
//! `uncovered_changes` (changed ranges no symbol maps) and `unanchored`
//! (symbols the change touched in the base that the graph no longer holds).
//!
//! These run against a real repository and a real graph build, not a
//! hand-inserted store: the whole question is whether the extractor's line
//! ranges cover the diff's line ranges, and a fixture that chooses both
//! would answer it by construction.

use std::path::{Path, PathBuf};

use pixel_graph::build::build_graph;
use pixel_graph::changes::{ChangesReport, UncoveredMotif, detect};
use pixel_graph::store::GraphStore;

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pixel-uncovered-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repository with one committed TypeScript file, built line by line so
/// every assertion below can name a line number:
///
/// ```text
/// 1  // header comment
/// 2  // second header line
/// 3  export function keep(n: number): number {
/// 4    return n + 1
/// 5  }
/// 6  export function gone(n: number): number {
/// 7    return n + 2
/// 8  }
/// ```
fn source_v1() -> String {
    [
        "// header comment",
        "// second header line",
        "export function keep(n: number): number {",
        "  return n + 1",
        "}",
        "export function gone(n: number): number {",
        "  return n + 2",
        "}",
        "",
    ]
    .join("\n")
}

fn repo(name: &str) -> PathBuf {
    let root = tmpdir(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/work.ts"), source_v1()).unwrap();
    std::fs::write(root.join(".gitignore"), "graph.db*\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    root
}

/// Build the graph over the tree as it stands now and report against it.
/// The db lives outside the repository so it never enters the diff.
fn report(root: &Path, base: Option<&str>) -> ChangesReport {
    let db = tmpdir("db").join("graph.db");
    build_graph(root, &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    detect(&store, root, base, false).unwrap()
}

fn motifs(report: &ChangesReport) -> Vec<(String, UncoveredMotif)> {
    report
        .uncovered_changes
        .iter()
        .map(|u| (u.path.clone(), u.motif))
        .collect()
}

#[test]
fn an_edit_inside_a_function_leaves_nothing_uncovered() {
    let root = repo("covered");
    // Line 4 is inside `keep`, on both sides of the diff.
    let edited = source_v1().replace("  return n + 1", "  return n + 11");
    std::fs::write(root.join("src/work.ts"), edited).unwrap();

    let r = report(&root, Some("HEAD"));
    assert!(
        r.uncovered_changes.is_empty(),
        "an edit inside a symbol is covered: {:?}",
        r.uncovered_changes
    );
    assert!(r.unanchored.is_empty(), "{:?}", r.unanchored);
    assert!(!r.uncovered_lower_bound);
    assert!(r.uncovered_note.is_empty(), "{}", r.uncovered_note);
    // The symbol itself is still reported as changed.
    let names: Vec<&str> = r.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["keep"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_edit_between_symbols_is_uncovered_at_its_own_lines() {
    let root = repo("outside");
    // Line 2 is a header comment: inside the file, outside every symbol.
    let edited = source_v1().replace("// second header line", "// second header line, edited");
    std::fs::write(root.join("src/work.ts"), edited).unwrap();

    let r = report(&root, Some("HEAD"));
    // One entry per side: the line exists in both, and each side is judged
    // against its own coordinates.
    assert_eq!(
        motifs(&r),
        vec![
            ("src/work.ts".to_string(), UncoveredMotif::OutsideSymbol),
            ("src/work.ts".to_string(), UncoveredMotif::OutsideSymbol),
        ],
        "{:?}",
        r.uncovered_changes
    );
    assert_eq!(r.uncovered_changes[0].new_lines, Some([2, 2]));
    assert_eq!(r.uncovered_changes[0].old_lines, None);
    assert_eq!(r.uncovered_changes[1].old_lines, Some([2, 2]));
    assert_eq!(r.uncovered_changes[1].new_lines, None);
    // Nothing was deleted, so nothing lost its anchor.
    assert!(r.unanchored.is_empty(), "{:?}", r.unanchored);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn deleting_a_function_leaves_it_unanchored_under_its_old_uid() {
    let root = repo("deleted-symbol");
    // Drop `gone` entirely: lines 6-8 disappear.
    let edited = source_v1().replace(
        "export function gone(n: number): number {\n  return n + 2\n}\n",
        "",
    );
    std::fs::write(root.join("src/work.ts"), edited).unwrap();

    let r = report(&root, Some("HEAD"));
    let uids: Vec<&str> = r.unanchored.iter().map(|u| u.uid.as_str()).collect();
    assert_eq!(
        uids,
        vec!["src/work.ts#gone#function"],
        "the deleted symbol is named by the uid a reader can look up"
    );
    assert_eq!(r.unanchored[0].name, "gone");
    assert_eq!(r.unanchored[0].path, "src/work.ts");
    assert_eq!(r.unanchored[0].old_lines, [6, 8]);
    // `keep` survives the edit, so it is not reported as lost.
    assert_eq!(r.unanchored.len(), 1);
    // A deletion writes no line: it must not be reported as an uncovered
    // addition at the line the hunk anchors on.
    assert!(
        r.uncovered_changes.iter().all(|u| u.new_lines.is_none()),
        "{:?}",
        r.uncovered_changes
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn deleting_a_file_unanchors_its_symbols_even_when_the_graph_still_holds_them() {
    let root = repo("deleted-file");
    // The graph is built while the file is still there, then the file goes:
    // a stale graph still answers for it, which is exactly the case a
    // reachability answer must not be allowed to trust.
    let db = tmpdir("db").join("graph.db");
    build_graph(&root, &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert!(
        store
            .symbol_by_uid("src/work.ts#gone#function")
            .unwrap()
            .is_some(),
        "the graph must still hold the file for this test to mean anything"
    );
    std::fs::remove_file(root.join("src/work.ts")).unwrap();

    let r = detect(&store, &root, Some("HEAD"), false).unwrap();
    let mut names: Vec<&str> = r.unanchored.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["gone", "keep"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_change_in_a_file_no_grammar_reads_is_unsupported_language() {
    let root = repo("unsupported");
    std::fs::write(root.join("config.toml"), "[a]\nb = 1\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "config"]);
    std::fs::write(root.join("config.toml"), "[a]\nb = 2\n").unwrap();

    let r = report(&root, Some("HEAD"));
    assert_eq!(
        motifs(&r),
        vec![
            (
                "config.toml".to_string(),
                UncoveredMotif::UnsupportedLanguage
            ),
            (
                "config.toml".to_string(),
                UncoveredMotif::UnsupportedLanguage
            ),
        ],
        "{:?}",
        r.uncovered_changes
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_binary_change_is_a_non_text_change() {
    let root = repo("binary");
    std::fs::write(root.join("data.bin"), [0u8, 1, 2, 3, 0, 4]).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "blob"]);
    std::fs::write(root.join("data.bin"), [0u8, 9, 9, 9, 0, 4]).unwrap();

    let r = report(&root, Some("HEAD"));
    assert_eq!(
        motifs(&r),
        vec![("data.bin".to_string(), UncoveredMotif::NonTextChange)],
        "{:?}",
        r.uncovered_changes
    );
    // git prints no hunk for it, so there is no line to report either way.
    assert_eq!(r.uncovered_changes[0].old_lines, None);
    assert_eq!(r.uncovered_changes[0].new_lines, None);
    // And it counts as a changed file: a change with no hunk is still a
    // change, and a report that dropped it would claim to cover the diff.
    assert_eq!(r.changed_files, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_mode_only_change_is_a_non_text_change() {
    let root = repo("mode");
    let script = root.join("src/work.ts");
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let r = report(&root, Some("HEAD"));
    assert_eq!(
        motifs(&r),
        vec![("src/work.ts".to_string(), UncoveredMotif::NonTextChange)],
        "a mode change carries no hunk and must not pass for covered: {:?}",
        r.uncovered_changes
    );
    assert_eq!(r.changed_files, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_the_graph_does_not_hold_yet_is_not_indexed() {
    let root = repo("not-indexed");
    let db = tmpdir("db").join("graph.db");
    build_graph(&root, &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    // Added after the build: indexable, simply not in the graph.
    std::fs::write(
        root.join("src/fresh.ts"),
        "export function fresh(): number {\n  return 3\n}\n",
    )
    .unwrap();
    git(&root, &["add", "src/fresh.ts"]);

    let r = detect(&store, &root, Some("HEAD"), false).unwrap();
    assert_eq!(
        motifs(&r),
        vec![("src/fresh.ts".to_string(), UncoveredMotif::NotIndexed)],
        "{:?}",
        r.uncovered_changes
    );
    assert_eq!(r.uncovered_changes[0].new_lines, Some([1, 3]));
    let _ = std::fs::remove_dir_all(&root);
}

/// Reading the base side costs a `git show` and a parse per file, so it is
/// capped. Past the cap the removed ranges are reported as unexamined
/// rather than assumed anchored, and the report says so: abstaining is the
/// conservative direction, silently covering less is not.
#[test]
fn past_the_base_read_cap_the_old_side_is_reported_unexamined() {
    let root = tmpdir("base-cap");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // One more file than `BASE_EXTRACTION_CAP` (200), each with a line the
    // edit below removes, so every one of them asks for a base read.
    let files: Vec<String> = (0..=200).map(|i| format!("src/f{i:03}.ts")).collect();
    for (i, f) in files.iter().enumerate() {
        std::fs::write(
            root.join(f),
            format!("export function f{i:03}(): number {{\n  return 1\n}}\n"),
        )
        .unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    for (i, f) in files.iter().enumerate() {
        std::fs::write(
            root.join(f),
            format!("export function f{i:03}(): number {{\n  return 2\n}}\n"),
        )
        .unwrap();
    }

    let r = report(&root, Some("HEAD"));
    assert_eq!(r.changed_files, 201);
    // The symbol judgement shares the cap: the 201st file is judged by its
    // status alone, and says so.
    let basis = |name: &str| {
        r.symbols
            .iter()
            .find(|s| s.name == name)
            .map(|s| (s.change.clone(), s.change_basis.clone()))
    };
    assert_eq!(basis("f199"), Some(("modified".into(), "symbol".into())));
    assert_eq!(basis("f200"), Some(("modified".into(), "file".into())));
    assert!(
        r.uncovered_lower_bound,
        "a capped scan is a lower bound: {:?}",
        r.uncovered_changes
    );
    assert!(
        r.uncovered_note
            .contains("the base side of 1 file(s) was not examined"),
        "the note must count the files it skipped: {}",
        r.uncovered_note
    );
    assert!(
        !r.uncovered_note.contains("uncovered ranges found"),
        "nothing was truncated here: {}",
        r.uncovered_note
    );
    // Every edit sits inside its function, so the one and only entry is the
    // file whose base side the cap stopped us from reading.
    assert_eq!(
        motifs(&r),
        vec![("src/f200.ts".to_string(), UncoveredMotif::NotIndexed)],
        "{:?}",
        r.uncovered_changes
    );
    assert_eq!(r.uncovered_changes[0].old_lines, Some([2, 2]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn with_no_base_ref_the_old_side_is_read_from_the_index() {
    let root = repo("index-base");
    // Stage a version holding a symbol HEAD never had, then remove it from
    // the working tree. A plain `git diff` compares the working tree to the
    // index, so the removal's base side only exists in the index: reading
    // HEAD instead would find no `staged_only` and report nothing lost.
    let staged = format!(
        "{}{}",
        source_v1(),
        [
            "export function staged_only(): number {",
            "  return 7",
            "}",
            ""
        ]
        .join("\n")
    );
    std::fs::write(root.join("src/work.ts"), &staged).unwrap();
    git(&root, &["add", "src/work.ts"]);
    std::fs::write(root.join("src/work.ts"), source_v1()).unwrap();

    let r = report(&root, None);
    let names: Vec<&str> = r.unanchored.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, vec!["staged_only"]);
    assert_eq!(r.unanchored[0].uid, "src/work.ts#staged_only#function");
    let _ = std::fs::remove_dir_all(&root);
}
