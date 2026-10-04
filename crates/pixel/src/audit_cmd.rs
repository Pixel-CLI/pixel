// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel audit` — what an agent reads to learn what this repository's
//! largest source files contain, whole against `pixel list-signatures`.
//!
//! The same measurement as `/benchmarks/` ("Well-known files") and
//! `scripts/bench-read-savings.sh`, run on the user's own code: for each of
//! the largest indexed files, the file's bytes against the bytes of the
//! outline `list-signatures` prints, both counted by
//! [`pixel_actionlog::read_tokens`]. Per-language coverage closes the report,
//! since a language the graph does not index is a file no outline covers.
//!
//! Read-only and in-process like `pixel coverage`: it opens `graph.db`
//! directly and sends nothing anywhere. A file whose bytes no longer hash to
//! its stored row is left out and counted, never measured against an outline
//! of other contents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pixel_actionlog::{read_tokens, saved_percent};
use pixel_daemon::api::GRAPH_DB_FILE;
use pixel_graph::build::{FRESHNESS_KEY, content_oid, read_source_file};
use pixel_graph::extract::lang_of;
use pixel_graph::store::{FileRow, GraphStore};
use pixel_index::index::SHARD_DIR;
use serde_json::{Value, json};

use crate::coverage_cmd;

/// Files measured when `--top` is not given: the size of the published
/// well-known-files table, enough for a median without a long report.
pub const AUDIT_DEFAULT_TOP: u32 = 20;

#[derive(Debug, Clone)]
pub struct AuditOptions {
    pub path: PathBuf,
    pub top: u32,
    pub json: bool,
}

/// One measured file: its bytes and the bytes of its outline.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Measured {
    path: String,
    lang: String,
    lines: u64,
    signatures: u64,
    file_bytes: u64,
    outline_bytes: u64,
}

impl Measured {
    fn full_tokens(&self) -> u64 {
        read_tokens(self.file_bytes)
    }

    fn outline_tokens(&self) -> u64 {
        read_tokens(self.outline_bytes)
    }

    /// The saving in whole percent; 0 when the outline is not smaller.
    fn saved(&self) -> i64 {
        saved_percent(self.full_tokens(), self.outline_tokens()).unwrap_or(0)
    }
}

/// Why a candidate among the largest files was not measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftOut {
    /// Its bytes differ from the ones the graph indexed (or it is gone):
    /// the stored outline describes other contents.
    Stale,
    /// The graph holds no signature for it: it defines nothing to outline
    /// (a module of assignments such as a Sphinx `conf.py`), or its grammar
    /// did not parse it. Either way a saving would measure an empty outline.
    NoSignatures,
}

#[derive(Debug, Default)]
struct Report {
    measured: Vec<Measured>,
    stale: Vec<String>,
    no_signatures: Vec<String>,
    /// Indexed source files, the pool the largest are taken from.
    candidates: usize,
    /// Candidates walked before `top` were measured: fewer than
    /// `candidates` means the report is capped.
    examined: usize,
    top: usize,
    /// The graph's freshness signature: which snapshot the rows came from.
    graph_signature: Option<String>,
}

impl Report {
    /// Files remain that `--top` kept out of the report.
    fn capped(&self) -> bool {
        self.examined < self.candidates
    }
}

/// The exact stdout of `pixel list-signatures` for one file: a header, then
/// one line per symbol that has a signature. `symbols` yields
/// `(start_line, kind, sig)`; an empty list prints the build hint instead.
pub(crate) fn render_outline<'a>(
    file: &str,
    lang: &str,
    symbols: impl IntoIterator<Item = (u64, &'a str, &'a str)>,
) -> String {
    let mut output = format!("// {file} [{lang}]\n");
    let mut any = false;
    for (line, kind, sig) in symbols {
        any = true;
        if !sig.is_empty() {
            output.push_str(&format!("  L{line:>5}  {kind}  {sig}\n"));
        }
    }
    if !any {
        output.push_str("// (no indexed symbols — run `pixel build-index .` first)\n");
    }
    output
}

/// Lines as an editor counts them: a last line without a newline counts.
fn line_count(content: &[u8]) -> u64 {
    let newlines = content.iter().filter(|b| **b == b'\n').count() as u64;
    newlines + u64::from(content.last().is_some_and(|b| *b != b'\n'))
}

