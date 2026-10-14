// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The two reads of the code graph the question-kind packs need and no
//! daemon op answers read-only: the map of the repository (its indexed files
//! with their top-level declarations) and the calls a function makes,
//! followed in the order the source makes them.
//!
//! Both read the published graph through the connection of a store the caller
//! has already proven current (`Live::with_graph`); neither writes. The
//! daemon's `list-areas` and `list-flows` persist what they compute on first
//! use, and `repo-map` answers with every symbol of up to two thousand files,
//! so none of them can run inside a prompt hook.

use std::collections::HashSet;

use pixel_graph::GraphStore;
use pixel_graph::store::EdgeKind;

use super::chain::SymbolHit;
use super::kinds::{FlowNode, MapFile, RepoMap, TopSymbol};

/// Declarations of the top level that tell what a module is about: types
/// and functions, not fields, variants, constants or the synthetic scope of
/// a script.
const TOP_KINDS: &str = "'class','struct','enum','trait','interface','function','module'";
/// Top-level declarations read at most; a repository with more keeps the
/// ones of the first files in path order.
const MAX_TOPS: usize = 20_000;
/// Files read at most.
const MAX_FILES: usize = 20_000;
/// Calls followed out of one function.
const FANOUT: usize = 5;

fn sql(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// The map of the repository as the graph has it.
pub(crate) fn read_repo_map(store: &GraphStore) -> Result<RepoMap, String> {
    let conn = store.conn();
    let mut files: Vec<MapFile> = Vec::new();
    let mut statement = conn
        .prepare(
            "SELECT f.path, f.lang, COUNT(s.id) FROM files f
             LEFT JOIN symbols s ON s.file_id = f.id
             GROUP BY f.id ORDER BY f.path LIMIT ?1",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([MAX_FILES as i64], |row| {
            Ok(MapFile {
                path: row.get(0)?,
                lang: row.get(1)?,
                symbols: usize::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
            })
        })
        .map_err(sql)?;
    for row in rows {
        files.push(row.map_err(sql)?);
    }
    let symbols = files.iter().map(|file| file.symbols).sum();
    let tops = read_tops(
        store,
        &format!(
            "SELECT f.path, s.name, s.kind, s.start_line, s.end_line, s.sig
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.qualified = s.name AND s.kind IN ({TOP_KINDS})
             ORDER BY f.path, s.start_line LIMIT {MAX_TOPS}"
        ),
    )?;
    let mains = read_tops(
        store,
        "SELECT f.path, s.name, s.kind, s.start_line, s.end_line, s.sig
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = 'main' AND s.kind = 'function'
         ORDER BY f.path, s.start_line LIMIT 40",
    )?;
    Ok(RepoMap {
        files,
        symbols,
        tops,
        mains,
    })
}

fn read_tops(store: &GraphStore, query: &str) -> Result<Vec<TopSymbol>, String> {
    let mut statement = store.conn().prepare(query).map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok(TopSymbol {
                path: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get::<_, i64>(3)?.max(0) as u64,
                end: row.get::<_, i64>(4)?.max(0) as u64,
                sig: row.get(5)?,
            })
        })
        .map_err(sql)?;
    rows.map(|row| row.map_err(sql)).collect()
}

/// Functions and methods whose name contains any of `terms` (in any case),
/// at most `limit`, in path order.
pub(crate) fn read_symbols_like(
    store: &GraphStore,
    terms: &[String],
    limit: usize,
) -> Result<Vec<SymbolHit>, String> {
    let words: Vec<&String> = terms.iter().filter(|term| !term.is_empty()).collect();
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let likes: Vec<String> = (1..=words.len())
        .map(|n| format!("s.name LIKE ?{n} ESCAPE '\\'"))
        .collect();
    let query = format!(
        "SELECT s.uid, s.name, s.kind, f.path, s.start_line, s.end_line
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.kind IN ('function','method') AND ({})
         ORDER BY f.path, s.start_line LIMIT {limit}",
        likes.join(" OR ")
    );
    let patterns: Vec<String> = words
        .iter()
        .map(|term| {
            let escaped = term
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        })
        .collect();
    let mut statement = store.conn().prepare(&query).map_err(sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(patterns.iter()), |row| {
            Ok(SymbolHit {
                uid: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                path: row.get(3)?,
                start_line: row.get::<_, i64>(4)?.max(0) as u64,
                end_line: row.get::<_, i64>(5)?.max(0) as u64,
            })
        })
        .map_err(sql)?;
    rows.map(|row| row.map_err(sql)).collect()
}

/// The declaration `uid` names, or the only one called that.
fn root_symbol(store: &GraphStore, uid: &str) -> Result<i64, String> {
    if let Some(row) = store.symbol_by_uid(uid).map_err(sql)? {
        return Ok(row.id);
    }
    let rows = store.symbols_by_name(uid, None, 2).map_err(sql)?;
    match rows.as_slice() {
        [only] => Ok(only.id),
        [] => Err(format!("no symbol named '{uid}'")),
        _ => Err(format!("'{uid}' names more than one symbol")),
    }
}

