// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A [`ResolveIndex`] for one batch of call and reference sites, loaded from
//! the store instead of built over the whole graph: the full build decides
//! its sites a batch at a time, so what it holds is bounded by the batch,
//! never by the tree.
//!
//! A decision reads two kinds of facts. Per-file ones belong to the calling
//! file: its language and module path, its symbols (the caller's qualified
//! name and kind), its imports and their bindings. Per-name ones belong to
//! the name called: every symbol carrying it, the callable ones with their
//! qualified names and their files' language and module path. A batch loads
//! the per-file facts of its files, and the per-name facts of every name its
//! sites call or reference and every source name its files' imports bind,
//! through `idx_symbols_name`. The per-name facts are kept across batches
//! in a cache bounded in bytes ([`NameCache`]): a common name (`new`,
//! `get`) is read once, not once per batch.
//!
//! The result is the index [`ResolveIndex::build`] makes, restricted to what
//! the batch's decisions read, so every decision is the same.

use std::sync::Arc;

use rusqlite::params;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::{
    Candidate, ImportTarget, ResolveIndex, callable, ruby, rust_crate_depth, rust_module_path,
};
use crate::store::{GraphStore, StoreError, SymbolKind, decode_bindings, decode_scope};

/// What a decision reads of a file other than the caller's: whether it is
/// a Ruby file, and a Rust file's module path and crate depth.
#[derive(Debug, Clone)]
struct FileFacts {
    ruby: bool,
    rust: Option<(Vec<String>, Option<usize>)>,
}

impl FileFacts {
    fn of(lang: &str, path: &str) -> Self {
        Self {
            ruby: lang == "ruby",
            rust: (lang == "rust").then(|| (rust_module_path(path), rust_crate_depth(path))),
        }
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .rust
                .as_ref()
                .map_or(0, |(module, _)| module.iter().map(|s| s.len() + 24).sum())
    }
}

/// The per-name facts of one name.
struct NameEntry {
    /// Some symbol carries the name, whatever its kind or language.
    carried: bool,
    /// The callable candidates `ResolveIndex::build` keeps for the name,
    /// with their qualified names.
    candidates: Vec<(Candidate, String)>,
    /// The facts of every candidate's file.
    files: Vec<(i64, FileFacts)>,
}

impl NameEntry {
    fn bytes(&self, name: &str) -> usize {
        name.len()
            + std::mem::size_of::<Self>()
            + self
                .candidates
                .iter()
                .map(|(_, q)| std::mem::size_of::<(Candidate, String)>() + q.len())
                .sum::<usize>()
            + self.files.iter().map(|(_, f)| 8 + f.bytes()).sum::<usize>()
    }
}

/// Per-name facts kept across batches, least recently used first out once
/// they pass `cap` bytes.
pub struct NameCache {
    entries: HashMap<String, (Arc<NameEntry>, u64)>,
    bytes: usize,
    cap: usize,
    tick: u64,
}

impl NameCache {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::default(),
            bytes: 0,
            cap,
            tick: 0,
        }
    }

    /// The facts of `name`, from the cache or the store.
    fn get(
        &mut self,
        store: &GraphStore,
        name: &str,
        walled: bool,
    ) -> Result<Arc<NameEntry>, StoreError> {
        self.tick += 1;
        if let Some((entry, used)) = self.entries.get_mut(name) {
            *used = self.tick;
            return Ok(Arc::clone(entry));
        }
        let entry = Arc::new(load_name(store, name, walled)?);
        self.bytes += entry.bytes(name);
        self.entries
            .insert(name.to_string(), (Arc::clone(&entry), self.tick));
        Ok(entry)
    }

    /// Drops the least recently used entries until the cache holds at most
    /// three quarters of its cap, once it holds more than the cap.
    fn trim(&mut self) {
        if self.bytes <= self.cap {
            return;
        }
        let mut by_use: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(name, (_, used))| (*used, name.clone()))
            .collect();
        by_use.sort_unstable();
        let goal = self.cap / 4 * 3;
        for (_, name) in by_use {
            if self.bytes <= goal {
                break;
            }
            if let Some((entry, _)) = self.entries.remove(&name) {
                self.bytes = self.bytes.saturating_sub(entry.bytes(&name));
            }
        }
    }

    /// Bytes the cache accounts for.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Reads the per-name facts of `name` through `idx_symbols_name`.
fn load_name(store: &GraphStore, name: &str, walled: bool) -> Result<NameEntry, StoreError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT s.file_id, s.id, s.kind, s.start_line, s.trait_impl, s.qualified, f.lang, f.path
           FROM symbols s JOIN files f ON f.id = s.file_id
          WHERE s.name = ?1
          ORDER BY s.id",
    )?;
    let mut rows = stmt.query(params![name])?;
    let mut entry = NameEntry {
        carried: false,
        candidates: Vec::new(),
        files: Vec::new(),
    };
    let mut seen: HashSet<i64> = HashSet::default();
    while let Some(row) = rows.next()? {
        entry.carried = true;
        let candidate = Candidate {
            file_id: row.get(0)?,
            symbol_id: row.get(1)?,
            kind: SymbolKind::parse(&row.get::<_, String>(2)?),
            start_line: row.get(3)?,
            trait_impl: row.get(4)?,
        };
        let lang: String = row.get(6)?;
        if !callable(candidate.kind) || (walled && lang == "rust") {
            continue;
        }
        if seen.insert(candidate.file_id) {
            entry.files.push((
                candidate.file_id,
                FileFacts::of(&lang, &row.get::<_, String>(7)?),
            ));
        }
        entry.candidates.push((candidate, row.get(5)?));
    }
    Ok(entry)
}

