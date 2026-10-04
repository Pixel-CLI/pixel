// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Sniper-target candidate generators — the graph-side signals behind
//! `gitpixel targets`. Each generator returns path-keyed rows in a total
//! deterministic order (strength desc, path asc) so fusion upstream is
//! byte-stable across runs.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use rusqlite::params;

use crate::impact::{file_path_by_id, split_ident_words, symbol_by_id};
use crate::store::{Envelope, GraphStore, StoreError, SymbolRow};

/// A file that defines symbols whose split words intersect the task keywords.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub path: String,
    /// Matched (symbol, matched-keyword) pairs, capped at 5 per file.
    pub symbols: Vec<(SymbolRow, String)>,
    /// Distinct task keywords matched anywhere in this file's symbol names.
    pub distinct_keywords: usize,
    /// True when an exact token (backticked identifier) equals the name of a
    /// symbol that defines code. An external `mod foo;` names a file, so it
    /// does not count: the declaring file is not where the module's code is.
    /// An inline `mod foo { … }` defines code here, so it does count.
    pub exact_name_hit: bool,
}

const MAX_SYMBOLS_PER_FILE: usize = 5;

/// One pass over the symbols table, matching `split_ident_words(name)` against
/// the keyword set in Rust (LIKE '%kw%' can't use the name index anyway; a
/// single scan is simpler, deterministic, and ms-scale at 100k symbols).
/// Output sorted: exact hits first, then distinct-keyword count desc, then
/// matched-symbol count desc, then path asc.
pub fn symbol_hits(
    store: &GraphStore,
    keywords: &[String],
    exact_tokens: &[String],
) -> Result<Vec<SymbolHit>, StoreError> {
    struct Acc {
        symbols: Vec<(SymbolRow, String)>,
        matched_words: BTreeSet<String>,
        symbol_count: usize,
        exact_name_hit: bool,
    }

    let kw: HashSet<&str> = keywords.iter().map(String::as_str).collect();
    let exact: HashSet<&str> = exact_tokens.iter().map(String::as_str).collect();
    if kw.is_empty() && exact.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = store.conn().prepare(
        "SELECT s.id, s.uid, s.file_id, s.name, s.qualified, s.kind,
                s.start_line, s.end_line, s.sig, f.path, s.module_decl
         FROM symbols s JOIN files f ON f.id = s.file_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            SymbolRow {
                id: r.get(0)?,
                uid: r.get(1)?,
                file_id: r.get(2)?,
                name: r.get(3)?,
                qualified: r.get(4)?,
                kind: crate::store::SymbolKind::parse(&r.get::<_, String>(5)?),
                start_line: r.get(6)?,
                end_line: r.get(7)?,
                sig: r.get(8)?,
            },
            r.get::<_, String>(9)?,
            r.get::<_, bool>(10)?,
        ))
    })?;

    let mut by_path: BTreeMap<String, Acc> = BTreeMap::new();

    for row in rows {
        let (sym, path, module_decl) = row?;
        // An external module declaration (`mod foo;`) names a file rather
        // than defining what is in it: an exact token matching it must not
        // grant the declaring file the exact-name promotion. An inline
        // `mod foo { … }` defines code here and keeps the promotion. Either
        // form still counts as a plain keyword symbol hit below.
        let is_exact = !module_decl && exact.contains(sym.name.as_str());
        let words = split_ident_words(&sym.name);
        let matched: Vec<&String> = words.iter().filter(|w| kw.contains(w.as_str())).collect();
        if matched.is_empty() && !is_exact {
            continue;
        }
        let acc = by_path.entry(path).or_insert_with(|| Acc {
            symbols: Vec::new(),
            matched_words: BTreeSet::new(),
            symbol_count: 0,
            exact_name_hit: false,
        });
        acc.symbol_count += 1;
        acc.exact_name_hit |= is_exact;
        for w in &matched {
            acc.matched_words.insert((*w).clone());
        }
        if acc.symbols.len() < MAX_SYMBOLS_PER_FILE {
            let matched_kw = if is_exact {
                sym.name.clone()
            } else {
                matched.first().map(|w| (*w).clone()).unwrap_or_default()
            };
            acc.symbols.push((sym, matched_kw));
        }
    }

    let mut out: Vec<SymbolHit> = by_path
        .into_iter()
        .map(|(path, acc)| SymbolHit {
            path,
            distinct_keywords: acc.matched_words.len(),
            exact_name_hit: acc.exact_name_hit,
            symbols: acc.symbols,
        })
        .collect();
    out.sort_by(|a, b| {
        b.exact_name_hit
            .cmp(&a.exact_name_hit)
            .then(b.distinct_keywords.cmp(&a.distinct_keywords))
            .then(b.symbols.len().cmp(&a.symbols.len()))
            .then(a.path.cmp(&b.path))
    });
    Ok(out)
}

