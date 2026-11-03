// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `prune-noise`: a brief is useful for the blocks the answer reads, and the
//! rest costs tokens and attention. This step only removes: it rewrites the
//! brief it receives as a smaller one and adds no evidence.
//!
//! Ported from `probes/prune-noise/probe`. It runs after the other probes
//! (`stage = "post"`, [`super::POST`]), on the brief they produced. The rules,
//! each generic (they read the block's header shape, never a name, and the
//! question's own words):
//!
//! - doc: documentation sections go unless the question asks for an overview or
//!   documentation;
//! - list: test-name lists go when the brief shows test bodies; config-registry
//!   rows go unless the row names a reader site and the key is named by the
//!   question; a bare slice only stays when a kept row cites it;
//! - complete: shell-completion files go unless the question names completions;
//! - merge: overlapping line ranges of one file are joined into one block;
//! - region: lines of a whole-file region that another block of the file
//!   already shows are removed;
//! - trace: the call trace is rewritten as `caller -> callee path:line`,
//!   constructors and edge-less entries removed;
//! - cap: no block above 2.5 KB (18 lines for a test body): the window with the
//!   most question words stays;
//! - total: 8 KB, 14 KB for flow and test questions; the least valuable blocks
//!   go first.
//!
//! It replaces the whole brief (`drop: .`, then its own blocks, mode lead) and
//! prints nothing when nothing shrank. Read-only, deterministic. A block the
//! script could not render (a numbered header with notes and no line) made it
//! fail and print nothing; the step prints nothing then too. The script's
//! `PRUNE_NOISE_STATS` debug line has no counterpart.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::subwords;
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "prune-noise",
    when_kinds: &[],
    skip_kinds: &[],
    when_prompt: None,
    when_gate_closed: false,
    budget_ms: 800,
    mode: ProbeMode::Lead,
    run,
};

/// Bytes of a block above which the total pass may shrink it.
const BLOCK_CAP: usize = 2_500;
/// Bytes a block keeps whatever the total.
const HARD_CAP: usize = 6_000;
/// Lines a test body keeps.
const TEST_LINES: usize = 18;
/// Bytes the brief keeps, and for a question about a flow or tests.
const TOTAL_CAP: usize = 8_000;
const TOTAL_CAP_WIDE: usize = 14_000;
/// How many question words an identifier of the question is worth.
const SYM_WEIGHT: usize = 3;
/// Added to the weight of the signature line of a definition.
const SIGNATURE_WEIGHT: i64 = 6;
/// Added to the weight of an assertion line of a test.
const ASSERT_WEIGHT: i64 = 3;
/// Added to the hits of a block so that a block with none is not worth zero.
const HITS_FLOOR: f64 = 0.5;
/// Added to what a shrink loses, so that a shrink losing nothing is not free.
const SHRINK_FLOOR: f64 = 0.25;
const FOOTER_PREFIXES: [&str; 4] = [
    "Answer from this context.",
    "These are the regions closest",
    "pack incomplete",
    "confidence:",
];
const WIDE_KINDS: [&str; 3] = ["flow", "tests", "overview"];
const STOP: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "what", "which", "where", "when", "does",
    "how", "are", "was", "its", "can", "not", "has", "any", "all", "but", "you", "our", "into",
    "than", "then", "each", "code", "file", "files", "line", "value", "values", "function",
];

static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^### (.+?):([0-9]+)(?:-([0-9]+))?(?: — (.*))?$")
        .expect("the header pattern is a literal")
});
static NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+)\| ?(.*)$").expect("the line pattern is a literal"));
static WIDE_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(trace|flow|call chain|end to end|from \w+ through|tests?)\b")
        .expect("the wide-question pattern is a literal")
});
static DOC_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(readme|documentation|docs?|guide|manual|overview|tour|describe|contains?|made of|make up|consists?)\b",
    )
    .expect("the doc-question pattern is a literal")
});
static COMPLETE_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)complet|shell").expect("the completion pattern is a literal"));
static COMPLETE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|/)(complete|completions?|completer)(/|$)|\.(zsh|fish|bash|ps1)$")
        .expect("the completion path pattern is a literal")
});
static IDENTIFIER_PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_-]+").expect("the token pattern is a literal"));
static SYMBOL_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"_|-{2}|[a-z][A-Z]|^[A-Z0-9_]{4,}$").expect("the symbol pattern is a literal")
});
static SEARCHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"searched (.+?) in the symbol graph").expect("the searched pattern is a literal")
});
static TEST_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\((?:a )?test(?: of [^)]*)?\)$").expect("the test label pattern is a literal")
});
static ASSERTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"ass.rt|expect|panic|should").expect("the assertion pattern is a literal")
});
static READ_SITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"read at ([^\s,;]+?):([0-9]+)").expect("the reader site pattern is a literal")
});
static TRACE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\s*)(entry|calls) (\S+) — (\S+?):([0-9]+)")
        .expect("the trace line pattern is a literal")
});