/// Measure one indexed file against its current bytes. The outer error is
/// a store failure, never a verdict on the file; the inner one says why the
/// file was left out. The bytes are read under the graph's own rules
/// ([`read_source_file`]: a regular file within its size cap), so a file
/// grown past what the graph would index is stale, not loaded.
fn measure(
    store: &GraphStore,
    root: &Path,
    row: &FileRow,
) -> Result<Result<Measured, LeftOut>, String> {
    let Some(content) = read_source_file(&root.join(&row.path)) else {
        return Ok(Err(LeftOut::Stale));
    };
    if content_oid(&content) != row.blob_oid {
        return Ok(Err(LeftOut::Stale));
    }
    let symbols = store
        .symbols_in_file(row.id)
        .map_err(|e| format!("audit: {}: {e}", row.path))?;
    let signatures = symbols.iter().filter(|s| !s.sig.is_empty()).count() as u64;
    if signatures == 0 {
        return Ok(Err(LeftOut::NoSignatures));
    }
    let outline = render_outline(
        &row.path,
        &row.lang,
        symbols
            .iter()
            .map(|s| (u64::from(s.start_line), s.kind.as_str(), s.sig.as_str())),
    );
    Ok(Ok(Measured {
        path: row.path.clone(),
        lang: row.lang.clone(),
        lines: line_count(&content),
        signatures,
        file_bytes: content.len() as u64,
        outline_bytes: outline.len() as u64,
    }))
}

/// The largest indexed source files first, by their size on disk; a file
/// gone from disk sorts last, where it is reported stale if reached.
fn largest_first(root: &Path, rows: Vec<FileRow>) -> Vec<FileRow> {
    let mut sized: Vec<(u64, FileRow)> = rows
        .into_iter()
        .filter(|row| lang_of(&row.path).is_some())
        .map(|row| {
            let size = std::fs::metadata(root.join(&row.path)).map_or(0, |m| m.len());
            (size, row)
        })
        .collect();
    sized.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    sized.into_iter().map(|(_, row)| row).collect()
}

/// Walk the largest files until `top` are measured, recording every
/// candidate left out on the way, so the report says what it skipped.
fn collect(store: &GraphStore, root: &Path, top: usize) -> Result<Report, String> {
    let rows = largest_first(root, store.files().map_err(|e| format!("audit: {e}"))?);
    let mut report = Report {
        candidates: rows.len(),
        top,
        graph_signature: store
            .meta_get(FRESHNESS_KEY)
            .map_err(|e| format!("audit: {e}"))?,
        ..Report::default()
    };
    for row in rows {
        if report.measured.len() == top {
            break;
        }
        report.examined += 1;
        match measure(store, root, &row)? {
            Ok(m) => report.measured.push(m),
            Err(LeftOut::Stale) => report.stale.push(row.path),
            Err(LeftOut::NoSignatures) => report.no_signatures.push(row.path),
        }
    }
    Ok(report)
}

/// The median of the per-file savings: the middle value, or the rounded
/// mean of the two middle ones. `None` for an empty list.
fn median(values: &[i64]) -> Option<i64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[mid]),
        _ => Some(((sorted[mid - 1] + sorted[mid]) as f64 / 2.0).round() as i64),
    }
}

/// Summed tokens of the measured files: `(full, outline)`.
fn totals(measured: &[Measured]) -> (u64, u64) {
    (
        measured.iter().map(Measured::full_tokens).sum(),
        measured.iter().map(Measured::outline_tokens).sum(),
    )
}

fn saving_text(pct: Option<i64>) -> String {
    pct.map_or_else(|| "no saving".to_string(), |p| format!("-{p}%"))
}

