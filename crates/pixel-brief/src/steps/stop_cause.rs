// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `cand-j-stop-cause` (`specs/brief.md`, "Recipes"): a "why does X stop, skip
//! or ignore …" question is answered by the code that implements the effect
//! and the tests that pin it down.
//!
//! The question's rare words (idf over the tracked source: a word in more than
//! half the files names nothing) each form a group, with a few verb synonyms
//! (`stop` = `quit` = `halt`, …). Every 30-line window of every source file
//! scores the groups it holds, and only a window that holds the two heaviest
//! groups counts. A window inside a multi-line string literal (flag
//! documentation), one with little code, and one that is mostly test code are
//! left out. The best window of each of the five best files is recentred on
//! the group words, cut to the lines that hold them and snapped to whole
//! syntax; a file named after a group weighs 1.15, one the brief already
//! points at 1.3. The first caller of a small function whose name carries a
//! group is shown when the graph answers. When the question also asks about
//! tests, proof or coverage, the best test units follow, with the siblings of
//! their family.
//!
//! Every block of the pack that is not a definition goes
//! ([`super::Drop::NotDefinitions`]): the step shows where the words meet.
//! Read-only and deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::chain::SymbolHit;
use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Drop, Outcome, Step};

/// Lines of a scored window.
const WINDOW: usize = 30;
/// Groups the question must have before the step answers.
const MIN_GROUPS: usize = 3;
/// Source files the step reads, and the fewest that make a vocabulary.
const MAX_FILES: usize = 4_000;
const MIN_FILES: usize = 4;
/// Groups, heaviest first, that a window must hold.
const REQUIRED: usize = 2;
/// Source windows shown, and their bytes (a line costs its length plus 8).
const MAX_SRC: usize = 5;
const SRC_BYTES: usize = 5_300;
/// A window with more prose than this, or less code, is left out.
const MAX_PROSE: f64 = 0.1;
const MIN_DENSITY: f64 = 0.35;
const STEM_BOOST: f64 = 1.15;
const BRIEF_BOOST: f64 = 1.3;
/// Test units: groups a unit must hold, files, siblings of a family, lines of
/// a unit, bytes.
const MIN_UNIT_GROUPS: usize = 2;
const MAX_TEST: usize = 3;
const FAMILY: usize = 3;
const TEST_CAP: usize = 12;
const TEST_BYTES: usize = 3_000;
/// A unit in a file named after a group weighs one and a half.
const DEDICATED_BOOST: f64 = 1.5;
/// The caller shown is at most this many lines before and after its call.
const CALLER_BEFORE: usize = 14;
const CALLER_AFTER: usize = 12;
/// A function this short, in lines past its first, is "small".
const SMALL_DECL: u64 = 12;
const LINE_CHARS: usize = 200;
/// How far a test unit's closing brace is looked for.
const UNIT_SCAN: usize = 400;
/// How far a window is snapped over a comment run or an open brace.
const SNAP_REACH: usize = 10;
/// A trimmed window keeps this many lines around the group words, and at least
/// `TRIM_FLOOR` lines.
const TRIM_CONTEXT: usize = 2;
const TRIM_FLOOR: usize = 8;

/// Words that name no subject: function words and the verbs of asking.
const STOP: &str = "what where which that this with from does how the and for are was were why when who whom whose into onto than then them they their there these those have has had been being will would should could can may might must about after before between through during over under again further once here some such only same also just very more most other each both any all not our your its his her him she you out off use used using make made get got let trace explain tell show describe find list name names code function functions method methods file files line lines value values data";
/// Prefixes of the words that frame a question instead of naming its subject.
const FRAME: &[&str] = &[
    "test", "prov", "proo", "cove", "defi", "call", "beha", "work", "happ", "deci", "expl", "show",
    "mean",
];
/// Verbs that stand for each other in code.
const SYNONYMS: &[&[&str]] = &[
    &[
        "stop",
        "quit",
        "halt",
        "abort",
        "terminate",
        "bail",
        "cancel",
    ],
    &[
        "skip", "suppress", "ignore", "hide", "filter", "refuse", "reject", "discard", "omit",
        "exclude",
    ],
    &["fail", "error", "crash", "panic"],
    &["print", "display", "output", "emit"],
];

static WHEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\bwhy\b.*\b(stops?|stopp\w+|skips?|skipp\w+|ignor\w+|hid\w+|hides?|suppress\w+|refus\w+|never|silently|missing|drops?|dropp\w+|fails?|not)\b",
    )
    .expect("the stop-cause question pattern is a literal")
});
static ASKS_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(tests?|specs?|prov\w*|proof|cover\w*)\b")
        .expect("the tests-ask pattern is a literal")
});
static SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.(rs|py|js|jsx|ts|tsx|go|java|kt|c|h|cc|cpp|hpp|rb|php|swift|cs|scala|sh|lua)$")
        .expect("the source pattern is a literal")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(tests?|__tests__|spec|testdata)/|[._-](test|spec)s?\.\w+$|(^|/)test_\w+\.\w+$|_tests?\.\w+$",
    )
    .expect("the test-path pattern is a literal")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{4,}").expect("the word pattern is a literal"));
static DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+|extern\s+"[^"]*"\s+|static\s+|export\s+|default\s+)*(?:fn|def|function|func)\s+([A-Za-z_]\w*)|^\s*(?:[A-Za-z_]\w*!)\(\s*([A-Za-z_]\w*)\s*,\s*(?:\||move\s*\||\{|fn\b)|^\s*(?:test|it|describe)\(\s*['"`]([^'"`]+)"#,
    )
    .expect("the declaration pattern is a literal")
});
static COMMENT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(//|#\[|#\s|/\*|\*|@)").expect("the comment pattern is a literal")
});
static TEST_MARK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\s*#\[(?:tokio::)?test|\s*@(?:Test|pytest))")
        .expect("the test-mark pattern is a literal")
});
static STRINGS_AND_COMMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])'|//.*$"#).expect("the mask pattern is a literal")
});
static LITERAL_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"r#*"|"""|'''|`"#).expect("the literal-open pattern is a literal")
});
static DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("the digits pattern is a literal"));

/// A source file: its lines, and the same lines lowercased.
struct Source {
    lines: Vec<String>,
    lower: Vec<String>,
}

impl Source {
    fn new(lines: Vec<String>) -> Self {
        let lower = lines.iter().map(|line| line.to_lowercase()).collect();
        Self { lines, lower }
    }
}

/// A rare word of the question with its synonyms, and how much it says.
#[derive(Debug, PartialEq)]
struct Group {
    weight: f64,
    members: BTreeSet<String>,
}

impl Group {
    /// How many times the members occur in a lowercased line.
    fn count_in(&self, lower: &str) -> usize {
        self.members
            .iter()
            .map(|member| lower.matches(member.as_str()).count())
            .sum()
    }

    fn in_line(&self, lower: &str) -> bool {
        self.members.iter().any(|member| lower.contains(member))
    }
}

/// A scored window of a file: 1-based inclusive lines.
#[derive(Debug, PartialEq)]
struct Window {
    score: f64,
    first: usize,
    last: usize,
}

/// A test unit of a file: 1-based inclusive lines, its comment lines included.
#[derive(Debug, PartialEq, Eq)]
struct Unit {
    first: usize,
    last: usize,
    name: String,
}

/// A range of a file the step shows.
type Span = (String, usize, usize);

/// `word` lowercased with `ing`, `ed`, `es` or `s` cut when four letters stay.
fn root(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in ["ing", "ed", "es", "s"] {
        if let Some(cut) = word.strip_suffix(suffix)
            && cut.chars().count() >= 4
        {
            return cut.to_string();
        }
    }
    word
}

/// One group per rare word of the prompt, in the order the prompt gave them.
fn groups_of(prompt: &str, sources: &BTreeMap<String, Source>) -> Vec<Group> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for found in WORD.find_iter(prompt) {
        let word = found.as_str().to_lowercase();
        if !seen.insert(word.clone())
            || STOP.split_whitespace().any(|stop| stop == word)
            || FRAME.iter().any(|frame| word.starts_with(frame))
        {
            continue;
        }
        let stem = root(&word);
        let df = sources
            .values()
            .filter(|source| source.lower.iter().any(|line| line.contains(&stem)))
            .count();
        if df == 0 || df * 2 > sources.len() {
            continue;
        }
        let mut members = BTreeSet::from([stem.clone()]);
        for synonyms in SYNONYMS {
            if synonyms.contains(&stem.as_str()) || synonyms.iter().any(|x| root(x) == stem) {
                members.extend(synonyms.iter().map(|x| root(x)));
            }
        }
        let weight = (sources.len() as f64 / df as f64).ln().powi(2);
        out.push(Group { weight, members });
    }
    out
}

/// Prefix sums with a leading zero: the sum of `values[a..b]` is `p[b] - p[a]`.
fn prefix(values: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut sums = vec![0];
    for value in values {
        sums.push(sums[sums.len() - 1] + value);
    }
    sums
}

/// Which lines sit inside a multi-line string literal (Rust raw strings, triple
/// quotes, template literals), the closing line included.
fn prose_lines(lines: &[String]) -> Vec<bool> {
    let mut inside: Option<String> = None;
    let mut flags = Vec::with_capacity(lines.len());
    for line in lines {
        if let Some(close) = &inside {
            if line.contains(close.as_str()) {
                inside = None;
            }
            flags.push(true);
            continue;
        }
        let open = LITERAL_OPEN.find(line);
        let close = open.map(|open| {
            let token = open.as_str();
            if token.starts_with('r') {
                format!("\"{}", "#".repeat(token.len() - 2))
            } else {
                token.to_string()
            }
        });
        match (open, close) {
            (Some(open), Some(close)) if !line[open.end()..].contains(&close) => {
                inside = Some(close);
                flags.push(true);
            }
            _ => flags.push(false),
        }
    }
    flags
}

/// Whether a line reads like code: not a comment or a bare string, and it ends
/// a statement or calls something.
fn code_like(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty()
        && !["//", "#", "*", "/*", "\"", "'"]
            .iter()
            .any(|lead| line.starts_with(lead))
        && (line.ends_with([';', '{', '}', ')', ',']) || line.contains('('))
}

/// Every 30-line window (the whole file when shorter) that holds the two
/// heaviest groups, is not prose and has enough code, scored by the groups it
/// holds and how often.
fn windows(source: &Source, groups: &[Group]) -> Vec<Window> {
    let n = source.lines.len();
    let mut required: Vec<usize> = (0..groups.len()).collect();
    required.sort_by(|x, y| groups[*y].weight.total_cmp(&groups[*x].weight));
    required.truncate(REQUIRED);
    let held: Vec<Vec<usize>> = groups
        .iter()
        .map(|group| prefix(source.lower.iter().map(|line| group.count_in(line))))
        .collect();
    let prose = prefix(prose_lines(&source.lines).into_iter().map(usize::from));
    let code = prefix(source.lines.iter().map(|line| usize::from(code_like(line))));
    let mut out = Vec::new();
    for a in 0..=n.saturating_sub(WINDOW) {
        let b = (a + WINDOW).min(n);
        let counts: Vec<usize> = held.iter().map(|sums| sums[b] - sums[a]).collect();
        if required.iter().any(|group| counts[*group] == 0) {
            continue;
        }
        let len = (b - a) as f64;
        if (prose[b] - prose[a]) as f64 / len > MAX_PROSE
            || ((code[b] - code[a]) as f64 / len) < MIN_DENSITY
        {
            continue;
        }
        let score = groups
            .iter()
            .zip(&counts)
            .filter(|(_, count)| **count > 0)
            .map(|(group, count)| group.weight * (1.0 + (*count as f64).ln()))
            .sum();
        out.push(Window {
            score,
            first: a + 1,
            last: b,
        });
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|ch| ch.is_whitespace()).count()
}

/// The line, from `i`, where the braces of the declaration at `i` close.
fn closing_brace(lines: &[String], i: usize) -> Option<usize> {
    let mut depth = 0_i64;
    let mut seen = false;
    for (j, line) in lines.iter().enumerate().take(i + UNIT_SCAN).skip(i) {
        for ch in STRINGS_AND_COMMENTS.replace_all(line, "").chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if seen && depth <= 0 {
            return Some(j);
        }
    }
    None
}

/// The last line of the declaration at `i` that is indented past it.
fn indented_end(lines: &[String], i: usize) -> usize {
    let indent = indent_of(&lines[i]);
    let run = lines[i + 1..]
        .iter()
        .take_while(|line| line.trim().is_empty() || indent_of(line) > indent)
        .count();
    (i..=i + run)
        .rev()
        .find(|k| *k == i || !lines[*k].trim().is_empty())
        .unwrap_or(i)
}

/// The test units of a file. In a test file every function is one; elsewhere a
/// function is when it is marked, named `test_*`, a `name!(…, |…|` case or a
/// `test('…'`/`it('…'` title.
fn units(path: &str, lines: &[String]) -> Vec<Unit> {
    let in_test_file = TEST_PATH.is_match(path);
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = DECL.captures(line) else {
            continue;
        };
        let Some(name) = (1..=3).find_map(|group| caps.get(group)) else {
            continue;
        };
        let first = (0..i)
            .rev()
            .take_while(|k| COMMENT_LINE.is_match(&lines[*k]))
            .last()
            .unwrap_or(i);
        let marked = (first..=i).any(|k| TEST_MARK.is_match(&lines[k]));
        if !(in_test_file
            || marked
            || name.as_str().starts_with("test_")
            || caps.get(2).is_some()
            || caps.get(3).is_some())
        {
            continue;
        }
        let end = closing_brace(lines, i).unwrap_or_else(|| indented_end(lines, i));
        out.push(Unit {
            first: first + 1,
            last: end + 1,
            name: name.as_str().to_string(),
        });
    }
    out
}

/// How many groups a unit's lines hold, and what they weigh.
fn unit_score(source: &Source, unit: &Unit, groups: &[Group]) -> (usize, f64) {
    let lower = &source.lower[unit.first - 1..unit.last];
    let mut held = 0;
    let mut total = 0.0;
    for group in groups {
        let count: usize = lower.iter().map(|line| group.count_in(line)).sum();
        if count > 0 {
            held += 1;
            total += group.weight * (1.0 + (count as f64).ln());
        }
    }
    (held, total)
}

/// `[a, b]` as a window of the same size centred on the weighted centroid of
/// the lines that hold a group word (ties round to even, as the script's did).
fn recentre(source: &Source, a: usize, b: usize, groups: &[Group]) -> (usize, usize) {
    let weights: Vec<f64> = (a..=b)
        .map(|n| {
            groups
                .iter()
                .filter(|group| group.in_line(&source.lower[n - 1]))
                .map(|group| group.weight)
                .sum()
        })
        .collect();
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return (a, b);
    }
    let centre: f64 = (a..=b)
        .zip(&weights)
        .map(|(n, weight)| n as f64 * weight)
        .sum::<f64>()
        / total;
    let size = b - a + 1;
    let wanted = (centre - size as f64 / 2.0).round_ties_even() as i64;
    let last_start = source.lines.len() as i64 - size as i64 + 1;
    let first = wanted.min(last_start).max(1) as usize;
    (first, first + size - 1)
}

/// `[a, b]` cut to the lines that hold a group word with two lines around
/// them, grown back to eight lines when it got shorter.
fn trim(source: &Source, a: usize, b: usize, groups: &[Group]) -> (usize, usize) {
    let hits: Vec<usize> = (a..=b)
        .filter(|n| {
            groups
                .iter()
                .any(|group| group.in_line(&source.lower[*n - 1]))
        })
        .collect();
    let (Some(first), Some(last)) = (hits.first(), hits.last()) else {
        return (a, b);
    };
    let mut lo = a.max(first.saturating_sub(TRIM_CONTEXT));
    let mut hi = b.min(last + TRIM_CONTEXT);
    for _ in 0..TRIM_FLOOR {
        if hi - lo + 1 >= TRIM_FLOOR || (lo == a && hi == b) {
            break;
        }
        lo = a.max(lo.saturating_sub(1));
        hi = b.min(hi + 1);
    }
    (lo, hi)
}

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    ["//", "#", "*", "/*"]
        .iter()
        .any(|lead| line.starts_with(lead))
}

fn brace_balance(line: &str) -> i64 {
    line.matches('{').count() as i64 - line.matches('}').count() as i64
}

/// `[a, b]` widened to whole syntax: a window that starts inside a comment run
/// starts at the run, and one that ends inside an open brace ends where it
/// closes (within ten lines).
fn snap(source: &Source, a: usize, b: usize) -> (usize, usize) {
    let lines = &source.lines;
    let mut start = a;
    if is_comment(&lines[a - 1]) {
        for _ in 0..SNAP_REACH {
            if start > 1 && is_comment(&lines[start - 2]) {
                start -= 1;
            } else {
                break;
            }
        }
    }
    let mut depth: i64 = (start..=b).map(|n| brace_balance(&lines[n - 1])).sum();
    if depth <= 0 {
        return (start, b);
    }
    for n in b + 1..=lines.len().min(b + SNAP_REACH) {
        depth += brace_balance(&lines[n - 1]);
        if depth <= 0 {
            return (start, n);
        }
    }
    (start, b)
}

/// The bytes a range of lines costs: each line, plus 8 for its number.
fn cost(source: &Source, a: usize, b: usize) -> usize {
    source.lines[a - 1..b]
        .iter()
        .map(|line| line.chars().count() + 8)
        .sum()
}

/// Whether the file's name carries a group word.
fn named_after(path: &str, groups: &[Group]) -> bool {
    let stem = Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    groups.iter().any(|group| group.in_line(&stem))
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::CandJStopCause,
        &[
            QuestionKind::Overview,
            QuestionKind::Review,
            QuestionKind::History,
        ],
    ) || !WHEN.is_match(ctx.typed)
    {
        return;
    }
    let _timer = ctx.timer(Step::CandJStopCause);
    let Some(sources) = read_sources(ctx) else {
        return;
    };
    let groups = groups_of(ctx.typed, &sources);
    if groups.len() < MIN_GROUPS {
        return;
    }
    let mut chosen = source_windows(ctx, &sources, &groups);
    chosen.extend(small_callers(ctx, &sources, &groups, &chosen));
    if ASKS_TESTS.is_match(ctx.typed) {
        chosen.extend(test_units(&sources, &groups));
    }
    if chosen.is_empty() {
        return;
    }
    chosen.sort();
    chosen.dedup();
    out.drops.push(Drop::NotDefinitions);
    out.rescuing = true;
    out.lead.extend(chosen.iter().map(|(path, a, b)| {
        let source = &sources[path];
        let kind = if TEST_PATH.is_match(path) {
            "test"
        } else {
            "code"
        };
        let body = (*a..=*b)
            .filter(|n| !source.lines[n - 1].trim().is_empty())
            .map(|n| {
                let line: String = source.lines[n - 1].chars().take(LINE_CHARS).collect();
                format!("{n}| {line}")
            })
            .collect();
        Block::new(
            format!("{path}:{a}-{b} — where the question's words meet ({kind})"),
            body,
        )
    }));
}

/// The non-empty source files of the repository, by path; `None` when the
/// files cannot be listed or the step ran out of time, and when there are too
/// few for their vocabulary to say what is rare.
fn read_sources(ctx: &Ctx) -> Option<BTreeMap<String, Source>> {
    let mut sources = BTreeMap::new();
    for path in ctx
        .files()?
        .iter()
        .filter(|path| SOURCE.is_match(path))
        .take(MAX_FILES)
    {
        if ctx.timed_out() {
            return None;
        }
        let Some(text) = ctx.read(path) else {
            continue;
        };
        let lines: Vec<String> = text.lines().map(ToString::to_string).collect();
        if !lines.is_empty() {
            sources.insert(path.clone(), Source::new(lines));
        }
    }
    (sources.len() >= MIN_FILES).then_some(sources)
}

/// The best window of each of the best source files, within the byte budget.
fn source_windows(ctx: &Ctx, sources: &BTreeMap<String, Source>, groups: &[Group]) -> Vec<Span> {
    let mut rows: Vec<(f64, &str, usize, usize)> = Vec::new();
    for (path, source) in sources {
        if TEST_PATH.is_match(path) {
            continue;
        }
        let found = windows(source, groups);
        if found.is_empty() {
            continue;
        }
        let in_test = test_lines(path, &source.lines);
        let stem = if named_after(path, groups) {
            STEM_BOOST
        } else {
            1.0
        };
        let brief = if ctx.near.contains(path) {
            BRIEF_BOOST
        } else {
            1.0
        };
        for window in found {
            let tested = in_test[window.last] - in_test[window.first - 1];
            if tested * 2 < window.last - window.first + 1 {
                rows.push((window.score * stem * brief, path, window.first, window.last));
            }
        }
    }
    rows.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| x.1.cmp(y.1))
            .then(x.2.cmp(&y.2))
    });
    let mut chosen: Vec<Span> = Vec::new();
    let mut used = 0;
    for (_, path, first, last) in rows {
        if chosen.len() >= MAX_SRC {
            break;
        }
        if chosen.iter().any(|(held, _, _)| held == path) {
            continue;
        }
        let source = &sources[path];
        let (a, b) = recentre(source, first, last, groups);
        let (a, b) = trim(source, a, b, groups);
        let (a, b) = snap(source, a, b);
        let size = cost(source, a, b);
        if used + size > SRC_BYTES {
            continue;
        }
        used += size;
        chosen.push((path.to_string(), a, b));
    }
    chosen
}

/// Prefix counts of the lines of a source file that sit inside a test unit.
fn test_lines(path: &str, lines: &[String]) -> Vec<usize> {
    let mut inside = vec![false; lines.len()];
    for unit in units(path, lines) {
        inside[unit.first - 1..unit.last].fill(true);
    }
    prefix(inside.into_iter().map(usize::from))
}

/// The first caller of each small function a window declares whose name carries
/// a group word, as the lines around its call; nothing when the graph does not
/// answer.
fn small_callers(
    ctx: &Ctx,
    sources: &BTreeMap<String, Source>,
    groups: &[Group],
    chosen: &[Span],
) -> Vec<Span> {
    let mut out = Vec::new();
    for (path, a, b) in chosen {
        let source = &sources[path];
        for k in *a..=*b {
            let Some(name) = DECL
                .captures(&source.lines[k - 1])
                .and_then(|caps| caps.get(1))
            else {
                continue;
            };
            let lower = name.as_str().to_lowercase();
            if !groups.iter().any(|group| group.in_line(&lower)) {
                continue;
            }
            let Ok(found) = ctx.evidence.symbols(name.as_str(), ctx.deadline) else {
                continue;
            };
            let Some(symbol) = found
                .into_iter()
                .find(|symbol| symbol.path == *path && symbol.start_line == k as u64)
            else {
                continue;
            };
            if symbol.end_line.saturating_sub(symbol.start_line) > SMALL_DECL {
                continue;
            }
            if let Some(span) = first_caller(ctx, sources, &symbol) {
                out.push(span);
            }
        }
    }
    out
}

/// The lines around the first call of `symbol`, by caller path then line, cut
/// to the caller's own span when the graph has it.
fn first_caller(ctx: &Ctx, sources: &BTreeMap<String, Source>, symbol: &SymbolHit) -> Option<Span> {
    let mut callers = ctx.evidence.callers(&symbol.uid, ctx.deadline).ok()?;
    callers.sort_by(|x, y| x.path.cmp(&y.path).then(x.line.cmp(&y.line)));
    let hit = callers.into_iter().next()?;
    let source = sources.get(&hit.path)?;
    let site = usize::try_from(hit.line).ok()?;
    let (mut lo, mut hi) = (site.saturating_sub(CALLER_BEFORE), site + CALLER_AFTER);
    let span = ctx
        .evidence
        .symbols(&hit.via, ctx.deadline)
        .ok()?
        .into_iter()
        .find(|c| c.path == hit.path && c.start_line <= hit.line && hit.line <= c.end_line);
    if let Some(span) = span {
        lo = lo.max(usize::try_from(span.start_line).ok()?);
        hi = hi.min(usize::try_from(span.end_line).ok()?);
    }
    let (lo, hi) = (lo.max(1), hi.min(source.lines.len()));
    (lo <= hi).then_some((hit.path, lo, hi))
}

/// The best test unit of each of the best files, with the siblings of its
/// family (`stop_after_binary_1`, `stop_after_binary_2`), within the budget.
fn test_units(sources: &BTreeMap<String, Source>, groups: &[Group]) -> Vec<Span> {
    let mut scored: Vec<(f64, &str, Unit)> = Vec::new();
    for (path, source) in sources {
        let stem = if named_after(path, groups) {
            STEM_BOOST
        } else {
            1.0
        };
        for unit in units(path, &source.lines) {
            let (held, total) = unit_score(source, &unit, groups);
            if held >= MIN_UNIT_GROUPS {
                scored.push((total * stem, path, unit));
            }
        }
    }
    scored.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| x.1.cmp(y.1))
            .then(x.2.first.cmp(&y.2.first))
    });
    // The units a file contributes: two in a file named after a group, else one,
    // never two of one family.
    let mut best: BTreeMap<&str, Vec<(f64, Unit, String)>> = BTreeMap::new();
    for (total, path, unit) in scored {
        let dedicated = named_after(path, groups);
        let family = family_of(&unit.name);
        let slot = best.entry(path).or_default();
        let room = if dedicated { 2 } else { 1 };
        if slot.len() < room && slot.iter().all(|held| held.2 != family) {
            let weight = if dedicated { DEDICATED_BOOST } else { 1.0 };
            slot.push((total * weight, unit, family));
        }
    }
    let mut picks: Vec<(f64, &str, Unit, String)> = best
        .into_iter()
        .flat_map(|(path, slot)| {
            slot.into_iter()
                .map(move |(total, unit, family)| (total, path, unit, family))
        })
        .collect();
    picks.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| x.1.cmp(y.1))
            .then(x.2.first.cmp(&y.2.first))
    });
    let mut files: Vec<&str> = Vec::new();
    let mut used = 0;
    let mut out = Vec::new();
    for (_, path, unit, family) in picks {
        if !files.contains(&path) {
            if files.len() >= MAX_TEST {
                continue;
            }
            files.push(path);
        }
        let source = &sources[path];
        let norm = digits_out(&unit.name);
        let mut group: Vec<Unit> = Vec::new();
        if family != norm {
            group.extend(units(path, &source.lines).into_iter().filter(|other| {
                digits_out(&other.name).starts_with(&format!("{family}_"))
                    && other.first != unit.first
            }));
            group.truncate(FAMILY - 1);
        }
        group.push(unit);
        group.sort_by_key(|other| other.first);
        for member in group {
            let last = member.last.min(member.first + TEST_CAP - 1);
            let size: usize = source.lines[member.first - 1..last]
                .iter()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.chars().count() + 8)
                .sum();
            if used + size > TEST_BYTES {
                continue;
            }
            used += size;
            out.push((path.to_string(), member.first, last));
        }
    }
    out
}

fn digits_out(name: &str) -> String {
    DIGITS.replace_all(name, "").into_owned()
}

/// The first two `_`-separated words of a name with three or more, else the
/// name; digits do not count (`stop_after_binary_2` is `stop_after`).
fn family_of(name: &str) -> String {
    let norm = digits_out(name);
    if norm.matches('_').count() >= 2 {
        norm.split('_').take(2).collect::<Vec<_>>().join("_")
    } else {
        norm
    }
}

#[cfg(test)]
mod tests;
