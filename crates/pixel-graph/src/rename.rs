// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Renaming a symbol through the sites the graph resolved to it
//! (`specs/graph.md`, "Analyses").
//!
//! [`plan`] turns the symbol's definition, its `Proved` callers and the
//! import lines that bring it into their files into text edits, each checked
//! against a fresh parse: the token must be an identifier on the line the
//! graph names, not text in a comment or a string. A site the plan cannot
//! place exactly once is skipped with its reason, never guessed at. `Likely`
//! sites are skipped unless asked for, and the sites no rule resolved are
//! counted: a rename through the graph is a lower bound, so the plan says
//! how many same-name sites it did not see.
//!
//! [`apply`] writes the edits only to files that are still the bytes the
//! generation indexed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use pixel_model::hash::Hash;
use pixel_model::{EdgeKind, Symbol, SymbolKind, Tier, leaf};
use tree_sitter::Node;

use crate::lang;
use crate::store::{Store, StoreError};

/// Why a site was left as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// The site is a guess (`Likely`) and guesses were not asked for.
    Likely,
    /// The name is not written on the site's line: the symbol is reached
    /// through an alias, or the generation is older than the file.
    NotWritten,
    /// The name is written this many times on the line; which one is the
    /// symbol is not known.
    Ambiguous(usize),
    /// No syntax tree could be read for the file.
    Unparsed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Definition,
    Call,
    Import,
}

/// One token to replace, by byte offsets in its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub file: String,
    pub line: u32,
    pub start: usize,
    pub end: usize,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip {
    pub file: String,
    pub line: u32,
    pub reason: Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub symbol: Symbol,
    pub from: String,
    pub to: String,
    /// Sorted by file, then offset.
    pub edits: Vec<Edit>,
    pub skipped: Vec<Skip>,
    /// Sites no rule resolved that share the old name: the plan does not
    /// see them.
    pub open_sites: usize,
}

