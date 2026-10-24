// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The reads of the code graph the question-kind packs need and no wire op
//! answers read-only: the map of the repository (its indexed files with their
//! top-level declarations), the functions whose name holds a word, and the
//! calls a function makes, followed in the order the source makes them.
//!
//! All of them read the published generation through a [`Store`]; none writes.
//! `map` and the persisted analyses answer with every symbol of up to two
//! thousand files, so none of them could run inside a prompt hook.

use std::collections::{HashMap, HashSet};

use pixel_brief::chain::SymbolHit;
use pixel_brief::kinds::{FlowNode, MapFile, RepoMap, TopSymbol};
use pixel_graph::store::Store;
use pixel_model::{EdgeKind, Symbol, SymbolKind, Tier, is_test_path, leaf};

/// Top-level declarations read at most; a repository with more keeps the
/// ones of the first files in path order.
const MAX_TOPS: usize = 20_000;
/// Files read at most.
const MAX_FILES: usize = 20_000;
/// `main` functions listed at most.
const MAX_MAINS: usize = 40;
/// Calls followed out of one function.
const FANOUT: usize = 5;

/// Declarations of the top level that tell what a module is about: types and
/// functions, not fields, variants, constants or the synthetic scope of a
/// script.
fn is_top_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Function
            | SymbolKind::Module
    )
}

/// A name written at the top level, with no owner before it.
fn is_unqualified(name: &str) -> bool {
    leaf(name) == name
}

/// Every declaration of the generation: its symbols without the `File` and
/// `Package` rows, which stand for a file or a manifest and name nothing a
/// prompt asks about (their names are paths).
pub(super) fn declarations(store: &Store) -> Result<Vec<Symbol>, String> {
    let mut symbols = store.all_symbols().map_err(sql)?;
    symbols.retain(|symbol| !matches!(symbol.kind, SymbolKind::File | SymbolKind::Package));
    Ok(symbols)
}

fn top(symbol: &Symbol) -> TopSymbol {
    TopSymbol {
        path: symbol.file.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind.as_str().to_owned(),
        line: u64::from(symbol.line),
        end: u64::from(symbol.end_line),
        sig: symbol.signature.clone(),
    }
}

fn sql(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// The map of the repository as the graph has it.
pub(super) fn read_repo_map(store: &Store) -> Result<RepoMap, String> {
    let rows = store.file_rows().map_err(sql)?;
    let symbols = declarations(store)?;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for symbol in &symbols {
        *counts.entry(symbol.file.as_str()).or_default() += 1;
    }
    let files: Vec<MapFile> = rows
        .iter()
        .take(MAX_FILES)
        .map(|row| MapFile {
            path: row.path.clone(),
            lang: row.lang.as_str().to_owned(),
            symbols: counts.get(row.path.as_str()).copied().unwrap_or(0),
        })
        .collect();
    let total = files.iter().map(|file| file.symbols).sum();
    let tops = symbols
        .iter()
        .filter(|symbol| is_top_kind(symbol.kind) && is_unqualified(&symbol.name))
        .take(MAX_TOPS)
        .map(top)
        .collect();
    let mains = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function && symbol.name == "main")
        .take(MAX_MAINS)
        .map(top)
        .collect();
    Ok(RepoMap {
        files,
        symbols: total,
        tops,
        mains,
    })
}

/// Functions and methods whose name contains any of `terms` (in any case),
/// at most `limit`, in path order.
pub(super) fn read_symbols_like(
    store: &Store,
    terms: &[String],
    limit: usize,
) -> Result<Vec<SymbolHit>, String> {
    let words: Vec<String> = terms
        .iter()
        .filter(|term| !term.is_empty())
        .map(|term| term.to_ascii_lowercase())
        .collect();
    if words.is_empty() {
        return Ok(Vec::new());
    }
    Ok(declarations(store)?
        .into_iter()
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
        .filter(|symbol| {
            let name = leaf(&symbol.name).to_ascii_lowercase();
            words.iter().any(|word| name.contains(word.as_str()))
        })
        .take(limit)
        .map(|symbol| hit(&symbol))
        .collect())
}

