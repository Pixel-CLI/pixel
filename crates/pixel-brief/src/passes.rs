// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The render passes (`specs/brief.md`, "Recipes"): `rank-blocks` and
//! `prune-noise`, applied to the blocks of the pack the recipe steps did not
//! add.
//!
//! * **rank-blocks**: the answer is often in the pack already, buried among
//!   blocks that share no word with the question. Each block scores by the
//!   question's words its head names; the blocks that share none go, the rest
//!   move to the front by score. Nothing is dropped when no block shares a
//!   word (it never empties a pack), and the overview, review, flow and history
//!   kinds are left alone.
//! * **prune-noise**: the pack is useful for the blocks the answer reads, and
//!   the rest costs tokens and attention. It only removes: lists of test names
//!   when test bodies are shown, shell-completion files unless the question
//!   names completions, overlapping ranges of one file joined into one block,
//!   lines of a whole-file region that another block of the file already shows,
//!   and no block above [`BLOCK_BYTES`] (the window with the most question words
//!   stays).
//!
//! Each rule reads a block's head shape and the question's own words, never a
//! name. Both passes are steps with flags (`rank-blocks`, `prune-noise`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::region_pack::{Block, Candidate, Part};
use crate::steps::{stem, subwords};

/// What the passes need to know about the pack being built.
#[derive(Clone, Debug, Default)]
pub(crate) struct Passes {
    pub(crate) rank: bool,
    pub(crate) prune: bool,
    pub(crate) prompt: String,
    /// Identifiers the pack searched for: a line naming one counts for more.
    pub(crate) names: Vec<String>,
}

/// No block above this many bytes of source, unless it is a change.
const BLOCK_BYTES: usize = 6_000;
/// Lines of a test body that stay.
const TEST_LINES: usize = 18;
/// A symbol named by the question weighs as much as this many plain words.
const SYMBOL_WEIGHT: usize = 3;
/// Shortest run of lines a region keeps once other blocks took theirs.
const MIN_RUN: usize = 3;

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "the", "and", "how", "is",
    "a", "of", "it", "defined", "define", "number", "returns", "return", "set", "get", "use",
    "used", "make", "could", "there",
];
const LINE_STOP: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "what", "which", "where", "when", "does",
    "how", "are", "was", "its", "can", "not", "has", "any", "all", "but", "you", "our", "into",
    "than", "then", "each", "code", "file", "files", "line", "value", "values", "function",
];

static HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+)(?:-(\d+))?(?: — (.*))?$").expect("the head pattern is a literal")
});
static NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\| ?(.*)$").expect("the numbered pattern is a literal"));
static COMPLETE_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)complet|shell").expect("the completion pattern is a literal")
});
static COMPLETE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|/)(complete|completions?|completer)(/|$)|\.(zsh|fish|bash|ps1)$")
        .expect("the completion path pattern is a literal")
});
static TEST_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\((?:a )?test(?: of [^)]*)?\)$").expect("the test label pattern is a literal")
});
static ASSERTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"ass.rt|expect|panic|should").expect("the assertion pattern is a literal")
});

/// The content stems of `text`.
fn words(text: &str) -> BTreeSet<String> {
    subwords(text)
        .into_iter()
        .filter(|word| word.chars().count() > 2 && word.chars().all(char::is_alphabetic))
        .filter(|word| !STOP.contains(&word.as_str()))
        .map(|word| stem(&word))
        .collect()
}

/// The pack's candidates after the passes: `exempt` heads (the steps' own
/// blocks) are never touched.
pub(crate) fn refine(
    mut candidates: Vec<Candidate>,
    exempt: &[String],
    passes: &Passes,
) -> Vec<Candidate> {
    if passes.rank {
        candidates = rank_blocks(candidates, exempt, &passes.prompt);
    }
    if passes.prune {
        candidates = prune_noise(candidates, exempt, passes);
    }
    candidates
}

fn is_own(candidate: &Candidate, exempt: &[String]) -> bool {
    exempt.iter().any(|head| head == candidate.block.head())
}

