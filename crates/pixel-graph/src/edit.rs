// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What an edit to one file does to the code that depends on it
//! (`specs/graph.md`, "New API for the edit hooks"; `specs/hooks.md`).
//!
//! [`check`] re-extracts the file in memory (one parse, no link, nothing
//! published), compares the declaration headers with the ones the
//! generation stored (`shape::diff_shapes`) and, for each symbol whose shape
//! changed or that went away, reads its callers from the generation. A
//! uid carries no line number, so a caller stored in another file keeps
//! pointing at the edited symbol across the re-extract.
//!
//! The answer is a lower bound: a site no rule could resolve is counted in
//! [`SymbolEdit::open_sites`], never listed as a caller.

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::Path;

use pixel_model::{Symbol, Tier, is_test_path, leaf};

use crate::shape::{ShapeChange, diff_shapes};
use crate::store::{Store, StoreError};
use crate::{extract, lang};

/// The largest file an edit check reads: past it the check is silent, as the
/// indexer treats such a file as unread and it has no shapes to compare.
pub const EDIT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How far up the callers the covering tests are looked for.
pub const TEST_DEPTH: usize = 3;

/// The most symbols visited while looking for covering tests.
const TEST_VISIT_CAP: usize = 400;

/// A site that reaches an edited symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub file: String,
    pub line: u32,
    /// The symbol that holds the site.
    pub holder: String,
}

/// One symbol of the edited file whose callers the edit may leave behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolEdit {
    pub change: ShapeChange,
    /// The symbol as the generation stored it.
    pub symbol: Symbol,
    /// Sites outside the symbol that a rule proved reach it, by file, line.
    pub proved: Vec<Site>,
    /// Sites outside the symbol that probably reach it.
    pub likely: Vec<Site>,
    /// Sites no rule could resolve whose callee has the symbol's last name
    /// segment: the callers above are a lower bound by this many.
    pub open_sites: usize,
    /// Test files within [`TEST_DEPTH`] callers of the symbol, sorted.
    pub tests: Vec<String>,
}

/// What [`check`] found: empty when nothing the edit does can strand a
/// caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditReport {
    pub path: String,
    /// Symbols of the file whose declaration header the edit changes, removes
    /// or renames, whether or not anything outside them calls them.
    pub reshaped: usize,
    pub edits: Vec<SymbolEdit>,
}

