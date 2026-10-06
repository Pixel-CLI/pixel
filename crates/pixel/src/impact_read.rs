// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded impact queries against an existing graph, without index maintenance.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pixel_graph::GraphStore;
use pixel_graph::build::{
    EXTRACTOR_VERSION, EXTRACTOR_VERSION_KEY, FRESHNESS_KEY, freshness_signature_trusting_stat,
};
use serde_json::Value;

const QUERY_DEADLINE: Duration = Duration::from_millis(1500);
const MAX_RESULT_BYTES: usize = 32_768;
const MAX_DEPTH: u32 = 3;

/// Includes repository discovery, freshness verification and graph traversal in one deadline.
/// `direction` is the daemon's spelling (`upstream`/`downstream`); `depth`
/// defaults to the daemon's [`pixel_daemon::api::IMPACT_DEFAULT_DEPTH`].
pub(crate) fn query(
    path: PathBuf,
    symbol: String,
    direction: &'static str,
    depth: Option<u32>,
) -> Result<Value, String> {
    let depth = depth.unwrap_or(pixel_daemon::api::IMPACT_DEFAULT_DEPTH);
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

fn read(path: &Path, symbol: &str, direction: &str, depth: u32) -> Result<Value, String> {
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
    // Only files edited since the last full build are read and hashed, so
    // the check fits the deadline on a large tree; its stat trust is
    // acceptable for a read the caller verifies against source.
    if signature != freshness_signature_trusting_stat(&root, &store).map_err(|e| e.to_string())? {
        return Err("graph is stale; continue with native tools".into());
    }
    let mut data = pixel_daemon::api::impact_on_graph(&store, symbol, direction, Some(depth))?;
    data["epistemics"] = serde_json::to_value(pixel_proto::Epistemics {
        basis: "existing graph; source signature checked (files older than the build by mtime)"
            .into(),
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