fn run(ctx: &Ctx) -> String {
    let brief = ctx.brief;
    if !brief.contains("### ") {
        return String::new();
    }
    let (head, raw, footer) = split_blocks(brief);
    let Some(blocks) = prune(ctx, &head, &raw, &footer) else {
        return String::new();
    };
    let mut rendered: Vec<String> = Vec::new();
    for block in &blocks {
        let Some(lines) = block.render() else {
            return String::new();
        };
        rendered.extend(lines);
    }
    let new = rendered.join("\n");
    let old = raw.iter().flatten().cloned().collect::<Vec<_>>().join("\n");
    if blocks.is_empty() || new.chars().count() >= old.chars().count() {
        return String::new();
    }
    format!("drop: .\n{new}\n")
}

/// `(head, blocks, footer)` of a brief: the lines before the first `### `
/// line, the blocks (a header and the lines to the next, trailing blank lines
/// removed), and the closing lines of the brief.
fn split_blocks(brief: &str) -> (Vec<String>, Vec<Vec<String>>, Vec<String>) {
    let mut lines: Vec<&str> = brief.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut footer: Vec<&str> = Vec::new();
    while let Some(last) = lines.last()
        && FOOTER_PREFIXES.iter().any(|prefix| last.starts_with(prefix))
    {
        footer.insert(0, lines.pop().unwrap_or_default());
    }
    let (mut head, mut blocks): (Vec<String>, Vec<Vec<String>>) = (Vec::new(), Vec::new());
    for line in lines {
        if line.starts_with("### ") {
            blocks.push(vec![line.to_owned()]);
        } else if let Some(block) = blocks.last_mut() {
            block.push(line.to_owned());
        } else {
            head.push(line.to_owned());
        }
    }
    for block in &mut blocks {
        while block.len() > 1 && block.last().is_some_and(String::is_empty) {
            block.pop();
        }
    }
    (
        head,
        blocks,
        footer.into_iter().map(ToOwned::to_owned).collect(),
    )
}

/// What a block is, by the shape of its header and its label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cat {
    Registry,
    List,
    Trace,
    Doc,
    Commit,
    Other,
    Bare,
    Test,
    Region,
    Definition,
    Usage,
    Flag,
}

impl Cat {
    /// Which class wins a merge.
    fn weight(self) -> u32 {
        match self {
            Self::Commit => 100,
            Self::Definition => 90,
            Self::Test => 80,
            Self::Flag => 70,
            Self::Usage => 60,
            Self::Other => 50,
            Self::Region => 40,
            Self::Trace => 30,
            Self::Doc => 20,
            Self::List => 10,
            Self::Registry | Self::Bare => 0,
        }
    }

    /// How much a byte of this class is worth when the total is over.
    fn tier(self) -> f64 {
        match self {
            Self::Commit | Self::Other => 1_000.0,
            Self::Definition | Self::Test | Self::Flag => 2.0,
            Self::Trace | Self::Doc | Self::List => 0.3,
            Self::Usage | Self::Region | Self::Registry | Self::Bare => 1.0,
        }
    }
}

/// A block of the brief. A block with numbered lines (`N| text`) is `ranged`:
/// its lines are kept by number, so that they can be merged, cut and
/// subtracted; any other block is kept or dropped whole.
struct Block {
    lines: Vec<String>,
    header: String,
    path: String,
    label: String,
    lo: usize,
    hi: usize,
    body: BTreeMap<usize, String>,
    notes: Vec<String>,
    ranged: bool,
    cat: Cat,
    dropped: bool,
}

