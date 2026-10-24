// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `cand-k-code-sites` (`specs/brief.md`, "Recipes"): the code a configuration
//! question reads, not the manual that describes it.
//!
//! Two shapes, both derived from the prompt and the repository:
//!
//! * **env**: an `ALL_CAPS_NAME` of the prompt that a source file spells (not
//!   a doc) is a variable. The function that reads it is shown, then the
//!   functions of the same file it calls, then the function that calls it
//!   (who applies the loaded values). Documentation blocks are dropped when
//!   the code was found: the manual restates what the code shows.
//! * **sites**: "which flags/options/settings govern X, and where are they
//!   used?": the prompt's rare identifier stems (`follows` becomes `follow`)
//!   are searched in non-test code; every hit is mapped to its enclosing
//!   definition, and the definitions that rank highest (a header naming the
//!   stem, several hits, several stems) are shown, at most three per file.
//!
//! Read-only and deterministic; nothing when it has nothing to add.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

use crate::region_pack::Block;

use super::{Ctx, Drop, Outcome, Step, subwords};

const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp", "cs",
    "rb", "swift", "php", "scala",
];
const QUESTION_WORDS: &[&str] = &[
    "which",
    "where",
    "apply",
    "applies",
    "does",
    "what",
    "that",
    "this",
    "with",
    "from",
    "when",
    "they",
    "them",
    "their",
    "used",
    "uses",
    "using",
    "have",
    "flag",
    "flags",
    "option",
    "options",
    "setting",
    "settings",
    "enable",
    "enables",
    "disable",
    "disables",
    "affect",
    "affects",
    "decide",
    "decides",
    "turn",
    "turns",
    "toggle",
    "toggles",
    "switch",
    "parameter",
    "argument",
    "code",
    "value",
    "values",
    "handled",
    "handle",
    "handles",
    "found",
    "find",
    "finds",
    "read",
    "reads",
    "show",
    "shows",
    "make",
    "makes",
    "determine",
];
const ENV_REGION: usize = 60;
const CALLEE_REGION: usize = 56;
const SITE_REGION: usize = 26;
const BUDGET: usize = 7_000;
const LINE_CHARS: usize = 118;
const MAX_FILE_BYTES: u64 = 300_000;
/// Rows a search over the text index may return.
const SEARCH_ROWS: usize = 4_000;
const MAX_ENV_TOKENS: usize = 2;
const MAX_SITE_BLOCKS: usize = 8;
const MAX_PER_FILE: usize = 3;
/// Functions taking a setting as an argument that are followed to their uses.
const MAX_HOPS: usize = 6;
/// Past this many lines a definition is scanned no further.
const SCAN_LINES: usize = 400;

static TESTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(tests?|__tests__|spec|benches|examples?|fuzz|vendor|node_modules|target|testdata)/|(_test|\.test|\.spec)\.\w+$|(^|/)test_[^/]*$",
    )
    .expect("the test-path pattern is a literal")
});
static DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(fn|def|func|function|class|struct|enum|impl|trait|interface|mod|object)\b",
    )
    .expect("the declaration pattern is a literal")
});
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:export\s+)?(?:const|let)\s+\w+\s*=\s*(?:async\s*)?(?:\([^)]*\)|\w+)\s*(?::[^=]+)?=>",
    )
    .expect("the arrow pattern is a literal")
});
static NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:fn|def|function|func)\s*(?:\([^)]*\)\s*)?([A-Za-z_]\w*)")
        .expect("the name pattern is a literal")
});
static ENV_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][A-Z0-9]*_[A-Z0-9_]*[A-Z0-9]\b").expect("the env pattern is a literal")
});
static WHERE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bwhere\b.*\b(appl\w+|used|handled|enforced|checked|honou?red|read)\b")
        .expect("the where pattern is a literal")
});
static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(flags?|options?|settings?|switch(?:es)?|parameters?|arguments?|toggles?)\b")
        .expect("the subject pattern is a literal")
});
static LONG_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{4,}").expect("the word pattern is a literal"));
static CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-z_]\w*)\s*\(").expect("the call pattern is a literal"));
static CALL_WITH_ARGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([a-z_]\w*)\s*\(([^()]*)\)").expect("the call pattern is a literal")
});
static STRING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*""#).expect("the string pattern is a literal"));
static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("the identifier pattern is a literal")
});
static TEST_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*#\[(cfg\(test\)|test)\]").expect("the test attribute pattern is a literal")
});
static COLON_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":\s*$").expect("the colon pattern is a literal"));
static IMPL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*impl\b").expect("the impl pattern is a literal"));
static UNIT_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum)\b").expect("the type pattern is a literal")
});
static TYPE_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum|impl|trait|class|interface|object)\b")
        .expect("the type pattern is a literal")
});

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_comment(line: &str) -> bool {
    let line = line.trim();
    line.is_empty()
        || ["//", "#", "*", "/*", "--", "<!--", "\"\"\"", "'''"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
}

/// Index of the last line of the definition that starts at `d`.
fn decl_end(lines: &[String], d: usize) -> usize {
    let n = indent(&lines[d]);
    let head = lines[d..lines.len().min(d + 3)].join(" ");
    if lines[d].trim_end().ends_with(';') && !lines[d].contains('{') {
        return d;
    }
    // Python style: a colon ends the header and the body is indented.
    if COLON_END.is_match(lines[d].trim_end()) && !head.contains('{') {
        for (j, line) in lines
            .iter()
            .enumerate()
            .take((d + SCAN_LINES).min(lines.len()))
            .skip(d + 1)
        {
            if !line.trim().is_empty() && indent(line) <= n {
                return j - 1;
            }
        }
        return lines.len() - 1;
    }
    for (j, line) in lines
        .iter()
        .enumerate()
        .take((d + SCAN_LINES).min(lines.len()))
        .skip(d + 1)
    {
        if indent(line) == n && line.trim().starts_with('}') {
            return j;
        }
    }
    (lines.len() - 1).min(d + 40)
}

/// A source file: its lines, and, once asked, its definitions with their last
/// lines and the lines of its inline test code.
struct Source {
    lines: Vec<String>,
    defs: OnceCell<Vec<(usize, usize)>>,
    tests: OnceCell<BTreeSet<usize>>,
}

impl Source {
    fn parse(text: &str) -> Self {
        Self {
            lines: text.lines().map(ToString::to_string).collect(),
            defs: OnceCell::new(),
            tests: OnceCell::new(),
        }
    }

    /// The definitions, each with the index of its last line.
    fn defs(&self) -> &[(usize, usize)] {
        self.defs.get_or_init(|| {
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, line)| DECL.is_match(line) || ARROW.is_match(line))
                .map(|(d, _)| (d, decl_end(&self.lines, d)))
                .collect()
        })
    }

    /// Whether line `i` is inline test code (`#[cfg(test)]`, `#[test]`).
    fn is_test(&self, i: usize) -> bool {
        self.tests
            .get_or_init(|| {
                let mut tests = BTreeSet::new();
                for (n, line) in self.lines.iter().enumerate() {
                    if TEST_ATTR.is_match(line) {
                        for d in n + 1..self.lines.len().min(n + 4) {
                            if DECL.is_match(&self.lines[d]) {
                                tests.extend(n..=decl_end(&self.lines, d));
                                break;
                            }
                        }
                    }
                }
                tests
            })
            .contains(&i)
    }

    /// The definitions containing line `i`, innermost first.
    fn chain(&self, i: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .defs()
            .iter()
            .take_while(|(d, _)| *d <= i)
            .filter(|(_, end)| *end >= i)
            .map(|(d, _)| *d)
            .collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out
    }

    fn end(&self, d: usize) -> usize {
        self.defs()
            .iter()
            .find(|(start, _)| *start == d)
            .map_or_else(|| decl_end(&self.lines, d), |(_, end)| *end)
    }

    /// The first line of the definition, with the doc comments and attributes above it.
    fn with_docs(&self, mut d: usize) -> usize {
        while d > 0
            && ["///", "//!", "#[", "@", "/**", "* ", "*/", "//"]
                .iter()
                .any(|prefix| self.lines[d - 1].trim().starts_with(prefix))
        {
            d -= 1;
        }
        d
    }

    /// An `impl ... for Name` also shows the one-line `struct Name;` just
    /// above it, with its docs.
    fn with_type(&self, d: usize) -> usize {
        if !IMPL.is_match(&self.lines[d]) {
            return d;
        }
        let mut e = d.saturating_sub(1);
        while e > 0 && self.lines[e].trim().is_empty() {
            e -= 1;
        }
        if d >= 2
            && e <= d - 2
            && UNIT_TYPE.is_match(&self.lines[e])
            && self.lines[e].trim_end().ends_with(';')
        {
            return self.with_docs(e);
        }
        d
    }

    fn name(&self, d: usize) -> Option<&str> {
        NAME.captures(&self.lines[d])
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str())
    }

    fn block(&self, path: &str, lo: usize, hi: usize, label: &str) -> Block {
        let body = (lo..=hi)
            .map(|n| {
                let line: String = self.lines[n].chars().take(LINE_CHARS).collect();
                format!("{}| {line}", n + 1)
            })
            .collect();
        Block::new(format!("{path}:{}-{} — {label}", lo + 1, hi + 1), body)
    }
}

