// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Per-language graph coverage, as `pixel audit` reports it (the standalone
//! `coverage` command is the `coverage` plugin now).
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
use std::path::Path;

use pixel_daemon::api::GRAPH_DB_FILE;
use pixel_graph::extract::lang_of;
use pixel_graph::store::GraphStore;
use pixel_index::index::{SHARD_DIR, policy_walk};

#[derive(Debug, Default)]
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
}