impl EditReport {
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

#[derive(Debug)]
pub enum EditError {
    Store(StoreError),
    Read(std::io::Error),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Store(err) => err.fmt(f),
            EditError::Read(err) => write!(f, "cannot read the file: {err}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<StoreError> for EditError {
    fn from(err: StoreError) -> Self {
        EditError::Store(err)
    }
}

/// The edit `proposed` (the file's whole new content; `None`: the file as it
/// is on disk) makes to `path`, in terms of the callers it strands.
///
/// Silent (an empty report) for a file that is not indexed, whose language
/// nothing reads, that is past [`EDIT_MAX_BYTES`], or whose edit leaves every
/// shape alone or touches only symbols nothing outside them calls.
pub fn check(
    store: &Store,
    root: &Path,
    path: &str,
    proposed: Option<&[u8]>,
) -> Result<EditReport, EditError> {
    let mut report = EditReport {
        path: path.to_owned(),
        reshaped: 0,
        edits: Vec::new(),
    };
    let Some(language) = lang::wanted(path) else {
        return Ok(report);
    };
    let stored = store.symbols_in(path)?;
    if stored.is_empty() {
        return Ok(report);
    }
    let fresh = match proposed {
        Some(bytes) => bytes.to_vec(),
        None => match read(root, path)? {
            Some(bytes) => bytes,
            None => return Ok(report),
        },
    };
    if fresh.len() as u64 > EDIT_MAX_BYTES {
        return Ok(report);
    }
    let rows = extract::rows(path, language, &fresh);
    let changes = diff_shapes(&stored, &rows.symbols);
    report.reshaped = changes.len();
    for change in changes {
        let Some(symbol) = stored.iter().find(|s| s.uid == change.uid()).cloned() else {
            continue;
        };
        let (proved, likely) = outside_callers(store, &symbol)?;
        if proved.is_empty() && likely.is_empty() {
            continue;
        }
        let open_sites = store
            .open_sites(leaf(&symbol.name))?
            .iter()
            .filter(|edge| edge.src != symbol.uid)
            .count();
        let tests = covering_tests(store, &symbol)?;
        report.edits.push(SymbolEdit {
            change,
            symbol,
            proved,
            likely,
            open_sites,
            tests,
        });
    }
    Ok(report)
}

/// The sites outside `symbol` that reach it, `Proved` and `Likely` apart.
/// A `Symbol` the generation holds supplies the line range that is "inside".
pub fn outside_callers(
    store: &Store,
    symbol: &Symbol,
) -> Result<(Vec<Site>, Vec<Site>), StoreError> {
    let mut proved = Vec::new();
    let mut likely = Vec::new();
    for caller in store.callers(&symbol.uid, Tier::Likely)? {
        let inside = caller.site.file == symbol.file
            && symbol.line <= caller.edge.line
            && caller.edge.line <= symbol.end_line;
        if inside {
            continue;
        }
        let site = Site {
            file: caller.site.file,
            line: caller.edge.line,
            holder: caller.site.name,
        };
        match caller.edge.tier {
            Tier::Proved => proved.push(site),
            _ => likely.push(site),
        }
    }
    Ok((proved, likely))
}

/// The test files a change to `symbol` is within [`TEST_DEPTH`] callers of.
pub fn covering_tests(store: &Store, symbol: &Symbol) -> Result<Vec<String>, StoreError> {
    let mut tests = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([symbol.uid.clone()]);
    let mut frontier = vec![symbol.uid.clone()];
    for _ in 0..TEST_DEPTH {
        let mut next = Vec::new();
        for uid in &frontier {
            for caller in store.callers(uid, Tier::Likely)? {
                if seen.len() >= TEST_VISIT_CAP || !seen.insert(caller.site.uid.clone()) {
                    continue;
                }
                if is_test_path(&caller.site.file) {
                    tests.push(caller.site.file.clone());
                }
                next.push(caller.site.uid);
            }
        }
        frontier = next;
    }
    tests.sort_unstable();
    tests.dedup();
    Ok(tests)
}

/// The bytes of `path` under `root`, or `None` for a path that is not a
/// regular file inside it (a link, a directory, one that escapes the root).
pub fn read(root: &Path, path: &str) -> Result<Option<Vec<u8>>, EditError> {
    let Ok(full) = pixel_io::repo_path::confine(root, path) else {
        return Ok(None);
    };
    let file = match pixel_io::nofollow::open_read(&full) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(EditError::Read(err)),
    };
    let mut bytes = Vec::new();
    file.take(EDIT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(EditError::Read)?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refresh;
    use crate::testutil::repo_with;

    const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";

    /// A crate whose `b::go` calls `a::run` (proved, by `use`), whose
    /// `tests/it.rs` calls `b::go`, and an unrelated `c::other`.
    fn indexed() -> (tempfile::TempDir, Store) {
        let repo = repo_with(&[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n"),
            (
                "src/b.rs",
                "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n",
            ),
            ("src/c.rs", "pub fn other() -> u32 {\n    7\n}\n"),
            (
                "tests/it.rs",
                "use demo::b::go;\n\nfn t() {\n    go();\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).expect("refresh");
        let store = Store::open(repo.path()).expect("store");
        (repo, store)
    }

    fn report(repo: &tempfile::TempDir, store: &Store, path: &str, text: &str) -> EditReport {
        check(store, repo.path(), path, Some(text.as_bytes())).expect("check")
    }

    #[test]
    fn a_new_parameter_on_a_called_function_names_its_proved_caller() {
        let (repo, store) = indexed();
        let found = report(
            &repo,
            &store,
            "src/a.rs",
            "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
        );
        assert_eq!(found.edits.len(), 1);
        let edit = &found.edits[0];
        assert_eq!(edit.symbol.name, "run");
        assert!(matches!(
            &edit.change,
            ShapeChange::Reshaped { old, new, .. }
                if old == "pub fn run(x: u32) -> u32" && new == "pub fn run(x: u32, y: u32) -> u32"
        ));
        assert_eq!(
            edit.proved,
            [Site {
                file: "src/b.rs".to_owned(),
                line: 4,
                holder: "go".to_owned()
            }]
        );
        assert!(edit.likely.is_empty());
        assert_eq!(edit.open_sites, 0);
    }

    #[test]
    fn a_body_only_edit_is_silent() {
        let (repo, store) = indexed();
        let found = report(
            &repo,
            &store,
            "src/a.rs",
            "pub fn run(x: u32) -> u32 {\n    x + 1\n}\n",
        );
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(found.reshaped, 0);
    }

    #[test]
    fn a_reshaped_function_nothing_calls_is_silent() {
        let (repo, store) = indexed();
        let found = report(
            &repo,
            &store,
            "src/c.rs",
            "pub fn other(a: u8) -> u32 {\n    7\n}\n",
        );
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            found.reshaped, 1,
            "the shape changed; only the callers are missing"
        );
    }

    #[test]
    fn removing_or_renaming_a_called_function_fires() {
        let (repo, store) = indexed();
        let removed = report(&repo, &store, "src/a.rs", "pub fn keep() {}\n");
        assert!(matches!(
            removed.edits[0].change,
            ShapeChange::Removed { .. }
        ));
        let renamed = report(
            &repo,
            &store,
            "src/a.rs",
            "pub fn execute(x: u32) -> u32 {\n    x\n}\n",
        );
        assert!(matches!(
            &renamed.edits[0].change,
            ShapeChange::Renamed { to, .. } if to == "src/a.rs#execute#function"
        ));
        assert_eq!(renamed.edits[0].proved.len(), 1);
    }

    #[test]
    fn the_tests_that_reach_the_symbol_through_its_callers_are_listed() {
        let (repo, store) = indexed();
        let found = report(
            &repo,
            &store,
            "src/a.rs",
            "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
        );
        assert_eq!(found.edits[0].tests, ["tests/it.rs"]);
    }

    #[test]
    fn the_file_on_disk_is_checked_when_no_text_is_proposed() {
        let (repo, store) = indexed();
        std::fs::write(
            repo.path().join("src/a.rs"),
            "pub fn run() -> u32 {\n    1\n}\n",
        )
        .unwrap();
        let found = check(&store, repo.path(), "src/a.rs", None).expect("check");
        assert_eq!(found.edits.len(), 1);
        assert!(
            check(&store, repo.path(), "src/gone.rs", None)
                .expect("check")
                .is_empty()
        );
    }

    #[test]
    fn a_file_that_is_not_indexed_or_not_a_language_is_silent() {
        let (repo, store) = indexed();
        assert!(report(&repo, &store, "src/new.rs", "pub fn x() {}\n").is_empty());
        assert!(report(&repo, &store, "README.md", "# x\n").is_empty());
    }

    #[test]
    fn a_file_past_the_cap_is_silent() {
        let (repo, store) = indexed();
        let big = vec![b'a'; EDIT_MAX_BYTES as usize + 1];
        let found = check(&store, repo.path(), "src/a.rs", Some(&big)).expect("check");
        assert!(found.is_empty());
    }

    #[test]
    fn an_open_site_of_the_same_name_lowers_the_bound_instead_of_listing_a_caller() {
        let repo = repo_with(&[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod d;\n"),
            ("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n"),
            (
                "src/b.rs",
                "pub fn go(handle: Unknown) {\n    handle.run(1);\n}\n",
            ),
            (
                "src/d.rs",
                "use crate::a::run;\n\npub fn also() {\n    run(2);\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).expect("refresh");
        let store = Store::open(repo.path()).expect("store");
        let sites = store.open_sites("run").unwrap();
        assert_eq!(sites.len(), 1, "the receiver's type is unknown");
        // The open site is a bound on the answer, never a caller.
        let found = report(&repo, &store, "src/a.rs", "pub fn run() {}\n");
        let edit = &found.edits[0];
        assert_eq!(
            edit.proved
                .iter()
                .map(|s| s.file.as_str())
                .collect::<Vec<_>>(),
            ["src/d.rs"]
        );
        assert_eq!(edit.open_sites, 1);
    }
}