/// At most `size` lines of `[lo, hi]`, starting at the definition and then
/// around the hits.
fn window(lo: usize, hi: usize, hits: &[usize], size: usize) -> (usize, usize) {
    if hi - lo < size {
        return (lo, hi);
    }
    let mid = match (hits.iter().min(), hits.iter().max()) {
        (Some(low), Some(high)) => (low + high) / 2,
        _ => lo + size / 2,
    };
    let start = mid.saturating_sub(size / 2).max(lo).min(hi + 1 - size);
    (start.max(lo), hi.min(start + size - 1))
}

/// The files a step reads, parsed once.
struct Sources<'a> {
    ctx: &'a Ctx<'a>,
    tracked: &'a [String],
    cache: RefCell<BTreeMap<String, Rc<Source>>>,
}

impl<'a> Sources<'a> {
    fn get(&self, path: &str) -> Rc<Source> {
        if let Some(held) = self.cache.borrow().get(path) {
            return Rc::clone(held);
        }
        let source = Rc::new(Source::parse(
            &self
                .ctx
                .read_up_to(path, MAX_FILE_BYTES)
                .unwrap_or_default(),
        ));
        self.cache
            .borrow_mut()
            .insert(path.to_string(), Rc::clone(&source));
        source
    }

    fn is_code(path: &str) -> bool {
        path.rsplit_once('.')
            .is_some_and(|(_, ext)| CODE_EXTENSIONS.contains(&ext))
            && !TESTY.is_match(path)
    }

    /// Rows of the text index that match `pattern`: `(path, line - 1, text)`.
    fn rows(&self, pattern: &str) -> Vec<(String, usize, String)> {
        let Ok(found) = self
            .ctx
            .evidence
            .grep(pattern, SEARCH_ROWS, self.ctx.deadline)
        else {
            return Vec::new();
        };
        found
            .hits
            .into_iter()
            .filter_map(|hit| {
                let text = hit.text?;
                Some((
                    hit.path,
                    usize::try_from(hit.line).ok()?.checked_sub(1)?,
                    text,
                ))
            })
            .collect()
    }

    /// The code rows of a whole-word search for `name`.
    fn word_rows(&self, name: &str) -> Vec<(String, usize, String)> {
        self.rows(&format!(r"\b{}\b", regex::escape(name)))
            .into_iter()
            .filter(|(path, ..)| Self::is_code(path))
            .collect()
    }

    fn in_test(&self, path: &str, i: usize) -> bool {
        self.get(path).is_test(i)
    }