fn render_human(
    report: &Report,
    coverage: &BTreeMap<String, coverage_cmd::Row>,
    root: &Path,
) -> String {
    let mut out = format!(
        "pixel audit — what an agent reads to learn what a file contains\nroot: {}\n\n",
        root.display()
    );
    if report.measured.is_empty() {
        out.push_str("no indexed source file could be measured\n");
    } else {
        out.push_str(&format!(
            "{:>11}  {:>11}  {:>9}  {:>5}  {:>6}  file\n",
            "full read", "outline", "saved", "sigs", "lines"
        ));
        for m in &report.measured {
            out.push_str(&format!(
                "{:>7} tok  {:>7} tok  {:>9}  {:>5}  {:>6}  {}\n",
                m.full_tokens(),
                m.outline_tokens(),
                saving_text(saved_percent(m.full_tokens(), m.outline_tokens())),
                m.signatures,
                m.lines,
                m.path
            ));
        }
        let (full, outline) = totals(&report.measured);
        out.push_str(&format!(
            "\ntotal, {} of {} indexed source files: full read {full} tok, pixel answer {outline} tok ({})\n",
            report.measured.len(),
            report.candidates,
            saving_text(saved_percent(full, outline))
        ));
        let savings: Vec<i64> = report.measured.iter().map(Measured::saved).collect();
        if let (Some(med), Some(min), Some(max)) =
            (median(&savings), savings.iter().min(), savings.iter().max())
        {
            out.push_str(&format!(
                "per file: median {med}% saved, from {min}% to {max}%\n"
            ));
        }
    }
    if !report.stale.is_empty() {
        out.push_str(&format!(
            "left out: {} changed since indexing (`pixel prepare-repo .` refreshes them)\n",
            report.stale.len()
        ));
    }
    if !report.no_signatures.is_empty() {
        out.push_str(&format!(
            "left out: {} with no signature to outline (no definitions, or a grammar that missed them): {}\n",
            report.no_signatures.len(),
            report.no_signatures.join(", ")
        ));
    }
    if !coverage.is_empty() {
        let langs: Vec<String> = coverage
            .iter()
            .map(|(lang, r)| {
                format!(
                    "{lang} {}/{} ({:.1}%)",
                    r.indexed,
                    r.on_disk,
                    coverage_cmd::pct(r.indexed, r.on_disk)
                )
            })
            .collect();
        out.push_str(&format!("\nindexed: {}\n", langs.join(", ")));
    }
    out.push_str(
        "\nThis counts what an agent reads to learn a file's contents, not your bill.\n\
         Counts are UTF-8 bytes / 4, rounded down, as `pixel list-signatures` prints them;\n\
         re-check a row with `pixel list-signatures <file>`. After a few days of agent\n\
         sessions, `pixel token-savings` reports what Pixel's answers actually spared.\n",
    );
    out
}

/// What every count stands on, as the JSON envelope states it.
const AUDIT_BASIS: &str = "graph snapshot rows against each file's current bytes; utf-8 bytes / 4, rounded down, per file";

fn render_json(
    report: &Report,
    coverage: &BTreeMap<String, coverage_cmd::Row>,
    root: &Path,
) -> Value {
    let (full, outline) = totals(&report.measured);
    let savings: Vec<i64> = report.measured.iter().map(Measured::saved).collect();
    let marker = if report.capped() {
        "capped"
    } else {
        "complete"
    };
    json!({
        "root": root.display().to_string(),
        "marker": marker,
        "epistemics": {
            "closed_world": false,
            "lower_bound": report.capped(),
            "basis": AUDIT_BASIS,
            "confidence": marker,
        },
        "snapshot": {
            "graph_signature": report.graph_signature,
            "indexed_source_files": report.candidates,
            "examined": report.examined,
            "top": report.top,
        },
        "files": report.measured.iter().map(|m| json!({
            "path": m.path,
            "lang": m.lang,
            "lines": m.lines,
            "signatures": m.signatures,
            "full_tokens": m.full_tokens(),
            "outline_tokens": m.outline_tokens(),
            "saved_pct": saved_percent(m.full_tokens(), m.outline_tokens()),
        })).collect::<Vec<_>>(),
        "totals": {
            "files": report.measured.len(),
            "full_tokens": full,
            "outline_tokens": outline,
            "saved_pct": saved_percent(full, outline),
            "median_saved_pct": median(&savings),
        },
        "left_out": {
            "stale": report.stale,
            "no_signatures": report.no_signatures,
        },
        "coverage": coverage.iter().map(|(lang, r)| json!({
            "lang": lang,
            "on_disk": r.on_disk,
            "indexed": r.indexed,
            "coverage_pct": coverage_cmd::pct(r.indexed, r.on_disk),
        })).collect::<Vec<_>>(),
    })
}