#[derive(Debug)]
pub enum RenameError {
    /// The new name is not an identifier, or is the old one.
    InvalidName(String),
    /// The symbol is a file or a package, or its name is not an identifier.
    NotRenamable(String),
    /// A symbol of the new name already exists in a file the rename edits.
    Conflict(String),
    /// A file the rename edits is no longer what the generation indexed.
    Stale(String),
    Store(StoreError),
    Io(std::io::Error),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenameError::InvalidName(name) => write!(f, "{name:?} is not a usable new name"),
            RenameError::NotRenamable(why) => write!(f, "cannot rename: {why}"),
            RenameError::Conflict(what) => write!(f, "the new name is already defined: {what}"),
            RenameError::Stale(file) => write!(
                f,
                "{file} changed since it was indexed; run `pixel prepare` and plan again"
            ),
            RenameError::Store(err) => err.fmt(f),
            RenameError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RenameError {}

impl From<StoreError> for RenameError {
    fn from(err: StoreError) -> Self {
        RenameError::Store(err)
    }
}

impl From<std::io::Error> for RenameError {
    fn from(err: std::io::Error) -> Self {
        RenameError::Io(err)
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The plan to rename `symbol` to `to`: its definition, the lines of its
/// `Proved` callers (and `Likely` ones when `include_likely`), and the
/// import lines of those files.
pub fn plan(
    store: &Store,
    root: &Path,
    symbol: &Symbol,
    to: &str,
    include_likely: bool,
) -> Result<Plan, RenameError> {
    if matches!(symbol.kind, SymbolKind::File | SymbolKind::Package) {
        return Err(RenameError::NotRenamable(format!(
            "{} is a {}",
            symbol.name, symbol.kind
        )));
    }
    let from = leaf(&symbol.name).to_owned();
    if !is_identifier(&from) {
        return Err(RenameError::NotRenamable(format!(
            "{from:?} is not a plain identifier"
        )));
    }
    if !is_identifier(to) || to == from {
        return Err(RenameError::InvalidName(to.to_owned()));
    }
    let mut files = Files::new(store, root)?;
    let mut plan = Plan {
        symbol: symbol.clone(),
        from: from.clone(),
        to: to.to_owned(),
        edits: Vec::new(),
        skipped: Vec::new(),
        open_sites: 0,
    };

    // The definition.
    match files.tokens(&symbol.file, symbol.line, &from)? {
        Some(tokens) if !tokens.is_empty() => plan.edits.push(Edit {
            file: symbol.file.clone(),
            line: symbol.line,
            start: tokens[0].0,
            end: tokens[0].1,
            role: Role::Definition,
        }),
        Some(_) => {
            return Err(RenameError::NotRenamable(format!(
                "{from} is not written on line {} of {}",
                symbol.line, symbol.file
            )));
        }
        None => {
            return Err(RenameError::NotRenamable(format!(
                "{} cannot be parsed",
                symbol.file
            )));
        }
    }

    // The sites the graph resolved to it.
    let mut caller_files: BTreeSet<String> = BTreeSet::new();
    for caller in store.callers(&symbol.uid, Tier::Likely)? {
        if !matches!(caller.edge.kind, EdgeKind::Call | EdgeKind::Reference) {
            continue;
        }
        let (file, line) = (caller.site.file.clone(), caller.edge.line);
        if caller.edge.tier == Tier::Likely && !include_likely {
            plan.skipped.push(Skip {
                file,
                line,
                reason: Skipped::Likely,
            });
            continue;
        }
        match files.tokens(&file, line, &from)? {
            None => plan.skipped.push(Skip {
                file,
                line,
                reason: Skipped::Unparsed,
            }),
            Some(tokens) => match tokens.as_slice() {
                [] => plan.skipped.push(Skip {
                    file,
                    line,
                    reason: Skipped::NotWritten,
                }),
                [(start, end)] => {
                    caller_files.insert(file.clone());
                    plan.edits.push(Edit {
                        file,
                        line,
                        start: *start,
                        end: *end,
                        role: Role::Call,
                    });
                }
                many => plan.skipped.push(Skip {
                    file,
                    line,
                    reason: Skipped::Ambiguous(many.len()),
                }),
            },
        }
    }

    // The imports that name it: a language that resolves an import to the
    // item (Rust's `use a::run`) points the import edge at the symbol.
    for import in store.imports_of(&symbol.uid)? {
        let Some(holder) = store.symbol(&import.src)? else {
            continue;
        };
        if let Some(tokens) = files.tokens(&holder.file, import.line, &from)?
            && let [(start, end)] = tokens.as_slice()
        {
            plan.edits.push(Edit {
                file: holder.file,
                line: import.line,
                start: *start,
                end: *end,
                role: Role::Import,
            });
        }
    }

    // The imports that bring it into the files that call it: a language
    // that resolves an import to a file (TypeScript, Python) points the
    // edge at the file symbol.
    let defining_file = format!("{}#{}#file", symbol.file, symbol.file);
    for file in caller_files.iter().filter(|f| **f != symbol.file) {
        for edge in store.edges_of(file)? {
            if edge.kind != EdgeKind::Import || edge.dst.as_deref() != Some(defining_file.as_str())
            {
                continue;
            }
            if let Some(tokens) = files.tokens(file, edge.line, &from)?
                && let [(start, end)] = tokens.as_slice()
            {
                plan.edits.push(Edit {
                    file: file.clone(),
                    line: edge.line,
                    start: *start,
                    end: *end,
                    role: Role::Import,
                });
            }
        }
    }

    plan.edits
        .sort_by(|a, b| (&a.file, a.start, a.end).cmp(&(&b.file, b.start, b.end)));
    plan.edits
        .dedup_by(|a, b| (&a.file, a.start) == (&b.file, b.start));
    plan.open_sites = store
        .open_sites(&from)?
        .iter()
        .filter(|edge| edge.src != symbol.uid)
        .count();

    // A name already defined in a file the rename edits would collide.
    let touched: BTreeSet<&str> = plan.edits.iter().map(|e| e.file.as_str()).collect();
    for other in store.named_symbols(to)? {
        if touched.contains(other.file.as_str()) && leaf(&other.name) == to {
            return Err(RenameError::Conflict(format!(
                "{} in {}:{}",
                other.name, other.file, other.line
            )));
        }
    }
    Ok(plan)
}

/// Writes `plan`: every edited file must still be the bytes the generation
/// indexed, or nothing is written. The files written, sorted.
pub fn apply(store: &Store, root: &Path, plan: &Plan) -> Result<Vec<String>, RenameError> {
    let mut by_file: BTreeMap<&str, Vec<&Edit>> = BTreeMap::new();
    for edit in &plan.edits {
        by_file.entry(edit.file.as_str()).or_default().push(edit);
    }
    let mut files = Files::new(store, root)?;
    let mut rewritten = Vec::new();
    for (file, edits) in &by_file {
        let content = files
            .content(file)?
            .ok_or_else(|| RenameError::Stale((*file).to_owned()))?;
        let mut text = content.to_vec();
        for edit in edits.iter().rev() {
            if text.get(edit.start..edit.end) != Some(plan.from.as_bytes()) {
                return Err(RenameError::Stale((*file).to_owned()));
            }
            text.splice(edit.start..edit.end, plan.to.bytes());
        }
        rewritten.push(((*file).to_owned(), text));
    }
    for (file, text) in &rewritten {
        let path = root.join(file);
        pixel_io::nofollow::write_replace(&path, text, mode_of(&path))?;
    }
    Ok(rewritten.into_iter().map(|(file, _)| file).collect())
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map_or(0o644, |meta| meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_path: &Path) -> u32 {
    0o644
}

/// The files a plan reads: content checked against the generation's hash,
/// and the syntax tree of each.
struct Files<'a> {
    root: &'a Path,
    hashes: BTreeMap<String, Hash>,
    cache: BTreeMap<String, Option<Vec<u8>>>,
}

impl<'a> Files<'a> {
    fn new(store: &Store, root: &'a Path) -> Result<Self, StoreError> {
        Ok(Files {
            root,
            hashes: store
                .file_rows()?
                .into_iter()
                .map(|row| (row.path, row.hash))
                .collect(),
            cache: BTreeMap::new(),
        })
    }

    /// The bytes of `file`, if they are the ones the generation indexed.
    fn content(&mut self, file: &str) -> Result<Option<&[u8]>, RenameError> {
        if !self.cache.contains_key(file) {
            let bytes = std::fs::read(self.root.join(file)).ok();
            let matches = bytes
                .as_deref()
                .zip(self.hashes.get(file))
                .is_some_and(|(bytes, hash)| Hash::of(bytes) == *hash);
            self.cache
                .insert(file.to_owned(), if matches { bytes } else { None });
        }
        Ok(self.cache[file].as_deref())
    }

    /// The byte ranges of identifier tokens spelled `name` on `line` of
    /// `file`; `None` when the file is not what was indexed or has no tree.
    fn tokens(
        &mut self,
        file: &str,
        line: u32,
        name: &str,
    ) -> Result<Option<Vec<(usize, usize)>>, RenameError> {
        let Some(content) = self.content(file)? else {
            return Err(RenameError::Stale(file.to_owned()));
        };
        let Some(language) = lang::wanted(file) else {
            return Ok(None);
        };
        let Some(tree) = lang::rules(language).parse(file, content) else {
            return Ok(None);
        };
        let mut found = Vec::new();
        collect(
            tree.root_node(),
            content,
            line.saturating_sub(1) as usize,
            name,
            &mut found,
        );
        Ok(Some(found))
    }
}

/// Leaves spelled `name` that start on `row`, outside comments and strings.
fn collect(node: Node, content: &[u8], row: usize, name: &str, found: &mut Vec<(usize, usize)>) {
    if node.start_position().row > row || node.end_position().row < row {
        return;
    }
    let kind = node.kind();
    if kind.contains("comment") || kind.contains("string") || kind.contains("regex") {
        return;
    }
    if node.child_count() == 0 {
        if node.start_position().row == row
            && content.get(node.byte_range()) == Some(name.as_bytes())
        {
            found.push((node.start_byte(), node.end_byte()));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, content, row, name, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fixture::project;
    use crate::analysis::impact::resolve_target;
    use crate::refresh;

    fn target(store: &Store, name: &str) -> Symbol {
        resolve_target(store, name).unwrap().remove(0)
    }

    fn placed(plan: &Plan) -> Vec<(&str, u32, Role)> {
        plan.edits
            .iter()
            .map(|e| (e.file.as_str(), e.line, e.role))
            .collect()
    }

    #[test]
    fn the_definition_the_call_and_the_import_are_one_plan() {
        let (repo, store) = project();
        let plan = plan(
            &store,
            repo.path(),
            &target(&store, "run"),
            "execute",
            false,
        )
        .unwrap();
        assert_eq!(
            placed(&plan),
            [
                ("src/a.rs", 1, Role::Definition),
                ("src/b.rs", 1, Role::Import),
                ("src/b.rs", 5, Role::Call),
            ]
        );
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.open_sites, 0);
    }

    #[test]
    fn applying_a_plan_rewrites_the_files_and_the_next_index_agrees() {
        let (repo, store) = project();
        let plan = plan(
            &store,
            repo.path(),
            &target(&store, "run"),
            "execute",
            false,
        )
        .unwrap();
        let written = apply(&store, repo.path(), &plan).unwrap();
        assert_eq!(written, ["src/a.rs", "src/b.rs"]);
        let a = std::fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
        assert_eq!(a, "pub fn execute(x: u32) -> u32 {\n    x\n}\n");
        let b = std::fs::read_to_string(repo.path().join("src/b.rs")).unwrap();
        assert!(b.contains("use crate::a::execute;") && b.contains("execute(1) + other()"));
        refresh(repo.path(), None, None).unwrap();
        let fresh = Store::open(repo.path()).unwrap();
        let renamed = target(&fresh, "execute");
        let callers = fresh.callers(&renamed.uid, Tier::Proved).unwrap();
        assert_eq!(callers.len(), 1, "the call still resolves to the symbol");
        assert!(resolve_target(&fresh, "run").unwrap().is_empty());
    }

    #[test]
    fn a_name_in_a_comment_or_a_string_on_the_line_is_not_a_token() {
        let repo = crate::testutil::repo_with(&[
            ("Cargo.toml", crate::analysis::fixture::MANIFEST),
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn run() {}\n"),
            (
                "src/b.rs",
                "use crate::a::run;\n\npub fn go() {\n    run(); // run \"run\"\n    let _ = \"run\"; run();\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let plan = plan(&store, repo.path(), &target(&store, "run"), "go2", false).unwrap();
        // Line four holds one token outside the comment; line five holds
        // one outside the string.
        let calls: Vec<u32> = plan
            .edits
            .iter()
            .filter(|e| e.role == Role::Call)
            .map(|e| e.line)
            .collect();
        assert_eq!(calls, [4, 5]);
    }

    #[test]
    fn a_name_written_twice_on_a_line_is_skipped_as_ambiguous() {
        let repo = crate::testutil::repo_with(&[
            ("Cargo.toml", crate::analysis::fixture::MANIFEST),
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n"),
            (
                "src/b.rs",
                "use crate::a::run;\n\npub fn go() -> u32 {\n    run(run(1))\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let plan = plan(&store, repo.path(), &target(&store, "run"), "x", false).unwrap();
        assert!(
            plan.skipped
                .iter()
                .any(|s| s.reason == Skipped::Ambiguous(2))
        );
        assert!(plan.edits.iter().all(|e| e.role != Role::Call));
    }

    #[test]
    fn a_new_name_that_is_not_an_identifier_or_is_taken_is_refused() {
        let (repo, store) = project();
        let run = target(&store, "run");
        for bad in ["", "two words", "9lives", "run", "a-b"] {
            assert!(
                matches!(
                    plan(&store, repo.path(), &run, bad, false),
                    Err(RenameError::InvalidName(_))
                ),
                "{bad:?}"
            );
        }
        // `go` is defined in a file the rename edits.
        assert!(matches!(
            plan(&store, repo.path(), &run, "go", false),
            Err(RenameError::Conflict(_))
        ));
        let file = store
            .symbols_in("src/a.rs")
            .unwrap()
            .into_iter()
            .find(|s| s.kind == SymbolKind::File)
            .unwrap();
        assert!(matches!(
            plan(&store, repo.path(), &file, "x", false),
            Err(RenameError::NotRenamable(_))
        ));
    }

    #[test]
    fn a_file_edited_since_it_was_indexed_is_stale_and_nothing_is_written() {
        let (repo, store) = project();
        let renaming = plan(
            &store,
            repo.path(),
            &target(&store, "run"),
            "execute",
            false,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/b.rs"),
            "// edited\nuse crate::a::run;\n",
        )
        .unwrap();
        assert!(matches!(
            apply(&store, repo.path(), &renaming),
            Err(RenameError::Stale(file)) if file == "src/b.rs"
        ));
        let a = std::fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
        assert!(
            a.starts_with("pub fn run"),
            "the definition was not touched"
        );
        assert!(matches!(
            plan(&store, repo.path(), &target(&store, "run"), "z", false),
            Err(RenameError::Stale(_))
        ));
    }

    #[test]
    fn a_typescript_import_that_resolves_to_a_file_is_renamed_with_its_call() {
        let repo = crate::testutil::repo_with(&[
            (
                "lib/a.ts",
                "export function greet(name: string) {\n  return name;\n}\n",
            ),
            (
                "main.ts",
                "import { greet } from './lib/a';\n\nexport function main() {\n  return greet('x');\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let plan = plan(
            &store,
            repo.path(),
            &target(&store, "greet"),
            "hello",
            false,
        )
        .unwrap();
        assert_eq!(
            placed(&plan),
            [
                ("lib/a.ts", 1, Role::Definition),
                ("main.ts", 1, Role::Import),
                ("main.ts", 4, Role::Call),
            ]
        );
        apply(&store, repo.path(), &plan).unwrap();
        let main = std::fs::read_to_string(repo.path().join("main.ts")).unwrap();
        assert!(
            main.contains("import { hello } from './lib/a';")
                && main.contains("return hello('x');")
        );
    }

    #[test]
    fn a_likely_site_is_left_unless_guesses_are_asked_for() {
        let repo = crate::testutil::repo_with(&[
            ("Cargo.toml", crate::analysis::fixture::MANIFEST),
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn unique_name() {}\n"),
            (
                "src/b.rs",
                "pub fn go(x: Unknown) {\n    x.unique_name();\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let symbol = target(&store, "unique_name");
        let sites = store.callers(&symbol.uid, Tier::Likely).unwrap();
        // Only meaningful when the receiver rule made it a guess.
        if sites.iter().any(|c| c.edge.tier == Tier::Likely) {
            let careful = plan(&store, repo.path(), &symbol, "other_name", false).unwrap();
            assert!(careful.skipped.iter().any(|s| s.reason == Skipped::Likely));
            let bold = plan(&store, repo.path(), &symbol, "other_name", true).unwrap();
            assert!(bold.edits.iter().any(|e| e.role == Role::Call));
        }
    }
}