    fn code_count(&self) -> usize {
        self.tracked
            .iter()
            .filter(|path| Self::is_code(path))
            .count()
            .max(1)
    }
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(Step::CodeSites, &[]) {
        return;
    }
    let env = ENV_TOKEN
        .find_iter(ctx.typed)
        .any(|found| found.as_str().len() > 5);
    let sites = WHERE.is_match(ctx.typed) && SUBJECT.is_match(ctx.typed);
    if !(env || sites) {
        return;
    }
    let _timer = ctx.timer(Step::CodeSites);
    let Some(tracked) = ctx.files() else {
        return;
    };
    let sources = Sources {
        ctx,
        tracked,
        cache: RefCell::default(),
    };
    // A prompt that names a variable the code never reads falls through to
    // the question about flags, if it is one.
    if let Some(blocks) = env_mode(&sources).filter(|blocks| !blocks.is_empty()) {
        for block in &blocks {
            if let Some(path) = block.head().split(':').next() {
                out.drops.push(Drop::WholeFile(path.to_string()));
            }
        }
        out.drops.push(Drop::Docs);
        out.rescuing = true;
        out.lead.extend(blocks);
        return;
    }
    if let Some((blocks, stems)) = sites_mode(&sources) {
        out.drops.push(Drop::ConfigRegistry);
        out.drops.push(Drop::Unnamed(stems));
        out.drops.push(Drop::Docs);
        out.rescuing = true;
        out.lead.extend(blocks);
    }
}

/// The unique `ALL_CAPS` names of the prompt, longer than five characters.
fn env_tokens(prompt: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ENV_TOKEN
        .find_iter(prompt)
        .map(|found| found.as_str().to_string())
        .filter(|token| token.len() > 5 && seen.insert(token.clone()))
        .collect()
}

/// `None` when the prompt names no variable; otherwise the blocks that fit
/// the budget (possibly none).
fn env_mode(sources: &Sources) -> Option<Vec<Block>> {
    let tokens = env_tokens(sources.ctx.typed);
    if tokens.is_empty() {
        return None;
    }
    // (priority, block): the reading function first, then who applies the
    // values, then the functions it calls.
    let mut blocks: Vec<(usize, Block)> = Vec::new();
    let mut seen: BTreeSet<(String, usize)> = BTreeSet::new();
    for token in tokens.iter().take(MAX_ENV_TOKENS) {
        let mut sites: Vec<(String, usize)> = sources
            .word_rows(token)
            .into_iter()
            .filter(|(path, i, text)| !is_comment(text) && !sources.in_test(path, *i))
            .map(|(path, i, _)| (path, i))
            .collect();
        sites.sort();
        sites.dedup();
        for (path, i) in sites.into_iter().take(2) {
            if sources.ctx.timed_out() {
                return Some(select(blocks));
            }
            let source = sources.get(&path);
            let Some(&d) = source.chain(i).first() else {
                continue;
            };
            let (lo, hi) = window(source.with_docs(d), source.end(d), &[i], ENV_REGION);
            if !seen.insert((path.clone(), lo)) {
                continue;
            }
            blocks.push((0, source.block(&path, lo, hi, &format!("reads {token}"))));
            let own = source.name(d).map(ToString::to_string);
            let label = own.clone().unwrap_or_else(|| "?".to_string());

            // The functions of this file it calls, two levels deep.
            let mut callees: Vec<usize> = Vec::new();
            let mut depth_of: BTreeMap<usize, usize> = BTreeMap::new();
            let mut frontier = vec![d];
            for depth in 0..2 {
                let mut found = Vec::new();
                for &f in &frontier {
                    let body = source.lines[f..=source.end(f)].join("\n");
                    let mut called = BTreeSet::new();
                    for caps in CALL.captures_iter(&body) {
                        let name = &caps[1];
                        if own.as_deref() == Some(name) || !called.insert(name.to_string()) {
                            continue;
                        }
                        let target = source.defs().iter().map(|(e, _)| *e).find(|e| {
                            source.name(*e) == Some(name)
                                && !callees.contains(e)
                                && *e != d
                                && indent(&source.lines[*e]) <= indent(&source.lines[d]) + 4
                        });
                        if let Some(e) = target {
                            callees.push(e);
                            depth_of.insert(e, depth);
                            found.push(e);
                        }
                    }
                }
                frontier = found;
            }
            callees.sort_unstable();
            for &e in callees.iter().take(4) {
                let (clo, chi) = window(source.with_docs(e), source.end(e), &[], CALLEE_REGION);
                if seen.insert((path.clone(), clo)) {
                    blocks.push((
                        2 + depth_of[&e],
                        source.block(&path, clo, chi, &format!("callee of {label}")),
                    ));
                }
            }
            // Who calls it: where the loaded values take effect.
            let Some(own) = own else {
                continue;
            };
            let stem = path
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit_once('.').map(|(stem, _)| stem))
                .unwrap_or("");
            let qualified = format!(r"\b{}::{}\s*\(", regex::escape(stem), regex::escape(&own));
            let mut calls: Vec<(String, usize, String)> = sources
                .rows(&qualified)
                .into_iter()
                .filter(|(path, ..)| Sources::is_code(path))
                .collect();
            if calls.is_empty() && own.len() > 5 {
                calls = sources
                    .rows(&format!(r"\b{}\s*\(", regex::escape(&own)))
                    .into_iter()
                    .filter(|(path, _, text)| {
                        Sources::is_code(path) && !DECL.is_match(text) && !is_comment(text)
                    })
                    .collect();
            }
            for (cpath, cline, _) in calls.into_iter().take(2) {
                let csource = sources.get(&cpath);
                let Some(&e) = csource.chain(cline).first() else {
                    continue;
                };
                let (clo, chi) = window(
                    csource.with_docs(e),
                    csource.end(e),
                    &[cline],
                    CALLEE_REGION,
                );
                if seen.insert((cpath.clone(), clo)) {
                    blocks.push((
                        1,
                        csource.block(
                            &cpath,
                            clo,
                            chi,
                            &format!("calls {own}, where the loaded values take effect"),
                        ),
                    ));
                }
            }
        }
    }
    Some(select(blocks))
}