/// Blocks that name none of the question's words go (when some block does),
/// and those that do move to the front by how many they name.
fn rank_blocks(candidates: Vec<Candidate>, exempt: &[String], prompt: &str) -> Vec<Candidate> {
    let want = words(prompt);
    let scores: Vec<Option<usize>> = candidates
        .iter()
        .map(|candidate| {
            if is_own(candidate, exempt) {
                return None;
            }
            let head = candidate.block.head();
            let (label, rest) = head.split_once(" — ").unwrap_or((head, ""));
            Some(
                want.intersection(&words(&format!("{rest} {label}")))
                    .count(),
            )
        })
        .collect();
    if !scores.iter().flatten().any(|score| *score > 0) {
        return candidates;
    }
    let mut positive: Vec<(usize, usize)> = scores
        .iter()
        .enumerate()
        .filter_map(|(index, score)| score.filter(|s| *s > 0).map(|s| (s, index)))
        .collect();
    positive.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let place: BTreeMap<usize, usize> = positive
        .iter()
        .enumerate()
        .map(|(position, (_, index))| (*index, position))
        .collect();
    candidates
        .into_iter()
        .enumerate()
        .filter(|(index, _)| scores[*index] != Some(0))
        .map(|(index, mut candidate)| {
            if let Some(position) = place.get(&index) {
                candidate.order = (0, *position, candidate.order.2);
            }
            candidate
        })
        .collect()
}

/// A block of numbered source: its file, its lines by number and its label.
#[derive(Clone, Debug)]
struct Ranged {
    path: String,
    label: String,
    lines: BTreeMap<usize, String>,
}

impl Ranged {
    fn parse(block: &Block) -> Option<Self> {
        let caps = HEAD.captures(block.head())?;
        let mut lines = BTreeMap::new();
        for line in block.body() {
            let numbered = NUMBERED.captures(line)?;
            lines.insert(numbered[1].parse().ok()?, numbered[2].to_string());
        }
        (!lines.is_empty()).then(|| Self {
            path: caps[1].to_string(),
            label: caps
                .get(4)
                .map_or(String::new(), |m| m.as_str().to_string()),
            lines,
        })
    }

    fn lo(&self) -> usize {
        *self.lines.keys().next().unwrap_or(&0)
    }

    fn hi(&self) -> usize {
        *self.lines.keys().next_back().unwrap_or(&0)
    }

    fn is_region(&self) -> bool {
        self.label.ends_with(" region") || self.label.contains("whole file")
    }

    fn is_test(&self) -> bool {
        TEST_LABEL.is_match(&self.label)
    }

    fn bytes(&self) -> usize {
        self.lines
            .iter()
            .map(|(n, text)| format!("{n}| {text}").len() + 1)
            .sum()
    }

    fn into_block(self) -> Block {
        let span = if self.lo() == self.hi() {
            self.lo().to_string()
        } else {
            format!("{}-{}", self.lo(), self.hi())
        };
        let head = if self.label.is_empty() {
            format!("{}:{span}", self.path)
        } else {
            format!("{}:{span} — {}", self.path, self.label)
        };
        Block::new(
            head,
            self.lines
                .iter()
                .map(|(n, text)| format!("{n}| {text}"))
                .collect(),
        )
    }
}

fn prune_noise(candidates: Vec<Candidate>, exempt: &[String], passes: &Passes) -> Vec<Candidate> {
    let complete_ok = COMPLETE_ASK.is_match(&passes.prompt);
    let has_test_body = candidates.iter().any(|candidate| {
        !is_own(candidate, exempt)
            && Ranged::parse(&candidate.block).is_some_and(|ranged| ranged.is_test())
    });
    // The lists and the completion files go first.
    let mut kept: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| {
            if is_own(candidate, exempt) {
                return true;
            }
            let head = candidate.block.head();
            let list = head.starts_with("tests matching") || head.starts_with("tests of ");
            if list && has_test_body {
                return false;
            }
            let ranged = Ranged::parse(&candidate.block);
            !(ranged.is_some_and(|ranged| COMPLETE_PATH.is_match(&ranged.path)) && !complete_ok)
        })
        .collect();
    merge_overlaps(&mut kept, exempt);
    subtract_regions(&mut kept, exempt);
    cap_blocks(&mut kept, exempt, passes);
    kept
}

