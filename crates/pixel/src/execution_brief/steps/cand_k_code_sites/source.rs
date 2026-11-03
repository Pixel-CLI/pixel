// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What `cand-k-code-sites` reads of the repository: the lines of a tracked
//! file with its `#[cfg(test)]` regions, the definition that contains a line,
//! and the search of the code files that `rg` did for the script.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

use super::Ctx;
use crate::execution_brief::steps::text::{head, indent, is_word, re, splitlines_owned};


/// Extensions of the files a question about code is answered from.
const CODE_EXT: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp", "cs",
    "rb", "swift", "php", "scala",
];
/// Characters of a quoted line.
const LINE_CHARS: usize = 118;
/// Bytes of a file whose lines are quoted; a longer one is read as empty.
const MAX_FILE_BYTES: u64 = 300_000;
/// Bytes of a file that is searched; `rg` has no limit, a repository's
/// generated bundle must not stop the walk.
const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
/// Bytes of searched text kept for the next search of the run.
const MAX_CACHED_BYTES: usize = 96 * 1024 * 1024;

static TESTY: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(^|/)(tests?|__tests__|spec|benches|examples?|fuzz|vendor|node_modules|target|testdata)/|(_test|\.test|\.spec)\.\w+$|(^|/)test_[^/]*$",
    )
});
static DECL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(fn|def|func|function|class|struct|enum|impl|trait|interface|mod|object)\b",
    )
});
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^\s*(?:export\s+)?(?:const|let)\s+\w+\s*=\s*(?:async\s*)?(?:\([^)]*\)|\w+)\s*(?::[^=]+)?=>",
    )
});
static NAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:fn|def|function|func)\s*(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"));
static TEST_ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*#\[(cfg\(test\)|test)\]"));
static IMPL: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*impl\b"));
static ONE_LINE_TYPE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum)\b"));
static TYPE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    re(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum|impl|trait|class|interface|object)\b")
});

/// A line that matched a search: 0-based line, the line as `rg` printed it.
#[derive(Clone)]
pub(super) struct Hit {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
}

/// The lines of a file and the lines of its test-only definitions.
pub(super) struct Lines {
    pub(super) rows: Vec<String>,
    tests: HashSet<usize>,
}

/// The tracked files of the repository, read through the step's context.
pub(super) struct Source<'a, 'b> {
    ctx: &'a Ctx<'b>,
    lines: HashMap<String, Rc<Lines>>,
    texts: HashMap<String, Option<Rc<Vec<String>>>>,
    cached: usize,
    searchable: Option<Vec<String>>,
}

impl<'a, 'b> Source<'a, 'b> {
    pub(super) fn new(ctx: &'a Ctx<'b>) -> Self {
        Self {
            ctx,
            lines: HashMap::new(),
            texts: HashMap::new(),
            cached: 0,
            searchable: None,
        }
    }

    /// Whether `path` is a source file that is not a test, an example or a
    /// vendored tree.
    pub(super) fn is_code(path: &str) -> bool {
        path.rsplit('.')
            .next()
            .is_some_and(|ext| CODE_EXT.contains(&ext))
            && !TESTY.is_match(path)
    }

    /// How many tracked files are code.
    pub(super) fn code_files(&self) -> usize {
        self.ctx
            .files()
            .map_or(0, |files| files.iter().filter(|path| Self::is_code(path)).count())
    }

    /// The lines of `path` (`splitlines`), empty when it cannot be read.
    pub(super) fn lines(&mut self, path: &str) -> Rc<Lines> {
        if let Some(found) = self.lines.get(path) {
            return Rc::clone(found);
        }
        let rows = self
            .ctx
            .read(path, MAX_FILE_BYTES)
            .map_or_else(Vec::new, |text| splitlines_owned(&text));
        let mut tests = HashSet::new();
        for (n, line) in rows.iter().enumerate() {
            if TEST_ATTRIBUTE.is_match(line) {
                for d in n + 1..rows.len().min(n + 4) {
                    if DECL.is_match(&rows[d]) {
                        tests.extend(n..=decl_end(&rows, d));
                        break;
                    }
                }
            }
        }
        let entry = Rc::new(Lines { rows, tests });
        self.lines.insert(path.to_owned(), Rc::clone(&entry));
        entry
    }