/// The node for symbol `id`, at `depth` below `parent`, or `None` for an id
/// no row carries.
fn node(
    store: &GraphStore,
    id: i64,
    depth: usize,
    parent: Option<usize>,
) -> Result<Option<FlowNode>, String> {
    let mut statement = store
        .conn()
        .prepare(
            "SELECT s.uid, s.name, s.kind, s.start_line, s.end_line, f.path
             FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id = ?1",
        )
        .map_err(sql)?;
    let mut rows = statement
        .query_map([id], |row| {
            Ok(FlowNode {
                depth,
                parent,
                uid: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                start: row.get::<_, i64>(3)?.max(0) as u64,
                end: row.get::<_, i64>(4)?.max(0) as u64,
                path: row.get(5)?,
            })
        })
        .map_err(sql)?;
    rows.next().transpose().map_err(sql)
}

/// The calls out of `uid`, in the order the source makes them and depth
/// first, as a pre-order list of at most `limit` nodes `depth` levels deep:
/// the function itself, its first call and what that calls, its second call.
/// A function reached twice is listed once, a call into a test file is left
/// out, and no function is followed into more than [`FANOUT`] calls.
pub(crate) fn read_callee_tree(
    store: &GraphStore,
    uid: &str,
    depth: usize,
    limit: usize,
) -> Result<Vec<FlowNode>, String> {
    let root = root_symbol(store, uid)?;
    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut stack: Vec<(i64, usize, Option<usize>)> = vec![(root, 0, None)];
    while let Some((id, level, parent)) = stack.pop() {
        if nodes.len() >= limit || !seen.insert(id) {
            continue;
        }
        let Some(here) = node(store, id, level, parent)? else {
            continue;
        };
        if level > 0 && super::answer::is_test_path(&here.path) {
            continue;
        }
        let index = nodes.len();
        nodes.push(here);
        if level >= depth {
            continue;
        }
        let mut calls = store.edges_from(id, Some(EdgeKind::Calls)).map_err(sql)?;
        calls.sort_by_key(|edge| edge.site_line);
        let mut targets: Vec<i64> = Vec::new();
        for edge in calls {
            if edge.dst_id != id && !seen.contains(&edge.dst_id) && !targets.contains(&edge.dst_id)
            {
                targets.push(edge.dst_id);
            }
        }
        targets.truncate(FANOUT);
        // Pushed in reverse so the first call is popped first.
        for target in targets.into_iter().rev() {
            stack.push((target, level + 1, Some(index)));
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::store::{EdgeRow, SymbolKind, Tier};

    /// A store with `files` as `(path, lang)` and `symbols` as `(file index,
    /// name, qualified, kind, start, end, sig)`.
    fn store(
        files: &[(&str, &str)],
        symbols: &[(usize, &str, &str, SymbolKind, u32, u32, &str)],
    ) -> (GraphStore, Vec<i64>) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let ids: Vec<i64> = files
            .iter()
            .map(|(path, lang)| store.replace_file(path, "oid", lang).unwrap())
            .collect();
        let mut symbol_ids = Vec::new();
        for (file, name, qualified, kind, start, end, sig) in symbols {
            let uid = format!("{}#{qualified}#{}", files[*file].0, kind.as_str());
            symbol_ids.push(
                store
                    .insert_symbol(ids[*file], &uid, name, qualified, *kind, *start, *end, sig)
                    .unwrap(),
            );
        }
        (store, symbol_ids)
    }

    fn call(store: &GraphStore, from: i64, to: i64, line: u32) {
        store
            .insert_edge(&EdgeRow {
                src_id: from,
                dst_id: to,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: line,
                receiver: None,
                callee: None,
            })
            .unwrap();
    }

    #[test]
    fn read_repo_map_should_count_symbols_per_file_and_keep_the_top_level_only() {
        let (store, _) = store(
            &[
                ("src/a.py", "python"),
                ("src/b.py", "python"),
                ("docs.py", "python"),
            ],
            &[
                (
                    0,
                    "Session",
                    "Session",
                    SymbolKind::Class,
                    10,
                    90,
                    "class Session:",
                ),
                (
                    0,
                    "send",
                    "Session.send",
                    SymbolKind::Method,
                    20,
                    30,
                    "def send(self):",
                ),
                (
                    0,
                    "merge",
                    "merge",
                    SymbolKind::Function,
                    100,
                    110,
                    "def merge():",
                ),
                (1, "main", "main", SymbolKind::Function, 3, 9, "def main():"),
                (1, "LIMIT", "LIMIT", SymbolKind::Const, 1, 1, "LIMIT = 3"),
            ],
        );
        let map = read_repo_map(&store).unwrap();
        let paths: Vec<&str> = map.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["docs.py", "src/a.py", "src/b.py"]);
        let counts: Vec<usize> = map.files.iter().map(|file| file.symbols).collect();
        assert_eq!(counts, [0, 3, 2]);
        assert_eq!(map.symbols, 5);
        let tops: Vec<(&str, &str)> = map
            .tops
            .iter()
            .map(|top| (top.path.as_str(), top.name.as_str()))
            .collect();
        // A method and a constant are not a module's face.
        assert_eq!(
            tops,
            [
                ("src/a.py", "Session"),
                ("src/a.py", "merge"),
                ("src/b.py", "main")
            ]
        );
        assert_eq!(map.tops[0].line, 10);
        assert_eq!(map.tops[0].end, 90);
        assert_eq!(map.mains.len(), 1);
        assert_eq!(
            (map.mains[0].path.as_str(), map.mains[0].line),
            ("src/b.py", 3)
        );
    }

    #[test]
    fn read_repo_map_should_answer_an_empty_graph_with_an_empty_map() {
        let map = read_repo_map(&GraphStore::open_in_memory().unwrap()).unwrap();
        assert!(map.files.is_empty() && map.tops.is_empty() && map.mains.is_empty());
        assert_eq!(map.symbols, 0);
    }

    fn chain_store() -> (GraphStore, Vec<i64>) {
        // handle -> parse -> tokenize, handle -> render -> tokenize,
        // handle -> check (in a test file), parse -> handle (a cycle).
        let (store, ids) = store(
            &[
                ("src/h.ts", "ts"),
                ("src/p.ts", "ts"),
                ("tests/h.test.ts", "ts"),
            ],
            &[
                (
                    0,
                    "handle",
                    "handle",
                    SymbolKind::Function,
                    1,
                    20,
                    "function handle()",
                ),
                (
                    1,
                    "parse",
                    "parse",
                    SymbolKind::Function,
                    1,
                    9,
                    "function parse()",
                ),
                (
                    1,
                    "tokenize",
                    "tokenize",
                    SymbolKind::Function,
                    10,
                    19,
                    "function tokenize()",
                ),
                (
                    0,
                    "render",
                    "render",
                    SymbolKind::Function,
                    21,
                    30,
                    "function render()",
                ),
                (
                    2,
                    "check",
                    "check",
                    SymbolKind::Function,
                    1,
                    5,
                    "function check()",
                ),
            ],
        );
        call(&store, ids[0], ids[3], 12);
        call(&store, ids[0], ids[1], 4);
        call(&store, ids[0], ids[4], 15);
        call(&store, ids[1], ids[2], 5);
        call(&store, ids[1], ids[0], 7);
        call(&store, ids[3], ids[2], 22);
        (store, ids)
    }

    #[test]
    fn read_callee_tree_should_follow_calls_in_source_order_depth_first() {
        let (store, _) = chain_store();
        let tree = read_callee_tree(&store, "src/h.ts#handle#function", 3, 20).unwrap();
        let shape: Vec<(usize, &str)> = tree
            .iter()
            .map(|node| (node.depth, node.name.as_str()))
            .collect();
        // `parse` (line 4) before `render` (line 12); `tokenize` once, under
        // its first caller; the call into a test file and the cycle back to
        // `handle` are left out.
        assert_eq!(
            shape,
            [(0, "handle"), (1, "parse"), (2, "tokenize"), (1, "render")]
        );
        assert_eq!(tree[2].parent, Some(1));
        assert_eq!(tree[3].parent, Some(0));
        assert_eq!(
            (tree[0].path.as_str(), tree[0].start, tree[0].end),
            ("src/h.ts", 1, 20)
        );
        assert_eq!(tree[0].kind, "function");
    }

    #[test]
    fn read_callee_tree_should_stop_at_the_depth_and_the_limit_and_resolve_a_name() {
        let (store, _) = chain_store();
        let shallow = read_callee_tree(&store, "handle", 1, 20).unwrap();
        assert_eq!(
            shallow
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["handle", "parse", "render"]
        );
        let short = read_callee_tree(&store, "handle", 3, 2).unwrap();
        assert_eq!(short.len(), 2);
        assert_eq!(read_callee_tree(&store, "handle", 0, 5).unwrap().len(), 1);
        assert!(
            read_callee_tree(&store, "nowhere", 1, 5)
                .unwrap_err()
                .contains("no symbol")
        );
    }

    #[test]
    fn read_callee_tree_should_refuse_an_ambiguous_name() {
        let (store, _) = store(
            &[("a.rs", "rust"), ("b.rs", "rust")],
            &[
                (0, "run", "run", SymbolKind::Function, 1, 2, "fn run()"),
                (1, "run", "run", SymbolKind::Function, 1, 2, "fn run()"),
            ],
        );
        assert!(
            read_callee_tree(&store, "run", 1, 5)
                .unwrap_err()
                .contains("more than one")
        );
    }
}
