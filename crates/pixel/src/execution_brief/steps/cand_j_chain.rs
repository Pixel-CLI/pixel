// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-j-chain`: "trace how A turns into a T, from an entry through ..." is
//! answered by the call chain between two symbols the question names, not by
//! the callees of one of them.
//!
//! Ported from `probes/cand-j-chain/probe`. The entry is the function-shaped
//! word after "from" (else the first word that resolves to a function), the
//! target is the first type-shaped word that resolves to a type; the target's
//! constructors are the functions of its file whose signature returns it.
//! `call-path` gives the shortest chain from the entry to a constructor. Each
//! hop is shown at its call site (its whole body when short), the other
//! callees of the entry that take the target in their signature are shown at
//! their head, and, at each hop, the callees whose names carry the question's
//! words (generic words weigh nothing) follow, two levels deep. The brief's
//! own flow blocks are replaced. Prints nothing without a daemon or a chain.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::graph::{Edge, Hop, Role, Sym};
use super::text::{LETTERS, escape, head, lines_of, stem};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-j-chain",
    when_kinds: &[],
    skip_kinds: &["overview", "review", "history"],
    when_prompt: Some(
        r"(?i)\b(trace|call (chain|path)|end to end|walk (me )?through|from \w+ (through|to|into))\b",
    ),
    when_gate_closed: true,
    budget_ms: 2500,
    mode: ProbeMode::Lead,
    run,
};

const TYPE_KINDS: &[&str] = &[
    "struct",
    "class",
    "enum",
    "trait",
    "interface",
    "type",
    "type_alias",
];
const FUNC_KINDS: &[&str] = &["function", "method"];
/// Words of the frame of a question, never of its subject (by prefix).
const FRAME: &[&str] = &[
    "trac", "comm", "line", "becom", "valu", "from", "thro", "main", "what", "whic", "wher",
    "does", "argu", "happ", "code", "file", "data", "into", "goes", "flow", "work", "step",
];
/// Words that never name a symbol of the question.
const SKIPPED_WORDS: &[&str] = &["the", "how", "and", "trace", "from"];
/// Lines of a callee's body shown at most.
const BODY_CAP: i64 = 180;
/// Lines of a far callee's body shown at most.
const FAR_CAP: i64 = 100;
/// A hop this short is shown whole.
const HOP_CAP: i64 = 100;
/// Lines of the constructor shown.
const HEAD_CAP: i64 = 230;
/// Ranked callees kept per level: half of this.
const MAX_RANKED: usize = 4;
/// Bytes of source lines shown at most.
const BYTES: usize = 26_000;
/// Bytes of a file that is read; a longer one is not quoted.
const MAX_FILE_BYTES: u64 = 300_000;
/// Offset past which a symbol's call edges are not paged.
const MAX_EDGE_OFFSET: usize = 100;
/// Constructors of the target tried.
const MAX_CONSTRUCTORS: usize = 10;

static WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("the word pattern is a literal")
});
static ENTRY_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:from|starting (?:at|from)|at|in)\s+`?([A-Za-z_]\w*)")
        .expect("the entry pattern is a literal")
});
static SHOWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^### (\S+?):\d+-\d+").expect("the header pattern is a literal")
});
static BRIEF_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^### (.+)$").expect("the header pattern is a literal"));
static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+)*(?:fn|def|function|func)\s+([A-Za-z_]\w*)",
    )
    .expect("the declaration pattern is a literal")
});
static RUST_FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+([A-Za-z_]\w*)")
        .expect("the fn pattern is a literal")
});

fn run(ctx: &Ctx) -> String {
    chain(ctx).unwrap_or_default()
}

/// Symbols by the word that names them, in the order the words came.
type Named = Vec<(String, Vec<Sym>)>;

fn named_push(map: &mut Named, word: &str, sym: Sym) {
    match map.iter_mut().find(|(key, _)| key == word) {
        Some((_, syms)) => syms.push(sym),
        None => map.push((word.to_owned(), vec![sym])),
    }
}

fn chain(ctx: &Ctx) -> Option<String> {
    let mut words: Vec<&str> = Vec::new();
    for found in WORD.find_iter(ctx.prompt) {
        if !words.contains(&found.as_str()) {
            words.push(found.as_str());
        }
    }
    let shown: HashSet<&str> = SHOWN
        .captures_iter(ctx.brief)
        .filter_map(|caps| caps.get(1))
        .map(|path| path.as_str())
        .collect();
    let (mut funcs, mut types): (Named, Named) = (Vec::new(), Vec::new());
    for word in words {
        if word.chars().count() < 3 || SKIPPED_WORDS.contains(&word.to_lowercase().as_str()) {
            continue;
        }
        if ctx.timed_out() {
            return None;
        }
        for sym in ctx.symbols_named(word) {
            if sym.name != word {
                continue;
            }
            if FUNC_KINDS.contains(&sym.kind.as_str()) {
                named_push(&mut funcs, word, sym);
            } else if TYPE_KINDS.contains(&sym.kind.as_str()) {
                named_push(&mut types, word, sym);
            }
        }
    }
    let ((first_word, _), (type_word, _)) = (funcs.first()?, types.first()?);
    let entry_word = ENTRY_WORD
        .captures(ctx.prompt)
        .and_then(|caps| caps.get(1))
        .map(|word| word.as_str())
        .filter(|word| funcs.iter().any(|(key, _)| key == word))
        .unwrap_or(first_word.as_str());
    let entry = funcs
        .iter()
        .find(|(key, _)| key == entry_word)?
        .1
        .iter()
        .min_by_key(|sym| {
            (
                !shown.contains(sym.path.as_str()),
                -(sym.end_line - sym.start_line),
                sym.path.as_str(),
            )
        })?
        .clone();
    let target = types
        .iter()
        .find(|(key, _)| key == type_word)?
        .1
        .iter()
        .min_by_key(|sym| (!shown.contains(sym.path.as_str()), sym.path.as_str()))?
        .clone();
    let best = shortest_chain(ctx, &entry, &target)?;

    let terms: BTreeSet<String> = LETTERS
        .find_iter(ctx.prompt)
        .map(|word| word.as_str())
        .filter(|word| {
            let lower = word.to_lowercase();
            !FRAME.iter().any(|frame| lower.starts_with(frame))
        })
        .map(stem)
        .collect();
    let mut walk = Walk {
        ctx,
        target: &target,
        terms,
        uids: best.iter().map(|hop| hop.uid.clone()).collect(),
        files: HashMap::new(),
        edges: HashMap::new(),
        symbols: HashMap::new(),
        out: Vec::new(),
        emitted: Vec::new(),
        used: 0,
    };
    let chain_line = best
        .iter()
        .map(|hop| format!("{} ({}:{})", hop.name, hop.path, hop.line))
        .collect::<Vec<_>>()
        .join(" -> ");
    walk.out.push("### call trace, entry to target".to_owned());
    walk.out.push(format!("chain {chain_line}"));
    walk.show_chain(&best, &entry);
    if ctx.timed_out() {
        return None;
    }
    walk.show_ranked(&best);
    if walk.out.len() <= 2 {
        return None;
    }
    let ours: HashSet<&str> = walk
        .out
        .iter()
        .filter_map(|line| line.strip_prefix("### "))
        .collect();
    let own: Vec<String> = BRIEF_HEADER
        .captures_iter(ctx.brief)
        .filter_map(|caps| caps.get(1))
        .map(|header| header.as_str())
        .filter(|header| !ours.contains(header))
        .map(escape)
        .collect();
    let mut text = String::new();
    if !own.is_empty() {
        let _ = writeln!(text, "drop: ^(?:{})$", own.join("|"));
    }
    for line in &walk.out {
        text.push_str(line);
        text.push('\n');
    }
    Some(text)
}

/// The shortest chain from `entry` to a constructor of `target`: a function of
/// the target's file whose signature returns it (or `Self`).
fn shortest_chain(ctx: &Ctx, entry: &Sym, target: &Sym) -> Option<Vec<Hop>> {
    let lines = lines_of(ctx, &target.path, MAX_FILE_BYTES);
    let returns = Regex::new(&format!(r"\b({}|Self)\b", regex::escape(&target.name))).ok()?;
    let mut constructors: Vec<&str> = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(name) = DECLARATION
            .captures(line)
            .and_then(|caps| caps.get(1))
            .map(|name| name.as_str())
        else {
            continue;
        };
        let signature = lines[at..lines.len().min(at + 4)].join(" ");
        let signature = signature.split('{').next().unwrap_or_default();
        let returned = signature.split_once("->").map_or("", |(_, ret)| ret);
        if returns.is_match(returned) && !constructors.contains(&name) {
            constructors.push(name);
        }
    }
    let mut best: Option<Vec<Hop>> = None;
    for name in constructors.into_iter().take(MAX_CONSTRUCTORS) {
        for sym in ctx.symbols_named(name) {
            let qualified = sym.qualified.starts_with(&format!("{}::", target.name));
            if sym.path != target.path || !(qualified || sym.kind == "function") {
                continue;
            }
            let hops = ctx.call_path(&entry.uid, &sym.uid).unwrap_or_default();
            if hops.len() >= 2 && best.as_ref().is_none_or(|best| hops.len() < best.len()) {
                best = Some(hops);
            }
        }
        if ctx.timed_out() {
            return None;
        }
    }
    best
}

/// The lines the step prints, and what it needs to choose them.
struct Walk<'a, 'c> {
    ctx: &'a Ctx<'c>,
    target: &'a Sym,
    /// Stems of the question's words that name something.
    terms: BTreeSet<String>,
    /// The chain's own symbols.
    uids: HashSet<String>,
    files: HashMap<String, Vec<String>>,
    edges: HashMap<String, Vec<Edge>>,
    symbols: HashMap<String, Vec<Sym>>,
    out: Vec<String>,
    /// `(path, first, last)` of every range already printed.
    emitted: Vec<(String, i64, i64)>,
    used: usize,
}

impl Walk<'_, '_> {
    /// The call edges out of `uid`, asked once.
    fn callees(&mut self, uid: &str) -> Vec<Edge> {
        let ctx = self.ctx;
        self.edges
            .entry(uid.to_owned())
            .or_insert_with(|| ctx.call_edges(uid, Role::Callees, MAX_EDGE_OFFSET))
            .clone()
    }

    fn symbols_named(&mut self, name: &str) -> Vec<Sym> {
        let ctx = self.ctx;
        self.symbols
            .entry(name.to_owned())
            .or_insert_with(|| ctx.symbols_named(name))
            .clone()
    }

    /// Prints lines `a..=b` of `path` under a header unless a printed range
    /// already holds them or the byte cap is reached; whether it did.
    fn emit(&mut self, path: &str, a: i64, b: i64, label: &str) -> bool {
        let ctx = self.ctx;
        let lines = self
            .files
            .entry(path.to_owned())
            .or_insert_with(|| lines_of(ctx, path, MAX_FILE_BYTES));
        let a = a.max(1);
        let b = b.min(i64::try_from(lines.len()).unwrap_or(i64::MAX));
        if a > b
            || self
                .emitted
                .iter()
                .any(|(p, x, y)| p == path && *x <= a && b <= *y)
        {
            return false;
        }
        let row = |n: i64| {
            usize::try_from(n - 1)
                .ok()
                .and_then(|at| lines.get(at))
                .map_or("", String::as_str)
        };
        let rows: Vec<i64> = (a..=b).filter(|n| !row(*n).trim().is_empty()).collect();
        let size: usize = rows
            .iter()
            .map(|n| head(row(*n), 200).chars().count() + 8)
            .sum();
        if self.used + size > BYTES {
            return false;
        }
        self.used += size;
        self.emitted.push((path.to_owned(), a, b));
        self.out.push(format!("### {path}:{a}-{b} — {label}"));
        self.out
            .extend(rows.iter().map(|n| format!("{n}| {}", head(row(*n), 200))));
        true
    }

    /// Each hop at its call site, the constructor at its head, and the other
    /// callees of the entry that take the target.
    fn show_chain(&mut self, best: &[Hop], entry: &Sym) {
        let target_name = self.target.name.clone();
        for (at, hop) in best.iter().enumerate() {
            let Some(sym) = self.find_hop(hop) else {
                continue;
            };
            let Some(next) = best.get(at + 1) else {
                let label = format!("{} (constructor of {target_name}), head", sym.name);
                self.emit(&sym.path, sym.start_line, sym.start_line + HEAD_CAP - 1, &label);
                continue;
            };
            let site = self
                .callees(&hop.uid)
                .iter()
                .find(|edge| edge.symbol.uid == next.uid)
                .and_then(|edge| edge.site_line)
                .filter(|site| *site != 0)
                .unwrap_or(sym.start_line);
            let size = sym.end_line - sym.start_line + 1;
            let calls = format!("{} calls {}", sym.name, next.name);
            if size <= HOP_CAP {
                self.emit(&sym.path, sym.start_line, sym.end_line, &calls);
                continue;
            }
            if site - 2 > sym.start_line {
                let label = format!("{} (declaration)", sym.name);
                self.emit(&sym.path, sym.start_line, sym.start_line, &label);
            }
            self.emit(&sym.path, (site - 2).max(sym.start_line), site + 4, &calls);
        }
        let takes = Regex::new(&format!(r"\b{}\b", regex::escape(&target_name))).ok();
        for edge in self.callees(&entry.uid) {
            let callee = edge.symbol;
            if self.uids.contains(&callee.uid)
                || !FUNC_KINDS.contains(&callee.kind.as_str())
                || !takes.as_ref().is_some_and(|re| re.is_match(&callee.sig))
            {
                continue;
            }
            let label = format!("{} receives the {target_name}", callee.name);
            self.emit(&callee.path, callee.start_line, callee.start_line + 9, &label);
        }
    }

    /// The graph's row of a hop of the chain.
    fn find_hop(&mut self, hop: &Hop) -> Option<Sym> {
        let name = hop.uid.split('#').nth(1)?.rsplit("::").next()?;
        self.symbols_named(name)
            .into_iter()
            .find(|sym| sym.uid == hop.uid)
    }

    /// The callees of `uid` whose names carry the question's words, best two.
    fn ranked(&mut self, uid: &str) -> Vec<Sym> {
        let mut found: Vec<(usize, Sym)> = Vec::new();
        for edge in self.callees(uid) {
            let callee = edge.symbol;
            if self.uids.contains(&callee.uid)
                || !FUNC_KINDS.contains(&callee.kind.as_str())
                || callee.path.to_lowercase().contains("test")
            {
                continue;
            }
            let hay = callee.qualified_or_name().to_lowercase();
            let score = self
                .terms
                .iter()
                .filter(|term| hay.contains(term.as_str()))
                .count();
            if score > 0 && callee.end_line - callee.start_line >= 2 {
                found.push((score, callee));
            }
        }
        found.sort_by(|(a, x), (b, y)| {
            b.cmp(a)
                .then_with(|| x.path.cmp(&y.path))
                .then_with(|| x.start_line.cmp(&y.start_line))
        });
        let mut seen = HashSet::new();
        found
            .into_iter()
            .map(|(_, callee)| callee)
            .filter(|callee| seen.insert(callee.uid.clone()))
            .take(MAX_RANKED / 2)
            .collect()
    }

    fn body(&mut self, callee: &Sym, why: &str, cap: i64) -> bool {
        let last = callee.end_line.min(callee.start_line + cap - 1);
        self.emit(&callee.path, callee.start_line, last, why)
    }

    /// The function of `callee`'s impl whose name carries the question's
    /// words, nearest first: at most one.
    fn siblings(&mut self, callee: &Sym) -> Vec<Sym> {
        let qualified = callee.qualified_or_name();
        let Some((owner, _)) = qualified.rsplit_once("::") else {
            return Vec::new();
        };
        let prefix = format!("{owner}::");
        let ctx = self.ctx;
        let lines = self
            .files
            .entry(callee.path.clone())
            .or_insert_with(|| lines_of(ctx, &callee.path, MAX_FILE_BYTES))
            .clone();
        let names: BTreeSet<&str> = lines
            .iter()
            .filter_map(|line| RUST_FN.captures(line))
            .filter_map(|caps| caps.get(1))
            .map(|name| name.as_str())
            .filter(|name| {
                let lower = name.to_lowercase();
                self.terms.iter().any(|term| lower.contains(term.as_str()))
            })
            .filter(|name| *name != callee.name)
            .collect();
        let mut found = Vec::new();
        for name in names.into_iter().take(6) {
            for sym in self.symbols_named(name) {
                if sym.path == callee.path
                    && sym.qualified.starts_with(&prefix)
                    && !self.uids.contains(&sym.uid)
                {
                    found.push(sym);
                }
            }
        }
        found.sort_by_key(|sym| sym.start_line);
        found.truncate(1);
        found
    }

    /// For every hop but the constructor: the callees named by the question,
    /// the sibling beside each, their own callees named by the question, and
    /// the two largest callees from other files.
    fn show_ranked(&mut self, best: &[Hop]) {
        for hop in best.iter().take(best.len().saturating_sub(1)) {
            for callee in self.ranked(&hop.uid) {
                if self.ctx.timed_out() {
                    return;
                }
                let why = format!("{} (called by {}, named by the question)", callee.name, hop.name);
                if !self.body(&callee, &why, BODY_CAP) {
                    continue;
                }
                for beside in self.siblings(&callee) {
                    let why = format!("{} (beside {}, named by the question)", beside.name, callee.name);
                    self.body(&beside, &why, BODY_CAP);
                }
                for deeper in self.ranked(&callee.uid) {
                    let why = format!(
                        "{} (called by {}, named by the question)",
                        deeper.name, callee.name
                    );
                    if !self.body(&deeper, &why, BODY_CAP) {
                        continue;
                    }
                    for beside in self.siblings(&deeper) {
                        let why = format!(
                            "{} (beside {}, named by the question)",
                            beside.name, deeper.name
                        );
                        self.body(&beside, &why, BODY_CAP);
                    }
                }
                let mut far: HashMap<String, Sym> = HashMap::new();
                for edge in self.callees(&callee.uid) {
                    let other = edge.symbol;
                    if other.path != callee.path
                        && FUNC_KINDS.contains(&other.kind.as_str())
                        && other.end_line - other.start_line >= 8
                        && !other.path.to_lowercase().contains("test")
                    {
                        far.insert(other.uid.clone(), other);
                    }
                }
                let mut far: Vec<Sym> = far.into_values().collect();
                far.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));
                for other in far.into_iter().take(2) {
                    let why = format!("{} (called by {})", other.name, callee.name);
                    self.body(&other, &why, FAR_CAP);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
