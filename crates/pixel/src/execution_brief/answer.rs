// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What lets a confident brief be answered from: the search receipt (which
//! searches ran, over what) and answer-sized excerpts built from the evidence
//! the chain gathered. Every function here is pure: the chain reads the
//! files, these shape the text, so each kind's excerpt is tested on its own.

use std::fmt::Write as _;

use super::chain::RichHit;
use super::decision_log;
use super::relevance::RelevanceInput;

/// Environment switch of the search receipt (`0`, `false` or `off` removes it).
pub(crate) const RECEIPT_ENV: &str = "PIXEL_BRIEF_RECEIPT";
/// Environment switch of the answer-sized excerpts.
pub(crate) const ANSWER_ENV: &str = "PIXEL_BRIEF_ANSWER";

/// Terms and ignored words one receipt line names.
const MAX_TERMS_SHOWN: usize = 6;
const MAX_TERM_CHARS: usize = 24;
/// Lines of source read before the matched line, to reach the signature and
/// its doc comment, and after it.
pub(crate) const WINDOW_BEFORE: u64 = 12;
#[cfg(test)]
const WINDOW_AFTER: u64 = 9;
/// Body lines an excerpt shows after the signature and doc line.
#[cfg(test)]
const BODY_LINES: usize = 8;
/// Chars of one excerpt line.
const LINE_CHARS: usize = 110;
/// Hops a flow excerpt names.
pub(crate) const MAX_HOPS: usize = 5;
/// Test functions a tests excerpt names.
pub(crate) const MAX_TESTS: usize = 4;
/// Lines of a test file read to find its test functions.
pub(crate) const TEST_FILE_LINES: u64 = 4000;
/// Lines one test function is searched for its first assertion and its
/// mention of the target.
const TEST_SPAN: usize = 40;
/// Lines a config excerpt shows.
const MAX_CONFIG_LINES: usize = 4;

/// Environment kill switch of the ZERO step (`0`, `false` or `off`): the
/// brief then never tells the agent to answer from its excerpt.
pub(crate) const ZERO_ENV: &str = "PIXEL_BRIEF_ZERO";

pub(crate) fn zero_enabled() -> bool {
    decision_log::enabled(std::env::var(ZERO_ENV).ok().as_deref())
}

/// A ZERO rule: the best rank of the top chunk among the meaning search's,
/// the best rank of its file among the lexical co-files, and the fewest
/// distinct probed keywords inside the chunk.
pub(crate) type ZeroRule = (usize, usize, usize);

/// The rule the brief applies. `None`: no rule tried on the dev split of
/// `eval/brief-gate/answer_spans.jsonl` kept the span precision at 0.9 with
/// two firings (the strict rule `(0, 0, 2)` fired once, right; the loosest
/// fired 5 times, right twice), so the ZERO step never fires.
pub(crate) const ZERO_RULE: Option<ZeroRule> = None;

/// The ZERO rule `rule`: the top excerpt is the meaning search's chunk of
/// rank at most `rule.0`, lies in a lexical co-file of rank at most
/// `rule.1`, and holds at least `rule.2` distinct probed keywords.
pub(crate) fn rule_holds(top: &Excerpt, lexical_rank: Option<usize>, rule: ZeroRule) -> bool {
    top.from_meaning
        && top.meaning_rank.is_some_and(|rank| rank <= rule.0)
        && lexical_rank.is_some_and(|rank| rank <= rule.1)
        && top.density >= rule.2
}

pub(crate) fn receipt_enabled() -> bool {
    decision_log::enabled(std::env::var(RECEIPT_ENV).ok().as_deref())
}

pub(crate) fn answer_enabled() -> bool {
    decision_log::enabled(std::env::var(ANSWER_ENV).ok().as_deref())
}

/// What the search of one brief covered, taken from the probes that answered
/// and from nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    terms: Vec<String>,
    ignored: Vec<String>,
    files_considered: usize,
    /// Chunks the meaning search returned; `None` when it did not answer.
    chunks: Option<usize>,
}

impl Receipt {
    /// The receipt of a relevance probe that answered (`input`) and, when it
    /// answered too, a meaning search that returned `chunks` chunks. `None`
    /// when the probe weighed no term.
    pub(crate) fn new(input: &RelevanceInput, chunks: Option<usize>) -> Option<Self> {
        let mut terms: Vec<String> = Vec::new();
        let mut ignored: Vec<String> = Vec::new();
        for stat in &input.keywords {
            let list = if stat.weight > 0.0 {
                &mut terms
            } else {
                &mut ignored
            };
            if !list.contains(&stat.keyword) {
                list.push(stat.keyword.clone());
            }
        }
        (!terms.is_empty()).then_some(Self {
            terms,
            ignored,
            files_considered: input.files_considered,
            chunks,
        })
    }

    /// The terms the probe weighed, as the receipt lists them.
    pub(crate) fn terms(&self) -> &[String] {
        &self.terms
    }

    /// The receipt lines: facts, then the instruction they support. Only
    /// `agreed` (two independent retrievers named the same file) earns the
    /// instruction to answer from the matches; otherwise the line asks the
    /// agent to check them.
    pub(crate) fn lines(&self, agreed: bool) -> Vec<String> {
        let mut searched = format!(
            "searched: content+symbols+paths for {} ({} terms, {} files)",
            names(&self.terms),
            self.terms.len(),
            self.files_considered
        );
        if let Some(chunks) = self.chunks {
            let _ = write!(searched, " · meaning search returned {chunks} chunks");
        }
        let mut lines = vec![searched];
        if !self.ignored.is_empty() {
            lines.push(format!("ignored (too common): {}", names(&self.ignored)));
        }
        let scope = if self.chunks.is_some() {
            "both searches"
        } else {
            "the search"
        };
        lines.push(if agreed {
            format!(
                "result: the matches below are the best across {scope}; answer from them if they suffice, search further only if they don't"
            )
        } else {
            format!(
                "result: the matches below are the best across {scope}; verify they answer the question"
            )
        });
        lines
    }
}