/// The brief's view of a declaration.
pub(super) fn hit(symbol: &Symbol) -> SymbolHit {
    SymbolHit {
        uid: symbol.uid.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind.as_str().to_owned(),
        path: symbol.file.clone(),
        start_line: u64::from(symbol.line),
        end_line: u64::from(symbol.end_line),
    }
}

/// The declaration `token` names: a uid, or the only symbol called that (a
/// qualified name included).
pub(super) fn root_symbol(store: &Store, token: &str) -> Result<Symbol, String> {
    if let Some(symbol) = store.symbol(token).map_err(sql)? {
        return Ok(symbol);
    }
    let mut named = store.named_symbols(token).map_err(sql)?;
    match named.len() {
        1 => Ok(named.remove(0)),
        0 => Err(format!("no symbol named '{token}'")),
        _ => Err(format!("'{token}' names more than one symbol")),
    }
}

fn node(symbol: &Symbol, depth: usize, parent: Option<usize>) -> FlowNode {
    FlowNode {
        depth,
        parent,
        uid: symbol.uid.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind.as_str().to_owned(),
        path: symbol.file.clone(),
        start: u64::from(symbol.line),
        end: u64::from(symbol.end_line),
    }
}

/// The calls out of `uid`, in the order the source makes them and depth
/// first, as a pre-order list of at most `limit` nodes `depth` levels deep:
/// the function itself, its first call and what that calls, its second call.
/// A function reached twice is listed once, a call into a test file is left
/// out, and no function is followed into more than [`FANOUT`] calls. An edge
/// at any tier that names a target is followed, as a `Likely` one is a
/// labelled guess the brief lists like the rest.
pub(super) fn read_callee_tree(
    store: &Store,
    uid: &str,
    depth: usize,
    limit: usize,
) -> Result<Vec<FlowNode>, String> {
    let root = root_symbol(store, uid)?;
    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<(Symbol, usize, Option<usize>)> = vec![(root, 0, None)];
    while let Some((symbol, level, parent)) = stack.pop() {
        if nodes.len() >= limit || !seen.insert(symbol.uid.clone()) {
            continue;
        }
        if level > 0 && is_test_path(&symbol.file) {
            continue;
        }
        let index = nodes.len();
        nodes.push(node(&symbol, level, parent));
        if level >= depth {
            continue;
        }
        let mut targets: Vec<Symbol> = Vec::new();
        for edge in store.callees(&symbol.uid).map_err(sql)? {
            if edge.kind != EdgeKind::Call || edge.tier == Tier::Open {
                continue;
            }
            let Some(dst) = edge.dst else { continue };
            if dst == symbol.uid
                || seen.contains(&dst)
                || targets.iter().any(|held| held.uid == dst)
            {
                continue;
            }
            if let Some(target) = store.symbol(&dst).map_err(sql)? {
                targets.push(target);
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
    use crate::testutil::{open, repo};

    const FILES: &[(&str, &str)] = &[
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod h;\npub mod p;\npub mod tests;\n"),
        (
            "src/h.rs",
            "use crate::p::parse;\n\npub fn handle() {\n    parse();\n    render();\n    crate::tests::util::check();\n}\n\npub fn render() {\n    crate::p::tokenize();\n}\n",
        ),
        (
            "src/p.rs",
            "use crate::h::handle;\n\npub fn parse() {\n    tokenize();\n    handle();\n}\n\npub fn tokenize() {}\n\npub struct Parser;\n\nimpl Parser {\n    pub fn feed(&self) {}\n}\n\nfn main() {}\n",
        ),
        ("src/tests/mod.rs", "pub mod util;\n"),
        ("src/tests/util.rs", "pub fn check() {}\n"),
    ];

    fn store() -> (tempfile::TempDir, Store) {
        let (dir, root) = repo(FILES);
        let (_, store) = open(&root);
        (dir, store)
    }

    #[test]
    fn the_map_counts_declarations_per_file_and_keeps_the_top_level_only() {
        let (_dir, store) = store();
        let map = read_repo_map(&store).unwrap();
        let count = |path: &str| {
            map.files
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.symbols)
        };
        assert_eq!(count("src/h.rs"), Some(2));
        assert_eq!(
            count("src/p.rs"),
            Some(5),
            "the file row is not a declaration"
        );
        assert_eq!(count("Cargo.toml"), Some(0));
        assert_eq!(
            map.symbols,
            map.files.iter().map(|file| file.symbols).sum::<usize>()
        );
        let tops: Vec<(&str, &str)> = map
            .tops
            .iter()
            .filter(|top| top.path == "src/p.rs")
            .map(|top| (top.name.as_str(), top.kind.as_str()))
            .collect();
        // A method is not a module's face.
        assert_eq!(
            tops,
            [
                ("parse", "function"),
                ("tokenize", "function"),
                ("Parser", "struct"),
                ("main", "function")
            ]
        );
        assert_eq!(
            map.mains
                .iter()
                .map(|top| top.path.as_str())
                .collect::<Vec<_>>(),
            ["src/p.rs"]
        );
        let parse = map.tops.iter().find(|top| top.name == "parse").unwrap();
        assert_eq!((parse.line, parse.end), (3, 6));
        assert_eq!(parse.sig, "pub fn parse() {");
    }

    #[test]
    fn functions_are_found_by_a_word_of_their_name_in_any_case() {
        let (_dir, store) = store();
        let found = read_symbols_like(&store, &["TOKEN".to_owned()], 5).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|hit| hit.name.as_str())
                .collect::<Vec<_>>(),
            ["tokenize"]
        );
        let methods = read_symbols_like(&store, &["feed".to_owned()], 5).unwrap();
        assert_eq!(methods.len(), 1, "a method counts, by its last segment");
        assert!(
            read_symbols_like(&store, &[String::new()], 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            read_symbols_like(&store, &["a".to_owned()], 2)
                .unwrap()
                .len(),
            2,
            "cut at the limit"
        );
    }

    #[test]
    fn the_callee_tree_follows_calls_in_source_order_depth_first() {
        let (_dir, store) = store();
        let tree = read_callee_tree(&store, "src/h.rs#handle#function", 3, 20).unwrap();
        let shape: Vec<(usize, &str)> = tree
            .iter()
            .map(|node| (node.depth, node.name.as_str()))
            .collect();
        // `parse` (line 4) before `render` (line 5); `tokenize` once, under
        // its first caller; the call into a test file and the cycle back to
        // `handle` are left out.
        assert_eq!(
            shape,
            [(0, "handle"), (1, "parse"), (2, "tokenize"), (1, "render")]
        );
        assert_eq!((tree[2].parent, tree[3].parent), (Some(1), Some(0)));
        assert_eq!(
            (
                tree[0].path.as_str(),
                tree[0].start,
                tree[0].end,
                tree[0].kind.as_str()
            ),
            ("src/h.rs", 3, 7, "function")
        );
    }

    #[test]
    fn the_callee_tree_stops_at_the_depth_and_the_limit_and_resolves_a_name() {
        let (_dir, store) = store();
        let names = |tree: Vec<FlowNode>| -> Vec<String> {
            tree.into_iter().map(|node| node.name).collect()
        };
        assert_eq!(
            names(read_callee_tree(&store, "handle", 1, 20).unwrap()),
            ["handle", "parse", "render"]
        );
        assert_eq!(read_callee_tree(&store, "handle", 3, 2).unwrap().len(), 2);
        assert_eq!(read_callee_tree(&store, "handle", 0, 5).unwrap().len(), 1);
        assert!(
            read_callee_tree(&store, "nowhere", 1, 5)
                .unwrap_err()
                .contains("no symbol")
        );
    }

    #[test]
    fn a_name_two_declarations_share_is_refused_and_a_qualified_one_resolves() {
        let (_dir, root) = repo(&[
            (
                "a.rs",
                "pub struct Log;\nimpl Log {\n    pub fn finish(&self) {}\n}\n",
            ),
            (
                "b.rs",
                "pub struct Hasher;\nimpl Hasher {\n    pub fn finish(&self) {}\n}\n",
            ),
        ]);
        let (_, store) = open(&root);
        assert!(
            read_callee_tree(&store, "finish", 1, 5)
                .unwrap_err()
                .contains("more than one")
        );
        let tree = read_callee_tree(&store, "Log::finish", 1, 5).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].path, "a.rs");
    }
}