/// Builds the [`ResolveIndex`] of each batch of sites, sharing what does
/// not change between batches: whether the graph's Rust edges are
/// pixel-mvp's, the Ruby constant index, and the per-name cache.
pub struct BatchResolver {
    walled: bool,
    ruby_constants: Arc<ruby::Index>,
    cache: NameCache,
}

impl BatchResolver {
    /// A resolver over `store`, caching at most about `cache_bytes` of
    /// per-name facts. The store's symbols and imports must not change
    /// while it is used: deciding a site writes edges and unresolved rows
    /// only.
    pub fn new(store: &GraphStore, cache_bytes: usize) -> Result<Self, StoreError> {
        Ok(Self {
            walled: crate::mvp_adapter::rust_is_walled(store)?,
            ruby_constants: Arc::new(ruby::Index::build(store)?),
            cache: NameCache::new(cache_bytes),
        })
    }

    /// Bytes of per-name facts the cache holds.
    pub fn cached_bytes(&self) -> usize {
        self.cache.bytes()
    }

    /// The index deciding the sites of `files` that call or reference
    /// `names`: every decision it makes for them is the one
    /// [`ResolveIndex::build`]'s index makes.
    pub fn index_for<'a>(
        &mut self,
        store: &GraphStore,
        files: &HashSet<i64>,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<ResolveIndex, StoreError> {
        let mut idx = ResolveIndex {
            by_name: HashMap::default(),
            ruby_files: HashSet::default(),
            ruby_constants: Arc::clone(&self.ruby_constants),
            containers: HashSet::default(),
            qualified_of: HashMap::default(),
            import_bindings: HashMap::default(),
            rust_modules: HashMap::default(),
            walled: self.walled,
            carried: HashSet::default(),
            bound: HashSet::default(),
        };
        let mut wanted: HashSet<String> = names.into_iter().map(str::to_string).collect();
        let conn = store.conn();
        for &file_id in files {
            let file: Option<(String, String)> = {
                use rusqlite::OptionalExtension;
                conn.prepare_cached("SELECT lang, path FROM files WHERE id = ?1")?
                    .query_row(params![file_id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?
            };
            let Some((lang, path)) = file else {
                continue;
            };
            let facts = FileFacts::of(&lang, &path);
            if facts.ruby {
                idx.ruby_files.insert(file_id);
            }
            if let Some(module) = facts.rust {
                idx.rust_modules.insert(file_id, module);
            }
            let mut stmt =
                conn.prepare_cached("SELECT id, kind, qualified FROM symbols WHERE file_id = ?1")?;
            let mut rows = stmt.query(params![file_id])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                if matches!(
                    SymbolKind::parse(&row.get::<_, String>(1)?),
                    SymbolKind::Class | SymbolKind::Module
                ) {
                    idx.containers.insert(id);
                }
                idx.qualified_of.insert(id, row.get(2)?);
            }
            let mut stmt = conn.prepare_cached(
                "SELECT resolved_file_id, bindings, scope FROM imports WHERE file_id = ?1",
            )?;
            let mut rows = stmt.query(params![file_id])?;
            while let Some(row) = rows.next()? {
                let resolved: Option<i64> = row.get(0)?;
                let bindings = decode_bindings(&row.get::<_, String>(1)?);
                let scope = decode_scope(&row.get::<_, String>(2)?);
                for binding in bindings {
                    idx.bound.insert((file_id, binding.local.clone()));
                    if let Some(dst) = resolved {
                        wanted.insert(binding.source.clone());
                        idx.import_bindings
                            .entry((file_id, binding.local))
                            .or_default()
                            .push(ImportTarget {
                                file_id: dst,
                                source: binding.source,
                                scope: scope.clone(),
                            });
                    }
                }
            }
        }
        for name in wanted {
            let entry = self.cache.get(store, &name, self.walled)?;
            for (file_id, facts) in &entry.files {
                if facts.ruby {
                    idx.ruby_files.insert(*file_id);
                }
                if let Some(module) = &facts.rust {
                    idx.rust_modules
                        .entry(*file_id)
                        .or_insert_with(|| module.clone());
                }
            }
            if !entry.candidates.is_empty() {
                for (candidate, qualified) in &entry.candidates {
                    idx.qualified_of
                        .entry(candidate.symbol_id)
                        .or_insert_with(|| qualified.clone());
                }
                idx.by_name.insert(
                    name.clone(),
                    entry.candidates.iter().map(|(c, _)| *c).collect(),
                );
            }
            if entry.carried {
                idx.carried.insert(name);
            }
        }
        self.cache.trim();
        Ok(idx)
    }
}
