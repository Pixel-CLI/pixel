// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The language table: one grammar and one rule file per language
//! (ADR-0017, `design/architecture.md`, "Indexer"). Adding a language is a
//! rule module and a row in [`RULES`]; no generic module changes.

use mvp_model::{Edge, EdgeKind, Lang, Symbol, SymbolKind};

use crate::{ruby, rust};

/// What one language knows. Every method is a pure function of its
/// arguments: no I/O, no global state.
pub trait Rules: Sync {
    fn lang(&self) -> Lang;

    /// Bumped by any change to the grammar or the rules; a new version
    /// re-extracts every file of the language (`EXTRACTOR_VERSION` in
    /// `design/architecture.md`).
    fn version(&self) -> u32;

    /// From the path alone: does this language read the file?
    fn wants(&self, path: &str) -> bool;

    /// After reading: is the file the language's to parse? A binstub needs
    /// a Ruby shebang, or the file is not the language's; a generated blob
    /// is refused, and indexed unread (`tree::unread`). A file shorter than
    /// a generated blob (`guard::GENERATED_MIN_BYTES`) is refused only when
    /// it is not the language's, so the scan asks about a file's head to
    /// tell the two refusals apart.
    fn accepts(&self, path: &str, content: &[u8]) -> bool;

    /// One file's definitions, sites and facts. Never fails: a file the
    /// grammar cannot parse yields what could be read, at least its `File`
    /// symbol.
    fn extract(&self, path: &str, content: &[u8]) -> Extraction;

    /// One file indexed unread (`tree::unread`: past the size cap, or a
    /// generated blob), in place of [`Rules::extract`]: its definitions are
    /// unknown, so the file is in [`World::unread`], never in
    /// [`World::files`]. A rule file may scan the text lexically for the
    /// names such a file might define and record them in its facts, which
    /// its resolver reads there, so that it never proves a target those
    /// names could shadow (C-10). The default records nothing: the file is
    /// its `File` symbol alone, and its language resolves nothing while it
    /// is unread ([`World::unread`]).
    fn unread(&self, _path: &str, _content: &[u8]) -> Extraction {
        Extraction::default()
    }

    /// Decides every site of this language in the generation: for
    /// `world.edges[i]`, the index in `world.symbols` of its target, or
    /// `None` when the rules cannot prove one. Never a guess (C-10).
    fn resolve(&self, world: &World<'_>) -> Vec<Option<usize>>;
}

/// The rule files, one per language.
pub const RULES: &[&dyn Rules] = &[&ruby::Ruby, &rust::Rust];

pub fn rules(lang: Lang) -> &'static dyn Rules {
    *RULES
        .iter()
        .find(|rules| rules.lang() == lang)
        .expect("every Lang has a rule file")
}

/// The language that reads `path`, from the path alone.
pub fn wanted(path: &str) -> Option<Lang> {
    RULES
        .iter()
        .find(|rules| rules.wants(path))
        .map(|rules| rules.lang())
}

pub fn accepts(lang: Lang, path: &str, content: &[u8]) -> bool {
    rules(lang).accepts(path, content)
}

/// `ruby.1,rust.1`: the extractor version of every language (C-11).
pub fn extractor() -> String {
    RULES
        .iter()
        .map(|rules| format!("{}.{}", rules.lang(), rules.version()))
        .collect::<Vec<_>>()
        .join(",")
}

/// What a rule file extracts from one file, before uids exist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extraction {
    pub defs: Vec<Def>,
    pub sites: Vec<Site>,
    /// Declarations the rule file reads back at resolution (ancestors,
    /// `use` scopes, manifests), in its own encoding, written with `codec`
    /// (`extract::UNREAD` relies on it).
    pub facts: String,
}

/// A definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    pub kind: SymbolKind,
    /// Qualified.
    pub name: String,
    /// The text at `line`, one line (see `text::signature`).
    pub signature: String,
    pub line: u32,
    pub end_line: u32,
}

/// [`Site::owner`] for a site that belongs to the file itself (its `File`
/// symbol): code outside every definition, which the line alone cannot
/// tell from a definition written on the same line (`end; run`).
pub const FILE_OWNER: usize = usize::MAX;

/// A call, reference, import, route or association, undecided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub kind: EdgeKind,
    pub line: u32,
    /// The index in `defs` of the definition the site belongs to, or
    /// [`FILE_OWNER`]; `None` lets the driver pick the smallest definition
    /// spanning `line`.
    pub owner: Option<usize>,
    /// The target as written.
    pub target: String,
    /// The rule file's own encoding of what it needs to decide the site.
    pub context: String,
}

/// One language's slice of a generation, read by `Rules::resolve`.
pub struct World<'a> {
    /// The language's files read: path and facts.
    pub files: &'a [(String, String)],
    /// The symbols of those files.
    pub symbols: &'a [Symbol],
    /// The sites of those files; `dst` is ignored.
    pub edges: &'a [Edge],
    /// The language's files indexed unread: path and the facts
    /// [`Rules::unread`] recorded. Their definitions are unknown, so
    /// neither they nor their rows are in the fields above. A resolver that
    /// ignored them would take a name they may define as defined nowhere,
    /// so `link` resolves no site of a language that has one until its
    /// rule file reads this field (D24).
    pub unread: &'a [(String, String)],
}
