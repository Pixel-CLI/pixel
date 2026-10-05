// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! SQLite persistence — THE schema contract for the graph crate.
//!
//! `extract`/`resolve`/`build` write through this API; analyses read through
//! it (plus ad-hoc SQL via `conn()` when a bespoke join is clearer). WAL
//! mode; per-file replacement is transactional.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::extract::ImportBinding;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "graph store: {e}"),
        }
    }
}
impl std::error::Error for StoreError {}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The concept schema version stamped into a new graph db's `meta`
/// (`concepts_version`) and reported by `resolve`'s `index_state`. It is
/// written once and never compared, so bumping it forces nothing: a change to
/// the concept extractor's output bumps `build::EXTRACTOR_VERSION`, which
/// makes the next build re-extract every file.
pub const CONCEPTS_VERSION: &str = "2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    /// A member of an enum (`enum Command { SelfUpdate }`): a definition the
    /// ident tier can hit exactly, carrying `Enum::Variant` as its qualified
    /// name.
    Variant,
    Trait,
    Interface,
    Const,
    Module,
    /// Synthetic file scope used to own top-level calls in script languages.
    Script,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Variant => "variant",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Const => "const",
            SymbolKind::Module => "module",
            SymbolKind::Script => "script",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "variant" => SymbolKind::Variant,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "const" => SymbolKind::Const,
            "module" => SymbolKind::Module,
            "script" => SymbolKind::Script,
            _ => SymbolKind::Function,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Calls,
    Imports,
    Extends,
    Implements,
    HasMethod,
    /// A symbol passed as an argument to a call (e.g. `schema.plugin(fn)` or
    /// `emitter.on('e', handler)`). Weaker than `Calls`: it means "may be
    /// invoked", not "directly called".
    References,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::HasMethod => "has_method",
            EdgeKind::References => "references",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "imports" => EdgeKind::Imports,
            "extends" => EdgeKind::Extends,
            "implements" => EdgeKind::Implements,
            "has_method" => EdgeKind::HasMethod,
            "references" => EdgeKind::References,
            _ => EdgeKind::Calls,
        }
    }
}

/// Resolution confidence tier. Unresolved calls are NOT edges — they live in
/// `unresolved_calls` and surface through the epistemic envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// T0 same-file scope chain or T1 import-resolved.
    Exact,
    /// T2 unique-name within the import-connected component.
    Probable,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Probable => "probable",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "probable" {
            Tier::Probable
        } else {
            Tier::Exact
        }
    }
}