/// The blocks, best priority first, that fit the budget, in their first order.
fn select(blocks: Vec<(usize, Block)>) -> Vec<Block> {
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by_key(|n| (blocks[*n].0, *n));
    let mut keep = BTreeSet::new();
    let mut used = 0;
    for n in order {
        let size = blocks[n].1.text().len() + 1;
        if used + size <= BUDGET {
            keep.insert(n);
            used += size;
        }
    }
    blocks
        .into_iter()
        .enumerate()
        .filter(|(n, _)| keep.contains(n))
        .map(|(_, (_, block))| block)
        .collect()
}

/// The prompt's identifier stems: words of four letters or more that are not
/// question words, with one inflection cut.
fn stems_of(prompt: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for found in LONG_WORD.find_iter(prompt) {
        let mut word = found.as_str().to_lowercase();
        if !seen.insert(word.clone()) || QUESTION_WORDS.contains(&word.as_str()) {
            continue;
        }
        for suffix in ["ing", "ed", "es", "s"] {
            if word.chars().count() > 5
                && let Some(cut) = word.strip_suffix(suffix)
            {
                word = cut.to_string();
                break;
            }
        }
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// The lowercase subwords of every identifier in `text`.
fn parts_of(text: &str) -> BTreeSet<String> {
    IDENT
        .find_iter(text)
        .flat_map(|ident| subwords(ident.as_str()))
        .collect()
}

/// Whether `text` holds `word` as a whole identifier, whatever its case.
fn has_ident(text: &str, word: &str) -> bool {
    let lower = text.to_lowercase();
    let is_ident = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
    lower.match_indices(word).any(|(start, found)| {
        lower[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident(ch))
            && lower[start + found.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !is_ident(ch))
    })
}

/// How much a hit says about the value being applied: a declaration or an
/// initialiser is plumbing, a call or a condition is where it takes effect.
struct Weigher {
    declaration: Regex,
    call: Regex,
}

static CONDITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(if|while|match|&&|\|\|)\b").expect("the condition pattern is a literal")
});