fn names(list: &[String]) -> String {
    let shown: Vec<String> = list
        .iter()
        .take(MAX_TERMS_SHOWN)
        .map(|name| name.chars().take(MAX_TERM_CHARS).collect())
        .collect();
    let hidden = list.len().saturating_sub(MAX_TERMS_SHOWN);
    if hidden == 0 {
        shown.join(", ")
    } else {
        format!("{} (+{hidden})", shown.join(", "))
    }
}

/// One block of answer evidence: a label and its source lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) label: String,
    /// The signature line the excerpt was cut under, when it found one.
    pub(crate) sig: Option<u64>,
    /// The whole declaration around it, for a `read:` range.
    pub(crate) read: Option<(u64, u64)>,
    /// What it declares.
    pub(crate) symbol: Option<String>,
    /// Rank of the chunk among the meaning search's, best first; `None`
    /// for a lexical hit.
    pub(crate) meaning_rank: Option<usize>,
    /// Distinct probed keywords inside the chunk.
    pub(crate) density: usize,
    /// The file the block was cut from, empty for a kind block that spans
    /// several.
    pub(crate) path: String,
    /// The block comes from the meaning search's own chunk.
    pub(crate) from_meaning: bool,
    pub(crate) lines: Vec<String>,
}

/// Words of the question that ask about tests, and about documentation.
const TEST_TERMS: &[&str] = &["test", "tests", "tested", "testing", "spec", "specs"];
const DOC_TERMS: &[&str] = &[
    "doc",
    "docs",
    "documentation",
    "readme",
    "changelog",
    "guide",
    "manual",
    "benchmark",
    "benchmarks",
    "bench",
    "eval",
    "evals",
];

/// Whether `terms` (the probed words of the question) name any of `words`.
pub(crate) fn asks_about(terms: &[String], words: &[&str]) -> bool {
    terms
        .iter()
        .any(|term| words.contains(&term.to_lowercase().as_str()))
}

pub(crate) fn asks_tests(terms: &[String]) -> bool {
    asks_about(terms, TEST_TERMS)
}

pub(crate) fn asks_docs(terms: &[String]) -> bool {
    asks_about(terms, DOC_TERMS)
}

/// Whether `path` is a test file by its name or its directory.
pub(crate) fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|part| matches!(part, "tests" | "test" | "__tests__" | "e2e"))
        || name.contains("_test.")
        || name.contains("_tests.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name == "tests.rs"
}

/// Whether `path` is prose or measurement a reader opens for documentation,
/// not code: markdown and text, `docs/`, `changelog.d/`, `eval/`, a
/// `CHANGELOG*` file.
pub(crate) fn is_docs_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit('.').next().unwrap_or("");
    matches!(extension, "md" | "mdx" | "rst" | "txt" | "adoc")
        || name.to_ascii_uppercase().starts_with("CHANGELOG")
        || path.starts_with("docs/")
        || path.starts_with("changelog.d/")
        || path.starts_with("eval/")
}

/// What the typed question asks about, which lifts the demotion of the
/// matching files: `(tests, docs)`.
pub(crate) fn cues(typed: &str) -> (bool, bool) {
    let words: Vec<String> = typed
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    (asks_tests(&words), asks_docs(&words))
}

/// Whether a path ranks after the code files: a test, or documentation and
/// measurement, that the question did not ask about. The one predicate of
/// the ranked file list and of the excerpts.
pub(crate) fn demoted(path: &str, wants_tests: bool, wants_docs: bool) -> bool {
    (!wants_tests && is_test_path(path)) || (!wants_docs && is_docs_path(path))
}

/// Whether a symbol is named like a test.
pub(crate) fn is_test_symbol(name: &str) -> bool {
    name.contains("_should_") || name.starts_with("test_") || name.ends_with("_test")
}

/// Whether line `at` (1-based) of `source` sits inside a `#[cfg(test)] mod`.
/// Only an inline module counts: `#[cfg(test)] mod tests;` declares one whose
/// body lives in another file, so the lines after it are still production.
pub(crate) fn in_test_module(source: &[String], at: u64) -> bool {
    let end = usize::try_from(at).unwrap_or(usize::MAX).min(source.len());
    (0..end).any(|index| {
        source[index].trim() == "#[cfg(test)]"
            && source.get(index + 1).is_some_and(|next| {
                let next = next.trim();
                (next.starts_with("mod ") || next.starts_with("pub mod ")) && !next.ends_with(';')
            })
    })
}

/// The line range to read around `matched`: enough above it for the
/// signature and the doc comment, enough below for the body.
#[cfg(test)]
pub(crate) fn window_range(matched: u64) -> (u64, u64) {
    let matched = matched.max(1);
    (
        matched.saturating_sub(WINDOW_BEFORE).max(1),
        matched + WINDOW_AFTER,
    )
}

fn cut(text: &str, chars: usize) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(chars)
        .collect()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Number the chosen `(line number, text)` rows and take the common
