// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The rows the indexer produces and the store keeps: files, symbols,
//! edges, and the provenance of a generation (`design/architecture.md`).

use std::borrow::Cow;
use std::fmt;

use crate::hash::Hash;

/// A language of the first set (ADR-0017).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    Ruby,
    Rust,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Ruby, Lang::Rust];

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Ruby => "ruby",
            Lang::Rust => "rust",
        }
    }

    pub fn parse(text: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|lang| lang.as_str() == text)
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a symbol is. A closed list; each language's rule file says which
/// kinds it emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    /// A free function: a Rust `fn` outside an `impl`, a Ruby top-level `def`.
    Function,
    /// A function owned by a type: in an `impl`, a class or a module.
    Method,
    Class,
    Module,
    Struct,
    Enum,
    /// A member of an enum, qualified `Enum::Variant`.
    Variant,
    Trait,
    Const,
    Static,
    /// A Rust `type` alias.
    Type,
    /// A Rust `macro_rules!` definition.
    Macro,
    /// A file as a whole: the owner of top-level code (a Ruby script, a Rust
    /// module file).
    File,
    /// A package a manifest declares (a Rust crate in `Cargo.toml`).
    Package,
}

impl SymbolKind {
    pub const ALL: [SymbolKind; 14] = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Class,
        SymbolKind::Module,
        SymbolKind::Struct,
        SymbolKind::Enum,
        SymbolKind::Variant,
        SymbolKind::Trait,
        SymbolKind::Const,
        SymbolKind::Static,
        SymbolKind::Type,
        SymbolKind::Macro,
        SymbolKind::File,
        SymbolKind::Package,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Module => "module",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Variant => "variant",
            SymbolKind::Trait => "trait",
            SymbolKind::Const => "const",
            SymbolKind::Static => "static",
            SymbolKind::Type => "type",
            SymbolKind::Macro => "macro",
            SymbolKind::File => "file",
            SymbolKind::Package => "package",
        }
    }

    pub fn parse(text: &str) -> Option<SymbolKind> {
        SymbolKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an edge says (`design/architecture.md`, "Indexer").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// A call site.
    Call,
    /// A symbol named without being called: a callback, a `send`, an
    /// ancestor, a type in a signature.
    Reference,
    /// A `require` or a `use`.
    Import,
    /// A route the router file maps to a controller action.
    Route,
    /// A model association (`has_many`, `belongs_to`).
    Association,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 5] = [
        EdgeKind::Call,
        EdgeKind::Reference,
        EdgeKind::Import,
        EdgeKind::Route,
        EdgeKind::Association,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Call => "call",
            EdgeKind::Reference => "reference",
            EdgeKind::Import => "import",
            EdgeKind::Route => "route",
            EdgeKind::Association => "association",
        }
    }

    pub fn parse(text: &str) -> Option<EdgeKind> {
        EdgeKind::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One indexed file of a generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    /// Relative to the repository root, `/`-separated.
    pub path: String,
    pub lang: Lang,
    /// SHA-256 of the content.
    pub hash: Hash,
}

/// What a file's metadata said when its content was hashed: size,
/// modification and change times in nanoseconds since the epoch, inode
/// (ADR-0022 §2). The same values at a later scan stand for the same
/// content, as git's index takes them to: the hash is then taken from the
/// index, not read again. Any write moves the change time, which no user
/// tool sets back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub len: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
}

/// A path as a uid writes it: `%` as `%25` and `#` as `%23`, every other
/// character as is, so a path without either keeps its own spelling and
/// the first `#` of a uid ends it (`Symbol::uid`).
pub fn uid_path(path: &str) -> Cow<'_, str> {
    if !path.contains(['%', '#']) {
        return Cow::Borrowed(path);
    }
    Cow::Owned(path.replace('%', "%25").replace('#', "%23"))
}

