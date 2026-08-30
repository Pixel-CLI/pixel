//! SQLite persistence — THE schema contract for the graph crate.
//!
//! `extract`/`resolve`/`build` write through this API; analyses read through
//! it (plus ad-hoc SQL via `conn()` when a bespoke join is clearer). WAL
//! mode; per-file replacement is transactional.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

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

/// Bump whenever the concept extractor's output shape changes; forces a
/// one-time concept rebuild via the `concepts_version` meta key.
pub const CONCEPTS_VERSION: &str = "2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Const,
    Module,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Const => "const",
            SymbolKind::Module => "module",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "const" => SymbolKind::Const,
            "module" => SymbolKind::Module,
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
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::HasMethod => "has_method",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "imports" => EdgeKind::Imports,
            "extends" => EdgeKind::Extends,
            "implements" => EdgeKind::Implements,
            "has_method" => EdgeKind::HasMethod,
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

pub struct GraphStore {
    conn: Connection,
}

impl GraphStore {
    pub fn open(path: &Path) -> Result<Self> {
        // NOFOLLOW rejects a path with ANY symlinked component (newer SQLite),
        // which breaks legitimate symlinked prefixes like macOS's /var ->
        // /private/var in $TMPDIR. Canonicalize the parent directory so only
        // the db file itself keeps the no-symlink protection.
        let path = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                let _ = std::fs::create_dir_all(parent);
                parent
                    .canonicalize()
                    .map(|p| p.join(name))
                    .unwrap_or_else(|_| path.to_path_buf())
            }
            _ => path.to_path_buf(),
        };
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
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

    // --- write path (extract/resolve/build own these) ---

    /// Insert-or-replace a file row; deletes the file's symbols, imports,
    /// outgoing edges and unresolved calls (incoming edges from other files
    /// are the caller's responsibility to re-resolve). Returns file id.
    pub fn replace_file(&mut self, path: &str, blob_oid: &str, lang: &str) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .optional()?;
        let id = if let Some(id) = existing {
            tx.execute(
                "DELETE FROM edges WHERE src_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM edges WHERE dst_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM symbols WHERE file_id = ?1", params![id])?;
            tx.execute("DELETE FROM imports WHERE file_id = ?1", params![id])?;
            tx.execute(
                "DELETE FROM unresolved_calls WHERE file_id = ?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM concepts WHERE file_id = ?1", params![id])?;
            tx.execute(
                "UPDATE files SET blob_oid = ?2, lang = ?3 WHERE id = ?1",
                params![id, blob_oid, lang],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO files (path, blob_oid, lang) VALUES (?1, ?2, ?3)",
                params![path, blob_oid, lang],
            )?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        if let Some(id) = tx
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            tx.execute(
                "DELETE FROM edges WHERE src_id IN (SELECT id FROM symbols WHERE file_id = ?1)
                   OR dst_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM symbols WHERE file_id = ?1", params![id])?;
            tx.execute("DELETE FROM imports WHERE file_id = ?1", params![id])?;
            tx.execute(
                "DELETE FROM unresolved_calls WHERE file_id = ?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM concepts WHERE file_id = ?1", params![id])?;
            tx.execute("DELETE FROM files WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
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
        self.conn.execute(
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

    pub fn insert_import(
        &self,
        file_id: i64,
        spec: &str,
        resolved_file_id: Option<i64>,
        bindings: &[String],
    ) -> Result<()> {
        let bindings_csv = bindings.join(",");
        self.conn.execute(
            "INSERT INTO imports (file_id, spec, resolved_file_id, bindings) VALUES (?1, ?2, ?3, ?4)",
            params![file_id, spec, resolved_file_id, bindings_csv],
        )?;
        Ok(())
    }

    pub fn insert_edge(&self, e: &EdgeRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edges (src_id, dst_id, kind, tier, site_line, receiver)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                e.src_id,
                e.dst_id,
                e.kind.as_str(),
                e.tier.as_str(),
                e.site_line,
                e.receiver
            ],
        )?;
        Ok(())
    }

    pub fn insert_unresolved_call(
        &self,
        file_id: i64,
        name: &str,
        enclosing_symbol_id: Option<i64>,
        site_line: u32,
        receiver: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO unresolved_calls (file_id, name, enclosing_symbol_id, site_line, receiver)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file_id, name, enclosing_symbol_id, site_line, receiver],
        )?;
        Ok(())
    }

    // --- concept write path (Engine 1) ---

    /// Delete every concept row (and its inverted words) for a file. Called
    /// inside the same transaction as symbol refresh.
    pub fn delete_concepts_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
            params![file_id],
        )?;
        self.conn.execute("DELETE FROM concepts WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// Insert one concept row plus its inverted words. `owner_symbol_id` is
    /// the smallest enclosing symbol's id when one exists, else `None`.
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
        self.conn.execute(
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
            self.conn.execute(
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
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM concept_words WHERE concept_id IN (SELECT id FROM concepts WHERE file_id = ?1)",
            params![file_id],
        )?;
        tx.execute("DELETE FROM concepts WHERE file_id = ?1", params![file_id])?;
        for c in concepts {
            tx.execute(
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
                tx.execute(
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
        let param_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
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
        let mut p: Vec<Box<dyn rusqlite::ToSql>> =
            words.iter().map(|w| Box::new(*w) as Box<dyn rusqlite::ToSql>).collect();
        if let Some(k) = kind {
            sql.push_str(" AND c.kind = ?");
            p.push(Box::new(k.as_str()));
        }
        sql.push_str(" ORDER BY c.kind, c.id LIMIT ?");
        p.push(Box::new(limit));
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
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
            .query_row("SELECT COUNT(*) FROM concepts", [], |r| r.get(0))?)
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

    pub fn symbols_by_name(&self, name: &str, limit: u32) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "SELECT {} FROM symbols WHERE name = ?1 ORDER BY kind, uid LIMIT ?2",
            Self::SYMBOL_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![name, limit], Self::row_to_symbol)?;
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
                "SELECT src_id, dst_id, kind, tier, site_line, receiver FROM edges
                 WHERE {col} = ?1 AND kind = ?2"
            ),
            None => {
                format!(
                    "SELECT src_id, dst_id, kind, tier, site_line, receiver FROM edges WHERE {col} = ?1"
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
            |r| r.get(0),
        )?;
        Ok(Envelope {
            lower_bound: unresolved > 0,
            unresolved_same_name: unresolved,
        })
    }

    pub fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        let files: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let symbols: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
        let edges: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        let unresolved: u64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM unresolved_calls", [], |r| r.get(0))?;
        Ok((files, symbols, edges, unresolved))
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
  sig TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
CREATE TABLE IF NOT EXISTS imports (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL,
  spec TEXT NOT NULL,
  resolved_file_id INTEGER,
  bindings TEXT NOT NULL DEFAULT ''
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
  receiver TEXT
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
CREATE TABLE IF NOT EXISTS concept_words (
  word TEXT NOT NULL,
  concept_id INTEGER NOT NULL,
  PRIMARY KEY (word, concept_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_concept_words_concept ON concept_words(concept_id);
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
    if !has_column("edges", "receiver")? {
        conn.execute("ALTER TABLE edges ADD COLUMN receiver TEXT", [])?;
    }
    if !has_column("imports", "bindings")? {
        conn.execute(
            "ALTER TABLE imports ADD COLUMN bindings TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // Engine 1: stamp the concept extractor version so a change to the
    // extractor forces a one-time concept rebuild (the graph itself is a
    // cache; concepts are rebuilt alongside it).
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
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn open_rejects_database_symlink_without_modifying_target() {
        let dir = std::env::temp_dir().join(format!("gpx-store-link-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let external = dir.join("external.db");
        let conn = Connection::open(&external).unwrap();
        conn.execute("CREATE TABLE sentinel (id INTEGER)", []).unwrap();
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
}