/// 1-hop expansion from seed symbol ids over ALL edge kinds: files containing
/// callers (incoming) and callees (outgoing) of the seeds, excluding the seed
/// files themselves. Returns `(path, reason)` preserving seed order, deduped
/// on path keeping the first reason.
pub fn neighbor_files(
    store: &GraphStore,
    seed_symbol_ids: &[i64],
) -> Result<Vec<(String, String)>, StoreError> {
    let mut seed_files: HashSet<i64> = HashSet::new();
    let mut seeds: Vec<(i64, String)> = Vec::new();
    for &id in seed_symbol_ids {
        if let Some(sym) = symbol_by_id(store, id)? {
            seed_files.insert(sym.file_id);
            seeds.push((id, sym.name));
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for (id, name) in &seeds {
        for (edges, label) in [
            (store.edges_to(*id, None)?, "caller of"),
            (store.edges_from(*id, None)?, "callee of"),
        ] {
            // Deterministic within a seed: neighbor symbol id ascending.
            let mut nbrs: Vec<i64> = edges
                .iter()
                .map(|e| {
                    if label == "caller of" {
                        e.src_id
                    } else {
                        e.dst_id
                    }
                })
                .collect();
            nbrs.sort_unstable();
            nbrs.dedup();
            for nbr in nbrs {
                let Some(sym) = symbol_by_id(store, nbr)? else {
                    continue;
                };
                if seed_files.contains(&sym.file_id) {
                    continue;
                }
                let path = file_path_by_id(store, sym.file_id)?;
                if path.is_empty() || !seen.insert(path.clone()) {
                    continue;
                }
                out.push((path, format!("{label} `{name}`")));
            }
        }
    }
    Ok(out)
}

/// Files import-adjacent to the seed files, both directions, via
/// `imports.resolved_file_id`. Returns `(path, reason)`, seed order preserved,
/// deduped on path, seed files excluded.
pub fn import_adjacent_files(
    store: &GraphStore,
    seed_file_paths: &[String],
) -> Result<Vec<(String, String)>, StoreError> {
    let mut seed_ids: Vec<(i64, String)> = Vec::new();
    let mut seed_set: HashSet<String> = HashSet::new();
    for p in seed_file_paths {
        if let Some(f) = store.file_by_path(p)? {
            seed_ids.push((f.id, p.clone()));
            seed_set.insert(p.clone());
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for (fid, seed_path) in &seed_ids {
        // Files this seed imports.
        let mut stmt = store.conn().prepare(
            "SELECT DISTINCT f.path FROM imports i JOIN files f ON f.id = i.resolved_file_id
             WHERE i.file_id = ?1 ORDER BY f.path",
        )?;
        let imported: Vec<String> = stmt
            .query_map(params![fid], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        for p in imported {
            if !seed_set.contains(&p) && seen.insert(p.clone()) {
                out.push((p, format!("imported by {seed_path}")));
            }
        }
        // Files that import this seed.
        let mut stmt = store.conn().prepare(
            "SELECT DISTINCT f.path FROM imports i JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id = ?1 ORDER BY f.path",
        )?;
        let importers: Vec<String> = stmt
            .query_map(params![fid], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        for p in importers {
            if !seed_set.contains(&p) && seen.insert(p.clone()) {
                out.push((p, format!("imports {seed_path}")));
            }
        }
    }
    Ok(out)
}

/// Files sharing a cluster with any seed symbol (excluding the "misc" bucket
/// — it is noise by construction). Reason names the cluster and, when the
/// cluster's stored keywords intersect the task keywords, says so.
pub fn cluster_co_files(
    store: &GraphStore,
    seed_symbol_ids: &[i64],
    task_keywords: &[String],
) -> Result<Vec<(String, String)>, StoreError> {
    let kw: HashSet<&str> = task_keywords.iter().map(String::as_str).collect();
    let mut seed_files: HashSet<i64> = HashSet::new();
    for &id in seed_symbol_ids {
        if let Some(sym) = symbol_by_id(store, id)? {
            seed_files.insert(sym.file_id);
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for &id in seed_symbol_ids {
        let mut stmt = store.conn().prepare(
            "SELECT c.id, c.label, c.keywords FROM clusters c
             JOIN cluster_members cm ON cm.cluster_id = c.id
             WHERE cm.symbol_id = ?1 AND c.label != 'misc' ORDER BY c.id",
        )?;
        let clusters: Vec<(i64, String, String)> = stmt
            .query_map(params![id], |r| {
                Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        for (cid, label, keywords) in clusters {
            let kw_overlap = keywords
                .split(',')
                .map(str::trim)
                .any(|k| !k.is_empty() && kw.contains(k));
            let mut stmt = store.conn().prepare(
                "SELECT DISTINCT f.path FROM cluster_members cm
                 JOIN symbols s ON s.id = cm.symbol_id
                 JOIN files f ON f.id = s.file_id
                 WHERE cm.cluster_id = ?1 ORDER BY f.path",
            )?;
            let paths: Vec<String> = stmt
                .query_map(params![cid], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?;
            for p in paths {
                let in_seed = store
                    .file_by_path(&p)?
                    .is_some_and(|f| seed_files.contains(&f.id));
                if in_seed || !seen.insert(p.clone()) {
                    continue;
                }
                let mut reason = format!("same cluster '{label}'");
                if kw_overlap {
                    reason.push_str(" (cluster keywords match task)");
                }
                out.push((p, reason));
            }
        }
    }
    Ok(out)
}

/// Aggregate epistemic envelope over all matched symbol names — the sum of
/// same-name unresolved call sites (mirrors `GraphStore::envelope_for_name`).
pub fn envelope_for_names(store: &GraphStore, names: &[&str]) -> Result<Envelope, StoreError> {
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    let mut total: u64 = 0;
    for name in unique {
        total += store.envelope_for_name(name)?.unresolved_same_name;
    }
    Ok(Envelope {
        lower_bound: total > 0,
        unresolved_same_name: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};

    fn sym(store: &GraphStore, file_id: i64, file: &str, name: &str) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("{file}#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                1,
                5,
                "",
            )
            .unwrap()
    }

    fn call(store: &GraphStore, src: i64, dst: i64) {
        store
            .insert_edge(&EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
                callee: None,
            })
            .unwrap();
    }

    #[test]
    fn symbol_hits_matches_split_words_and_exact_tokens() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = store
            .replace_file("src/auth/login.ts", "oid", "ts")
            .unwrap();
        let fb = store
            .replace_file("src/util/strings.ts", "oid", "ts")
            .unwrap();
        sym(&store, fa, "src/auth/login.ts", "loginUser");
        sym(&store, fa, "src/auth/login.ts", "login_session");
        sym(&store, fb, "src/util/strings.ts", "padLeft");

        let hits = symbol_hits(
            &store,
            &["login".into(), "session".into()],
            &["loginUser".into()],
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/auth/login.ts");
        assert!(hits[0].exact_name_hit);
        assert_eq!(hits[0].distinct_keywords, 2);
        assert_eq!(hits[0].symbols.len(), 2);
    }

    #[test]
    fn symbol_hits_exact_matches_inline_modules_but_not_external_declarations() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let main = store.replace_file("src/main.rs", "oid", "rs").unwrap();
        let lib = store.replace_file("src/lib.rs", "oid", "rs").unwrap();
        // `mod search_compat;` in main.rs names another file …
        let decl = store
            .insert_symbol(
                main,
                "src/main.rs#search_compat#module",
                "search_compat",
                "search_compat",
                SymbolKind::Module,
                1,
                1,
                "mod search_compat;",
            )
            .unwrap();
        store.mark_module_decl(decl).unwrap();
        // … while `mod search_compat { … }` in lib.rs defines code there.
        store
            .insert_symbol(
                lib,
                "src/lib.rs#search_compat#module",
                "search_compat",
                "search_compat",
                SymbolKind::Module,
                1,
                10,
                "mod search_compat {",
            )
            .unwrap();

        let hits = symbol_hits(
            &store,
            &["search".into(), "compat".into()],
            &["search_compat".into()],
        )
        .unwrap();
        let declaring_file = hits.iter().find(|h| h.path == "src/main.rs").unwrap();
        assert!(
            !declaring_file.exact_name_hit,
            "an external `mod` declaration must not exact-match: {declaring_file:?}"
        );
        let inline_file = hits.iter().find(|h| h.path == "src/lib.rs").unwrap();
        assert!(
            inline_file.exact_name_hit,
            "an inline module defines the code an exact probe names: {inline_file:?}"
        );
    }

    #[test]
    fn neighbor_files_returns_callers_and_callees_excluding_seeds() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
        let fc = store.replace_file("src/c.ts", "oid", "ts").unwrap();
        let a = sym(&store, fa, "src/a.ts", "alpha");
        let b = sym(&store, fb, "src/b.ts", "beta");
        let c = sym(&store, fc, "src/c.ts", "gamma");
        call(&store, b, a); // b calls a  → b is a caller of alpha
        call(&store, a, c); // a calls c  → c is a callee of alpha

        let nbrs = neighbor_files(&store, &[a]).unwrap();
        assert_eq!(nbrs.len(), 2);
        assert_eq!(nbrs[0], ("src/b.ts".into(), "caller of `alpha`".into()));
        assert_eq!(nbrs[1], ("src/c.ts".into(), "callee of `alpha`".into()));
    }

    #[test]
    fn import_adjacency_both_directions() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
        let fc = store.replace_file("src/c.ts", "oid", "ts").unwrap();
        // a imports b; c imports a.
        store.insert_import(fa, "./b", Some(fb), &[]).unwrap();
        store.insert_import(fc, "./a", Some(fa), &[]).unwrap();

        let adj = import_adjacent_files(&store, &["src/a.ts".into()]).unwrap();
        assert_eq!(adj.len(), 2);
        assert_eq!(adj[0], ("src/b.ts".into(), "imported by src/a.ts".into()));
        assert_eq!(adj[1], ("src/c.ts".into(), "imports src/a.ts".into()));
    }

    #[test]
    fn cluster_co_files_skips_misc_and_seed_files() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
        let fm = store.replace_file("src/m.ts", "oid", "ts").unwrap();
        let a = sym(&store, fa, "src/a.ts", "alpha");
        let b = sym(&store, fb, "src/b.ts", "beta");
        let m = sym(&store, fm, "src/m.ts", "mu");
        let conn = store.conn();
        conn.execute(
            "INSERT INTO clusters (id, label, cohesion, keywords) VALUES (1, 'auth', 0.9, 'login,session')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clusters (id, label, cohesion, keywords) VALUES (2, 'misc', 0.1, '')",
            [],
        )
        .unwrap();
        for (cid, sid) in [(1, a), (1, b), (2, m)] {
            conn.execute(
                "INSERT INTO cluster_members (cluster_id, symbol_id) VALUES (?1, ?2)",
                params![cid, sid],
            )
            .unwrap();
        }

        let co = cluster_co_files(&store, &[a], &["login".into()]).unwrap();
        assert_eq!(co.len(), 1);
        assert_eq!(co[0].0, "src/b.ts");
        assert!(co[0].1.contains("same cluster 'auth'"));
        assert!(co[0].1.contains("keywords match task"));
    }

    #[test]
    fn envelope_sums_unresolved_over_names() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        store
            .insert_unresolved_call(fa, "alpha", None, 3, None, "calls")
            .unwrap();
        store
            .insert_unresolved_call(fa, "alpha", None, 9, None, "calls")
            .unwrap();
        store
            .insert_unresolved_call(fa, "beta", None, 4, None, "calls")
            .unwrap();

        let env = envelope_for_names(&store, &["alpha", "beta", "alpha"]).unwrap();
        assert!(env.lower_bound);
        assert_eq!(env.unresolved_same_name, 3);

        let env = envelope_for_names(&store, &["nope"]).unwrap();
        assert!(!env.lower_bound);
        assert_eq!(env.unresolved_same_name, 0);
    }
}