/// indentation off, so a nested method reads as compact as a free function.
fn numbered(rows: &[(u64, &str)]) -> Vec<String> {
    let indent = rows
        .iter()
        .map(|(_, text)| indent_of(text))
        .min()
        .unwrap_or(0);
    rows.iter()
        .map(|(number, text)| {
            let body = text.get(indent..).unwrap_or(text).trim_end();
            format!("{number}| {}", cut(body, LINE_CHARS))
        })
        .collect()
}

const SIGNATURE_STARTS: &[&str] = &[
    "fn ",
    "pub fn ",
    "pub(crate) fn ",
    "pub(super) fn ",
    "async fn ",
    "pub async fn ",
    "pub(crate) async fn ",
    "const fn ",
    "unsafe fn ",
    "struct ",
    "pub struct ",
    "pub(crate) struct ",
    "enum ",
    "pub enum ",
    "pub(crate) enum ",
    "trait ",
    "pub trait ",
    "pub(crate) trait ",
    "impl ",
    "impl<",
    "pub const ",
    "pub(crate) const ",
    "const ",
    "pub static ",
    "pub(crate) static ",
    "static ",
    "type ",
    "pub type ",
    "def ",
    "async def ",
    "class ",
    "function ",
    "async function ",
    "export ",
    "func ",
    "interface ",
];

fn is_signature(line: &str) -> bool {
    SIGNATURE_STARTS
        .iter()
        .any(|start| line.trim_start().starts_with(start))
}

fn is_doc(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("///")
        || line.starts_with("//!")
        || line.starts_with("/**")
        || line.starts_with("* ")
        || line.starts_with("//")
}

fn is_attribute(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("#[") || line.starts_with('@')
}

/// Index of the first line of the doc comment right above `sig`, skipping
/// attributes between them.
fn doc_above(window: &[String], sig: usize) -> Option<usize> {
    let mut at = sig;
    while at > 0 && is_attribute(&window[at - 1]) {
        at -= 1;
    }
    let mut top = None;
    while at > 0 && is_doc(&window[at - 1]) {
        at -= 1;
        top = Some(at);
    }
    top
}

/// The generic excerpt, and the lookup one: the signature of the symbol the
/// matched line belongs to, the first line of its doc comment, and up to
/// [`BODY_LINES`] lines of the matched region. `window[0]` is line `start`.
#[cfg(test)]
pub(crate) fn lookup_excerpt(start: u64, window: &[String], matched: u64) -> Vec<String> {
    excerpt_around(start, window, matched, BODY_LINES).0
}

/// Body lines of a chunk excerpt, after its signature and doc line.
pub(crate) const CHUNK_BODY_LINES: usize = 6;
/// Lines a chunk is read for: a chunk the meaning search reports longer than
/// this is looked at through its first lines only.
pub(crate) const MAX_CHUNK_LINES: u64 = 60;

/// The stem a keyword is looked for by: a plural loses its `s`.
fn stem(term: &str) -> String {
    let term = term.to_lowercase();
    match term.strip_suffix('s') {
        Some(rest) if term.len() > 3 => rest.to_string(),
        _ => term,
    }
}

/// How many distinct `terms` the text `lines` contains (case-insensitive,
/// plural-insensitive).
pub(crate) fn distinct_terms(lines: &[&str], terms: &[String]) -> usize {
    let text = lines.join("\n").to_lowercase();
    terms
        .iter()
        .filter(|term| text.contains(&stem(term)))
        .count()
}

/// The excerpt of one chunk (`chunk` = inclusive lines `(first, last)`;
/// `window[0]` is line `start`, early enough to hold the signature): the
/// signature of the declaration around the densest keyword region, its first
/// doc line, and [`CHUNK_BODY_LINES`] lines from that region, not from the
/// top of the file. Without any keyword in the chunk the region is its head.
pub(crate) fn chunk_excerpt(
    start: u64,
    window: &[String],
    chunk: (u64, u64),
    terms: &[String],
) -> (Vec<String>, Option<u64>) {
    if window.is_empty() {
        return (Vec::new(), None);
    }
    let index = |line: u64| usize::try_from(line.saturating_sub(start)).unwrap_or(0);
    let first = index(chunk.0).min(window.len() - 1);
    let last = index(chunk.1).clamp(first, window.len() - 1);
    let mut best = (0, first);
    for from in first..=last {
        let region: Vec<&str> = window[from..(from + CHUNK_BODY_LINES).min(last + 1)]
            .iter()
            .map(String::as_str)
            .collect();
        let density = distinct_terms(&region, terms);
        if density > best.0 {
            best = (density, from);
        }
    }
    // Start at the first line of the region that carries a keyword.
    let at = (best.1..=last)
        .find(|&line| best.0 > 0 && distinct_terms(&[window[line].as_str()], terms) > 0)
        .unwrap_or(best.1);
    excerpt_around(start, &window[..=last], start + at as u64, CHUNK_BODY_LINES)
}

/// The line of the declaration around `line` (the nearest signature at or
/// above it in `window`, whose first line is `start`), else `line` itself.
pub(crate) fn enclosing_start(start: u64, window: &[String], line: u64) -> u64 {
    let at = usize::try_from(line.saturating_sub(start))
        .unwrap_or(0)
        .min(window.len().saturating_sub(1));
    (0..=at)
        .rev()
        .find(|&index| window.get(index).is_some_and(|text| is_signature(text)))
        .map_or(line, |index| start + index as u64)
}