/// The last segment of a qualified name: what follows its last `::`, `#`
/// or `.` (`Billing::Charge.call` gives `call`, `store::Store` gives
/// `Store`), or the whole name.
pub fn leaf(name: &str) -> &str {
    let cut = [
        name.rfind("::").map(|at| at + 2),
        name.rfind('#').map(|at| at + 1),
        name.rfind('.').map(|at| at + 1),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    if cut >= name.len() {
        name
    } else {
        &name[cut..]
    }
}

/// The sub-words of a name or a path: split at every character that is not
/// a letter or a digit, at a lower-case letter or a digit followed by an
/// upper-case one, and before the last capital of a run followed by a
/// lower-case letter (`HTTPServer` gives `HTTP`, `Server`), in lower case.
pub fn sub_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1);
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
            });
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// One definition, true of the tree it was extracted from: its signature
/// is the text at `line` (C-10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// Unique in a generation: `path#name#kind`, and `~n` for the n-th
    /// repetition of the same triple in one file. The path writes `%` as
    /// `%25` and `#` as `%23`, so the first `#` ends it.
    pub uid: String,
    /// The file's path.
    pub file: String,
    pub kind: SymbolKind,
    /// The qualified name (`Billing::Charge.call`, `store::Store::open`).
    pub name: String,
    /// The definition's first line as written, one line, capped.
    pub signature: String,
    /// 1-based.
    pub line: u32,
    /// 1-based, inclusive.
    pub end_line: u32,
}

/// One site: a call, reference, import, route or association, resolved or
/// not. An unresolved edge (`dst` is `None`) is stored and counted, never
/// served as a caller (C-10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The uid of the smallest symbol enclosing the site.
    pub src: String,
    /// The uid of the target, when the language's rules resolved it.
    pub dst: Option<String>,
    pub kind: EdgeKind,
    /// 1-based line of the site in `src`'s file.
    pub line: u32,
    /// The target as written (`User.find`, `crate::store::open`).
    pub target: String,
    /// What the rule file needs to decide the site again without parsing the
    /// file: its own encoding, opaque to everything else.
    pub context: String,
}

impl Edge {
    pub fn resolved(&self) -> bool {
        self.dst.is_some()
    }
}

/// What a generation was built from (C-11): `tree=`, `gen=`, `extractor=`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The commit checked out when the index was built; `None` before the
    /// first commit.
    pub commit: Option<String>,
    /// Whether the working tree differed from that commit.
    pub dirty: bool,
    /// The generation id: SHA-256 of the (path, content hash) list and the
    /// extractor version, in hex.
    pub generation: String,
    /// The extractor version of each language, `ruby.1,rust.1`.
    pub extractor: String,
    /// Seconds since the Unix epoch when the generation was published.
    pub indexed_at: i64,
}

impl Provenance {
    /// The provenance fields of the map header (`design/map-format.md`):
    /// `tree=3c57d08a gen=9f2c1a extractor=ruby.1,rust.1`, with `+dirty`
    /// after the commit when the tree had changes and `tree=none` before
    /// the first commit.
    pub fn fields(&self) -> String {
        let commit = self.commit.as_deref().map_or("none", |sha| prefix(sha, 8));
        let dirty = if self.dirty { "+dirty" } else { "" };
        let generation = prefix(&self.generation, 6);
        format!(
            "tree={commit}{dirty} gen={generation} extractor={}",
            self.extractor
        )
    }
}

/// The first `chars` characters of `text`: a byte slice could split one.
fn prefix(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(end, _)| &text[..end])
}

/// `2026-10-07T08:12Z`: seconds since the Unix epoch as UTC, to the minute.
pub fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance(commit: Option<&str>, dirty: bool) -> Provenance {
        Provenance {
            commit: commit.map(str::to_owned),
            dirty,
            generation: "9f2c1a7b3e4d".to_owned(),
            extractor: "ruby.1,rust.1".to_owned(),
            indexed_at: 0,
        }
    }

    #[test]
    fn the_fields_name_the_commit_the_generation_and_the_extractor() {
        let clean = provenance(Some("3c57d08a91b2"), false);
        assert_eq!(
            clean.fields(),
            "tree=3c57d08a gen=9f2c1a extractor=ruby.1,rust.1"
        );
        assert!(
            provenance(Some("3c57d08a91b2"), true)
                .fields()
                .starts_with("tree=3c57d08a+dirty ")
        );
        assert!(provenance(None, false).fields().starts_with("tree=none "));
    }

    // D47
    #[test]
    fn the_fields_never_split_a_character() {
        let mut damaged = provenance(Some("abcdefg\u{e9}123"), false);
        damaged.generation = "abcd\u{e9}fg".to_owned();
        assert_eq!(
            damaged.fields(),
            "tree=abcdefg\u{e9} gen=abcd\u{e9}f extractor=ruby.1,rust.1"
        );
    }

    #[test]
    fn utc_renders_known_instants() {
        assert_eq!(utc(0), "1970-01-01T00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00Z");
        assert_eq!(utc(1_791_454_320), "2026-10-08T10:12Z");
    }
}