    /// Whether line `i` of `path` is inside a `#[test]` or `#[cfg(test)]` item.
    pub(super) fn is_test(&mut self, path: &str, i: usize) -> bool {
        self.lines(path).tests.contains(&i)
    }

    /// Every line of the code files that `matches`, in the order `rg --sort
    /// path` prints them. `None` when the clock ran out.
    pub(super) fn search(&mut self, matches: &dyn Fn(&str) -> bool) -> Option<Vec<Hit>> {
        let paths = self.searchable_paths();
        let mut hits = Vec::new();
        for path in paths {
            if self.ctx.timed_out() {
                return None;
            }
            let Some(text) = self.text(&path) else {
                continue;
            };
            for (i, line) in text.iter().enumerate() {
                if matches(line) {
                    hits.push(Hit {
                        path: path.clone(),
                        line: i,
                        text: line.clone(),
                    });
                }
            }
        }
        Some(hits)
    }

    /// The tracked code files `rg` would walk: no hidden component, sorted
    /// path component by component.
    fn searchable_paths(&mut self) -> Vec<String> {
        if let Some(paths) = &self.searchable {
            return paths.clone();
        }
        let mut paths: Vec<String> = self
            .ctx
            .files()
            .unwrap_or_default()
            .iter()
            .filter(|path| Self::is_code(path) && !path.split('/').any(|part| part.starts_with('.')))
            .cloned()
            .collect();
        paths.sort_by(|a, b| a.split('/').cmp(b.split('/')));
        self.searchable = Some(paths.clone());
        paths
    }

    /// The lines of `path` as `rg` splits them; `None` for a binary or an
    /// unreadable file.
    fn text(&mut self, path: &str) -> Option<Rc<Vec<String>>> {
        if let Some(found) = self.texts.get(path) {
            return found.clone();
        }
        let text = self
            .ctx
            .read(path, MAX_SEARCH_BYTES)
            .filter(|text| !text.contains('\0'));
        let found = text.as_ref().map(|text| {
            let mut rows: Vec<String> = text
                .split('\n')
                .map(|row| row.strip_suffix('\r').unwrap_or(row).to_owned())
                .collect();
            if text.ends_with('\n') {
                rows.pop();
            }
            Rc::new(rows)
        });
        let size = text.map_or(0, |text| text.len());
        if self.cached + size <= MAX_CACHED_BYTES {
            self.cached += size;
            self.texts.insert(path.to_owned(), found.clone());
        }
        found
    }
}

