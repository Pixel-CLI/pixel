// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel coverage` — per-language graph coverage.
//!
//! Compares the files the index policy can see on disk (via
//! [`pixel_index::policy_walk`], the same walk the builders use) with the
//! files the graph actually indexed, per language. The gap is the answer to
//! "why didn't `impact`/`evaluate` see this file": an unsupported
//! extension, or a file the extractor skipped.
//!
//! Read-only and in-process: it opens `graph.db` directly instead of going
//! through the daemon, so it reports on the snapshot on disk and also works
//! when no daemon is running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pixel_daemon::api::GRAPH_DB_FILE;
use pixel_graph::extract::lang_of;
use pixel_graph::store::GraphStore;
use pixel_index::index::{SHARD_DIR, policy_walk};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct CoverageOptions {
    pub path: PathBuf,
    pub json: bool,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct Row {
    pub(crate) on_disk: u64,
    pub(crate) indexed: u64,
    pub(crate) symbols: u64,
}

/// Per-language counts merged from the disk walk and the graph snapshot.
/// `unrecognized` counts files whose extension maps to no language — they
/// can never be indexed, which is also coverage information.
pub(crate) fn collect(root: &Path) -> Result<(BTreeMap<String, Row>, u64, bool), String> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    let mut unrecognized = 0u64;
    for entry in policy_walk(root).flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        match lang_of(&rel.to_string_lossy()) {
            Some(lang) => rows.entry(lang.to_string()).or_default().on_disk += 1,
            None => unrecognized += 1,
        }
    }

    let db = root.join(SHARD_DIR).join(GRAPH_DB_FILE);
    let graph_present = db.exists();
    if graph_present {
        let store = GraphStore::open(&db).map_err(|e| format!("coverage: {e}"))?;
        for file in store.files().map_err(|e| format!("coverage: {e}"))? {
            rows.entry(file.lang).or_default().indexed += 1;
        }
        for (lang, count) in store
            .symbols_by_lang()
            .map_err(|e| format!("coverage: {e}"))?
        {
            rows.entry(lang).or_default().symbols = count;
        }
    }
    Ok((rows, unrecognized, graph_present))
}