/// The honesty header every caller/impact answer carries: `lower_bound` is
/// true whenever same-name unresolved call sites exist, so an agent can
/// distinguish "0 callers" from "resolver gave up".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub lower_bound: bool,
    pub unresolved_same_name: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub blob_oid: String,
    pub lang: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRow {
    pub id: i64,
    /// Stable id: `path#qualified#kind`.
    pub uid: String,
    pub file_id: i64,
    pub name: String,
    pub qualified: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRow {
    pub id: i64,
    pub file_id: i64,
    pub spec: String,
    /// The path this row resolved (`RawImport::path`): `spec`, or one path of
    /// a Rust `use` naming several.
    pub path: String,
    /// Named bindings pulled from `spec` (decoded from the stored column).
    pub bindings: Vec<ImportBinding>,
    /// The repo file `spec` resolved to, when it did. `plan`'s prereq scan
    /// reads this to walk one import hop out from each plan target.
    pub resolved_file_id: Option<i64>,
}

/// The `imports.bindings` column: comma-separated bindings, each `name` when
/// imported under its own name and `local=source` under an alias. Neither
/// separator can occur in an identifier, and a plain `name` is what graphs
/// built before aliases were tracked stored, so they decode unchanged.
pub fn encode_bindings(bindings: &[ImportBinding]) -> String {
    bindings
        .iter()
        .map(|b| {
            if b.local == b.source {
                b.local.clone()
            } else {
                format!("{}={}", b.local, b.source)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The `imports.scope` column: comma-separated inclusive line ranges
/// `start-end`; empty when the bindings are in scope in the whole file.
pub fn encode_scope(scope: &[(u32, u32)]) -> String {
    scope
        .iter()
        .map(|(start, end)| format!("{start}-{end}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The inverse of [`encode_scope`]; a malformed range is dropped.
pub fn decode_scope(column: &str) -> Vec<(u32, u32)> {
    column
        .split(',')
        .filter_map(|range| {
            let (start, end) = range.trim().split_once('-')?;
            Some((start.parse().ok()?, end.parse().ok()?))
        })
        .collect()
}

/// The inverse of [`encode_bindings`]; empty entries are dropped.
pub fn decode_bindings(csv: &str) -> Vec<ImportBinding> {
    csv.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((local, source)) => ImportBinding::aliased(source, local),
            None => ImportBinding::named(entry),
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedRow {
    pub file_id: i64,
    pub site_line: u32,
    /// `"calls"` or `"references"`.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRow {
    pub src_id: i64,
    pub dst_id: i64,
    pub kind: EdgeKind,
    pub tier: Tier,
    pub site_line: u32,
    /// Receiver expression at the call site (e.g. `x` in `x.parse()`), if any.
    /// Preserved across incremental demote/re-resolve so receiver calls are
    /// never falsely promoted from Probable to Exact.
    pub receiver: Option<String>,
    /// The name the site wrote (`edges.callee`), when the resolver recorded
    /// it. It differs from the target's own name under an import alias
    /// (`leased()` → `push`), and re-resolving the site after an incremental
    /// update must replay it: the target's name is not in scope there.
    /// `None` reads as the target's name.
    pub callee: Option<String>,
}

/// One unique symbol selected for a bounded caller-example query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerTarget {
    pub name: String,
    pub qualified: String,
    pub path: String,
    pub blob_oid: String,
}

/// One indexed call-site example with metadata for bounded freshness checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerExample {
    pub caller_name: String,
    pub caller_qualified: String,
    pub path: String,
    pub site_line: u32,
    pub tier: String,
    pub blob_oid: String,
}

/// Up to three indexed callers for one unique symbol; this is not exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerExamples {
    pub target: CallerTarget,
    pub callers: Vec<CallerExample>,
    pub truncated: bool,
}

/// One human annotation row. Annotations are HUMAN-OWNED: they are keyed by
/// stable identity (`file_path` + symbol `name` or concept `norm`) and are
/// NOT deleted by `replace_file`/`remove_file`, so they survive re-indexes
/// and rebuilds. They are merged into symbol/`resolve`/`targets` results via
/// [`GraphStore::annotations_for_symbols`] and [`GraphStore::annotations_for_norms`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationRow {
    pub id: i64,
    /// Path of the file the annotation is attached to.
    pub file_path: String,
    /// Stable target identity inside that file: a symbol `name` OR a concept
    /// `norm`. Merge paths match on symbols.name and concepts.norm.
    pub target: String,
    /// Free-form human note. Survives rebuild untouched.
    pub note: String,
    /// Unix timestamp (seconds) of the last write.
    pub updated_at: i64,
}

/// One stored concept row (Engine 1). `owner_symbol_id` links the concept to
/// the smallest enclosing symbol when one exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConceptRow {
    pub id: i64,
    pub file_id: i64,
    pub kind: crate::concept::ConceptKind,
    pub raw: String,
    pub norm: String,
    pub detail: String,
    pub start_line: u32,
    pub end_line: u32,
    pub owner_symbol_id: Option<i64>,
}

/// ONE crux line — the guard/branch, state-mutation, or early-bail line that
/// CARRIES the logic of a body, decoupled from the whole-body span. Each line
/// keeps a **fingerprint** (FNV-1a of its trimmed text) that is stable across
/// line shifts, so retrieval can anchor a crux even after the file moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CruxLine {
    /// 1-based line number within the body snippet (for human debugging only;
    /// the fingerprint is the stable retrieval key, NOT this number).
    pub line: u32,
    /// The trimmed source line text, as extracted.
    pub text: String,
    /// Stable anchor: FNV-1a of `text`. Content-derived, independent of line
    /// position — survives when the file is rearranged.
    pub fingerprint: u64,
}

/// One extracted JSX element for plan queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsxElementRow {
    pub id: i64,
    pub file_id: i64,
    pub tag: String,
    pub has_handler: bool,
    pub text_content: String,
    pub start_line: u32,
    pub end_line: u32,
}

// ---------------------------------------------------------------------------
// Deterministic crux extraction (P2·3)
// ---------------------------------------------------------------------------
// The graft's crux lines are LLM-produced; this is the deterministic analogue
// that pixel invents here. It scores a symbol body line by three interpretable
// signals — guard/branch, state mutation, early bail — and keeps the lines
// whose score clears a threshold, in original order, each with a stable FNV-1a
// fingerprint. Deterministic: same input -> same output, no RNG / no LLM.

/// FNV-1a 64-bit hash of a byte slice. Stable across runs and platforms.
pub fn fnv1a64(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in text.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Score a single source line (already trimmed). Returns the weighted crux
/// score and the human-readable reasons that earned it. Delimiter/comment
/// lines score 0 so they never surface as crux.
fn score_crux_line(line: &str) -> (i64, Vec<&'static str>) {
    let t = line.trim();
    if t.is_empty() {
        return (0, vec![]);
    }
    // Pure structural delimiters carry no logic.
    if t.chars().all(|c| c == '{' || c == '}') {
        return (0, vec![]);
    }
    // Whole-line comments and doc-comments carry no logic either.
    if t.starts_with("//") || t.starts_with("#") || t.starts_with("\"") {
        return (0, vec![]);
    }
    let mut score = 0i64;
    let mut reasons = Vec::new();
    // (a) guard / branch condition — control-flow that routes the logic.
    let guard_words = [
        "if ", "else if", "while ", "for ", "match ", "catch", "when ", "guard", "assert", "check",
        "ensure", "validate", "case ", "switch",
    ];
    for w in guard_words {
        if t.contains(w) {
            score += 3;
            reasons.push("guard");
            break;
        }
    }
    // early-return / bail — leaves the body before the fall-through path.
    let bail_pat = [
        "return ",
        "break;",
        "continue;",
        "throw ",
        "panic!",
        "unwrap",
        "expect",
        "abort",
        "exit(",
    ];
    for b in bail_pat {
        if t.contains(b) {
            score += 3;
            reasons.push("bail");
            break;
        }
    }
    // `?` early-error propagation is also a bail.
    if t.ends_with("?") {
        score += 3;
        reasons.push("bail");
    }
    // (b) state mutation — assignment / return / push that writes observable state.
    //     Mutations are one of the three crux categories (guards, mutations,
    //     early-returns) per P2·3, so a bare mutation line clears the default
    //     threshold (3) on its own — like a lone guard or lone early-return.
    let mut_pat = [
        "=", "return ", "+=", "-=", "*=", "/=", "push", "insert", "remove", "set", "append",
    ];
    for m in mut_pat {
        if t.contains(m) {
            score += 3;
            reasons.push("mutation");
            break;
        }
    }
    (score, reasons)
}

/// Deterministically extract the logic-bearing lines from a body snippet.
/// Keeps every line whose crux score clears `threshold` (default 3), in
/// original order, each tagged with a stable FNV-1a fingerprint. Same input
/// always yields the same output (no RNG, no LLM).
pub fn extract_crux(body: &str, threshold: i64) -> Vec<CruxLine> {
    body.lines()
        .enumerate()
        .filter_map(|(idx, raw)| {
            let trimmed = raw.trim();
            let (score, _) = score_crux_line(trimmed);
            if score >= threshold {
                Some(CruxLine {
                    line: (idx + 1) as u32,
                    text: trimmed.to_string(),
                    fingerprint: fnv1a64(trimmed),
                })
            } else {
                None
            }
        })
        .collect()
}

pub struct GraphStore {
    conn: Connection,
}

/// The per-file delete [`GraphStore::replace_file`] and
/// [`GraphStore::remove_file`] run once for every file of an incremental
/// batch; `idx_unresolved_file` is what keeps it off a full-table scan.
const DELETE_FILE_UNRESOLVED_CALLS: &str = "DELETE FROM unresolved_calls WHERE file_id = ?1";

/// Statements the write path keeps prepared: a cold build runs the same few
/// dozen once per file, symbol, import and edge, so the connection's cache
/// holds all of them (rusqlite keeps 16 by default).
const STATEMENT_CACHE: usize = 64;
/// One extra row marks that caller examples were truncated after three.
const CALLER_QUERY_CAP: i64 = 4;
const CALLER_EXAMPLE_CAP: usize = 3;

/// `execute` through the connection's statement cache. The build loops run
/// the same statements hundreds of thousands of times; `Connection::execute`
/// parses and plans the SQL again on every call.
pub(crate) trait ExecCached {
    fn exec_cached<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize>;
}

impl ExecCached for Connection {
    fn exec_cached<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize> {
        Ok(self.prepare_cached(sql)?.execute(params)?)
    }
}

/// The path SQLite opens with `SQLITE_OPEN_NOFOLLOW`, which refuses a link
/// at any component. Links above the db's directory are resolved, so a
/// legitimate prefix such as macOS's `/var` -> `/private/var` still opens;
/// the directory itself (`.pixel`) and the file keep the refusal, so a
/// `.pixel` or a db committed as a link is never opened through.
fn nofollow_path(parent: &Path, name: &std::ffi::OsStr) -> std::path::PathBuf {
    match (parent.parent(), parent.file_name()) {
        (Some(grandparent), Some(dir)) => grandparent
            .canonicalize()
            .map_or_else(|_| parent.join(name), |above| above.join(dir).join(name)),
        _ => parent.join(name),
    }
}

/// One `imports` table row — shared by [`GraphStore::imports_to_file`] and
/// [`GraphStore::imports_from`] so both query directions read the same shape.
fn import_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRow> {
    Ok(ImportRow {
        id: r.get(0)?,
        file_id: r.get(1)?,
        spec: r.get(2)?,
        path: r.get(3)?,
        bindings: decode_bindings(&r.get::<_, String>(4)?),
        resolved_file_id: r.get(5)?,
    })
}

impl GraphStore {
    pub fn open(path: &Path) -> Result<Self> {
        let path = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                if std::fs::symlink_metadata(parent).is_err() {
                    let _ = std::fs::create_dir_all(parent);
                }
                nofollow_path(parent, name)
            }
            _ => path.to_path_buf(),
        };
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // WAL serializes writers: a second writer (the daemon's watcher
        // update behind a concurrent CLI rebuild) must wait for the first
        // one to commit instead of failing with SQLITE_BUSY ("database is
        // locked"). rusqlite already sets 5000 ms when it opens the
        // connection; pinning the same budget pixel-facts uses keeps it
        // explicit and independent of that default.
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an existing graph without migrations or write privileges.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_read_only_with_timeout(path, Duration::from_secs(1))
    }

    /// Opens a graph read-only and reports SQLite locks without waiting.
    pub fn open_read_only_nonblocking(path: &Path) -> Result<Self> {
        Self::open_read_only_with_timeout(path, Duration::ZERO)
    }

    fn open_read_only_with_timeout(path: &Path, timeout: Duration) -> Result<Self> {
        let path = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => nofollow_path(parent, name),
            _ => path.to_path_buf(),
        };
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(timeout)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn })
    }

    /// Reads at most three caller locations for a uniquely named symbol.
    ///
    /// Ambiguous and missing names return `None`. A fourth row marks the
    /// examples as truncated; callers must not present them as exhaustive.
    /// Both queries share one read transaction so concurrent graph updates
    /// cannot change the target between resolution and edge lookup.
    pub fn caller_examples_by_name(&self, name: &str) -> Result<Option<CallerExamples>> {
        let tx = self.conn.unchecked_transaction()?;
        let (target_id, target) = {
            let mut target_stmt = tx.prepare(
                "SELECT s.id, s.name, s.qualified, f.path, f.blob_oid
                   FROM symbols AS s JOIN files AS f ON f.id = s.file_id
                  WHERE s.name = ?1 LIMIT 2",
            )?;
            let mut targets = target_stmt.query_map(params![name], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    CallerTarget {
                        name: row.get(1)?,
                        qualified: row.get(2)?,
                        path: row.get(3)?,
                        blob_oid: row.get(4)?,
                    },
                ))
            })?;
            let Some((target_id, target)) = targets.next().transpose()? else {
                return Ok(None);
            };
            if targets.next().transpose()?.is_some() {
                return Ok(None);
            }
            (target_id, target)
        };

        let (callers, truncated) = {
            let mut caller_stmt = tx.prepare(
                "SELECT caller.name, caller.qualified, f.path, e.site_line, e.tier, f.blob_oid
                   FROM edges AS e
                   JOIN symbols AS caller ON caller.id = e.src_id
                   JOIN files AS f ON f.id = caller.file_id
                  WHERE e.dst_id = ?1 AND e.kind = 'calls' AND e.site_line > 0
                  LIMIT ?2",
            )?;
            let rows = caller_stmt.query_map(params![target_id, CALLER_QUERY_CAP], |row| {
                Ok(CallerExample {
                    caller_name: row.get(0)?,
                    caller_qualified: row.get(1)?,
                    path: row.get(2)?,
                    site_line: row.get(3)?,
                    tier: row.get(4)?,
                    blob_oid: row.get(5)?,
                })
            })?;
            let mut callers = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            let truncated = callers.len() > CALLER_EXAMPLE_CAP;
            callers.truncate(CALLER_EXAMPLE_CAP);
            (callers, truncated)
        };
        tx.commit()?;

        Ok(Some(CallerExamples {
            target,
            callers,
            truncated,
        }))
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Escape hatch for analyses needing bespoke SQL.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Open one write transaction on this connection (`BEGIN IMMEDIATE`, so
    /// a concurrent writer waits at the start instead of failing at commit).
    /// The write methods below open savepoints, which nest inside it; with no
    /// enclosing transaction a savepoint is its own transaction, so callers
    /// that never call this keep their per-call atomicity. An error before
    /// [`Self::commit_write`] rolls back when the connection drops: nothing
    /// written inside is visible to another connection until the commit.
    pub fn begin_write(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Commit the transaction opened by [`Self::begin_write`]. Everything
    /// written since becomes visible to other connections at once.
    pub fn commit_write(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    // --- write path (extract/resolve/build own these) ---

    /// Insert-or-replace a file row; deletes the file's symbols, imports,
    /// outgoing edges and unresolved calls (incoming edges from other files
    /// are the caller's responsibility to re-resolve). Returns file id.
    pub fn replace_file(&mut self, path: &str, blob_oid: &str, lang: &str) -> Result<i64> {
        let tx = self.conn.savepoint()?;
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .optional()?;
        let id = if let Some(id) = existing {
            tx.exec_cached(
                "DELETE FROM edges WHERE src_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached(
                "DELETE FROM edges WHERE dst_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached(
                "DELETE FROM symbol_crux WHERE symbol_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached("DELETE FROM symbols WHERE file_id = ?1", params![id])?;
            tx.exec_cached("DELETE FROM imports WHERE file_id = ?1", params![id])?;
            tx.exec_cached(DELETE_FILE_UNRESOLVED_CALLS, params![id])?;
            tx.exec_cached(
                "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached("DELETE FROM concepts WHERE file_id = ?1", params![id])?;
            tx.exec_cached("DELETE FROM jsx_elements WHERE file_id = ?1", params![id])?;
            tx.exec_cached(
                "UPDATE files SET blob_oid = ?2, lang = ?3 WHERE id = ?1",
                params![id, blob_oid, lang],
            )?;
            id
        } else {
            tx.exec_cached(
                "INSERT INTO files (path, blob_oid, lang) VALUES (?1, ?2, ?3)",
                params![path, blob_oid, lang],
            )?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<()> {
        let tx = self.conn.savepoint()?;
        if let Some(id) = tx
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            tx.exec_cached(
                "DELETE FROM edges WHERE src_id IN (SELECT id FROM symbols WHERE file_id = ?1)
                   OR dst_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached(
                "DELETE FROM symbol_crux WHERE symbol_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached("DELETE FROM symbols WHERE file_id = ?1", params![id])?;
            tx.exec_cached("DELETE FROM imports WHERE file_id = ?1", params![id])?;
            tx.exec_cached(DELETE_FILE_UNRESOLVED_CALLS, params![id])?;
            tx.exec_cached(
                "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
                params![id],
            )?;
            tx.exec_cached("DELETE FROM concepts WHERE file_id = ?1", params![id])?;
            tx.exec_cached("DELETE FROM jsx_elements WHERE file_id = ?1", params![id])?;
            tx.exec_cached("DELETE FROM files WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record that `symbol_id` implements a trait method: it is called
    /// through the trait, never by name, so dead-code findings skip it.
    pub fn mark_trait_impl(&self, symbol_id: i64) -> Result<()> {
        self.conn.exec_cached(
            "UPDATE symbols SET trait_impl = 1 WHERE id = ?1",
            params![symbol_id],
        )?;
        Ok(())
    }

    /// Record that `symbol_id` is an external module declaration (`mod foo;`):
    /// it names another file rather than defining code in this one, so
    /// `targets::symbol_hits` must not grant its file the exact-name bonus.
    pub fn mark_module_decl(&self, symbol_id: i64) -> Result<()> {
        self.conn.exec_cached(
            "UPDATE symbols SET module_decl = 1 WHERE id = ?1",
            params![symbol_id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_symbol(
        &self,
        file_id: i64,
        uid: &str,
        name: &str,
        qualified: &str,
        kind: SymbolKind,
        start_line: u32,
        end_line: u32,
        sig: &str,
    ) -> Result<i64> {
        self.conn.exec_cached(
            "INSERT OR REPLACE INTO symbols
               (uid, file_id, name, qualified, kind, start_line, end_line, sig)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                uid,
                file_id,
                name,
                qualified,
                kind.as_str(),
                start_line,
                end_line,
                sig
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // --- crux lines (P2·3: guarded logical lines, not a fixed span) ---
    //
    // A body's crux is the set of lines that actually CARRY the logic
    // (guards/branches, mutations, early bails), extracted deterministically
    // by [`extract_crux`] and stored here anchored to the symbol id, each
    // tagged with a stable content fingerprint. Retrieval (`symbol_crux_by_*`)
    // returns them so the excerpt stays correct even when the file moves.

    /// Replace the crux lines for one symbol. Old crux for the symbol is
    /// dropped first; the new set is inserted in one statement batch.
    pub fn set_symbol_crux(&self, symbol_id: i64, crux: &[CruxLine]) -> Result<()> {
        self.conn.exec_cached(
            "DELETE FROM symbol_crux WHERE symbol_id = ?1",
            params![symbol_id],
        )?;
        for c in crux {
            self.conn.exec_cached(
                "INSERT INTO symbol_crux (symbol_id, line, text, fingerprint) VALUES (?1, ?2, ?3, ?4)",
                // Store the 64-bit FNV hash as its signed bit pattern so
                // round-trips survive beyond i64::MAX on 32-bit rlims.
                params![symbol_id, c.line, c.text, c.fingerprint as i64],
            )?;
        }
        Ok(())
    }

    /// All crux lines of a symbol, in stored (source) order. `None` when the
    /// symbol has no crux rows (e.g. never indexed with crux extraction).
    pub fn symbol_crux_by_id(&self, symbol_id: i64) -> Result<Vec<CruxLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT line, text, fingerprint FROM symbol_crux WHERE symbol_id = ?1 ORDER BY line",
        )?;
        let rows = stmt.query_map(params![symbol_id], |r| {
            Ok(CruxLine {
                line: r.get::<_, i64>(0)? as u32,
                text: r.get::<_, String>(1)?,
                fingerprint: r.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Crux line(s) for a symbol whose fingerprint matches `fingerprint` — the
    /// stable anchor for retrieval. Returns every crux line of the symbol that
    /// carries that fingerprint (in practice 0..=1).
    pub fn symbol_crux_by_fingerprint(
        &self,
        symbol_id: i64,
        fingerprint: u64,
    ) -> Result<Vec<CruxLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT line, text, fingerprint FROM symbol_crux WHERE symbol_id = ?1 AND fingerprint = ?2 ORDER BY line",
        )?;
        let rows = stmt.query_map(params![symbol_id, fingerprint as i64], |r| {
            Ok(CruxLine {
                line: r.get::<_, i64>(0)? as u32,
                text: r.get::<_, String>(1)?,
                fingerprint: r.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn insert_import(
        &self,
        file_id: i64,
        spec: &str,
        resolved_file_id: Option<i64>,
        bindings: &[ImportBinding],
    ) -> Result<()> {
        self.insert_import_at(file_id, spec, spec, resolved_file_id, bindings, &[])
    }

    /// An import row whose resolved `path` is not the statement's `spec`
    /// (one path of a Rust `use` naming several, `RawImport::path`), or whose
    /// bindings are in scope on some lines only (`RawImport::scope`).
    pub fn insert_import_at(
        &self,
        file_id: i64,
        spec: &str,
        path: &str,
        resolved_file_id: Option<i64>,
        bindings: &[ImportBinding],
        scope: &[(u32, u32)],
    ) -> Result<()> {
        let bindings_csv = encode_bindings(bindings);
        self.conn.exec_cached(
            "INSERT INTO imports (file_id, spec, path, resolved_file_id, bindings, scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                spec,
                path,
                resolved_file_id,
                bindings_csv,
                encode_scope(scope)
            ],
        )?;
        Ok(())
    }

    pub fn insert_edge(&self, e: &EdgeRow) -> Result<()> {
        self.conn.exec_cached(
            "INSERT INTO edges (src_id, dst_id, kind, tier, site_line, receiver, callee)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.src_id,
                e.dst_id,
                e.kind.as_str(),
                e.tier.as_str(),
                e.site_line,
                e.receiver,
                e.callee
            ],
        )?;
        Ok(())
    }

    /// Insert an unresolved call or reference. `kind` is `"calls"` (default)
    /// or `"references"` — the latter prevents `resolve_all` from
    /// resurrecting a passed-argument reference as a `Calls` edge.
    pub fn insert_unresolved_call(
        &self,
        file_id: i64,
        name: &str,
        enclosing_symbol_id: Option<i64>,
        site_line: u32,
        receiver: Option<&str>,
        kind: &str,
    ) -> Result<()> {
        self.conn.exec_cached(
            "INSERT INTO unresolved_calls (file_id, name, enclosing_symbol_id, site_line, receiver, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![file_id, name, enclosing_symbol_id, site_line, receiver, kind],
        )?;
        Ok(())
    }

    pub fn insert_jsx_element(
        &self,
        file_id: i64,
        tag: &str,
        has_handler: bool,
        text_content: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<i64> {
        self.conn.exec_cached(
            "INSERT INTO jsx_elements (file_id, tag, has_handler, text_content, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                tag,
                has_handler,
                text_content,
                start_line,
                end_line,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // --- concept write path (Engine 1) ---

    /// Delete every concept row (and its inverted words) for a file. Called
    /// inside the same transaction as symbol refresh.
    pub fn delete_concepts_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.exec_cached(
            "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
            params![file_id],
        )?;
        self.conn
            .exec_cached("DELETE FROM concepts WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// Insert one concept row plus its inverted words. `owner_symbol_id` is
    /// the smallest enclosing symbol's id when one exists, else `None`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_concept(
        &self,
        file_id: i64,
        kind: crate::concept::ConceptKind,
        raw: &str,
        norm: &str,
        detail: &str,
        start_line: u32,
        end_line: u32,
        owner_symbol_id: Option<i64>,
    ) -> Result<i64> {
        self.conn.exec_cached(
            "INSERT INTO concepts
               (file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                file_id,
                kind.as_str(),
                raw,
                norm,
                detail,
                start_line,
                end_line,
                owner_symbol_id
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        for word in crate::concept::concept_words(norm) {
            self.conn.exec_cached(
                "INSERT OR IGNORE INTO concept_words (word, concept_id) VALUES (?1, ?2)",
                params![word, id],
            )?;
        }
        Ok(id)
    }

    /// Replace a file's concepts in one transaction (used by the concepts-only
    /// refresh path for non-graph files).
    pub fn replace_concepts(
        &mut self,
        file_id: i64,
        concepts: &[crate::concept::RawConcept],
    ) -> Result<()> {
        let tx = self.conn.savepoint()?;
        tx.exec_cached(
            "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
            params![file_id],
        )?;
        tx.exec_cached("DELETE FROM concepts WHERE file_id = ?1", params![file_id])?;
        for c in concepts {
            tx.exec_cached(
                "INSERT INTO concepts
                   (file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    file_id,
                    c.kind.as_str(),
                    c.raw,
                    c.norm,
                    c.detail,
                    c.start_line,
                    c.end_line,
                    c.owner_symbol_id
                ],
            )?;
            let id = tx.last_insert_rowid();
            for word in crate::concept::concept_words(&c.norm) {
                tx.exec_cached(
                    "INSERT OR IGNORE INTO concept_words (word, concept_id) VALUES (?1, ?2)",
                    params![word, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // --- concept read path (Engine 1) ---

    /// T0 exact-norm probe: all concepts whose normalized form equals `norm`.
    pub fn concepts_by_norm(&self, norm: &str, limit: u32) -> Result<Vec<ConceptRow>> {
        let sql = format!(
            "SELECT {} FROM concepts WHERE norm = ?1 ORDER BY kind, id LIMIT ?2",
            Self::CONCEPT_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![norm, limit], Self::row_to_concept)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// T1/T2 word-intersection: concepts that contain ALL of `words` in their
    /// inverted index, optionally restricted to a kind. `words` must be
    /// non-empty. Returns up to `limit` rows.
    pub fn concepts_by_words(
        &self,
        words: &[&str],
        kind: Option<crate::concept::ConceptKind>,
        limit: u32,
    ) -> Result<Vec<ConceptRow>> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = format!(
            "SELECT {} FROM concepts c WHERE c.id IN (
                 SELECT concept_id FROM concept_words WHERE word = ?1
             )",
            Self::CONCEPT_COLS
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(words[0])];
        for w in &words[1..] {
            sql.push_str(" AND c.id IN (SELECT concept_id FROM concept_words WHERE word = ?)");
            p.push(Box::new(*w));
        }
        if let Some(k) = kind {
            sql.push_str(" AND c.kind = ?");
            p.push(Box::new(k.as_str()));
        }
        sql.push_str(" ORDER BY c.kind, c.id LIMIT ?");
        p.push(Box::new(limit));
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(param_refs.as_slice(), Self::row_to_concept)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// T1/T2 word-intersection with OR fallback: concepts containing ANY of
    /// `words`, restricted to a kind. Used when the AND query returns nothing.
    pub fn concepts_by_any_word(
        &self,
        words: &[&str],
        kind: Option<crate::concept::ConceptKind>,
        limit: u32,
    ) -> Result<Vec<ConceptRow>> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; words.len()].join(",");
        let mut sql = format!(
            "SELECT {} FROM concepts c WHERE c.id IN (
                 SELECT concept_id FROM concept_words WHERE word IN ({placeholders})
             )",
            Self::CONCEPT_COLS
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = words
            .iter()
            .map(|w| Box::new(*w) as Box<dyn rusqlite::ToSql>)
            .collect();
        if let Some(k) = kind {
            sql.push_str(" AND c.kind = ?");
            p.push(Box::new(k.as_str()));
        }
        // Rank the complete indexed match set before truncating. Row/kind order
        // otherwise hides later high-coverage matches from the reranker.
        sql.push_str(&format!(
            " ORDER BY (SELECT COUNT(DISTINCT word) FROM concept_words \
             WHERE concept_id = c.id AND word IN ({placeholders})) DESC, c.kind, c.id LIMIT ?"
        ));
        p.extend(
            words
                .iter()
                .map(|w| Box::new(*w) as Box<dyn rusqlite::ToSql>),
        );
        p.push(Box::new(limit));
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(param_refs.as_slice(), Self::row_to_concept)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// T1 kind-directed: concepts of a given kind whose words intersect the
    /// query words (AND semantics, OR fallback handled by the caller).
    pub fn concepts_by_kind_words(
        &self,
        kind: crate::concept::ConceptKind,
        words: &[&str],
        limit: u32,
    ) -> Result<Vec<ConceptRow>> {
        self.concepts_by_words(words, Some(kind), limit)
    }

    /// T3 trigram fallback: concepts whose normalized form contains `needle`
    /// as a substring (case-insensitive via LIKE). Low confidence by design.
    pub fn concepts_like(&self, needle: &str, limit: u32) -> Result<Vec<ConceptRow>> {
        let sql = format!(
            "SELECT {} FROM concepts WHERE norm LIKE ?1 ORDER BY kind, id LIMIT ?2",
            Self::CONCEPT_COLS
        );
        let pattern = format!("%{}%", needle.to_lowercase());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern, limit], Self::row_to_concept)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Concepts owned by a symbol (used to attach owner info to matches).
    pub fn concepts_by_owner(&self, symbol_id: i64) -> Result<Vec<ConceptRow>> {
        let sql = format!(
            "SELECT {} FROM concepts WHERE owner_symbol_id = ?1 ORDER BY kind, id",
            Self::CONCEPT_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![symbol_id], Self::row_to_concept)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Total concept count (for `index_state`).
    pub fn concept_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM concepts", [], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })?)
    }

    /// The stored concept extractor version, if any.
    pub fn concepts_version(&self) -> Result<Option<String>> {
        self.meta_get("concepts_version")
    }

    fn row_to_concept(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConceptRow> {
        Ok(ConceptRow {
            id: r.get(0)?,
            file_id: r.get(1)?,
            kind: crate::concept::ConceptKind::parse(&r.get::<_, String>(2)?),
            raw: r.get(3)?,
            norm: r.get(4)?,
            detail: r.get(5)?,
            start_line: r.get(6)?,
            end_line: r.get(7)?,
            owner_symbol_id: r.get(8)?,
        })
    }

    const CONCEPT_COLS: &'static str =
        "id, file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id";

    // --- read path (analyses own these) ---

    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, blob_oid, lang FROM files WHERE path = ?1",
                params![path],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        blob_oid: r.get(2)?,
                        lang: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Import rows whose spec resolved to `resolved_file_id` — the files that
    /// pull bindings out of that file. `rename` reads these to rewrite the
    /// imported name at the `use`/`import` site.
    pub fn imports_to_file(&self, resolved_file_id: i64) -> Result<Vec<ImportRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, spec, path, bindings, resolved_file_id FROM imports \
             WHERE resolved_file_id = ?1",
        )?;
        let rows = stmt.query_map(params![resolved_file_id], import_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Import rows `file_id` declares — the read side of
    /// [`Self::imports_to_file`]. `plan`'s prereq scan reads these for the
    /// specifier catalog match (provider SDKs, auth packages, db drivers).
    pub fn imports_from(&self, file_id: i64) -> Result<Vec<ImportRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, spec, path, bindings, resolved_file_id FROM imports \
             WHERE file_id = ?1",
        )?;
        let rows = stmt.query_map(params![file_id], import_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Unresolved call/reference sites whose callee text is `name` — the
    /// ambiguous sites a rename must report rather than guess at.
    pub fn unresolved_named(&self, name: &str) -> Result<Vec<UnresolvedRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, site_line, kind FROM unresolved_calls WHERE name = ?1 ORDER BY file_id, site_line",
        )?;
        let rows = stmt.query_map(params![name], |r| {
            Ok(UnresolvedRow {
                file_id: r.get(0)?,
                site_line: r.get(1)?,
                kind: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, blob_oid, lang FROM files WHERE id = ?1",
                params![id],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        blob_oid: r.get(2)?,
                        lang: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, blob_oid, lang FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok(FileRow {
                id: r.get(0)?,
                path: r.get(1)?,
                blob_oid: r.get(2)?,
                lang: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn row_to_symbol(r: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
        Ok(SymbolRow {
            id: r.get(0)?,
            uid: r.get(1)?,
            file_id: r.get(2)?,
            name: r.get(3)?,
            qualified: r.get(4)?,
            kind: SymbolKind::parse(&r.get::<_, String>(5)?),
            start_line: r.get(6)?,
            end_line: r.get(7)?,
            sig: r.get(8)?,
        })
    }

    const SYMBOL_COLS: &'static str =
        "id, uid, file_id, name, qualified, kind, start_line, end_line, sig";

    pub fn symbol_by_uid(&self, uid: &str) -> Result<Option<SymbolRow>> {
        let sql = format!("SELECT {} FROM symbols WHERE uid = ?1", Self::SYMBOL_COLS);
        Ok(self
            .conn
            .query_row(&sql, params![uid], Self::row_to_symbol)
            .optional()?)
    }

    /// The symbols called `name`, optionally only those under `scope`,
    /// capped at `limit`.
    ///
    /// `scope` is part of the query rather than something a caller filters
    /// out of the result, because the cap applies to what the query
    /// returns. Narrowed afterwards, a name with more homonyms than the cap
    /// would come back empty for a directory that plainly contains it —
    /// or, worse, with the one row that happened to fit, which then looks
    /// unique while its rivals sit unseen beyond the cap.
    ///
    /// A scope is a directory: it covers itself and its descendants, never
    /// a sibling that merely shares a leading substring (`src/foo` covers
    /// `src/foo/x.ts`, not `src/foobar.ts`).
    pub fn symbols_by_name(
        &self,
        name: &str,
        scope: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SymbolRow>> {
        // One statement for both shapes: an absent scope binds as NULL and
        // the clause drops out. Two queries here would be two things to
        // keep in step, which is how the scoped read drifted from the plain
        // one in the first place.
        let sql = format!(
            "SELECT {} FROM symbols WHERE name = ?1 \
               AND (?2 IS NULL OR file_id IN \
                 (SELECT id FROM files \
                   WHERE path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')) \
             ORDER BY kind, uid LIMIT ?3",
            Self::SYMBOL_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![name, scope, limit], Self::row_to_symbol)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn symbols_in_file(&self, file_id: i64) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "SELECT {} FROM symbols WHERE file_id = ?1 ORDER BY start_line",
            Self::SYMBOL_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![file_id], Self::row_to_symbol)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    const JSX_ELEMENT_COLS: &'static str =
        "id, file_id, tag, has_handler, text_content, start_line, end_line";

    fn row_to_jsx_element(r: &rusqlite::Row<'_>) -> rusqlite::Result<JsxElementRow> {
        Ok(JsxElementRow {
            id: r.get(0)?,
            file_id: r.get(1)?,
            tag: r.get(2)?,
            has_handler: r.get::<_, i64>(3)? != 0,
            text_content: r.get(4)?,
            start_line: r.get(5)?,
            end_line: r.get(6)?,
        })
    }

    pub fn jsx_elements_in_file(&self, file_id: i64) -> Result<Vec<JsxElementRow>> {
        let sql = format!(
            "SELECT {} FROM jsx_elements WHERE file_id = ?1 ORDER BY start_line",
            Self::JSX_ELEMENT_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![file_id], Self::row_to_jsx_element)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn jsx_elements_dead(
        &self,
        file_id: Option<i64>,
        tag_filter: Option<&str>,
    ) -> Result<Vec<JsxElementRow>> {
        let mut sql = format!(
            "SELECT {} FROM jsx_elements WHERE has_handler = 0",
            Self::JSX_ELEMENT_COLS
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(fid) = file_id {
            sql.push_str(" AND file_id = ?");
            params.push(Box::new(fid));
        }
        if let Some(tag) = tag_filter {
            sql.push_str(" AND tag = ?");
            params.push(Box::new(tag));
        }
        sql.push_str(" ORDER BY start_line");
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(param_refs.as_slice(), Self::row_to_jsx_element)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Incoming edges of `dst` (callers when kind = Calls).
    pub fn edges_to(&self, dst_id: i64, kind: Option<EdgeKind>) -> Result<Vec<EdgeRow>> {
        self.edges_dir(dst_id, kind, false)
    }

    /// Outgoing edges of `src` (callees when kind = Calls).
    pub fn edges_from(&self, src_id: i64, kind: Option<EdgeKind>) -> Result<Vec<EdgeRow>> {
        self.edges_dir(src_id, kind, true)
    }

    fn edges_dir(&self, id: i64, kind: Option<EdgeKind>, outgoing: bool) -> Result<Vec<EdgeRow>> {
        let col = if outgoing { "src_id" } else { "dst_id" };
        let sql = match kind {
            Some(_) => format!(
                "SELECT src_id, dst_id, kind, tier, site_line, receiver, callee FROM edges
                 WHERE {col} = ?1 AND kind = ?2"
            ),
            None => {
                format!(
                    "SELECT src_id, dst_id, kind, tier, site_line, receiver, callee FROM edges WHERE {col} = ?1"
                )
            }
        };
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<EdgeRow> {
            Ok(EdgeRow {
                src_id: r.get(0)?,
                dst_id: r.get(1)?,
                kind: EdgeKind::parse(&r.get::<_, String>(2)?),
                tier: Tier::parse(&r.get::<_, String>(3)?),
                site_line: r.get(4)?,
                receiver: r.get(5)?,
                callee: r.get(6)?,
            })
        };
        let mut out = Vec::new();
        match kind {
            Some(k) => {
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(params![id, k.as_str()], map)?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(params![id], map)?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Epistemic envelope for a symbol name.
    pub fn envelope_for_name(&self, name: &str) -> Result<Envelope> {
        let unresolved: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM unresolved_calls WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0).map(|v| v as u64),
        )?;
        Ok(Envelope {
            lower_bound: unresolved > 0,
            unresolved_same_name: unresolved,
        })
    }

    pub fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        let files: u64 = self.conn.query_row("SELECT COUNT(*) FROM files", [], |r| {
            r.get::<_, i64>(0).map(|v| v as u64)
        })?;
        let symbols: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })?;
        let edges: u64 = self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| {
            r.get::<_, i64>(0).map(|v| v as u64)
        })?;
        let unresolved: u64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM unresolved_calls", [], |r| {
                    r.get::<_, i64>(0).map(|v| v as u64)
                })?;
        Ok((files, symbols, edges, unresolved))
    }

    /// Symbol count per indexed language, for the coverage report.
    pub fn symbols_by_lang(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.lang, COUNT(*) FROM symbols s \
             JOIN files f ON f.id = s.file_id GROUP BY f.lang",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Read a `meta` table value, or `None` if the key is absent.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    /// Upsert a `meta` table value.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // --- human annotations (survive rebuild) ---

    /// Upsert a human note keyed by `file_path` + `target` (a symbol `name`
    /// or concept `norm`). Human-owned: never deleted by re-index.
    pub fn set_annotation(&self, file_path: &str, target: &str, note: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.conn.execute(
            "INSERT INTO annotations (file_path, target, note, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file_path, target) DO UPDATE SET
               note = excluded.note, updated_at = excluded.updated_at",
            params![file_path, target, note, now],
        )?;
        Ok(())
    }

    /// Fetch the human note for `file_path` + `target`, if any.
    pub fn get_annotation(&self, file_path: &str, target: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT note FROM annotations WHERE file_path = ?1 AND target = ?2",
                params![file_path, target],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Remove a human note for `file_path` + `target`.
    pub fn delete_annotation(&self, file_path: &str, target: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM annotations WHERE file_path = ?1 AND target = ?2",
            params![file_path, target],
        )?;
        Ok(())
    }

    fn row_to_annotation(r: &rusqlite::Row<'_>) -> rusqlite::Result<AnnotationRow> {
        Ok(AnnotationRow {
            id: r.get(0)?,
            file_path: r.get(1)?,
            target: r.get(2)?,
            note: r.get(3)?,
            updated_at: r.get(4)?,
        })
    }

    const ANNOTATION_COLS: &'static str = "id, file_path, target, note, updated_at";

    /// All annotations for a file (used to attach notes to symbol results).
    pub fn annotations_for_file(&self, file_path: &str) -> Result<Vec<AnnotationRow>> {
        let sql = format!(
            "SELECT {} FROM annotations WHERE file_path = ?1 ORDER BY target, updated_at",
            Self::ANNOTATION_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![file_path], Self::row_to_annotation)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Merge-helper for symbol results: given a file's symbol `name`s, return
    /// the annotations whose `target` matches one of them. Callers attach
    /// these to the matching symbols (so a note follows its symbol through
    /// `resolve`/`targets` output).
    pub fn annotations_for_symbols(
        &self,
        file_path: &str,
        names: &[&str],
    ) -> Result<Vec<AnnotationRow>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; names.len()].join(",");
        let sql = format!(
            "SELECT {} FROM annotations WHERE file_path = ?1 AND target IN ({placeholders}) ORDER BY target",
            Self::ANNOTATION_COLS
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(file_path.to_string())];
        p.extend(
            names
                .iter()
                .map(|n| Box::new(n.to_string()) as Box<dyn rusqlite::ToSql>),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(param_refs.as_slice(), Self::row_to_annotation)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Merge-notes for concept results: given a file's concept `norm`s, return
    /// the annotations whose `target` matches a norm. Callers attach these to
    /// the matching concepts in `targets` output.
    pub fn annotations_for_norms(
        &self,
        file_path: &str,
        norms: &[&str],
    ) -> Result<Vec<AnnotationRow>> {
        // annotations are keyed by name/norm; reuse the symbol lookup since the
        // matching column is the same `target` column.
        self.annotations_for_symbols(file_path, norms)
    }

    /// Global probes: every annotation across all files whose `target` equals
    /// `target` (a symbol name or concept norm), regardless of file. Used by
    /// `resolve`/`targets` to surface notes even when the owning file is not
    /// the queried one.
    pub fn annotations_by_target(&self, target: &str, limit: u32) -> Result<Vec<AnnotationRow>> {
        let sql = format!(
            "SELECT {} FROM annotations WHERE target = ?1 ORDER BY file_path, updated_at LIMIT ?2",
            Self::ANNOTATION_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![target, limit], Self::row_to_annotation)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every annotation across all files, grouped by file then target (for
    /// `note list` without a file filter).
    pub fn all_annotations(&self, limit: u32) -> Result<Vec<AnnotationRow>> {
        let sql = format!(
            "SELECT {} FROM annotations ORDER BY file_path, target LIMIT ?1",
            Self::ANNOTATION_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit], Self::row_to_annotation)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Total annotation count (contributes to `index_state` health).
    pub fn annotation_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM annotations", [], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })?)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  blob_oid TEXT NOT NULL,
  lang TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  file_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  qualified TEXT NOT NULL,
  kind TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  sig TEXT NOT NULL DEFAULT '',
  trait_impl INTEGER NOT NULL DEFAULT 0,
  module_decl INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
CREATE TABLE IF NOT EXISTS imports (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL,
  spec TEXT NOT NULL,
  path TEXT NOT NULL DEFAULT '',
  resolved_file_id INTEGER,
  bindings TEXT NOT NULL DEFAULT '',
  scope TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);
CREATE INDEX IF NOT EXISTS idx_imports_resolved ON imports(resolved_file_id);
CREATE TABLE IF NOT EXISTS edges (
  id INTEGER PRIMARY KEY,
  src_id INTEGER NOT NULL,
  dst_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  tier TEXT NOT NULL,
  site_line INTEGER NOT NULL DEFAULT 0,
  receiver TEXT,
  callee TEXT
);
CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src_id);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst_id);
CREATE TABLE IF NOT EXISTS unresolved_calls (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  enclosing_symbol_id INTEGER,
  site_line INTEGER NOT NULL DEFAULT 0,
  receiver TEXT
);
CREATE INDEX IF NOT EXISTS idx_unresolved_name ON unresolved_calls(name);
-- replace_file/remove_file delete a file's unresolved calls by file_id once
-- per file of an incremental batch; without this index each delete scans the
-- whole table (552k rows on a 9.6k-file Ruby repository, ~20 ms a file).
CREATE INDEX IF NOT EXISTS idx_unresolved_file ON unresolved_calls(file_id);
CREATE TABLE IF NOT EXISTS processes (
  id INTEGER PRIMARY KEY,
  label TEXT NOT NULL,
  entry_symbol_id INTEGER NOT NULL,
  step_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS process_steps (
  process_id INTEGER NOT NULL,
  step INTEGER NOT NULL,
  symbol_id INTEGER NOT NULL,
  PRIMARY KEY (process_id, step)
);
CREATE INDEX IF NOT EXISTS idx_process_steps_symbol ON process_steps(symbol_id);
CREATE TABLE IF NOT EXISTS clusters (
  id INTEGER PRIMARY KEY,
  label TEXT NOT NULL,
  cohesion REAL NOT NULL DEFAULT 0,
  keywords TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS cluster_members (
  cluster_id INTEGER NOT NULL,
  symbol_id INTEGER NOT NULL,
  PRIMARY KEY (cluster_id, symbol_id)
);
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS annotations (
  id INTEGER PRIMARY KEY,
  file_path TEXT NOT NULL,
  target TEXT NOT NULL,
  note TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT 0,
  UNIQUE (file_path, target)
);
CREATE INDEX IF NOT EXISTS idx_annotations_target ON annotations(target);
CREATE INDEX IF NOT EXISTS idx_annotations_file ON annotations(file_path);
CREATE TABLE IF NOT EXISTS concepts (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  raw TEXT NOT NULL,
  norm TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  start_line INTEGER NOT NULL DEFAULT 0,
  end_line INTEGER NOT NULL DEFAULT 0,
  owner_symbol_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_concepts_norm ON concepts(norm);
CREATE INDEX IF NOT EXISTS idx_concepts_kind_norm ON concepts(kind, norm);
CREATE INDEX IF NOT EXISTS idx_concepts_file ON concepts(file_id);
CREATE TABLE IF NOT EXISTS symbol_crux (
  id INTEGER PRIMARY KEY,
  symbol_id INTEGER NOT NULL,
  line INTEGER NOT NULL,
  text TEXT NOT NULL,
  fingerprint INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbol_crux_symbol ON symbol_crux(symbol_id);
CREATE TABLE IF NOT EXISTS concept_words (
  word TEXT NOT NULL,
  concept_id INTEGER NOT NULL,
  PRIMARY KEY (word, concept_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_concept_words_concept ON concept_words(concept_id);
CREATE TABLE IF NOT EXISTS jsx_elements (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL,
  tag TEXT NOT NULL,
  has_handler INTEGER NOT NULL DEFAULT 0,
  text_content TEXT NOT NULL DEFAULT '',
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_jsx_elements_file_handler ON jsx_elements(file_id, has_handler);
CREATE INDEX IF NOT EXISTS idx_jsx_elements_tag ON jsx_elements(tag);
";

/// Idempotent schema migrations for graphs created before a column existed.
/// `CREATE TABLE IF NOT EXISTS` won't add columns to an existing table, so
/// additive columns are patched here. Each step introspects `table_info` and
/// only alters when the column is missing.
fn migrate(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, col: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for row in rows {
            if row? == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_column("unresolved_calls", "receiver")? {
        conn.execute("ALTER TABLE unresolved_calls ADD COLUMN receiver TEXT", [])?;
    }
    if !has_column("unresolved_calls", "kind")? {
        conn.execute(
            "ALTER TABLE unresolved_calls ADD COLUMN kind TEXT NOT NULL DEFAULT 'calls'",
            [],
        )?;
    }
    if !has_column("symbols", "trait_impl")? {
        conn.execute(
            "ALTER TABLE symbols ADD COLUMN trait_impl INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column("symbols", "module_decl")? {
        conn.execute(
            "ALTER TABLE symbols ADD COLUMN module_decl INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        // Graphs built before the inline/external distinction kept every
        // `Module` row out of exact-name promotion; marking them as
        // declarations keeps that behaviour until a rebuild re-extracts the
        // real flag (the extractor version bump forces one).
        conn.execute(
            "UPDATE symbols SET module_decl = 1 WHERE kind = 'module'",
            [],
        )?;
    }
    if !has_column("edges", "receiver")? {
        conn.execute("ALTER TABLE edges ADD COLUMN receiver TEXT", [])?;
    }
    if !has_column("edges", "callee")? {
        conn.execute("ALTER TABLE edges ADD COLUMN callee TEXT", [])?;
    }
    if !has_column("imports", "bindings")? {
        conn.execute(
            "ALTER TABLE imports ADD COLUMN bindings TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !has_column("imports", "path")? {
        // Rows written before a statement could name several paths resolved
        // their spec; the extractor version bump rewrites them.
        conn.execute(
            "ALTER TABLE imports ADD COLUMN path TEXT NOT NULL DEFAULT ''",
            [],
        )?;
        conn.execute("UPDATE imports SET path = spec", [])?;
    }
    if !has_column("imports", "scope")? {
        conn.execute(
            "ALTER TABLE imports ADD COLUMN scope TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // Engine 1: stamp the concept schema version once, at creation. The
    // rebuild after an extractor change is `EXTRACTOR_VERSION`'s job.
    if conn
        .query_row(
            "SELECT COUNT(*) FROM meta WHERE key = 'concepts_version'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        == 0
    {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('concepts_version', ?1)",
            [CONCEPTS_VERSION],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_only_should_pin_snapshot_and_reject_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read-only.db");
        let writer = GraphStore::open(&path).unwrap();
        writer.meta_set("generation", "one").unwrap();
        let reader = GraphStore::open_read_only(&path).unwrap();
        reader.conn().execute_batch("BEGIN").unwrap();
        assert_eq!(
            reader.meta_get("generation").unwrap().as_deref(),
            Some("one")
        );
        writer.meta_set("generation", "two").unwrap();
        assert_eq!(
            reader.meta_get("generation").unwrap().as_deref(),
            Some("one")
        );
        assert!(reader.meta_set("generation", "bad").is_err());
        reader.conn().execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            reader.meta_get("generation").unwrap().as_deref(),
            Some("two")
        );
        assert!(GraphStore::open_read_only(&dir.path().join("missing.db")).is_err());
        assert!(!dir.path().join("missing.db").exists());
    }

    /// A graph.db written before `symbols.trait_impl` existed opens with the
    /// column added (default 0), and marking a symbol sets it.
    #[test]
    fn opening_an_older_graph_adds_the_trait_impl_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE symbols (id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, \
                 file_id INTEGER NOT NULL, name TEXT NOT NULL, qualified TEXT NOT NULL, \
                 kind TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, \
                 sig TEXT NOT NULL DEFAULT '');\
                 INSERT INTO symbols VALUES (1, 'a#f#method', 1, 'f', 'f', 'method', 1, 2, '');",
            )
            .unwrap();
        }
        let store = GraphStore::open(&path).unwrap();
        let flag = |id: i64| -> i64 {
            store
                .conn()
                .query_row("SELECT trait_impl FROM symbols WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(flag(1), 0);
        store.mark_trait_impl(1).unwrap();
        assert_eq!(flag(1), 1);
    }

    /// A graph.db written before `symbols.module_decl` existed opens with the
    /// column added; module rows inherit the old exact-match exclusion (they
    /// are marked as declarations) while other rows stay definitions, and
    /// marking an external declaration sets the column.
    #[test]
    fn opening_an_older_graph_marks_module_rows_as_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.db");
        let old_schema = "CREATE TABLE symbols (id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, \
             file_id INTEGER NOT NULL, name TEXT NOT NULL, qualified TEXT NOT NULL, \
             kind TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, \
             sig TEXT NOT NULL DEFAULT '');";
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(old_schema).unwrap();
            conn.execute_batch(
                "INSERT INTO symbols VALUES (1, 'lib.rs#search_compat#module', 1, \
                 'search_compat', 'search_compat', 'module', 1, 1, 'mod search_compat;');\
                 INSERT INTO symbols VALUES (2, 'lib.rs#run#function', 1, \
                 'run', 'run', 'function', 3, 4, 'fn run()');",
            )
            .unwrap();
        }
        let store = GraphStore::open(&path).unwrap();
        let flag = |id: i64| -> i64 {
            store
                .conn()
                .query_row("SELECT module_decl FROM symbols WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(flag(1), 1, "a migrated module row keeps the old exclusion");
        assert_eq!(flag(2), 0, "a migrated non-module row is a definition");
        store.mark_module_decl(2).unwrap();
        assert_eq!(flag(2), 1);
    }

    use super::*;
    use std::os::unix::fs::symlink;

    fn caller_fixture_symbol(
        store: &mut GraphStore,
        path: &str,
        blob_oid: &str,
        name: &str,
        qualified: &str,
    ) -> i64 {
        let file_id = store.replace_file(path, blob_oid, "rust").unwrap();
        store
            .insert_symbol(
                file_id,
                &format!("{path}#{qualified}#function"),
                name,
                qualified,
                SymbolKind::Function,
                1,
                10,
                "fn",
            )
            .unwrap()
    }

    #[test]
    fn caller_examples_are_bounded_and_include_freshness_metadata() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let target = caller_fixture_symbol(
            &mut store,
            "src/target.rs",
            "target-oid",
            "dispatch",
            "dispatch",
        );
        for index in 0..5 {
            let path = format!("src/caller_{index}.rs");
            let name = format!("caller_{index}");
            let caller = caller_fixture_symbol(
                &mut store,
                &path,
                &format!("caller-oid-{index}"),
                &name,
                &name,
            );
            store
                .insert_edge(&EdgeRow {
                    src_id: caller,
                    dst_id: target,
                    kind: EdgeKind::Calls,
                    tier: if index % 2 == 0 {
                        Tier::Exact
                    } else {
                        Tier::Probable
                    },
                    site_line: index + 10,
                    receiver: None,
                    callee: None,
                })
                .unwrap();
        }

        let examples = store.caller_examples_by_name("dispatch").unwrap().unwrap();
        assert_eq!(
            examples.target,
            CallerTarget {
                name: "dispatch".to_owned(),
                qualified: "dispatch".to_owned(),
                path: "src/target.rs".to_owned(),
                blob_oid: "target-oid".to_owned(),
            }
        );
        assert_eq!(examples.callers.len(), CALLER_EXAMPLE_CAP);
        assert!(
            examples.truncated,
            "the fourth indexed caller marks truncation"
        );
        for caller in examples.callers {
            assert!(caller.site_line > 0);
            assert!(caller.path.starts_with("src/caller_"));
            let index = caller
                .caller_name
                .strip_prefix("caller_")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(caller.path, format!("src/caller_{index}.rs"));
            assert_eq!(caller.blob_oid, format!("caller-oid-{index}"));
            assert_eq!(caller.site_line, index as u32 + 10);
            assert_eq!(
                caller.tier,
                if index % 2 == 0 { "exact" } else { "probable" }
            );
        }
    }

    #[test]
    fn caller_examples_abstain_for_missing_or_ambiguous_names_and_ignore_non_calls() {
        let mut store = GraphStore::open_in_memory().unwrap();
        assert!(store.caller_examples_by_name("missing").unwrap().is_none());

        let target = caller_fixture_symbol(&mut store, "a.rs", "a-oid", "dispatch", "dispatch");
        let unrelated =
            caller_fixture_symbol(&mut store, "b.rs", "b-oid", "unrelated", "unrelated");
        let importer = caller_fixture_symbol(&mut store, "c.rs", "c-oid", "importer", "importer");
        store
            .insert_edge(&EdgeRow {
                src_id: importer,
                dst_id: target,
                kind: EdgeKind::Imports,
                tier: Tier::Exact,
                site_line: 12,
                receiver: None,
                callee: None,
            })
            .unwrap();
        store
            .insert_edge(&EdgeRow {
                src_id: unrelated,
                dst_id: target,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 0,
                receiver: None,
                callee: None,
            })
            .unwrap();

        let no_callers = store.caller_examples_by_name("dispatch").unwrap().unwrap();
        assert!(no_callers.callers.is_empty());
        assert!(!no_callers.truncated);

        caller_fixture_symbol(
            &mut store,
            "other.rs",
            "other-oid",
            "dispatch",
            "other::dispatch",
        );
        assert!(store.caller_examples_by_name("dispatch").unwrap().is_none());
    }

    #[test]
    fn nonblocking_read_only_open_reports_locks_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('generation', 'one');
                 BEGIN EXCLUSIVE;",
            )
            .unwrap();

        let reader = GraphStore::open_read_only_nonblocking(&path).unwrap();
        assert!(reader.meta_get("generation").is_err());
        drop(reader);
        writer.execute_batch("ROLLBACK;").unwrap();

        let reader = GraphStore::open_read_only_nonblocking(&path).unwrap();
        assert_eq!(
            reader.meta_get("generation").unwrap().as_deref(),
            Some("one")
        );
        assert!(reader.meta_set("generation", "changed").is_err());
        assert!(GraphStore::open_read_only_nonblocking(&dir.path().join("missing.db")).is_err());
        assert!(!dir.path().join("missing.db").exists());
    }

    /// A file's refresh drops its old concepts and their search words, and
    /// only its own: a word left behind would keep answering `find-code`
    /// with a concept that no longer exists, and a neighbour's rows lost
    /// would make it unfindable until its next edit.
    #[test]
    fn delete_concepts_for_file_drops_only_that_files_concepts_and_words() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("a.rs", "oid-a", "rust").unwrap();
        let b = store.replace_file("b.rs", "oid-b", "rust").unwrap();
        let kind = crate::concept::ConceptKind::String;
        let ca = store
            .insert_concept(a, kind, "payment failed", "payment failed", "", 1, 1, None)
            .unwrap();
        let cb = store
            .insert_concept(b, kind, "refund issued", "refund issued", "", 1, 1, None)
            .unwrap();
        let count = |sql: &str, id: i64| -> i64 {
            store.conn().query_row(sql, [id], |r| r.get(0)).unwrap()
        };
        let concepts = "SELECT COUNT(*) FROM concepts WHERE file_id = ?1";
        let words = "SELECT COUNT(*) FROM concept_words WHERE concept_id = ?1";
        assert!(
            count(words, ca) > 0 && count(words, cb) > 0,
            "both concepts indexed words"
        );
        let b_words = count(words, cb);

        store.delete_concepts_for_file(a).unwrap();

        assert_eq!(count(concepts, a), 0, "a.rs's concept is gone");
        assert_eq!(count(words, ca), 0, "and so are its words");
        assert_eq!(count(concepts, b), 1, "b.rs keeps its concept");
        assert_eq!(count(words, cb), b_words, "and all of its words");
    }

    #[test]
    fn crux_extraction_and_fingerprint_roundtrip() {
        let body = "pub fn run(cfg: &Config) -> i32 {\n    if cfg.dry_run {\n        return 0;\n    }\n    let mut total = 0;\n    for x in cfg.items {\n        total += x.val;\n    }\n    // a pure comment\n    if total > 100 {\n        bail!(\"too big\");\n    }\n    total\n}";

        // Deterministic heuristic: guard + mutation + bail lines surface,
        // delimiters/comments do not.
        let crux = extract_crux(body, 3);
        assert!(!crux.is_empty());
        let texts: Vec<&str> = crux.iter().map(|c| c.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("dry_run"))
                && texts.iter().any(|t| t.contains("return 0"))
                && texts.iter().any(|t| t.contains("total += x.val"))
        );
        // No pure delimiters or comment-only lines leak through.
        assert!(
            !texts
                .iter()
                .any(|t| *t == "{" || *t == "}" || t.ends_with("comment"))
        );
        // Fingerprint is content-stable: same text -> same hash, regardless of
        // which line number it sits at.
        // NOTE: the body line is `total += x.val;` (Rust semicolon), so the
        // extracted text carries the `;`; fingerprint any extracted line and
        // confirm it equals fnv1a64 of that exact text.
        let mut_found = crux.iter().find(|c| c.text.contains("x.val")).unwrap();
        assert_eq!(mut_found.fingerprint, fnv1a64(&mut_found.text));
        // Same text moved to a different line number keeps the same fingerprint.
        let moved = CruxLine {
            line: 999,
            text: mut_found.text.clone(),
            fingerprint: fnv1a64(&mut_found.text),
        };
        assert_eq!(moved.fingerprint, mut_found.fingerprint);

        // Storage round-trips through the symbol_crux table and retrieves by
        // stable fingerprint (the anchor used by retrieval).
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/lib.rs", "oid1", "rust").unwrap();
        let sid = store
            .insert_symbol(
                fid,
                "src/lib.rs#run#function",
                "run",
                "run",
                SymbolKind::Function,
                1,
                20,
                "fn run",
            )
            .unwrap();
        store.set_symbol_crux(sid, &crux).unwrap();
        let back = store.symbol_crux_by_id(sid).unwrap();
        assert_eq!(back.len(), crux.len());
        // Retrieval from the stable anchor.
        let anchor = fnv1a64(&mut_found.text);
        let found = store.symbol_crux_by_fingerprint(sid, anchor).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, mut_found.text);
        assert_eq!(found[0].fingerprint, anchor);
        // Re-index of the file clears the symbol's crux (no stale anchors).
        let sid2 = store
            .insert_symbol(
                fid,
                "src/lib.rs#run2#function",
                "run2",
                "run2",
                SymbolKind::Function,
                5,
                6,
                "",
            )
            .unwrap();
        let _ = sid2;
        let _ = back;
    }

    /// WAL serializes writers, so a second connection must wait for the
    /// first one's transaction to commit instead of failing on the spot
    /// with SQLITE_BUSY: the daemon's graph update would otherwise die
    /// whenever a CLI build holds the write lock. A timeout shorter than
    /// the 300 ms the holder keeps the lock fails the write here.
    #[test]
    fn second_writer_waits_for_a_concurrent_transaction_to_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.db");

        // The holder takes the write lock itself, then reports in: the
        // signal is sent after the INSERT, so the lock is provably held
        // when the main thread tries to write. It releases it 300 ms later.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let store = GraphStore::open(&holder_path).unwrap();
            let tx = store.conn().unchecked_transaction().unwrap();
            tx.execute("INSERT INTO meta (key, value) VALUES ('held', 'x')", [])
                .unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            tx.commit().unwrap();
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder takes the write lock");

        let second = GraphStore::open(&path).unwrap();
        let result = second.meta_set("second", "y");
        holder.join().unwrap();

        assert!(
            result.is_ok(),
            "second writer must wait for the lock, not fail busy: {result:?}"
        );
        assert_eq!(
            second.meta_get("second").unwrap().as_deref(),
            Some("y"),
            "the waiting writer's row must be committed"
        );
    }

    /// The query plan of one statement, joined detail lines.
    fn query_plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map([1_i64], |row| row.get::<_, String>(3))
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    /// An incremental graph update (a checkout's watcher batch, the drift
    /// delta applied on first use) runs this delete once per changed or
    /// removed file. On a scan it costs a pass over every unresolved call of
    /// the repository: ~20 ms a file at 552k rows, 17 s of an 874-file
    /// update. The plan must seek the file's rows through an index.
    #[test]
    fn a_files_unresolved_calls_should_be_deleted_through_an_index_not_a_table_scan() {
        let store = GraphStore::open_in_memory().unwrap();
        let plan = query_plan(store.conn(), DELETE_FILE_UNRESOLVED_CALLS);
        assert!(
            plan.contains("USING INDEX idx_unresolved_file")
                || plan.contains("USING COVERING INDEX idx_unresolved_file"),
            "per-file delete must seek by file_id, plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN unresolved_calls"),
            "per-file delete must not scan the table, plan: {plan}"
        );
    }

    /// Graphs built before the index existed must not need a rebuild to
    /// stop scanning: the next writable open adds it.
    #[test]
    fn a_graph_built_without_the_file_index_should_gain_it_on_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.db");
        let store = GraphStore::open(&path).unwrap();
        store
            .conn()
            .execute_batch("DROP INDEX idx_unresolved_file")
            .unwrap();
        drop(store);

        let reopened = GraphStore::open(&path).unwrap();
        let indexes: i64 = reopened
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'idx_unresolved_file'
                    AND tbl_name = 'unresolved_calls'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1, "reopening an old graph must add the index");
    }

    /// The index changes how the delete finds rows, never which ones: a
    /// replaced file loses its own unresolved calls and keeps everyone
    /// else's, and a removed file's calls go with it.
    #[test]
    fn replacing_or_removing_a_file_should_drop_only_that_files_unresolved_calls() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("a.rb", "oid-a", "ruby").unwrap();
        let b = store.replace_file("b.rb", "oid-b", "ruby").unwrap();
        store
            .insert_unresolved_call(a, "from_a", None, 1, None, "calls")
            .unwrap();
        store
            .insert_unresolved_call(b, "from_b", None, 2, None, "calls")
            .unwrap();
        let names = |store: &GraphStore| -> Vec<String> {
            let mut stmt = store
                .conn()
                .prepare("SELECT name FROM unresolved_calls ORDER BY name")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        store.replace_file("a.rb", "oid-a2", "ruby").unwrap();
        assert_eq!(names(&store), vec!["from_b".to_string()]);

        store.remove_file("b.rb").unwrap();
        assert!(names(&store).is_empty());
    }

    #[test]
    fn open_rejects_database_symlink_without_modifying_target() {
        let dir = std::env::temp_dir().join(format!("gpx-store-link-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let external = dir.join("external.db");
        let conn = Connection::open(&external).unwrap();
        conn.execute("CREATE TABLE sentinel (id INTEGER)", [])
            .unwrap();
        drop(conn);
        let graph = dir.join("graph.db");
        symlink(&external, &graph).unwrap();

        assert!(GraphStore::open(&graph).is_err());
        let conn = Connection::open(&external).unwrap();
        let graph_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('files', 'symbols', 'edges')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(graph_tables, 0);
        let sentinel: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sentinel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sentinel, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn human_notes_survive_reindex_and_remove_file() {
        // Contract for item P2·2: human annotations are OWNED by the author
        // (keyed by file_path + stable target), NOT by the graph. A rebuild
        // (`replace_file`) or a deletions (`remove_file`) must never drop them.
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/lib.rs", "oid1", "rust").unwrap();
        store
            .insert_symbol(
                fid,
                "src/lib.rs#main#function",
                "main",
                "main",
                SymbolKind::Function,
                1,
                3,
                "fn main()",
            )
            .unwrap();
        store
            .set_annotation("src/lib.rs", "main", "top entry point")
            .unwrap();
        assert_eq!(
            store.get_annotation("src/lib.rs", "main").unwrap().unwrap(),
            "top entry point"
        );
        assert_eq!(store.annotation_count().unwrap(), 1);

        // Re-index replaces the file's symbols/edges but keeps the note.
        let fid2 = store.replace_file("src/lib.rs", "blob2", "rust").unwrap();
        assert_eq!(fid, fid2);
        assert_eq!(
            store.get_annotation("src/lib.rs", "main").unwrap().unwrap(),
            "top entry point"
        );
        let merged = store
            .annotations_for_symbols("src/lib.rs", &["main"])
            .unwrap();
        assert_eq!(merged.len(), 1);

        // Deleting the file also must not delete the human note.
        store.remove_file("src/lib.rs").unwrap();
        assert_eq!(
            store.get_annotation("src/lib.rs", "main").unwrap().unwrap(),
            "top entry point"
        );
        assert_eq!(store.annotation_count().unwrap(), 1);

        // Only an explicit delete_annotation is permitted to remove it.
        store.delete_annotation("src/lib.rs", "main").unwrap();
        assert!(
            store
                .get_annotation("src/lib.rs", "main")
                .unwrap()
                .is_none()
        );
        assert_eq!(store.annotation_count().unwrap(), 0);
    }

    /// `symbols_by_lang` groups symbol counts by the file's language tag.
    #[test]
    fn symbols_by_lang_groups_counts_per_language() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let rust = store.replace_file("src/a.rs", "o1", "rust").unwrap();
        let ts = store.replace_file("src/b.ts", "o2", "ts").unwrap();
        store.replace_file("src/c.rs", "o3", "rust").unwrap();
        store
            .conn()
            .execute_batch(&format!(
                "INSERT INTO symbols (uid, file_id, name, qualified, kind, start_line, end_line) VALUES
                 ('a#f#function', {rust}, 'f', 'f', 'function', 1, 1),
                 ('a#g#function', {rust}, 'g', 'g', 'function', 2, 2),
                 ('b#h#function', {ts}, 'h', 'h', 'function', 1, 1);"
            ))
            .unwrap();
        let by_lang: std::collections::BTreeMap<String, u64> =
            store.symbols_by_lang().unwrap().into_iter().collect();
        assert_eq!(by_lang["rust"], 2);
        assert_eq!(by_lang["ts"], 1);
    }

    /// The column keeps `local=source` only under an alias, so a graph
    /// written before aliases were tracked (plain names) decodes as-is.
    #[test]
    fn bindings_column_encodes_an_alias_as_local_equals_source() {
        let bindings = [
            ImportBinding::named("push"),
            ImportBinding::aliased("push", "leased"),
        ];
        assert_eq!(encode_bindings(&bindings), "push,leased=push");
        assert_eq!(decode_bindings(" push, leased=push,,"), bindings);
        assert_eq!(
            decode_bindings("greet,farewell"),
            [
                ImportBinding::named("greet"),
                ImportBinding::named("farewell")
            ]
        );
        assert!(decode_bindings("").is_empty());
    }

    /// A `.pixel` or a graph db that is a link (a repository can commit
    /// either) is never opened through: the link target keeps its bytes and
    /// its directory stays empty.
    #[test]
    fn open_should_refuse_a_linked_pixel_dir_or_db() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        symlink(&elsewhere, root.join(".pixel")).unwrap();
        let db = root.join(".pixel/graph.v2.db");
        assert!(GraphStore::open(&db).is_err());
        assert!(GraphStore::open_read_only(&db).is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

        let real = base.path().join("real");
        std::fs::create_dir_all(real.join(".pixel")).unwrap();
        let victim = base.path().join("victim.db");
        std::fs::write(&victim, b"").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&victim, real.join(".pixel/graph.v2.db")).unwrap();
        assert!(GraphStore::open(&real.join(".pixel/graph.v2.db")).is_err());
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"",
            "an empty file became a db"
        );

        // A plain directory under a linked prefix still opens, for writing
        // and read-only alike (macOS's `/var` -> `/private/var` is such a
        // prefix): only the `.pixel` directory and the file keep the refusal.
        let linked_root = base.path().join("linked-root");
        symlink(&real, &linked_root).unwrap();
        std::fs::remove_file(real.join(".pixel/graph.v2.db")).unwrap();
        GraphStore::open(&linked_root.join(".pixel/graph.v2.db")).unwrap();
        GraphStore::open_read_only(&linked_root.join(".pixel/graph.v2.db")).unwrap();
    }
}