fn excerpt_around(
    start: u64,
    window: &[String],
    matched: u64,
    body: usize,
) -> (Vec<String>, Option<u64>) {
    if window.is_empty() {
        return (Vec::new(), None);
    }
    let at = usize::try_from(matched.saturating_sub(start))
        .unwrap_or(0)
        .min(window.len() - 1);
    // A match on the doc comment or attribute of a declaration belongs to the
    // declaration below it; any other match to the nearest one above.
    let below = (at..window.len())
        .find(|&index| {
            !(is_doc(&window[index])
                || is_attribute(&window[index])
                || window[index].trim().is_empty())
        })
        .filter(|&index| is_signature(&window[index]));
    let sig = below.or_else(|| (0..=at).rev().find(|&index| is_signature(&window[index])));
    let mut chosen: Vec<usize> = Vec::new();
    if let Some(sig) = sig {
        chosen.extend(doc_above(window, sig));
        chosen.push(sig);
    }
    let begin = match sig {
        Some(sig) if at <= sig + body => sig + 1,
        _ => at.saturating_sub(1),
    };
    chosen.extend(
        (begin..window.len())
            .filter(|index| !window[*index].trim().is_empty() && !chosen.contains(index))
            .take(body)
            .collect::<Vec<_>>(),
    );
    let rows: Vec<(u64, &str)> = chosen
        .iter()
        .map(|&index| (start + index as u64, window[index].as_str()))
        .collect();
    (numbered(&rows), sig.map(|index| start + index as u64))
}

/// Lines a `read:` range may span.
pub(crate) const READ_CAP: usize = 60;

/// The whole declaration that starts at line `sig` of `window` (whose first
/// line is `start`): a braced body to its closing brace, an indented body to
/// its last line, a `const` to its `;`; at most [`READ_CAP`] lines.
pub(crate) fn declaration_range(start: u64, window: &[String], sig: u64) -> (u64, u64) {
    let first = usize::try_from(sig.saturating_sub(start)).unwrap_or(0);
    if first >= window.len() {
        return (sig, sig);
    }
    let indented = window[first].trim_end().ends_with(':');
    let base = indent_of(&window[first]);
    let (mut depth, mut opened, mut end) = (0_i32, false, first);
    for (offset, line) in window[first..].iter().take(READ_CAP).enumerate() {
        let index = first + offset;
        if indented && offset > 0 && !line.trim().is_empty() && indent_of(line) <= base {
            break;
        }
        end = index;
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        let text = line.trim_end();
        if (opened && depth <= 0) || (!opened && !indented && text.ends_with(';')) {
            break;
        }
    }
    (start + first as u64, start + end as u64)
}

/// The name a signature line declares: the token after its keyword.
pub(crate) fn symbol_of_signature(line: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "struct",
        "enum",
        "trait",
        "const",
        "static",
        "type",
        "def",
        "class",
        "function",
        "func",
        "interface",
        "impl",
    ];
    let mut tokens = line.split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == ':'));
    tokens.find(|token| KEYWORDS.contains(token))?;
    tokens
        .find(|token| !token.is_empty() && !matches!(*token, "mut" | "pub" | "async"))
        .map(|name| name.trim_matches(':').to_string())
        .filter(|name| !name.is_empty())
}

/// One step of a flow: a function, where it is defined, and its first line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hop {
    pub(crate) name: String,
    pub(crate) site: Option<(String, u64)>,
    pub(crate) head: Option<String>,
}

/// The hops a flow excerpt names: the first [`MAX_HOPS`], or the first
/// `MAX_HOPS - 1` and the last when the chain is longer.
pub(crate) fn pick_hops(names: &[String]) -> Vec<String> {
    if names.len() <= MAX_HOPS {
        names.to_vec()
    } else {
        let mut picked = names[..MAX_HOPS - 1].to_vec();
        picked.extend(names.last().cloned());
        picked
    }
}

/// The ordered call chain, one hop per line: `path:line name — first line`.
/// Empty when no hop has a known site, so the caller falls back.
pub(crate) fn flow_excerpt(hops: &[Hop]) -> Vec<String> {
    if hops.iter().all(|hop| hop.site.is_none()) {
        return Vec::new();
    }
    hops.iter()
        .take(MAX_HOPS)
        .map(|hop| {
            let place = hop
                .site
                .as_ref()
                .map_or_else(String::new, |(path, line)| format!("{path}:{line} "));
            let head = hop
                .head
                .as_ref()
                .map_or_else(String::new, |head| format!(" — {}", head.trim()));
            cut(&format!("{place}{}{head}", hop.name), LINE_CHARS + 30)
        })
        .collect()
}