/// Blocks of one file and one class (region or not) whose ranges overlap
/// become the first of them.
fn merge_overlaps(candidates: &mut Vec<Candidate>, exempt: &[String]) {
    let mut firsts: BTreeMap<(String, bool), Vec<usize>> = BTreeMap::new();
    let mut merged: BTreeMap<usize, Ranged> = BTreeMap::new();
    let mut gone: BTreeSet<usize> = BTreeSet::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if is_own(candidate, exempt) {
            continue;
        }
        let Some(ranged) = Ranged::parse(&candidate.block) else {
            continue;
        };
        let class = (ranged.path.clone(), ranged.is_region());
        let target = firsts.get(&class).and_then(|held| {
            held.iter().copied().find(|other| {
                let other = &merged[other];
                other.lo() <= ranged.hi() && ranged.lo() <= other.hi()
            })
        });
        match target {
            Some(target) => {
                let held = merged.get_mut(&target).expect("a merge target is recorded");
                held.lines.extend(ranged.lines);
                gone.insert(index);
            }
            None => {
                firsts.entry(class).or_default().push(index);
                merged.insert(index, ranged);
            }
        }
    }
    if gone.is_empty() {
        return;
    }
    let mut index = 0;
    candidates.retain_mut(|candidate| {
        let current = index;
        index += 1;
        if gone.contains(&current) {
            return false;
        }
        if let Some(ranged) = merged.get(&current) {
            // Only a block that took lines from another changes.
            if ranged.lines.len() > candidate.block.body().len() {
                candidate.block = ranged.clone().into_block();
            }
        }
        true
    });
}

/// The lines of a whole-file region that another block of the file shows are
/// removed; runs of fewer than [`MIN_RUN`] lines go with them.
fn subtract_regions(candidates: &mut Vec<Candidate>, exempt: &[String]) {
    let mut shown: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for candidate in candidates.iter() {
        if is_own(candidate, exempt) {
            continue;
        }
        if let Some(ranged) = Ranged::parse(&candidate.block)
            && !ranged.is_region()
        {
            shown
                .entry(ranged.path)
                .or_default()
                .extend(ranged.lines.into_keys());
        }
    }
    let mut out: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        let ranged = (!is_own(&candidate, exempt))
            .then(|| Ranged::parse(&candidate.block))
            .flatten()
            .filter(Ranged::is_region);
        let Some(ranged) = ranged else {
            out.push(candidate);
            continue;
        };
        let covered = shown.get(&ranged.path);
        let keep: Vec<usize> = ranged
            .lines
            .keys()
            .copied()
            .filter(|n| covered.is_none_or(|set| !set.contains(n)))
            .collect();
        if keep.len() == ranged.lines.len() {
            out.push(candidate);
            continue;
        }
        let mut runs: Vec<Vec<usize>> = Vec::new();
        for n in keep {
            match runs.last_mut() {
                Some(run) if run.last().is_some_and(|last| *last + 1 == n) => run.push(n),
                _ => runs.push(vec![n]),
            }
        }
        for run in runs.into_iter().filter(|run| run.len() >= MIN_RUN) {
            let piece = Ranged {
                path: ranged.path.clone(),
                label: ranged.label.clone(),
                lines: run.iter().map(|n| (*n, ranged.lines[n].clone())).collect(),
            };
            out.push(Candidate {
                block: piece.into_block(),
                ..candidate.clone()
            });
        }
    }
    *candidates = out;
}

/// The question's words and identifiers, for weighing lines.
struct Anchors {
    words: BTreeSet<String>,
    symbols: BTreeSet<String>,
}