impl Block {
    /// The block of `lines` (a header, then its lines).
    fn new(lines: Vec<String>) -> Self {
        let header = lines.first().cloned().unwrap_or_default();
        let mut block = Self {
            lines,
            header,
            path: String::new(),
            label: String::new(),
            lo: 0,
            hi: 0,
            body: BTreeMap::new(),
            notes: Vec::new(),
            ranged: false,
            cat: Cat::Other,
            dropped: false,
        };
        let parsed = HEADER.captures(&block.header).and_then(|found| {
            let lo: usize = found.get(2)?.as_str().parse().ok()?;
            let hi: usize = found
                .get(3)
                .map_or(Some(lo), |hi| hi.as_str().parse().ok())?;
            let label = found.get(4).map_or("", |label| label.as_str());
            Some((found.get(1)?.as_str().to_owned(), lo, hi, label.to_owned()))
        });
        if let Some((path, lo, hi, label)) = parsed {
            (block.path, block.lo, block.hi, block.label) = (path, lo, hi, label);
            let rest: Vec<&String> = block.lines.iter().skip(1).filter(|x| !x.is_empty()).collect();
            if !rest.is_empty()
                && rest
                    .iter()
                    .all(|line| NUMBERED.is_match(line) || line.starts_with('…'))
            {
                block.ranged = true;
                for line in rest {
                    if let Some(found) = NUMBERED.captures(line) {
                        if let Some(number) = found.get(1).and_then(|n| n.as_str().parse().ok()) {
                            let text = found.get(2).map_or("", |text| text.as_str());
                            block.body.insert(number, text.to_owned());
                        }
                    } else {
                        block.notes.push(line.clone());
                    }
                }
            }
        }
        block.cat = block.classify();
        block
    }

    fn classify(&self) -> Cat {
        let header = self.header.get(4..).unwrap_or_default();
        if header.starts_with("config registry") {
            Cat::Registry
        } else if header.starts_with("tests matching") || header.starts_with("tests of ") {
            Cat::List
        } else if header.starts_with("call trace") {
            Cat::Trace
        } else if header.ends_with("(documentation section)") {
            Cat::Doc
        } else if header.starts_with("change ")
            || header.starts_with("diff of")
            || header.contains("in commit")
        {
            Cat::Commit
        } else if !self.ranged {
            Cat::Other
        } else if self.label.is_empty() {
            Cat::Bare
        } else if TEST_LABEL.is_match(&self.label) {
            Cat::Test
        } else if self.label.ends_with(" region") || self.label.contains("whole file") {
            Cat::Region
        } else if self.label.contains(" definition") {
            Cat::Definition
        } else if self.label.contains(" usage") {
            Cat::Usage
        } else {
            Cat::Flag
        }
    }

    /// Characters the block costs, one newline each line.
    fn size(&self) -> usize {
        if self.ranged {
            self.header.chars().count()
                + 1
                + self.body.iter().map(|(n, text)| line_cost(*n, text)).sum::<usize>()
                + self.notes.iter().map(|note| note.chars().count() + 1).sum::<usize>()
        } else {
            self.lines.iter().map(|line| line.chars().count() + 1).sum()
        }
    }

    /// The lines of the block: for a ranged one its header (the span of the
    /// lines left) and the lines by number, and its notes when the last line is
    /// the block's own. `None` for a ranged block with no line left, which the
    /// script could not render.
    fn render(&self) -> Option<Vec<String>> {
        if !self.ranged {
            return Some(self.lines.clone());
        }
        let first = *self.body.keys().next()?;
        let last = *self.body.keys().next_back()?;
        let span = if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        };
        let label = if self.label.is_empty() {
            String::new()
        } else {
            format!(" — {}", self.label)
        };
        let mut out = vec![format!("### {}:{span}{label}", self.path)];
        out.extend(self.body.iter().map(|(n, text)| format!("{n}| {text}")));
        if last == self.hi {
            out.extend(self.notes.iter().cloned());
        }
        Some(out)
    }
}

/// What a numbered line costs: `N| text` and its newline.
fn line_cost(number: usize, text: &str) -> usize {
    number.to_string().len() + 2 + text.chars().count() + 1
}

/// `text` lowercased, without anything but letters and digits.
fn squash(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        .collect()
}

/// The question's content words and identifiers, what a line is weighed by.
struct Anchors {
    words: HashSet<String>,
    syms: HashSet<String>,
}