fn word_in(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    haystack.match_indices(needle).any(|(at, found)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + found.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// The name of the test function that starts at `lines[index]`, when one does.
fn test_name(lines: &[String], index: usize) -> Option<String> {
    let line = lines[index].trim();
    let attributed = || {
        lines[index.saturating_sub(3)..index].iter().any(|before| {
            let before = before.trim();
            before.starts_with("#[test")
                || before.starts_with("#[tokio::test")
                || before.starts_with("#[rstest")
        })
    };
    let after = |prefixes: &[&str]| {
        prefixes.iter().find_map(|prefix| {
            let rest = line.strip_prefix(prefix)?;
            let end = rest.find(['(', '<'])?;
            Some(rest[..end].trim().to_string())
        })
    };
    if let Some(name) = after(&["fn ", "async fn "]).filter(|_| attributed()) {
        return Some(name);
    }
    if let Some(name) = after(&["def test_", "async def test_"]) {
        return Some(format!("test_{name}"));
    }
    if let Some(name) = after(&["func Test"]) {
        return Some(format!("Test{name}"));
    }
    ["it(", "test(", "it.each", "test.each"]
        .iter()
        .find(|prefix| line.starts_with(**prefix))
        .and_then(|_| {
            let quote = line.find(['"', '\'', '`'])?;
            let mark = line[quote..].chars().next()?;
            let rest = &line[quote + 1..];
            Some(rest[..rest.find(mark)?].to_string())
        })
}

fn is_assert(line: &str) -> bool {
    line.contains("assert") || line.contains("expect(") || line.contains(".should")
}

/// Test functions of `lines` (line 1 first) that mention `target`, each with
/// its first assertion, as `path:line name — assertion`; at most `max`.
pub(crate) fn tests_excerpt(path: &str, lines: &[String], target: &str, max: usize) -> Vec<String> {
    let starts: Vec<(usize, String)> = (0..lines.len())
        .filter_map(|index| test_name(lines, index).map(|name| (index, name)))
        .collect();
    let mut found = Vec::new();
    for (position, (index, name)) in starts.iter().enumerate() {
        if found.len() >= max {
            break;
        }
        let end = starts
            .get(position + 1)
            .map_or(lines.len(), |(next, _)| *next)
            .min(index + TEST_SPAN)
            .min(lines.len());
        let span = &lines[*index..end];
        if !span.iter().any(|line| word_in(line, target)) {
            continue;
        }
        let first = span
            .iter()
            .find(|line| is_assert(line))
            .map_or_else(String::new, |line| format!(" — {}", line.trim()));
        found.push(cut(
            &format!("{path}:{} {name}{first}", index + 1),
            LINE_CHARS + 30,
        ));
    }
    found
}

fn has_upper_snake(text: &str) -> bool {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|token| {
            token.len() >= 4
                && token.chars().any(|ch| ch.is_ascii_uppercase())
                && token
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        })
}

fn reads_env(text: &str) -> bool {
    [
        "env::var",
        "process.env",
        "getenv",
        "os.environ",
        "std::env",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn defines_setting(text: &str) -> bool {
    !reads_env(text)
        && (text.contains("const ") || text.contains("static ") || text.contains(" = "))
        && has_upper_snake(text)
}

/// The setting a config question asks about, from the matched lines the
/// search kept: the line that defines the constant (its default value is on
/// it) and the lines that read it from the environment. A line counts only
/// when its path or text carries one of the probed `terms` (any, when there
/// are none): a constant that merely matched the question's shape is noise.
pub(crate) fn config_excerpt(hits: &[RichHit], terms: &[String]) -> Vec<String> {
    let about = |hit: &RichHit, text: &str| {
        let haystack = format!("{} {text}", hit.path).to_lowercase();
        terms.is_empty()
            || terms
                .iter()
                .any(|term| haystack.contains(&term.to_lowercase()))
    };
    let site = |hit: &RichHit, text: &str| {
        cut(
            &format!("{}:{} — {}", hit.path, hit.line, text.trim()),
            LINE_CHARS + 30,
        )
    };
    let mut lines: Vec<String> = Vec::new();
    for (wanted, cap) in [(true, 2), (false, 2)] {
        let rows = hits
            .iter()
            .filter_map(|hit| hit.text.as_deref().map(|text| (hit, text)))
            .filter(|(hit, text)| about(hit, text))
            .filter(|(_, text)| {
                if wanted {
                    defines_setting(text)
                } else {
                    reads_env(text)
                }
            })
            .map(|(hit, text)| site(hit, text))
            .filter(|row| !lines.contains(row))
            .take(cap)
            .collect::<Vec<_>>();
        lines.extend(rows);
    }
    lines.truncate(MAX_CONFIG_LINES);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_brief::relevance::KeywordStat;

    fn lines(source: &[&str]) -> Vec<String> {
        source.iter().map(ToString::to_string).collect()
    }

    fn input(keywords: &[(&str, f64)]) -> RelevanceInput {
        RelevanceInput {
            graph: true,
            files_considered: 412,
            structural_files: 3,
            keywords: keywords
                .iter()
                .map(|(keyword, weight)| KeywordStat {
                    keyword: (*keyword).to_string(),
                    weight: *weight,
                    french_only: false,
                })
                .collect(),
            cofiles: Vec::new(),
        }
    }

    #[test]
    fn the_zero_switch_should_stay_on_unless_an_off_word_is_given() {
        assert!(decision_log::enabled(None));
        assert!(decision_log::enabled(Some("1")));
        for off in ["0", "false", "off"] {
            assert!(!decision_log::enabled(Some(off)), "{off}");
        }
    }

    #[test]
    fn receipt_should_count_the_probed_terms_and_list_the_common_ones_as_ignored() {
        let receipt = Receipt::new(
            &input(&[("watchdog", 2.0), ("does", 0.0), ("daemon", 1.0)]),
            None,
        )
        .unwrap();
        let text = receipt.lines(true).join("\n");
        assert!(
            text.contains(
                "searched: content+symbols+paths for watchdog, daemon (2 terms, 412 files)"
            ),
            "{text}"
        );
        assert!(text.contains("ignored (too common): does"), "{text}");
        assert!(text.contains("across the search;"), "{text}");
        assert!(!text.contains("meaning"), "{text}");
    }

    #[test]
    fn receipt_should_name_the_meaning_search_only_when_it_answered() {
        let weights = input(&[("watchdog", 2.0)]);
        let with = Receipt::new(&weights, Some(8))
            .unwrap()
            .lines(true)
            .join("\n");
        assert!(
            with.contains("· meaning search returned 8 chunks"),
            "{with}"
        );
        assert!(with.contains("across both searches;"), "{with}");
        let without = Receipt::new(&weights, None).unwrap().lines(true).join("\n");
        assert!(!without.contains("meaning"), "{without}");
    }

    #[test]
    fn receipt_should_not_exist_when_no_term_was_weighed() {
        assert_eq!(Receipt::new(&input(&[("the", 0.0)]), Some(3)), None);
        assert_eq!(Receipt::new(&input(&[]), None), None);
    }

    #[test]
    fn receipt_should_bound_the_terms_it_lists() {
        let many: Vec<(String, f64)> = (0..9).map(|n| (format!("term{n}"), 1.0)).collect();
        let pairs: Vec<(&str, f64)> = many.iter().map(|(k, w)| (k.as_str(), *w)).collect();
        let text = Receipt::new(&input(&pairs), None)
            .unwrap()
            .lines(true)
            .join("\n");
        assert!(text.contains("term5 (+3) (9 terms"), "{text}");
        assert!(!text.contains("term6"), "{text}");
    }

    #[test]
    fn window_range_should_start_above_the_match_and_never_at_zero() {
        assert_eq!(window_range(100), (88, 109));
        assert_eq!(window_range(3), (1, 12));
        assert_eq!(window_range(0), (1, 10));
    }

    const SOURCE: &[&str] = &[
        "use std::fmt;",
        "",
        "/// Keep the daemon alive.",
        "/// Second doc line, not shown.",
        "#[must_use]",
        "pub fn watchdog(period: u64) -> bool {",
        "    let mut tries = 0;",
        "    loop {",
        "        tries += 1;",
        "        if tries > period {",
        "            return false;",
        "        }",
        "    }",
        "}",
    ];

    #[test]
    fn lookup_excerpt_should_give_the_signature_the_first_doc_line_and_the_body() {
        let window = lines(SOURCE);
        let excerpt = lookup_excerpt(10, &window, 17);
        assert_eq!(excerpt[0], "12| /// Keep the daemon alive.");
        assert_eq!(excerpt[1], "15| pub fn watchdog(period: u64) -> bool {");
        assert_eq!(excerpt[2], "16|     let mut tries = 0;");
        assert!(!excerpt.iter().any(|line| line.contains("Second doc")));
        assert!(excerpt.len() <= 2 + BODY_LINES, "{excerpt:?}");
    }

    #[test]
    fn lookup_excerpt_should_bound_the_body_and_each_line() {
        let mut source: Vec<String> = vec!["pub fn big() {".into()];
        source.extend((0..30).map(|n| format!("    step_{n}();{}", "x".repeat(200))));
        let excerpt = lookup_excerpt(1, &source, 1);
        assert_eq!(excerpt.len(), 1 + BODY_LINES);
        assert!(
            excerpt
                .iter()
                .all(|line| line.chars().count() <= LINE_CHARS + 8)
        );
    }

    #[test]
    fn lookup_excerpt_should_dedent_a_nested_method() {
        let window = lines(&["impl A {", "    fn run(&self) {", "        go();", "    }"]);
        let excerpt = lookup_excerpt(1, &window, 3);
        assert_eq!(excerpt, ["2| fn run(&self) {", "3|     go();", "4| }"]);
    }

    #[test]
    fn lookup_excerpt_should_show_the_region_around_a_match_without_a_signature() {
        let window = lines(&["    a();", "    b();", "    c();", "    d();"]);
        let excerpt = lookup_excerpt(50, &window, 52);
        assert_eq!(excerpt[0], "51| b();");
        assert!(lookup_excerpt(1, &[], 1).is_empty());
    }

    #[test]
    fn flow_excerpt_should_print_each_hop_with_its_site_and_first_line() {
        let hops = vec![
            Hop {
                name: "start".into(),
                site: Some(("src/a.rs".into(), 10)),
                head: Some("pub fn start() {".into()),
            },
            Hop {
                name: "open".into(),
                site: None,
                head: None,
            },
        ];
        assert_eq!(
            flow_excerpt(&hops),
            ["src/a.rs:10 start — pub fn start() {", "open"]
        );
        let unknown = vec![Hop {
            name: "x".into(),
            site: None,
            head: None,
        }];
        assert!(flow_excerpt(&unknown).is_empty());
    }

    #[test]
    fn pick_hops_should_keep_five_and_the_destination_of_a_longer_chain() {
        let names: Vec<String> = (0..8).map(|n| format!("h{n}")).collect();
        assert_eq!(pick_hops(&names[..5]), names[..5]);
        assert_eq!(pick_hops(&names), ["h0", "h1", "h2", "h3", "h7"]);
    }

    #[test]
    fn tests_excerpt_should_name_each_test_that_mentions_the_target_with_its_first_assert() {
        let file = lines(&[
            "#[test]",
            "fn other() {",
            "    assert_eq!(1, 1);",
            "}",
            "#[test]",
            "fn watchdog_should_stop() {",
            "    let ok = watchdog(3);",
            "    assert!(!ok);",
            "}",
        ]);
        assert_eq!(
            tests_excerpt("t.rs", &file, "watchdog", 4),
            ["t.rs:6 watchdog_should_stop — assert!(!ok);"]
        );
        assert!(tests_excerpt("t.rs", &file, "missing", 4).is_empty());
        assert!(tests_excerpt("t.rs", &file, "watchdog", 0).is_empty());
    }

    #[test]
    fn tests_excerpt_should_read_javascript_and_python_tests_and_cap_the_count() {
        let js = lines(&[
            "it('stops the watchdog', () => {",
            "  expect(watchdog(1)).toBe(false)",
            "})",
        ]);
        assert_eq!(
            tests_excerpt("a.test.ts", &js, "watchdog", 4),
            ["a.test.ts:1 stops the watchdog — expect(watchdog(1)).toBe(false)"]
        );
        let py = lines(&[
            "def test_one():",
            "    assert watchdog(1)",
            "def test_two():",
            "    assert watchdog(2)",
        ]);
        let found = tests_excerpt("t.py", &py, "watchdog", 1);
        assert_eq!(found, ["t.py:1 test_one — assert watchdog(1)"]);
    }

    #[test]
    fn tests_excerpt_should_not_match_a_target_inside_a_longer_word() {
        let file = lines(&["#[test]", "fn t() {", "    assert!(rewatchdog());", "}"]);
        assert!(tests_excerpt("t.rs", &file, "watchdog", 4).is_empty());
    }

    fn rich(path: &str, line: u64, text: &str) -> RichHit {
        RichHit {
            path: path.into(),
            line,
            text: Some(text.into()),
        }
    }

    #[test]
    fn config_excerpt_should_give_the_definition_with_its_default_and_where_it_is_read() {
        let hits = vec![
            rich("a.rs", 9, "let v = std::env::var(RECEIPT_ENV).ok();"),
            rich(
                "a.rs",
                3,
                "pub(crate) const RECEIPT_ENV: &str = \"PIXEL_BRIEF_RECEIPT\";",
            ),
            rich("b.rs", 1, "fn unrelated() {}"),
        ];
        assert_eq!(
            config_excerpt(&hits, &[]),
            [
                "a.rs:3 — pub(crate) const RECEIPT_ENV: &str = \"PIXEL_BRIEF_RECEIPT\";",
                "a.rs:9 — let v = std::env::var(RECEIPT_ENV).ok();",
            ]
        );
        assert!(config_excerpt(&hits[2..], &[]).is_empty());
        assert!(config_excerpt(&[], &[]).is_empty());
        // Only lines that carry a probed term count.
        let terms = vec!["receipt".to_string()];
        assert_eq!(config_excerpt(&hits, &terms).len(), 2);
        assert!(config_excerpt(&hits, &["unrelated".to_string()]).is_empty());
    }

    #[test]
    fn path_and_symbol_predicates_should_tell_tests_and_docs_from_code() {
        for docs in [
            "README.md",
            "docs/a.rs",
            "CHANGELOG.md",
            "changelog.d/1.changed.md",
        ] {
            assert!(is_docs_path(docs), "{docs}");
        }
        for code in ["src/a.rs", "src/docs.rs", "web/a.ts"] {
            assert!(!is_docs_path(code), "{code}");
        }
        assert!(is_test_symbol("render_should_wrap"));
        assert!(is_test_symbol("test_render"));
        assert!(!is_test_symbol("render"));
        assert!(!is_test_symbol("latest_release"));
        let terms = vec!["Tests".to_string(), "x".to_string()];
        assert!(asks_tests(&terms) && !asks_docs(&terms));
        assert!(asks_docs(&["readme".to_string()]));
    }

    #[test]
    fn in_test_module_should_see_a_cfg_test_mod_above_the_line_only() {
        let source = lines(&[
            "fn real() {}",
            "#[cfg(test)]",
            "fn only_in_tests() {}",
            "#[cfg(test)]",
            "mod tests {",
            "    fn a() {}",
            "}",
        ]);
        assert!(
            !in_test_module(&source, 3),
            "a cfg(test) fn is not a module"
        );
        assert!(!in_test_module(&source, 1));
        assert!(in_test_module(&source, 6));
    }

    #[test]
    fn in_test_module_should_not_take_a_declared_test_mod_for_the_lines_after_it() {
        // `mod tests;` near the top of a lib.rs keeps its body in tests.rs:
        // the production code below it is not test code.
        let source = lines(&[
            "#[cfg(test)]",
            "mod tests;",
            "",
            "pub fn real() {}",
            "#[cfg(test)]",
            "pub mod helpers {",
            "    fn a() {}",
            "}",
        ]);
        assert!(!in_test_module(&source, 4));
        assert!(in_test_module(&source, 7));
    }

    #[test]
    fn lookup_excerpt_should_follow_a_match_on_a_doc_comment_down_to_its_declaration() {
        let window = lines(&[
            "fn previous() {}",
            "",
            "/// Wait for the daemon.",
            "#[must_use]",
            "pub fn wait() -> bool {",
            "    true",
            "}",
        ]);
        let excerpt = lookup_excerpt(10, &window, 12);
        assert_eq!(excerpt[0], "12| /// Wait for the daemon.");
        assert_eq!(excerpt[1], "14| pub fn wait() -> bool {");
        assert!(
            !excerpt.iter().any(|line| line.contains("previous")),
            "{excerpt:?}"
        );
    }

    #[test]
    fn demoted_should_hold_below_at_and_above_the_cue() {
        // Below: a plain code question demotes tests, docs, eval and changelogs.
        for path in [
            "tests/a.rs",
            "crates/x/tests/b.rs",
            "src/a_test.rs",
            "test_a.py",
            "web/a.spec.ts",
            "eval/run.sh",
            "docs/a.rs",
            "README.md",
            "CHANGELOG",
            "changelog.d/1.md",
        ] {
            assert!(demoted(path, false, false), "{path}");
        }
        for path in ["src/a.rs", "crates/x/src/latest.rs", "scripts/run.py"] {
            assert!(!demoted(path, false, false), "{path}");
        }
        // At: asking about tests lifts tests only; about docs lifts docs only.
        assert!(!demoted("tests/a.rs", true, false));
        assert!(demoted("docs/a.md", true, false));
        assert!(!demoted("docs/a.md", false, true));
        assert!(demoted("tests/a.rs", false, true));
        // Above: both cues lift both.
        assert!(!demoted("tests/a.rs", true, true) && !demoted("eval/a.py", true, true));
        assert_eq!(cues("which tests cover the parser"), (true, false));
        assert_eq!(cues("what do the benchmarks say"), (false, true));
        assert_eq!(cues("how does the parser work"), (false, false));
        assert_eq!(cues("CHANGELOG entry for tests"), (true, true));
    }

    fn terms(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn distinct_terms_should_count_each_keyword_once_ignoring_case_and_plurals() {
        let lines = ["Waits for a Key", "waits again"];
        assert_eq!(
            distinct_terms(&lines, &terms(&["waits", "key", "brief"])),
            2
        );
        assert_eq!(distinct_terms(&lines, &terms(&["keys"])), 1);
        assert_eq!(distinct_terms(&[], &terms(&["a"])), 0);
    }

    #[test]
    fn chunk_excerpt_should_centre_on_the_densest_keyword_region_with_its_signature() {
        let mut source: Vec<String> =
            vec!["/// Decide the gate.".into(), "pub fn decide() {".into()];
        source.extend((0..20).map(|n| format!("    let step_{n} = {n};")));
        source.push("    let window = brief_window_millis(750);".into());
        source.push("    wait_for_brief_window(window);".into());
        source.extend((0..5).map(|n| format!("    let tail_{n} = {n};")));
        source.push("}".into());
        let found = chunk_excerpt(10, &source, (10, 10 + 28), &terms(&["brief", "window"])).0;
        assert_eq!(found[0], "10| /// Decide the gate.");
        assert_eq!(found[1], "11| pub fn decide() {");
        let text = found.join("\n");
        assert!(text.contains("brief_window_millis"), "{text}");
        assert!(text.contains("wait_for_brief_window"), "{text}");
        assert!(!text.contains("step_0"), "{text}");
        assert!(found.len() <= 2 + CHUNK_BODY_LINES, "{found:?}");
        // No keyword inside: the head of the chunk, as before.
        let head = chunk_excerpt(10, &source, (10, 38), &terms(&["absent"])).0;
        assert_eq!(head[2], "12|     let step_0 = 0;");
    }

    #[test]
    fn chunk_excerpt_should_stay_inside_its_chunk() {
        let source = lines(&[
            "fn a() {",
            "    brief();",
            "}",
            "fn b() {",
            "    brief();",
            "}",
        ]);
        let found = chunk_excerpt(1, &source, (1, 3), &terms(&["brief"]))
            .0
            .join("\n");
        assert!(
            found.contains("fn a()") && !found.contains("fn b()"),
            "{found}"
        );
    }

    #[test]
    fn declaration_range_should_cover_a_braced_body_a_const_and_an_indented_body() {
        let braced = lines(&[
            "fn a() {",
            "    if x {",
            "        y();",
            "    }",
            "}",
            "fn b() {}",
        ]);
        assert_eq!(declaration_range(10, &braced, 10), (10, 14));
        let konst = lines(&[
            "pub const LIMITS: [u32; 2] = [",
            "    1,",
            "    2,",
            "];",
            "fn next() {}",
        ]);
        assert_eq!(declaration_range(1, &konst, 1), (1, 4));
        let one = lines(&["const A: u32 = 1;", "const B: u32 = 2;"]);
        assert_eq!(declaration_range(1, &one, 1), (1, 1));
        let python = lines(&[
            "def f():",
            "    a = 1",
            "",
            "    return a",
            "def g():",
            "    pass",
        ]);
        assert_eq!(declaration_range(1, &python, 1), (1, 4));
        // The cap: a body that never closes stops at READ_CAP lines.
        let open: Vec<String> = std::iter::once("fn long() {".to_string())
            .chain((0..100).map(|n| format!("    step({n});")))
            .collect();
        assert_eq!(declaration_range(1, &open, 1), (1, READ_CAP as u64));
        assert_eq!(declaration_range(1, &[], 5), (5, 5));
    }

    #[test]
    fn symbol_of_signature_should_name_what_the_line_declares() {
        for (line, name) in [
            ("pub(crate) async fn run_it(x: u8) {", "run_it"),
            ("pub const SWEEP_INTERVAL: Duration = x;", "SWEEP_INTERVAL"),
            ("    def watch(self):", "watch"),
            ("export function load() {", "load"),
            ("impl Service {", "Service"),
            ("pub struct Brief {", "Brief"),
        ] {
            assert_eq!(symbol_of_signature(line).as_deref(), Some(name), "{line}");
        }
        assert_eq!(symbol_of_signature("let x = 1;"), None);
    }

    #[test]
    fn a_rule_should_need_the_meaning_rank_the_lexical_rank_and_the_keywords() {
        let top = |rank: Option<usize>, density: usize, meaning: bool| Excerpt {
            from_meaning: meaning,
            meaning_rank: rank,
            density,
            ..Excerpt::default()
        };
        let rule = (0, 0, 2);
        assert!(rule_holds(&top(Some(0), 2, true), Some(0), rule));
        assert!(!rule_holds(&top(Some(1), 2, true), Some(0), rule));
        assert!(!rule_holds(&top(Some(0), 1, true), Some(0), rule));
        assert!(!rule_holds(&top(Some(0), 2, true), Some(1), rule));
        assert!(!rule_holds(&top(Some(0), 2, true), None, rule));
        assert!(!rule_holds(&top(None, 2, false), Some(0), rule));
        assert!(rule_holds(&top(Some(1), 1, true), Some(1), (1, 1, 1)));
    }
}
