// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded impact queries against an existing graph, without index maintenance.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pixel_graph::GraphStore;
use pixel_graph::build::{
    EXTRACTOR_VERSION, EXTRACTOR_VERSION_KEY, FRESHNESS_KEY, freshness_signature,
};
use pixel_graph::impact::{Direction, impact};
use serde_json::Value;

const QUERY_DEADLINE: Duration = Duration::from_millis(1500);
const MAX_RESULT_BYTES: usize = 32_768;
const MAX_DEPTH: u32 = 3;

/// Includes repository discovery, freshness verification and graph traversal in one deadline.
pub(crate) fn query(
    path: PathBuf,
    symbol: String,
    direction: Direction,
    depth: u32,
) -> Result<Value, String> {
    if !(1..=MAX_DEPTH).contains(&depth) {
        return Err("no-refresh impact requires depth 1 through 3".into());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read(&path, &symbol, direction, depth);
        let _ = tx.send(result);
    });
    // The command process exits after this error; an unavailable filesystem
    // cannot leave a background reader in the host agent process.
    rx.recv_timeout(QUERY_DEADLINE).map_err(|_| {
        "impact query unavailable within 1500 ms; continue with native tools".to_owned()
    })?
}

fn read(path: &Path, symbol: &str, direction: Direction, depth: u32) -> Result<Value, String> {
    let root = crate::discover_root(path)?;
    let database = root
        .join(pixel_index::index::SHARD_DIR)
        .join(pixel_daemon::api::GRAPH_DB_FILE);
    let store = GraphStore::open_read_only(&database).map_err(|error| error.to_string())?;
    store
        .conn()
        .busy_timeout(Duration::ZERO)
        .map_err(|error| error.to_string())?;
    // Metadata and symbols come from the same SQLite snapshot.
    store
        .conn()
        .execute_batch("BEGIN")
        .map_err(|error| error.to_string())?;
    let version = store
        .meta_get(EXTRACTOR_VERSION_KEY)
        .map_err(|error| error.to_string())?;
    if version.as_deref() != Some(EXTRACTOR_VERSION) {
        return Err(
            "graph extractor is unavailable or outdated; continue with native tools".into(),
        );
    }
    let signature = store
        .meta_get(FRESHNESS_KEY)
        .map_err(|error| error.to_string())?
        .ok_or("graph freshness is unknown; continue with native tools")?;
    if signature != freshness_signature(&root) {
        return Err("graph is stale; continue with native tools".into());
    }
    let target = if symbol.contains('#') {
        store
            .symbol_by_uid(symbol)
            .map_err(|error| error.to_string())?
            .ok_or("symbol is absent from the graph")?
    } else {
        let mut candidates = store
            .symbols_by_name(symbol, None, 2)
            .map_err(|error| error.to_string())?;
        if candidates.len() != 1 {
            return Err("symbol is missing or ambiguous; use a known uid or native tools".into());
        }
        candidates.remove(0)
    };
    let report = impact(
        &store,
        &target.uid,
        direction,
        depth,
        pixel_daemon::api::EDGE_LIMIT,
    )
    .map_err(|error| error.to_string())?;
    let mut data = serde_json::to_value(report).map_err(|error| error.to_string())?;
    data["epistemics"] = serde_json::to_value(pixel_proto::Epistemics {
        basis: "existing graph; source signature checked".into(),
        ..Default::default()
    })
    .map_err(|error| error.to_string())?;
    if serde_json::to_vec(&data)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_RESULT_BYTES
    {
        return Err(
            "impact result exceeds 32768 bytes; narrow the query or use native tools".into(),
        );
    }
    Ok(data)
}