impl Anchors {
    /// The content subwords of `prompt`, its identifiers (and the symbols the
    /// pack says it searched in `head`).
    fn new(prompt: &str, head: &str) -> Self {
        let words = subwords(prompt)
            .into_iter()
            .filter(|word| word.len() >= 3 && !STOP.contains(&word.as_str()))
            .collect();
        let mut syms: HashSet<String> = IDENTIFIER_PART
            .find_iter(prompt)
            .map(|token| token.as_str())
            .filter(|token| SYMBOL_SHAPE.is_match(token))
            .map(squash)
            .collect();
        if let Some(found) = SEARCHED.captures(head).and_then(|found| found.get(1)) {
            syms.extend(
                found
                    .as_str()
                    .split(',')
                    .map(squash)
                    .filter(|token| token.len() >= 4),
            );
        }
        syms.retain(|sym| sym.len() >= 4);
        Self { words, syms }
    }

    /// Question words `text` holds, and three for each identifier it holds
    /// once squashed; `extra` is one more identifier.
    fn weight(&self, text: &str, extra: Option<&str>) -> usize {
        let norm = squash(text);
        let tokens: HashSet<String> = subwords(text).into_iter().collect();
        let extra = extra.filter(|sym| !self.syms.contains(*sym));
        let hits = self.words.iter().filter(|word| tokens.contains(*word)).count();
        let syms = self
            .syms
            .iter()
            .map(String::as_str)
            .chain(extra)
            .filter(|sym| norm.contains(sym))
            .count();
        hits + SYM_WEIGHT * syms
    }
}

/// Keeps in `block` the run of lines that fits `cap_bytes` and `cap_lines` and
/// weighs most, then spends what is left of the budget on context after and
/// before. `false` when the block already fits.
fn window(block: &mut Block, cap_bytes: usize, cap_lines: usize, anchors: &Anchors) -> bool {
    let keys: Vec<usize> = block.body.keys().copied().collect();
    let sizes: Vec<usize> = block.body.iter().map(|(n, text)| line_cost(*n, text)).collect();
    if sizes.iter().sum::<usize>() <= cap_bytes && keys.len() <= cap_lines {
        return false;
    }
    let first_word = block.label.split(' ').next().unwrap_or_default();
    let sym = squash(first_word);
    let extra = (sym.len() >= 4).then_some(sym.as_str());
    let mut weights: Vec<i64> = block
        .body
        .values()
        .map(|text| i64::try_from(anchors.weight(text, extra)).unwrap_or(i64::MAX))
        .collect();
    if block.cat == Cat::Definition
        && let Some(signature) = weights.first_mut()
    {
        *signature += SIGNATURE_WEIGHT;
    }
    if block.cat == Cat::Test {
        for (weight, text) in weights.iter_mut().zip(block.body.values()) {
            if ASSERTION.is_match(text) {
                *weight += ASSERT_WEIGHT;
            }
        }
    }
    let (mut best, mut start, mut end, mut used) = (-1_i64, 0, 0, 0);
    for from in 0..keys.len() {
        let (mut spent, mut gain, mut to) = (0, 0, from);
        for index in from..keys.len() {
            if spent + sizes[index] > cap_bytes || index - from >= cap_lines {
                break;
            }
            spent += sizes[index];
            gain += weights[index];
            to = index + 1;
        }
        if to > from && gain > best {
            (best, start, end, used) = (gain, from, to, spent);
        }
    }
    // Spend the rest of the budget on context, after then before.
    loop {
        let mut grown = false;
        if end < keys.len() && used + sizes[end] <= cap_bytes && end - start < cap_lines {
            used += sizes[end];
            end += 1;
            grown = true;
        }
        if start > 0 && used + sizes[start - 1] <= cap_bytes && end - start < cap_lines {
            start -= 1;
            used += sizes[start];
            grown = true;
        }
        if !grown {
            break;
        }
    }
    let kept: HashSet<usize> = keys[start..end].iter().copied().collect();
    block.body.retain(|number, _| kept.contains(number));
    true
}

/// The rows of a config registry block that name a reader site and whose key the
/// question names, and those sites.
fn registry_keep(block: &Block, prompt: &str, sites: &mut HashSet<(String, usize)>) -> Vec<String> {
    let low = squash(prompt);
    let mut kept = Vec::new();
    for row in block.lines.iter().skip(1).filter(|row| row.starts_with("- ")) {
        let cells: Vec<&str> = row[2..].split(" | ").map(str::trim).collect();
        let key = squash(cells.first().copied().unwrap_or_default());
        let site = cells.last().and_then(|cell| READ_SITE.captures(cell));
        if let Some(site) = site
            && key.len() >= 4
            && low.contains(&key)
            && let Some(number) = site.get(2).and_then(|n| n.as_str().parse().ok())
        {
            kept.push(row.clone());
            sites.insert((site[1].to_owned(), number));
        }
    }
    kept
}