impl Weigher {
    fn new(stem: &str) -> Option<Self> {
        let stem = regex::escape(stem);
        Some(Self {
            declaration: Regex::new(&format!(r"(?i)^(pub(\([^)]*\))?\s+)?\w*{stem}\w*\s*:"))
                .ok()?,
            call: Regex::new(&format!(r"(?i)\w*{stem}\w*\s*\(")).ok()?,
        })
    }

    fn weight(&self, text: &str) -> f64 {
        let s = text.trim();
        if self.declaration.is_match(s) {
            0.3
        } else if self.call.is_match(s) || CONDITION.is_match(s) {
            2.0
        } else {
            1.0
        }
    }
}

/// A region of code the hits fall in.
#[derive(Default)]
struct Region {
    hits: BTreeSet<usize>,
    stems: BTreeSet<String>,
    end: usize,
    named: bool,
    is_type: bool,
    weight: f64,
}

type Hit = (String, usize, String);

/// `None` when the prompt asks about no flag or setting, or nothing rare is
/// declared in code; otherwise the blocks and the stems that name them.
fn sites_mode(sources: &Sources) -> Option<(Vec<Block>, Vec<String>)> {
    let prompt = sources.ctx.typed;
    if !(WHERE.is_match(prompt) && SUBJECT.is_match(prompt)) {
        return None;
    }
    let stems = stems_of(prompt);
    if stems.is_empty() {
        return None;
    }
    let ncode = sources.code_count();
    let alternation: Vec<String> = stems.iter().map(|stem| regex::escape(stem)).collect();
    let found: Vec<Hit> = sources
        .rows(&format!("(?i)(?:{})", alternation.join("|")))
        .into_iter()
        .filter(|(path, ..)| Sources::is_code(path))
        .collect();

    let threshold = 4.max((ncode as f64 * 0.15) as usize);
    let mut keep: Vec<(String, Vec<Hit>)> = Vec::new();
    for stem in &stems {
        if sources.ctx.timed_out() {
            return None;
        }
        // The cheap tests first, on the row alone: a stem that is a word of
        // half the repository is dismissed before any file is read.
        let plural = format!("{stem}s");
        let past = format!("{stem}ed");
        let named: Vec<&Hit> = found
            .iter()
            .filter(|(_, _, text)| text.to_lowercase().contains(stem.as_str()) && !is_comment(text))
            .filter(|(_, _, text)| {
                let parts = parts_of(text);
                parts.contains(stem) || parts.contains(&plural) || parts.contains(&past)
            })
            .collect();
        let cheap_files: BTreeSet<&str> = named.iter().map(|(path, ..)| path.as_str()).collect();
        if named.len() < 2 || cheap_files.len() > threshold * 2 {
            continue;
        }
        let hits: Vec<Hit> = named
            .into_iter()
            .filter(|(path, i, _)| {
                let source = sources.get(path);
                *i < source.lines.len() && !source.is_test(*i)
            })
            .cloned()
            .collect();
        let files: BTreeSet<&str> = hits.iter().map(|(path, ..)| path.as_str()).collect();
        let escaped = regex::escape(stem);
        let declared = Regex::new(&format!(
            r"(?i)\b(?:fn|struct|enum|trait|class|def|const|static|let|var|type|func|function)\s+(?:mut\s+)?\w*{escaped}|^\s*(?:pub(?:\([^)]*\))?\s+)?\w*{escaped}\w*\s*:\s*[A-Za-z&]"
        ))
        .ok()?;
        if !hits.iter().any(|(_, _, text)| declared.is_match(text)) {
            continue;
        }
        if hits.len() >= 2 && (1..=threshold).contains(&files.len()) {
            keep.push((stem.clone(), hits));
        }
    }
    if keep.is_empty() {
        return None;
    }

    // Functions that take the setting as an argument are sites too.
    let originals: Vec<(String, Vec<Hit>)> = {
        let mut sorted = keep.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted
    };
    for (stem, hits) in &originals {
        let mut hop = BTreeSet::new();
        for (_, _, text) in hits {
            let plain = STRING.replace_all(text, "\"\"");
            for caps in CALL_WITH_ARGS.captures_iter(&plain) {
                if has_ident(&caps[2], stem) {
                    hop.insert(caps[1].to_string());
                }
            }
        }
        for name in hop.into_iter().take(MAX_HOPS) {
            let extra: Vec<Hit> = sources
                .word_rows(&name)
                .into_iter()
                .filter(|(path, i, text)| {
                    let source = sources.get(path);
                    !is_comment(text) && *i < source.lines.len() && !source.is_test(*i)
                })
                .collect();
            let key = format!("%{name}");
            match keep.iter_mut().find(|(held, _)| *held == key) {
                Some((_, held)) => held.extend(extra),
                None => keep.push((key, extra)),
            }
        }
    }

    let mut regions: BTreeMap<(String, usize), Region> = BTreeMap::new();
    for (key, hits) in &keep {
        let stem = key.trim_start_matches('%');
        let weigher = Weigher::new(stem)?;
        for (path, i, text) in hits {
            if sources.ctx.timed_out() {
                return None;
            }
            let source = sources.get(path);
            let chain = source.chain(*i);
            let Some(&innermost) = chain.first() else {
                continue;
            };
            let mut pick = innermost;
            // The outermost definition that names the stem and is small enough.
            for &d in chain.iter().rev() {
                if source.lines[d].to_lowercase().contains(stem) && source.end(d) - d < 48 {
                    pick = d;
                    break;
                }
            }
            let region = regions
                .entry((path.clone(), pick))
                .or_insert_with(|| Region {
                    end: source.end(pick),
                    ..Region::default()
                });
            region.hits.insert(*i);
            region.stems.insert(stem.to_string());
            region.weight += weigher.weight(text);
            if source.lines[pick].to_lowercase().contains(stem) {
                region.named = true;
                region.is_type = TYPE_DECL.is_match(&source.lines[pick]);
            }
        }
    }
    let score = |(path_d, region): &(&(String, usize), &Region)| -> f64 {
        let (_, d) = path_d;
        let short = region.end.saturating_sub(*d) < 10 && !region.is_type;
        -(region.weight - if short { 3.0 } else { 0.0 }
            + if region.is_type { 3.0 } else { 0.0 }
            + 0.5 * region.stems.len() as f64)
    };
    let mut ranked: Vec<(&(String, usize), &Region)> = regions.iter().collect();
    ranked.sort_by(|a, b| score(a).total_cmp(&score(b)).then(a.0.cmp(b.0)));

    let mut blocks: Vec<Block> = Vec::new();
    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    let mut used = 0;
    for ((path, d), region) in ranked {
        if per_file.get(path.as_str()).copied().unwrap_or(0) >= MAX_PER_FILE
            || blocks.len() >= MAX_SITE_BLOCKS
            || region.end - d < 2
        {
            continue;
        }
        let source = sources.get(path);
        let size = if region.named {
            40
        } else if blocks.len() < 4 {
            SITE_REGION
        } else {
            16
        };
        let hits: Vec<usize> = region.hits.iter().copied().collect();
        let (lo, hi) = window(
            source.with_type(source.with_docs(*d)),
            region.end,
            &hits,
            size,
        );
        let label = format!(
            "names {}",
            region.stems.iter().cloned().collect::<Vec<_>>().join("/")
        );
        let block = source.block(path, lo, hi, &label);
        let cost = block.text().len() + 1;
        if used + cost > BUDGET {
            continue;
        }
        used += cost;
        *per_file.entry(path).or_default() += 1;
        named.extend(region.stems.iter().cloned());
        blocks.push(block);
    }
    (!blocks.is_empty()).then(|| (blocks, named.into_iter().collect()))
}

#[cfg(test)]
mod tests;