/// The report for `root` as text or JSON: the part `run` prints. It never
/// builds the graph; `run` does, once, when none exists.
fn report_for(root: &Path, top: u32, json: bool) -> Result<String, String> {
    let db = root.join(SHARD_DIR).join(GRAPH_DB_FILE);
    if !db.exists() {
        return Err(format!(
            "audit: no code graph under {}; run `pixel prepare-repo .` first",
            root.display()
        ));
    }
    let store = GraphStore::open(&db).map_err(|e| format!("audit: {e}"))?;
    let report = collect(&store, root, top as usize)?;
    let (coverage, _, _) = coverage_cmd::collect(root)?;
    if json {
        return serde_json::to_string_pretty(&render_json(&report, &coverage, root))
            .map(|s| s + "\n")
            .map_err(|e| e.to_string());
    }
    Ok(render_human(&report, &coverage, root))
}

/// The stderr line of a first run, which builds the graph before measuring.
const FIRST_RUN_NOTICE: &str = "pixel: audit: no code graph yet, building it (first run only)";

/// Build the graph only when there is none: a first run on a fresh clone
/// must answer without a separate `prepare-repo`, while an existing graph is
/// read as it stands (files changed since are left out and counted) rather
/// than paying a whole rebuild.
pub fn run(opts: AuditOptions) -> Result<(), String> {
    let root = crate::discover_root(&opts.path).map_err(|e| format!("audit: {e}"))?;
    if !root.join(SHARD_DIR).join(GRAPH_DB_FILE).exists() {
        eprintln!("{FIRST_RUN_NOTICE}");
        crate::execute(&root, crate::Request::Graph { if_stale: false }, false)
            .map_err(|e| format!("audit: {e}"))?;
    }
    print!("{}", report_for(&root, opts.top, opts.json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with a graph built the way production builds it: the
    /// largest file first on disk, a second one, and a file whose rows hold
    /// no signature.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("px-audit-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let big: String = (0..40)
                .map(|i| {
                    format!("/// Doc for f{i}.\npub fn f{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n")
                })
                .collect();
            std::fs::write(dir.join("src/big.rs"), big).unwrap();
            std::fs::write(
                dir.join("src/small.rs"),
                "pub fn one() -> u8 {\n    1\n}\npub fn two() -> u8 {\n    2\n}\n",
            )
            .unwrap();
            let root = dir.canonicalize().unwrap();
            let db = root.join(SHARD_DIR).join(GRAPH_DB_FILE);
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            pixel_graph::build::build_graph(&root, &db).unwrap();
            Self(root)
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.0.join(SHARD_DIR).join(GRAPH_DB_FILE)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn measured(path: &str, file_bytes: u64, outline_bytes: u64) -> Measured {
        Measured {
            path: path.to_string(),
            lang: "rust".to_string(),
            lines: 10,
            signatures: 3,
            file_bytes,
            outline_bytes,
        }
    }

    /// `list-signatures` prints this string and `audit` counts its bytes, so
    /// both must come from one renderer: a symbol without a signature prints
    /// nothing, and only an empty list prints the build hint.
    #[test]
    fn render_outline_is_the_list_signatures_text() {
        assert_eq!(
            render_outline(
                "src/a.rs",
                "rust",
                [
                    (3, "fn", "pub fn a()"),
                    (9, "mod", ""),
                    (12, "struct", "pub struct B")
                ]
            ),
            "// src/a.rs [rust]\n  L    3  fn  pub fn a()\n  L   12  struct  pub struct B\n"
        );
        assert_eq!(
            render_outline("src/a.rs", "rust", [(9, "mod", "")]),
            "// src/a.rs [rust]\n",
            "a symbol list without signatures is not the empty-list hint"
        );
        assert_eq!(
            render_outline("src/a.rs", "rust", []),
            "// src/a.rs [rust]\n// (no indexed symbols — run `pixel build-index .` first)\n"
        );
    }

    #[test]
    fn line_count_counts_a_last_line_without_newline() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\n\n"), 2);
        assert_eq!(line_count(b"\n"), 1);
    }

    #[test]
    fn median_is_the_middle_or_the_rounded_mean_of_the_two() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5));
        assert_eq!(median(&[97, 90, 95]), Some(95), "sorted before picking");
        assert_eq!(median(&[90, 95, 97, 99]), Some(96));
        assert_eq!(median(&[1, 2]), Some(2), "1.5 rounds half away from zero");
        assert_eq!(median(&[10, 20]), Some(15));
    }

    /// An outline no smaller than its file is no saving, never `-0%` or a
    /// negative saving folded into the median.
    #[test]
    fn a_file_without_saving_counts_zero_and_says_so() {
        let tie = measured("src/tie.rs", 400, 400);
        assert_eq!(tie.saved(), 0);
        assert_eq!(
            saving_text(saved_percent(tie.full_tokens(), tie.outline_tokens())),
            "no saving"
        );
        let win = measured("src/win.rs", 400, 100);
        assert_eq!(
            (win.full_tokens(), win.outline_tokens(), win.saved()),
            (100, 25, 75)
        );
        assert_eq!(totals(&[tie, win]), (200, 125));
    }

    /// The production path: a graph from `build_graph`, files ranked by size
    /// on disk, and the outline measured is the one `list-signatures` prints.
    #[test]
    fn collect_measures_the_largest_files_first_against_their_outline() {
        let fx = Fixture::new("rank");
        let store = fx.store();
        let report = collect(&store, &fx.0, 20).unwrap();
        let paths: Vec<&str> = report.measured.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["src/big.rs", "src/small.rs"]);
        assert!(report.stale.is_empty() && report.no_signatures.is_empty());

        let small = &report.measured[1];
        let content = std::fs::read(fx.0.join("src/small.rs")).unwrap();
        let row = store.file_by_path("src/small.rs").unwrap().unwrap();
        let symbols = store.symbols_in_file(row.id).unwrap();
        let outline = render_outline(
            "src/small.rs",
            "rust",
            symbols
                .iter()
                .map(|s| (u64::from(s.start_line), s.kind.as_str(), s.sig.as_str())),
        );
        assert_eq!(
            small,
            &Measured {
                path: "src/small.rs".to_string(),
                lang: "rust".to_string(),
                lines: 6,
                signatures: 2,
                file_bytes: content.len() as u64,
                outline_bytes: outline.len() as u64,
            }
        );
        assert_eq!(report.measured[0].signatures, 40);
        assert_eq!(report.measured[0].lines, 160);
    }

    /// Size decides, the path breaks a tie so the report is stable across
    /// runs, and a row whose file is gone sorts last instead of first.
    #[test]
    fn largest_first_orders_by_size_then_path_and_drops_non_code() {
        let dir = std::env::temp_dir().join(format!("px-audit-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, size) in [("b.rs", 10), ("a.rs", 10), ("big.rs", 30), ("notes.md", 90)] {
            std::fs::write(dir.join(name), "x".repeat(size)).unwrap();
        }
        let row = |id: i64, path: &str| FileRow {
            id,
            path: path.to_string(),
            blob_oid: String::new(),
            lang: "rust".to_string(),
        };
        let rows = vec![
            row(1, "gone.rs"),
            row(2, "b.rs"),
            row(3, "a.rs"),
            row(4, "big.rs"),
            row(5, "notes.md"),
        ];
        let order: Vec<String> = largest_first(&dir, rows)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(order, ["big.rs", "a.rs", "b.rs", "gone.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--top` stops the walk: only the largest file is measured, and files
    /// past the cut are neither measured nor reported left out.
    #[test]
    fn collect_stops_at_top() {
        let fx = Fixture::new("top");
        let report = collect(&fx.store(), &fx.0, 1).unwrap();
        let paths: Vec<&str> = report.measured.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["src/big.rs"]);
        assert!(report.stale.is_empty() && report.no_signatures.is_empty());
    }

    /// A file edited after indexing is left out as stale, not measured
    /// against the outline of its old contents, and it does not use up a
    /// `--top` slot: the next file is measured instead.
    #[test]
    fn a_file_changed_since_indexing_is_left_out_and_replaced() {
        let fx = Fixture::new("stale");
        let big = fx.0.join("src/big.rs");
        let mut content = std::fs::read(&big).unwrap();
        content.extend_from_slice(b"pub fn added() {}\n");
        std::fs::write(&big, content).unwrap();
        let report = collect(&fx.store(), &fx.0, 1).unwrap();
        assert_eq!(report.stale, ["src/big.rs"]);
        let paths: Vec<&str> = report.measured.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["src/small.rs"]);

        std::fs::remove_file(&big).unwrap();
        let report = collect(&fx.store(), &fx.0, 20).unwrap();
        assert_eq!(report.stale, ["src/big.rs"], "a deleted file is stale too");
    }

    /// Rows with no signature would count an empty outline as a saving:
    /// they are named, never measured. A non-code row (a concept file) is
    /// no candidate at all, however large.
    #[test]
    fn unoutlined_and_non_code_rows_are_not_measured() {
        let fx = Fixture::new("nosig");
        let empty = "// only a comment, no item\n".repeat(400);
        std::fs::write(fx.0.join("src/empty.rs"), &empty).unwrap();
        std::fs::write(fx.0.join("README.md"), "# Title\n".repeat(2000)).unwrap();
        {
            let mut store = fx.store();
            store
                .replace_file("src/empty.rs", &content_oid(empty.as_bytes()), "rust")
                .unwrap();
            let readme = std::fs::read(fx.0.join("README.md")).unwrap();
            store
                .replace_file("README.md", &content_oid(&readme), "concept")
                .unwrap();
        }
        let report = collect(&fx.store(), &fx.0, 20).unwrap();
        assert_eq!(report.no_signatures, ["src/empty.rs"]);
        assert!(report.stale.is_empty());
        let paths: Vec<&str> = report.measured.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["src/big.rs", "src/small.rs"]);
    }

    /// A report that walked every candidate is complete; one that stopped
    /// at `--top` with files left is capped, and says how many it saw.
    #[test]
    fn collect_records_the_pool_it_drew_from() {
        let fx = Fixture::new("pool");
        let store = fx.store();
        let whole = collect(&store, &fx.0, 20).unwrap();
        assert_eq!((whole.candidates, whole.examined, whole.top), (2, 2, 20));
        assert!(!whole.capped());
        let signature = store.meta_get(FRESHNESS_KEY).unwrap();
        assert!(
            signature.is_some(),
            "build_graph stores a freshness signature"
        );
        assert_eq!(whole.graph_signature, signature);

        let cut = collect(&store, &fx.0, 1).unwrap();
        assert_eq!((cut.candidates, cut.examined), (2, 1));
        assert!(cut.capped());
    }

    /// The exact edge: `--top` equal to the pool measures everything and is
    /// not capped, although the loop stops on `top`.
    #[test]
    fn a_top_equal_to_the_pool_is_complete() {
        let fx = Fixture::new("edge");
        let report = collect(&fx.store(), &fx.0, 2).unwrap();
        assert_eq!((report.candidates, report.examined), (2, 2));
        assert!(!report.capped());
    }

    /// A store that cannot answer is an error, not a file "with no
    /// signature": the audit fails instead of printing a wrong left-out line.
    #[test]
    fn a_symbol_query_failure_fails_the_audit() {
        let fx = Fixture::new("dberr");
        let store = fx.store();
        store.conn().execute_batch("DROP TABLE symbols").unwrap();
        let err = collect(&store, &fx.0, 20).unwrap_err();
        assert!(err.starts_with("audit: src/big.rs: "), "{err}");
    }

    /// A file grown past the graph's size cap is never loaded to be hashed:
    /// even with a row that matches its bytes, it is stale, as the graph
    /// would no longer index it.
    #[test]
    fn a_file_past_the_graph_size_cap_is_stale_unread() {
        let fx = Fixture::new("huge");
        let body = "pub fn huge() {}\n";
        let huge = body.repeat(4 * 1024 * 1024 / body.len() + 1);
        std::fs::write(fx.0.join("src/huge.rs"), &huge).unwrap();
        {
            let mut store = fx.store();
            let id = store
                .replace_file("src/huge.rs", &content_oid(huge.as_bytes()), "rust")
                .unwrap();
            store
                .insert_symbol(
                    id,
                    "src/huge.rs#huge#function",
                    "huge",
                    "huge",
                    pixel_graph::store::SymbolKind::Function,
                    1,
                    1,
                    "pub fn huge()",
                )
                .unwrap();
        }
        let report = collect(&fx.store(), &fx.0, 20).unwrap();
        assert_eq!(report.stale, ["src/huge.rs"]);
        let paths: Vec<&str> = report.measured.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["src/big.rs", "src/small.rs"]);
    }

    #[test]
    fn render_human_states_totals_median_and_what_it_left_out() {
        let report = Report {
            measured: vec![
                measured("src/a.rs", 4_000, 400),
                measured("src/b.rs", 2_000, 400),
                measured("src/c.rs", 400, 400),
            ],
            stale: vec!["src/old.rs".to_string()],
            no_signatures: vec!["src/gen.rs".to_string(), "src/raw.rs".to_string()],
            candidates: 7,
            examined: 6,
            top: 3,
            graph_signature: None,
        };
        let mut coverage = BTreeMap::new();
        coverage.insert(
            "rust".to_string(),
            coverage_cmd::Row {
                on_disk: 4,
                indexed: 3,
                symbols: 9,
            },
        );
        let text = render_human(&report, &coverage, Path::new("/repo"));
        for line in [
            "root: /repo",
            "   1000 tok      100 tok       -90%      3      10  src/a.rs",
            "    100 tok      100 tok  no saving      3      10  src/c.rs",
            "total, 3 of 7 indexed source files: full read 1600 tok, pixel answer 300 tok (-81%)",
            "per file: median 80% saved, from 0% to 90%",
            "left out: 1 changed since indexing (`pixel prepare-repo .` refreshes them)",
            "left out: 2 with no signature to outline (no definitions, or a grammar that missed them): src/gen.rs, src/raw.rs",
            "indexed: rust 3/4 (75.0%)",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in:\n{text}"
            );
        }
    }

    /// With nothing left out and nothing measured, the report says so and
    /// prints no left-out, total or coverage line.
    #[test]
    fn render_human_on_an_empty_report_prints_no_totals() {
        let text = render_human(&Report::default(), &BTreeMap::new(), Path::new("/repo"));
        assert!(
            text.contains("no indexed source file could be measured\n"),
            "{text}"
        );
        for absent in ["total,", "per file:", "left out:", "indexed:"] {
            assert!(!text.contains(absent), "{absent:?} in:\n{text}");
        }
    }

    #[test]
    fn render_json_carries_every_count() {
        let report = Report {
            measured: vec![
                measured("src/a.rs", 4_000, 400),
                measured("src/c.rs", 400, 400),
            ],
            stale: vec!["src/old.rs".to_string()],
            no_signatures: vec![],
            candidates: 5,
            examined: 3,
            top: 2,
            graph_signature: Some("abc123".to_string()),
        };
        let value = render_json(&report, &BTreeMap::new(), Path::new("/repo"));
        assert_eq!(value["marker"], "capped", "two files were never examined");
        assert_eq!(
            value["epistemics"],
            json!({
                "closed_world": false,
                "lower_bound": true,
                "basis": AUDIT_BASIS,
                "confidence": "capped",
            })
        );
        assert_eq!(
            value["snapshot"],
            json!({
                "graph_signature": "abc123",
                "indexed_source_files": 5,
                "examined": 3,
                "top": 2,
            })
        );
        assert_eq!(value["files"][0]["full_tokens"], 1000);
        assert_eq!(value["files"][0]["outline_tokens"], 100);
        assert_eq!(value["files"][0]["saved_pct"], 90);
        assert_eq!(value["files"][1]["saved_pct"], Value::Null);
        assert_eq!(
            value["totals"],
            json!({
                "files": 2,
                "full_tokens": 1100,
                "outline_tokens": 200,
                "saved_pct": 82,
                "median_saved_pct": 45,
            })
        );
        assert_eq!(value["left_out"]["stale"], json!(["src/old.rs"]));
    }

    #[test]
    fn report_for_without_a_graph_names_the_command_that_builds_it() {
        let dir = std::env::temp_dir().join(format!("px-audit-nograph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = report_for(&dir, 20, false).unwrap_err();
        assert!(
            err.contains("no code graph") && err.contains("pixel prepare-repo ."),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_for_prints_the_measured_rows() {
        let fx = Fixture::new("report");
        let text = report_for(&fx.0, 20, false).unwrap();
        assert!(
            text.contains("  src/big.rs\n") && text.contains("total, 2 of 2 indexed source files:"),
            "{text}"
        );
        let json: Value = serde_json::from_str(&report_for(&fx.0, 1, true).unwrap()).unwrap();
        assert_eq!(json["totals"]["files"], 1);
        assert_eq!(json["coverage"][0]["lang"], "rust");
        assert_eq!(json["coverage"][0]["indexed"], 2);
    }

    #[test]
    fn run_errors_on_a_missing_path() {
        let missing = std::env::temp_dir().join(format!("px-audit-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let err = run(AuditOptions {
            path: missing,
            top: AUDIT_DEFAULT_TOP,
            json: false,
        })
        .unwrap_err();
        assert!(err.starts_with("audit: "), "{err}");
    }
}
