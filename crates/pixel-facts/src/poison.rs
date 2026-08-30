//! `poison.rs` — structural fix for usable-git's ingest failure class.
//!
//! Decisions are made BEFORE git is spawned to produce diff text: (1) paths
//! passed as pathspec magic `:(exclude)…` so poison blobs are never emitted;
//! (2) skip rules recorded, never silent; (3) blob caps and content heuristics
//! on the first 4KB; (4) learned poison — a path that trips the cap joins
//! `poison_paths` and the exclude list forever.

use crate::store::{FactsStore, Result};

/// Blob cap either side (from `git cat-file --batch-check` in phase B).
pub const BLOB_CAP_BYTES: usize = 512 * 1024;
/// Per-file diff text cap kept.
pub const FILE_TEXT_CAP_BYTES: usize = 32 * 1024;
/// Per-commit diff text cap kept.
pub const COMMIT_TEXT_CAP_BYTES: usize = 256 * 1024;
/// First-4KB window examined by content heuristics.
pub const CONTENT_PROBE_BYTES: usize = 4096;

/// Paths whose diff text is machine noise (ported from usable-git + generated
/// segments). Recorded as skipped, never indexed.
pub fn skip_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    let name = segments.last().copied().unwrap_or(path);
    if segments.iter().any(|s| matches!(*s, "node_modules" | "vendor" | "dist")) {
        return true;
    }
    if matches!(
        name,
        "bun.lock"
            | "bun.lockb"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "Cargo.lock"
            | "composer.lock"
            | "Gemfile.lock"
            | "poetry.lock"
            | "uv.lock"
    ) {
        return true;
    }
    if is_minified_name(name) {
        return true;
    }
    if name.ends_with(".map") || name.ends_with(".snap") {
        return true;
    }
    if segments
        .iter()
        .any(|s| s.contains("generated") || s.contains("codegen") || s.contains("__generated__"))
    {
        return true;
    }
    if name.to_lowercase().ends_with(".svg") && file_size_heuristic_large_svg(name) {
        return true;
    }
    false
}

fn is_minified_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // foo.min.js / foo.min.css / foo.bundle.min.js etc.
    lower.rfind(".min.").is_some()
}

/// Big SVGs: heuristic on name — a `.svg` with a long basename is usually an
/// embedded/data-URI asset rather than source. (Real size check happens on the
/// blob cap in phase B.)
fn file_size_heuristic_large_svg(name: &str) -> bool {
    name.len() > 80 && name.to_ascii_lowercase().ends_with(".svg")
}

/// A content probe result on the first 4KB of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Binary,
    Minified,
    Generated,
    Text,
}

/// Content heuristics on the first `CONTENT_PROBE_BYTES` of a blob:
/// NUL → binary; mean line >400 or single line >2000 → minified;
/// >30% non-ASCII in json/js → generated.
pub fn classify_content(probe: &[u8], path: &str) -> ContentKind {
    if probe.contains(&0) {
        return ContentKind::Binary;
    }
    if probe.is_empty() {
        return ContentKind::Text;
    }
    let lower = path.to_ascii_lowercase();
    // Line-length heuristics over the probe window.
    let text = match std::str::from_utf8(probe) {
        Ok(t) => t,
        Err(_) => return ContentKind::Binary,
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let nonempty: Vec<&str> = lines.iter().copied().filter(|l| !l.is_empty()).collect();
    let mut total_len = 0usize;
    let mut max_line = 0usize;
    for l in &nonempty {
        let n = l.len();
        total_len += n;
        if n > max_line {
            max_line = n;
        }
    }
    if !nonempty.is_empty() {
        let mean = total_len / nonempty.len();
        if mean > 400 || max_line > 2000 {
            return ContentKind::Minified;
        }
    }
    // Generated JSON/JS: heavy non-ASCII (escaped unicode, emoji, glyph data).
    if lower.ends_with(".json") || lower.ends_with(".js") || lower.ends_with(".mjs") {
        let non_ascii = probe.iter().filter(|b| **b >= 0x80).count();
        let ratio = non_ascii as f64 / probe.len() as f64;
        if ratio > 0.30 {
            return ContentKind::Generated;
        }
    }
    ContentKind::Text
}

/// The exclude pathspec magic emitted for a path. Uses the `:(exclude)` form so
/// git never even emits the matching blob during phase C.
pub fn exclude_pathspec(path: &str) -> String {
    format!(":(exclude){}", path)
}

/// A learned poison path (one that tripped the blob cap in phase B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoisonPath {
    pub path: String,
    pub reason: String,
}

impl FactsStore {
    /// Load all learned poison paths from `poison_paths`.
    pub fn poison_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare("SELECT path FROM poison_paths")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Learn a new poison path (idempotent by path). Once learned it is
    /// excluded from diff ingest forever.
    pub fn learn_poison(&mut self, path: &str, reason: &str) -> Result<()> {
        let now = now_iso();
        self.conn().execute(
            "INSERT INTO poison_paths (path, reason, learned_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (path) DO NOTHING",
            rusqlite::params![path, reason, now],
        )?;
        Ok(())
    }

    /// The set of skip notes that apply to a path (structural + learned).
    /// Returns a human-readable reason, or None if the path should be indexed.
    pub fn skip_reason(&self, path: &str) -> Option<String> {
        if skip_path(path) {
            return Some("structural-skip".to_string());
        }
        let learned = self
            .conn
            .query_row(
                "SELECT reason FROM poison_paths WHERE path = ?1",
                [path],
                |r| r.get::<_, String>(0),
            )
            .ok();
        learned
    }
}

/// A batch-level decision: how a batch of changed paths should be requested.
#[derive(Debug, Clone)]
pub struct SkipPlan {
    /// Paths to exclude via pathspec magic (structural + learned poison).
    pub excludes: Vec<String>,
    /// Map of path -> reason for every path skipped (recorded, never silent).
    pub skipped: Vec<(String, String)>,
}

/// Decide skips BEFORE spawning git. Consumes `touched` (the changed paths in
/// this commit/batch) and returns the exclusion list plus the skip ledger.
pub fn decide_skips(store: &FactsStore, touched: &[String]) -> SkipPlan {
    let mut excludes = Vec::new();
    let mut skipped = Vec::new();
    for p in touched {
        match store.skip_reason(p) {
            Some(reason) => {
                excludes.push(exclude_pathspec(p));
                skipped.push((p.clone(), reason));
            }
            None => {}
        }
    }
    excludes.sort();
    SkipPlan { excludes, skipped }
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