/// The call trace as `caller -> callee path:line` edges (constructors, a
/// capitalised callee, and entries with no edge are left out); `None` when no
/// edge is left.
fn rewrite_trace(block: &Block) -> Option<Vec<String>> {
    let mut edges = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    for raw in block.lines.iter().skip(1) {
        let Some(found) = TRACE_LINE.captures(raw) else {
            continue;
        };
        let name = found[3].to_owned();
        if &found[2] == "entry" {
            stack = vec![name];
            continue;
        }
        stack.truncate(found[1].chars().count() / 2);
        let caller = stack.last().map_or("?", String::as_str).to_owned();
        let constructor = name.chars().next().is_some_and(char::is_uppercase);
        if !constructor {
            edges.push(format!("{caller} -> {name} {}:{}", &found[4], &found[5]));
        }
        stack.push(name);
    }
    (!edges.is_empty()).then(|| {
        let mut lines = vec![block.header.clone()];
        lines.extend(edges);
        lines
    })
}

/// Blocks of one file and one class (region or not) whose ranges overlap become
/// the first of them.
fn merge_overlaps(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut seen: HashMap<(String, bool), Vec<usize>> = HashMap::new();
    for block in blocks {
        if !block.ranged {
            out.push(block);
            continue;
        }
        let class = (block.path.clone(), block.cat == Cat::Region);
        let target = seen.get(&class).and_then(|held| {
            held.iter()
                .copied()
                .find(|at| out[*at].lo <= block.hi && block.lo <= out[*at].hi)
        });
        let Some(at) = target else {
            seen.entry(class).or_default().push(out.len());
            out.push(block);
            continue;
        };
        let target = &mut out[at];
        target.body.extend(block.body);
        target.lo = target.lo.min(block.lo);
        target.hi = target.hi.max(block.hi);
        if block.cat.weight() > target.cat.weight() {
            target.cat = block.cat;
        }
    }
    out
}

/// Removes from each whole-file region the lines another block of the file
/// already shows; the runs of three lines or more left become regions.
fn subtract_regions(blocks: Vec<Block>) -> Vec<Block> {
    let mut shown: HashMap<String, HashSet<usize>> = HashMap::new();
    for block in blocks
        .iter()
        .filter(|block| block.ranged && block.cat != Cat::Region)
    {
        shown
            .entry(block.path.clone())
            .or_default()
            .extend(block.body.keys().copied());
    }
    let mut result = Vec::new();
    for block in blocks {
        if !block.ranged || block.cat != Cat::Region {
            result.push(block);
            continue;
        }
        let covered = shown.get(&block.path);
        let keep: Vec<usize> = block
            .body
            .keys()
            .copied()
            .filter(|number| !covered.is_some_and(|held| held.contains(number)))
            .collect();
        if keep.len() == block.body.len() {
            result.push(block);
            continue;
        }
        let mut runs: Vec<Vec<usize>> = vec![Vec::new()];
        for number in keep {
            let continues = runs
                .last()
                .and_then(|run| run.last())
                .is_none_or(|last| number == last + 1);
            if !continues {
                runs.push(Vec::new());
            }
            if let Some(run) = runs.last_mut() {
                run.push(number);
            }
        }
        for run in runs.into_iter().filter(|run| run.len() >= 3) {
            let mut piece = Block::new(block.lines.iter().take(1).cloned().collect());
            piece.path.clone_from(&block.path);
            piece.label.clone_from(&block.label);
            piece.ranged = true;
            piece.cat = Cat::Region;
            piece.lo = run[0];
            piece.hi = run[run.len() - 1];
            piece.body = run
                .iter()
                .filter_map(|n| block.body.get(n).map(|text| (*n, text.clone())))
                .collect();
            result.push(piece);
        }
    }
    result
}

/// What the question is worth to a block: its hits per kilobyte, times the
/// tier of its class.
fn hits_of(block: &Block, anchors: &Anchors) -> usize {
    if block.ranged {
        block.body.values().map(|text| anchors.weight(text, None)).sum()
    } else {
        0
    }
}

/// What the total pass does to the block it chose.
enum Action {
    Drop,
    Shrink(Block),
}