pub fn run(opts: CoverageOptions) -> Result<(), String> {
    let root = opts
        .path
        .canonicalize()
        .map_err(|e| format!("coverage: {}: {e}", opts.path.display()))?;
    let (rows, unrecognized, graph_present) = collect(&root)?;
    let (disk_total, indexed_total): (u64, u64) = (
        rows.values().map(|r| r.on_disk).sum(),
        rows.values().map(|r| r.indexed).sum(),
    );
    if opts.json {
        let languages: Vec<_> = rows
            .iter()
            .map(|(lang, r)| {
                json!({
                    "lang": lang,
                    "on_disk": r.on_disk,
                    "indexed": r.indexed,
                    "coverage_pct": pct(r.indexed, r.on_disk),
                    "symbols": r.symbols,
                })
            })
            .collect();
        let out = json!({
            "root": root.display().to_string(),
            "graph_present": graph_present,
            "languages": languages,
            "unrecognized_files": unrecognized,
            "totals": {
                "on_disk": disk_total,
                "indexed": indexed_total,
                "coverage_pct": pct(indexed_total, disk_total),
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    print!(
        "{}",
        render_human(&rows, unrecognized, graph_present, &root)
    );
    Ok(())
}

/// The plain-text table, as a pure string so the conditional lines are
/// assertable — the `unrecognized` footnote and the no-graph note are the
/// parts that change what the user does next.
fn render_human(
    rows: &BTreeMap<String, Row>,
    unrecognized: u64,
    graph_present: bool,
    root: &Path,
) -> String {
    if rows.is_empty() {
        return format!("no recognized source files under {}\n", root.display());
    }
    let (disk_total, indexed_total): (u64, u64) = (
        rows.values().map(|r| r.on_disk).sum(),
        rows.values().map(|r| r.indexed).sum(),
    );
    let mut out = format!(
        "{:<10} {:>8} {:>8} {:>9} {:>8}\n",
        "language", "on-disk", "indexed", "coverage", "symbols"
    );
    for (lang, r) in rows {
        out.push_str(&format!(
            "{:<10} {:>8} {:>8} {:>8.1}% {:>8}\n",
            lang,
            r.on_disk,
            r.indexed,
            pct(r.indexed, r.on_disk),
            r.symbols
        ));
    }
    out.push_str(&format!(
        "{:<10} {:>8} {:>8} {:>8.1}%\n",
        "total",
        disk_total,
        indexed_total,
        pct(indexed_total, disk_total)
    ));
    if unrecognized > 0 {
        out.push_str(&format!(
            "{unrecognized} file(s) with unrecognized extensions (never indexed)\n"
        ));
    }
    if !graph_present {
        out.push_str("note: no graph.db — indexed counts are zero; run `pixel build-index`\n");
    }
    out
}

pub(crate) fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_is_zero_safe() {
        assert_eq!(pct(0, 0), 0.0);
        assert_eq!(pct(1, 4), 25.0);
        assert_eq!(pct(4, 4), 100.0);
    }

    #[test]
    fn collect_counts_recognized_and_unrecognized() {
        let dir = std::env::temp_dir().join(format!("px-cov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(dir.join("src/b.ts"), b"export const b = 1;\n").unwrap();
        std::fs::write(dir.join("README.md"), b"# x\n").unwrap();
        let (rows, unrecognized, graph_present) = collect(&dir).unwrap();
        assert_eq!(rows["rust"].on_disk, 1);
        assert_eq!(rows["ts"].on_disk, 1);
        assert_eq!(rows["rust"].indexed, 0);
        assert_eq!(unrecognized, 1);
        assert!(!graph_present);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The indexed side of the ratio: a graph.db naming files must move
    /// `indexed` — `+=` mutants that subtract or multiply leave it at zero.
    #[test]
    fn collect_reads_indexed_counts_from_the_graph() {
        let dir = std::env::temp_dir().join(format!("px-cov-graph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shard = dir.join(SHARD_DIR);
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), b"fn b() {}\n").unwrap();
        {
            let mut store = GraphStore::open(&shard.join(GRAPH_DB_FILE)).unwrap();
            store.replace_file("a.rs", "blob-a", "rust").unwrap();
        }
        let (rows, _, graph_present) = collect(&dir).unwrap();
        assert!(graph_present);
        assert_eq!(rows["rust"].on_disk, 2);
        assert_eq!(rows["rust"].indexed, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `run` on a path that does not exist must fail — a body replaced by
    /// `Ok(())` would swallow that error.
    #[test]
    fn run_errors_on_a_missing_path() {
        let missing = std::env::temp_dir().join(format!("px-cov-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let err = run(CoverageOptions {
            path: missing,
            json: false,
        })
        .unwrap_err();
        assert!(err.contains("coverage"), "{err}");
    }

    #[test]
    fn render_human_names_unrecognized_files_and_a_missing_graph() {
        let mut rows = BTreeMap::new();
        rows.insert(
            "rust".to_string(),
            Row {
                on_disk: 2,
                indexed: 1,
                symbols: 3,
            },
        );
        let root = Path::new("/repo");
        let out = render_human(&rows, 4, false, root);
        assert!(out.contains("rust"));
        assert!(out.contains("50.0%"));
        assert!(
            out.contains("4 file(s) with unrecognized extensions"),
            "{out}"
        );
        assert!(out.contains("no graph.db"), "{out}");
        // Neither footnote when both conditions are absent.
        let clean = render_human(&rows, 0, true, root);
        assert!(!clean.contains("unrecognized"), "{clean}");
        assert!(!clean.contains("no graph.db"), "{clean}");
        // Empty input gets its own line, not a table header.
        assert!(render_human(&BTreeMap::new(), 0, false, root).contains("no recognized"));
    }
}