/// `-w -F needle` of `rg`: an occurrence with no word character on either side.
pub(super) fn has_word(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = line[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before = line[..start].chars().next_back().is_none_or(|ch| !is_word(ch));
        let after = line[end..].chars().next().is_none_or(|ch| !is_word(ch));
        if before && after {
            return true;
        }
        from = start + line[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}


/// A blank line or one that starts a comment.
pub(super) fn is_comment(line: &str) -> bool {
    let line = line.trim();
    line.is_empty()
        || ["//", "#", "*", "/*", "--", "<!--", "\"\"\"", "'''"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
}

/// Index of the last line of the definition that starts at `d`.
pub(super) fn decl_end(lines: &[String], d: usize) -> usize {
    let n = indent(&lines[d]);
    let head = lines[d..lines.len().min(d + 3)].join(" ");
    let first = lines[d].trim_end();
    if first.ends_with(';') && !lines[d].contains('{') {
        return d;
    }
    let bound = lines.len().min(d + 400);
    if first.ends_with(':') && !head.contains('{') {
        return (d + 1..bound)
            .find(|j| !lines[*j].trim().is_empty() && indent(&lines[*j]) <= n)
            .map_or(lines.len() - 1, |j| j - 1);
    }
    (d + 1..bound)
        .find(|j| indent(&lines[*j]) == n && lines[*j].trim().starts_with('}'))
        .unwrap_or_else(|| (lines.len() - 1).min(d + 40))
}

/// The lines that start a definition.
pub(super) fn decls_of(lines: &[String]) -> Vec<usize> {
    (0..lines.len())
        .filter(|i| DECL.is_match(&lines[*i]) || ARROW.is_match(&lines[*i]))
        .collect()
}

/// The definitions containing line `i`, innermost first. `ends` remembers
/// where each one closes.
pub(super) fn chain(
    lines: &[String],
    decls: &[usize],
    ends: &mut HashMap<usize, usize>,
    i: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    for d in decls {
        if *d > i {
            break;
        }
        let end = *ends.entry(*d).or_insert_with(|| decl_end(lines, *d));
        if end >= i {
            out.push(*d);
        }
    }
    out.sort_by(|a, b| b.cmp(a));
    out
}

/// `d` moved up over the doc comments, attributes and decorators above it.
pub(super) fn with_docs(lines: &[String], mut d: usize) -> usize {
    const PREFIXES: [&str; 8] = ["///", "//!", "#[", "@", "/**", "* ", "*/", "//"];
    while d > 0 && PREFIXES.iter().any(|p| lines[d - 1].trim().starts_with(p)) {
        d -= 1;
    }
    d
}

/// An `impl ... for Name` also shows the one-line `struct Name;` just above it, with its docs.
pub(super) fn with_type(lines: &[String], d: usize) -> usize {
    if !IMPL.is_match(&lines[d]) {
        return d;
    }
    let mut e = d as i64 - 1;
    while e > 0 && lines[e as usize].trim().is_empty() {
        e -= 1;
    }
    if e <= d as i64 - 2
        && e >= 0
        && ONE_LINE_TYPE.is_match(&lines[e as usize])
        && lines[e as usize].trim_end().ends_with(';')
    {
        return with_docs(lines, e as usize);
    }
    d
}

/// Whether the line opens a type, an impl or a trait.
pub(super) fn is_type_head(line: &str) -> bool {
    TYPE_HEAD.is_match(line)
}

/// At most `size` lines of `[lo, hi]`, starting at the definition and then around the hits.
pub(super) fn window(lo: usize, hi: usize, hits: &[usize], size: usize) -> (usize, usize) {
    let (lo, hi, size) = (lo as i64, hi as i64, size as i64);
    if hi - lo < size {
        return (lo as usize, hi as usize);
    }
    let mid = match (hits.iter().min(), hits.iter().max()) {
        (Some(low), Some(high)) => (*low as i64 + *high as i64) / 2,
        _ => lo + size / 2,
    };
    let start = (mid - size / 2).max(lo).min(hi - size + 1);
    (lo.max(start) as usize, hi.min(start + size - 1) as usize)
}

/// The name a definition line declares.
pub(super) fn fname(lines: &[String], d: usize) -> Option<String> {
    NAME.captures(&lines[d])
        .and_then(|found| found.get(1))
        .map(|name| name.as_str().to_owned())
}

/// Whether the line declares a function, a type or a module.
pub(super) fn is_decl(line: &str) -> bool {
    DECL.is_match(line)
}

/// A `### path:first-last — label` header and the numbered lines `lo..=hi`.
pub(super) fn block(rows: &[String], path: &str, lo: usize, hi: usize, label: &str) -> Vec<String> {
    let mut out = vec![format!("### {path}:{}-{} — {label}", lo + 1, hi + 1)];
    for n in lo..=hi {
        let row = rows.get(n).map_or("", String::as_str);
        out.push(format!("{}| {}", n + 1, head(row, LINE_CHARS)));
    }
    out
}

/// The characters of a block, a newline after each line.
pub(super) fn size(block: &[String]) -> usize {
    block.iter().map(|line| line.chars().count() + 1).sum()
}