/// The blocks of the pruned brief; `None` when the step ran out of time.
fn prune(ctx: &Ctx, head: &[String], raw: &[Vec<String>], footer: &[String]) -> Option<Vec<Block>> {
    let prompt = ctx.prompt;
    let mut blocks: Vec<Block> = raw.iter().cloned().map(Block::new).collect();
    let anchors = Anchors::new(prompt, &head.join("\n"));
    let doc_ok = ctx.kind == Some("overview") || DOC_PROMPT.is_match(prompt);
    let complete_ok = COMPLETE_PROMPT.is_match(prompt);
    let has_test_body = blocks.iter().any(|block| block.cat == Cat::Test);
    let mut sites: HashSet<(String, usize)> = HashSet::new();
    for block in &mut blocks {
        if block.cat == Cat::Doc {
            let seen = block
                .lines
                .iter()
                .flat_map(|line| subwords(line))
                .any(|word| anchors.words.contains(&word));
            block.dropped = !doc_ok || !seen;
        } else if block.cat == Cat::List && has_test_body {
            block.dropped = true;
        } else if block.cat == Cat::Registry {
            let rows = registry_keep(block, prompt, &mut sites);
            if rows.is_empty() {
                block.dropped = true;
            } else {
                block.lines.truncate(1);
                block.lines.extend(rows);
            }
        } else if block.ranged && !complete_ok && COMPLETE_PATH.is_match(&block.path) {
            block.dropped = true;
        } else if block.cat == Cat::Trace {
            match rewrite_trace(block) {
                Some(lines) => block.lines = lines,
                None => block.dropped = true,
            }
        }
    }
    for index in 0..blocks.len() {
        let tied = blocks[..index].iter().any(|block| block.cat == Cat::Registry);
        let block = &blocks[index];
        let cited = sites
            .iter()
            .any(|(path, number)| *path == block.path && block.lo <= *number && *number <= block.hi);
        if block.cat == Cat::Bare && !block.dropped && tied && !cited {
            blocks[index].dropped = true;
        }
    }
    blocks.retain(|block| !block.dropped);
    let mut blocks = subtract_regions(merge_overlaps(blocks));
    for block in &mut blocks {
        if block.ranged {
            let lines = if block.cat == Cat::Test { TEST_LINES } else { usize::MAX };
            window(block, HARD_CAP, lines, &anchors);
        }
    }
    let wide = ctx.kind.is_some_and(|kind| WIDE_KINDS.contains(&kind)) || WIDE_PROMPT.is_match(prompt);
    let cap = if wide { TOTAL_CAP_WIDE } else { TOTAL_CAP };
    let fixed: usize = head
        .iter()
        .chain(footer)
        .map(|line| line.chars().count() + 1)
        .sum();
    while fixed + blocks.iter().map(Block::size).sum::<usize>() > cap && blocks.len() > 1 {
        if ctx.timed_out() {
            return None;
        }
        let mut best: Option<(f64, usize, Action)> = None;
        for (index, block) in blocks.iter().enumerate() {
            let tier = block.cat.tier();
            let head_hits = if block.ranged {
                2 * anchors.weight(&format!("{} {}", block.path, block.label), None)
            } else {
                0
            };
            let hits = hits_of(block, &anchors);
            let value = tier * ((hits + head_hits) as f64 + HITS_FLOOR) / block.size().max(1) as f64;
            let mut options = vec![(value, Action::Drop)];
            if block.ranged && block.cat != Cat::Commit && block.size() > BLOCK_CAP {
                let mut small = Block::new(block.render()?);
                small.cat = block.cat;
                small.label.clone_from(&block.label);
                let lines = if block.cat == Cat::Test { TEST_LINES } else { usize::MAX };
                if window(&mut small, BLOCK_CAP, lines, &anchors) {
                    let saved = block.size().saturating_sub(small.size());
                    let lost = tier * hits.saturating_sub(hits_of(&small, &anchors)) as f64
                        + SHRINK_FLOOR;
                    options.push((lost / saved.max(1) as f64, Action::Shrink(small)));
                }
            }
            for (score, action) in options {
                // Lowest score loses first; on a tie the later block.
                if best.as_ref().is_none_or(|(low, at, _)| (score, Reverse(index)) < (*low, Reverse(*at))) {
                    best = Some((score, index, action));
                }
            }
        }
        match best? {
            (_, index, Action::Shrink(small)) => blocks[index] = small,
            (_, index, Action::Drop) => {
                blocks.remove(index);
            }
        }
    }
    Some(blocks)
}

#[cfg(test)]
mod tests;
