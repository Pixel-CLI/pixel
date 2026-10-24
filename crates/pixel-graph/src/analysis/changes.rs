// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Which symbols a diff touches (`specs/graph.md`, "Analyses").
//!
//! A hunk is mapped to the symbols whose *own* lines it meets: a method's
//! lines belong to the method, not to the class around it, so a one-line
//! edit in a method names the method and not the class. A change that meets
//! the lines of a symbol's declaration header sets `signature_changed`, the
//! case callers can be stranded by. For each symbol the test files within
//! three callers are listed.

use std::path::Path;

use pixel_model::{Diff, DiffFile, Symbol, SymbolKind};

use crate::edit::covering_tests;
use crate::lang;
use crate::shape::header_lines;
use crate::store::{Store, StoreError};

/// A symbol a diff touches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub symbol: Symbol,
    /// The change meets the declaration header, not only the body.
    pub signature_changed: bool,
    /// Test files within [`crate::edit::TEST_DEPTH`] callers, sorted.
    pub tests: Vec<String>,
}

/// A changed source file the generation has no rows for: its changes cannot
/// be attributed to any symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unindexed {
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// By file, then line.
    pub symbols: Vec<Changed>,
    pub unindexed: Vec<Unindexed>,
}

/// The symbols of `diff` in the generation `store` holds, reading each
/// changed file under `root` for the length of its declaration headers.
///
/// The generation may be older than the working tree: a symbol is found
/// where the generation put it. The edit hooks refresh before they ask.
pub fn changes(store: &Store, root: &Path, diff: &Diff) -> Result<Changes, StoreError> {
    let mut out = Changes::default();
    for file in &diff.files {
        let Some(language) = lang::wanted(&file.path) else {
            continue;
        };
        let symbols: Vec<Symbol> = store
            .symbols_in(&file.path)?
            .into_iter()
            .filter(|s| !matches!(s.kind, SymbolKind::File | SymbolKind::Package))
            .collect();
        if symbols.is_empty() {
            // A file with no definition has nothing to attribute; one that
            // is not indexed at all is said so.
            if store.symbols_in(&file.path)?.is_empty() {
                out.unindexed.push(Unindexed {
                    path: file.path.clone(),
                });
            }
            continue;
        }
        let content = std::fs::read(root.join(&file.path)).ok();
        let source = content.as_deref().map(String::from_utf8_lossy);
        for symbol in &symbols {
            if !touches(symbol, &symbols, file) {
                continue;
            }
            let header = source.as_deref().map_or(1, |source| {
                header_lines(language, source, symbol.line, symbol.end_line)
            });
            let header_end = symbol.line + header - 1;
            out.symbols.push(Changed {
                symbol: symbol.clone(),
                signature_changed: file
                    .hunks
                    .iter()
                    .any(|&(a, b)| a <= header_end && symbol.line <= b),
                tests: covering_tests(store, symbol)?,
            });
        }
    }
    out.symbols
        .sort_by(|a, b| (&a.symbol.file, a.symbol.line).cmp(&(&b.symbol.file, b.symbol.line)));
    Ok(out)
}

/// Whether a hunk of `file` meets lines of `symbol` that no symbol nested in
/// it owns.
fn touches(symbol: &Symbol, all: &[Symbol], file: &DiffFile) -> bool {
    let mut children: Vec<(u32, u32)> = all
        .iter()
        .filter(|other| {
            other.uid != symbol.uid
                && symbol.line <= other.line
                && other.end_line <= symbol.end_line
                && (other.line, other.end_line) != (symbol.line, symbol.end_line)
        })
        .map(|other| (other.line, other.end_line))
        .collect();
    children.sort_unstable();
    for &(a, b) in &file.hunks {
        let (mut from, to) = (a.max(symbol.line), b.min(symbol.end_line));
        // Walk `from` past the children that cover it; what is left of the
        // range is the symbol's own.
        for &(child_start, child_end) in &children {
            if from > to {
                break;
            }
            if child_start <= from && from <= child_end {
                from = child_end.saturating_add(1);
            }
        }
        if from <= to {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use pixel_model::DiffFile;

    use super::*;
    use crate::analysis::fixture::project;

    fn diff(path: &str, hunks: &[(u32, u32)]) -> Diff {
        Diff {
            files: vec![DiffFile {
                path: path.to_owned(),
                hunks: hunks.to_vec(),
                added: hunks.to_vec(),
            }],
        }
    }

    fn names(found: &Changes) -> Vec<(&str, bool)> {
        found
            .symbols
            .iter()
            .map(|c| (c.symbol.name.as_str(), c.signature_changed))
            .collect()
    }

    #[test]
    fn a_body_edit_names_the_function_and_leaves_its_signature_alone() {
        let (repo, store) = project();
        let found = changes(&store, repo.path(), &diff("src/a.rs", &[(2, 2)])).unwrap();
        assert_eq!(names(&found), [("run", false)]);
        assert_eq!(found.symbols[0].tests, ["tests/it.rs"]);
    }

    #[test]
    fn an_edit_on_the_header_line_changes_the_signature() {
        let (repo, store) = project();
        let found = changes(&store, repo.path(), &diff("src/a.rs", &[(1, 1)])).unwrap();
        assert_eq!(names(&found), [("run", true)]);
    }

    #[test]
    fn a_change_outside_every_symbol_or_in_a_file_with_none_names_nothing() {
        let (repo, store) = project();
        let found = changes(&store, repo.path(), &diff("src/lib.rs", &[(1, 4)])).unwrap();
        assert!(found.symbols.is_empty());
        assert!(found.unindexed.is_empty());
        let found = changes(&store, repo.path(), &diff("README.md", &[(1, 1)])).unwrap();
        assert_eq!(found, Changes::default());
    }

    #[test]
    fn a_source_file_the_generation_has_not_seen_is_unindexed() {
        let (repo, store) = project();
        let found = changes(&store, repo.path(), &diff("src/new.rs", &[(1, 9)])).unwrap();
        assert_eq!(
            found.unindexed,
            [Unindexed {
                path: "src/new.rs".to_owned()
            }]
        );
    }

    #[test]
    fn a_hunk_in_a_method_names_the_method_and_not_the_type_around_it() {
        let repo = crate::testutil::repo_with(&[
            ("Cargo.toml", crate::analysis::fixture::MANIFEST),
            (
                "src/lib.rs",
                "pub struct S;\n\nimpl S {\n    pub fn one(&self) -> u32 {\n        1\n    }\n\n    pub fn two(&self) -> u32 {\n        2\n    }\n}\n",
            ),
        ]);
        crate::refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let found = changes(&store, repo.path(), &diff("src/lib.rs", &[(9, 9)])).unwrap();
        assert_eq!(names(&found), [("S::two", false)]);
        let wide = changes(&store, repo.path(), &diff("src/lib.rs", &[(1, 11)])).unwrap();
        let wide_names: Vec<&str> = wide
            .symbols
            .iter()
            .map(|c| c.symbol.name.as_str())
            .collect();
        assert_eq!(wide_names, ["S", "S::one", "S::two"]);
    }

    #[test]
    fn a_hunk_that_runs_to_the_end_of_the_file_is_clamped() {
        let (repo, store) = project();
        let found = changes(&store, repo.path(), &diff("src/c.rs", &[(1, u32::MAX)])).unwrap();
        assert_eq!(names(&found), [("other", true)]);
    }
}