fn squash(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

impl Anchors {
    fn new(passes: &Passes) -> Self {
        let words = subwords(&passes.prompt)
            .into_iter()
            .filter(|word| word.chars().count() >= 3 && !LINE_STOP.contains(&word.as_str()))
            .collect();
        let mut symbols: BTreeSet<String> = passes
            .prompt
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
            .filter(|token| {
                token.contains('_')
                    || token.contains("--")
                    || token
                        .chars()
                        .zip(token.chars().skip(1))
                        .any(|(a, b)| a.is_ascii_lowercase() && b.is_ascii_uppercase())
                    || (token.len() >= 4
                        && token
                            .chars()
                            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'))
            })
            .map(squash)
            .collect();
        symbols.extend(
            passes
                .names
                .iter()
                .map(|name| squash(name))
                .filter(|name| name.len() >= 4),
        );
        symbols.retain(|symbol| symbol.len() >= 4);
        Self { words, symbols }
    }

    fn weight(&self, text: &str, extra: Option<&str>) -> usize {
        let norm = squash(text);
        let parts: BTreeSet<String> = subwords(text).into_iter().collect();
        let plain = self.words.intersection(&parts).count();
        let named = self
            .symbols
            .iter()
            .map(String::as_str)
            .chain(extra)
            .filter(|symbol| norm.contains(symbol))
            .count();
        plain + SYMBOL_WEIGHT * named
    }
}

/// No block above [`BLOCK_BYTES`] (or [`TEST_LINES`] for a test): the window
/// with the most question words stays, then context fills the rest.
fn cap_blocks(candidates: &mut [Candidate], exempt: &[String], passes: &Passes) {
    let anchors = Anchors::new(passes);
    for candidate in candidates.iter_mut() {
        if is_own(candidate, exempt) {
            continue;
        }
        let Some(ranged) = Ranged::parse(&candidate.block) else {
            continue;
        };
        let cap_lines = if ranged.is_test() {
            TEST_LINES
        } else {
            usize::MAX
        };
        if ranged.bytes() <= BLOCK_BYTES && ranged.lines.len() <= cap_lines {
            continue;
        }
        if let Some(window) = window(&ranged, cap_lines, &anchors, candidate.part) {
            candidate.block = window.into_block();
        }
    }
}

fn window(block: &Ranged, cap_lines: usize, anchors: &Anchors, part: Part) -> Option<Ranged> {
    let keys: Vec<usize> = block.lines.keys().copied().collect();
    let sizes: Vec<usize> = keys
        .iter()
        .map(|n| format!("{n}| {}", block.lines[n]).len() + 1)
        .collect();
    let symbol = squash(block.label.split(' ').next().unwrap_or(""));
    let extra = (symbol.len() >= 4).then_some(symbol);
    let mut weights: Vec<usize> = keys
        .iter()
        .map(|n| anchors.weight(&block.lines[n], extra.as_deref()))
        .collect();
    if part == Part::Definition
        && !block.is_region()
        && let Some(first) = weights.first_mut()
    {
        // The signature is the line a definition is read for.
        *first += 6;
    }
    if block.is_test() {
        for (weight, n) in weights.iter_mut().zip(&keys) {
            if ASSERTION.is_match(&block.lines[n]) {
                *weight += 3;
            }
        }
    }
    let (mut best, mut lo, mut hi, mut used) = (None::<usize>, 0, 0, 0);
    for i in 0..keys.len() {
        let (mut j, mut bytes, mut gain) = (i, 0, 0);
        while j < keys.len() && bytes + sizes[j] <= BLOCK_BYTES && j - i < cap_lines {
            bytes += sizes[j];
            gain += weights[j];
            j += 1;
        }
        if j > i && best.is_none_or(|b| gain > b) {
            best = Some(gain);
            (lo, hi, used) = (i, j, bytes);
        }
    }
    best?;
    // The rest of the budget goes to context, after the window then before it.
    loop {
        let mut grown = false;
        if hi < keys.len() && used + sizes[hi] <= BLOCK_BYTES && hi - lo < cap_lines {
            used += sizes[hi];
            hi += 1;
            grown = true;
        }
        if lo > 0 && used + sizes[lo - 1] <= BLOCK_BYTES && hi - lo < cap_lines {
            lo -= 1;
            used += sizes[lo];
            grown = true;
        }
        if !grown {
            break;
        }
    }
    Some(Ranged {
        path: block.path.clone(),
        label: block.label.clone(),
        lines: keys[lo..hi]
            .iter()
            .map(|n| (*n, block.lines[n].clone()))
            .collect(),
    })
}

#[cfg(test)]
mod tests;
