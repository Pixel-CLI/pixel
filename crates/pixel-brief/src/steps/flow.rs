// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The flow steps (`specs/brief.md`, "Recipes"): a walkthrough question is
//! answered by the calls that carry its words, not by a depth-first prefix of
//! the entry's calls.
//!
//! * **`cand-j-chain`**: "trace how A turns into a T, from an entry through …"
//!   is answered by the call chain between two symbols the question names. The
//!   entry is the function-shaped word after "from" (else the first word the
//!   graph resolves to a function); the target is the type-shaped word that
//!   resolves to a type, and its constructors are the functions of its file
//!   whose signature returns it. The shortest call path from the entry to a
//!   constructor is shown hop by hop (the call site, or the whole body when
//!   short), then the other callees of the entry that take the target, then, at
//!   each hop, the callees whose names carry the question's words, two levels
//!   deep.
//! * **`flow-rank`**: from an entry the question names in code form
//!   (`client.fetch`, `handle_event`), every callee is ranked by the question's
//!   words (generic words weigh little, tests are left out), the best six are
//!   followed one level at a time down to depth 3, and the entry and each
//!   ranked body are shown.
//!
//! Both replace the pack's own call-trace blocks. Read-only and deterministic.
//! Not ported: `flow-rank`'s lanes for keyboard shortcuts, request routers and
//! event delivery, which read particular frameworks, and the sibling-method
//! rule of `cand-j-chain`, which needed the qualified names the graph reads
//! here do not carry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::chain::{PathHop, SymbolHit};
use crate::kinds::FlowNode;
use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Drop, Outcome, Step};

const FUNC_KINDS: &[&str] = &["function", "method"];
const TYPE_KINDS: &[&str] = &[
    "struct",
    "class",
    "enum",
    "trait",
    "interface",
    "type",
    "type_alias",
];
const FRAME: &[&str] = &[
    "trac", "comm", "line", "becom", "valu", "from", "thro", "main", "what", "whic", "wher",
    "does", "argu", "happ", "code", "file", "data", "into", "goes", "flow", "work", "step",
];
const BODY_CAP: usize = 180;
const FAR_CAP: usize = 100;
const HOP_CAP: usize = 100;
const HEAD_CAP: usize = 230;
const MAX_RANKED: usize = 4;
const BYTES: usize = 26_000;
const LINE_CHARS: usize = 200;
const CALLEES: usize = 64;
const MAX_CTORS: usize = 10;

const ENTRY_LINES: usize = 160;
const RANK_BODY_LINES: usize = 130;
const RANK_MAX: usize = 12;
const MIN_SCORE: f64 = 1.0;
const GENERIC: &[&str] = &[
    "element", "elements", "scene", "file", "files", "data", "state", "value", "type",
];
const NOISE: &[&str] = &[
    "walk", "through", "happens", "happen", "someone", "until", "from", "shows", "when", "onto",
    "that", "file", "with", "what", "into",
];

static ASKS_CHAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(trace|call (chain|path)|end to end|walk (me )?through|from \w+ (through|to|into))\b")
        .expect("the chain question pattern is a literal")
});
static ENTRY_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:from|starting (?:at|from)|at|in)\s+`?([A-Za-z_]\w*)")
        .expect("the entry pattern is a literal")
});
static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("the identifier pattern is a literal")
});
static LONG_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{4,}").expect("the word pattern is a literal"));
static DECL_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+)*(?:fn|def|function|func)\s+([A-Za-z_]\w*)")
        .expect("the declaration pattern is a literal")
});
static BACKTICKED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`([A-Za-z_][\w.]*)`").expect("the backticked pattern is a literal")
});
static DOTTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+\b").expect("the dotted pattern is a literal")
});
static CODE_SHAPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b|\b[a-z]+[A-Z]\w*\b")
        .expect("the code-shaped pattern is a literal")
});

fn stem_chain(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in ["ing", "ed", "es", "s"] {
        if word.ends_with(suffix) && word.chars().count() - suffix.len() >= 4 {
            return word[..word.len() - suffix.len()].to_string();
        }
    }
    word
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if ctx.wants(
        Step::CandJChain,
        &[
            QuestionKind::Overview,
            QuestionKind::Review,
            QuestionKind::History,
        ],
    ) && ASKS_CHAIN.is_match(ctx.typed)
    {
        let _timer = ctx.timer(Step::CandJChain);
        if chain(ctx, out) {
            return;
        }
    }
    if ctx.kind == QuestionKind::Flow && ctx.steps.enabled(Step::FlowRank) {
        let _timer = ctx.timer(Step::FlowRank);
        flow_rank(ctx, out);
    }
}

/// The calls out of `uid`, one level.
fn callees(ctx: &Ctx, uid: &str) -> Vec<FlowNode> {
    ctx.evidence
        .flow_tree(uid, 1, CALLEES, ctx.deadline)
        .map(|nodes| nodes.into_iter().filter(|node| node.depth == 1).collect())
        .unwrap_or_default()
}

/// Lines of a file, each a `String`.
fn lines_of(ctx: &Ctx, path: &str) -> Vec<String> {
    ctx.read(path)
        .map(|text| text.lines().map(ToString::to_string).collect())
        .unwrap_or_default()
}

/// Emits numbered source ranges within a byte budget and never twice.
struct Emitter<'a> {
    ctx: &'a Ctx<'a>,
    emitted: Vec<(String, usize, usize)>,
    blocks: Vec<Block>,
    used: usize,
}

impl Emitter<'_> {
    fn emit(&mut self, path: &str, a: usize, b: usize, label: &str) -> bool {
        let lines = lines_of(self.ctx, path);
        let (a, b) = (a.max(1), b.min(lines.len()));
        if a > b
            || self
                .emitted
                .iter()
                .any(|(p, x, y)| p == path && *x <= a && b <= *y)
        {
            return false;
        }
        let rows: Vec<usize> = (a..=b)
            .filter(|n| !lines[n - 1].trim().is_empty())
            .collect();
        let size: usize = rows
            .iter()
            .map(|n| lines[n - 1].chars().take(LINE_CHARS).count() + 8)
            .sum();
        if self.used + size > BYTES {
            return false;
        }
        self.used += size;
        self.emitted.push((path.to_string(), a, b));
        let body = rows
            .iter()
            .map(|n| {
                format!(
                    "{n}| {}",
                    lines[n - 1].chars().take(LINE_CHARS).collect::<String>()
                )
            })
            .collect();
        self.blocks
            .push(Block::new(format!("{path}:{a}-{b} — {label}"), body));
        true
    }

    fn body(&mut self, symbol: &FlowNode, why: &str, cap: usize) -> bool {
        let first = usize::try_from(symbol.start).unwrap_or(0);
        let last = usize::try_from(symbol.end)
            .unwrap_or(0)
            .min(first + cap - 1);
        self.emit(&symbol.path, first, last, why)
    }
}

fn is_test_path(path: &str) -> bool {
    path.to_lowercase().contains("test")
}

/// A word of the prompt and the symbols of the graph it names.
type Named = Vec<(String, Vec<SymbolHit>)>;

/// The function-shaped and type-shaped symbols the prompt's words name, in
/// the order the prompt names them.
fn named_symbols(ctx: &Ctx) -> (Named, Named) {
    let (mut funcs, mut types): (Named, Named) = (Vec::new(), Vec::new());
    let mut seen = BTreeSet::new();
    for found in IDENT.find_iter(ctx.typed) {
        let word = found.as_str();
        if word.len() < 3
            || ["the", "how", "and", "trace", "from"].contains(&word.to_lowercase().as_str())
            || !seen.insert(word.to_string())
        {
            continue;
        }
        let Ok(hits) = ctx.evidence.symbols(word, ctx.deadline) else {
            continue;
        };
        for hit in hits.into_iter().filter(|hit| hit.name == word) {
            let table = if FUNC_KINDS.contains(&hit.kind.as_str()) {
                &mut funcs
            } else if TYPE_KINDS.contains(&hit.kind.as_str()) {
                &mut types
            } else {
                continue;
            };
            match table.iter_mut().find(|(name, _)| name == word) {
                Some((_, held)) => held.push(hit),
                None => table.push((word.to_string(), vec![hit])),
            }
        }
    }
    (funcs, types)
}

fn node_of(hit: &SymbolHit) -> FlowNode {
    FlowNode {
        depth: 0,
        parent: None,
        uid: hit.uid.clone(),
        name: hit.name.clone(),
        kind: hit.kind.clone(),
        path: hit.path.clone(),
        start: hit.start_line,
        end: hit.end_line,
    }
}

/// The call chain from the entry the question names to a constructor of the
/// type it names. `true` when it produced blocks.
fn chain(ctx: &Ctx, out: &mut Outcome) -> bool {
    let (funcs, types) = named_symbols(ctx);
    if funcs.is_empty() || types.is_empty() {
        return false;
    }
    let entry_word = ENTRY_AFTER
        .captures(ctx.typed)
        .map(|caps| caps[1].to_string())
        .filter(|word| funcs.iter().any(|(name, _)| name == word))
        .unwrap_or_else(|| funcs[0].0.clone());
    let shown = |path: &str| ctx.near.iter().any(|near| near == path);
    let mut entries = funcs
        .iter()
        .find(|(name, _)| *name == entry_word)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    entries.sort_by(|a, b| {
        (!shown(&a.path))
            .cmp(&!shown(&b.path))
            .then((b.end_line - b.start_line).cmp(&(a.end_line - a.start_line)))
            .then(a.path.cmp(&b.path))
    });
    let Some(entry) = entries.first().cloned() else {
        return false;
    };
    let mut targets = types[0].1.clone();
    targets.sort_by(|a, b| {
        (!shown(&a.path))
            .cmp(&!shown(&b.path))
            .then(a.path.cmp(&b.path))
    });
    let Some(target) = targets.first().cloned() else {
        return false;
    };

    // The target's constructors: the functions of its file whose signature
    // returns it.
    let tlines = lines_of(ctx, &target.path);
    let returns = Regex::new(&format!(r"\b({}|Self)\b", regex::escape(&target.name))).ok();
    let mut ctor_names: Vec<String> = Vec::new();
    for (i, line) in tlines.iter().enumerate() {
        let Some(decl) = DECL_NAME.captures(line) else {
            continue;
        };
        let sig = tlines[i..tlines.len().min(i + 4)].join(" ");
        let sig = sig.split('{').next().unwrap_or("");
        let ret = sig.split_once("->").map_or("", |(_, ret)| ret);
        if returns.as_ref().is_some_and(|re| re.is_match(ret))
            && !ctor_names.contains(&decl[1].to_string())
        {
            ctor_names.push(decl[1].to_string());
        }
    }
    let mut best: Option<Vec<PathHop>> = None;
    for name in ctor_names.iter().take(MAX_CTORS) {
        let Ok(hits) = ctx.evidence.symbols(name, ctx.deadline) else {
            continue;
        };
        for hit in hits
            .into_iter()
            .filter(|hit| hit.path == target.path && FUNC_KINDS.contains(&hit.kind.as_str()))
        {
            if let Ok(Some(path)) = ctx.evidence.call_path(&entry.uid, &hit.uid, ctx.deadline)
                && path.len() >= 2
                && best.as_ref().is_none_or(|held| path.len() < held.len())
            {
                best = Some(path);
            }
        }
    }
    let Some(best) = best else {
        return false;
    };

    let mut emitter = Emitter {
        ctx,
        emitted: Vec::new(),
        blocks: Vec::new(),
        used: 0,
    };
    let mut chain_line = String::new();
    for hop in &best {
        if !chain_line.is_empty() {
            chain_line.push_str(if hop.likely { " ~> " } else { " -> " });
        }
        chain_line.push_str(&format!(
            "{} ({}:{})",
            hop.symbol.name, hop.symbol.path, hop.symbol.start_line
        ));
    }
    let mut chain = vec![format!("chain {chain_line}")];
    if best.iter().any(|hop| hop.likely) {
        chain.push("(~> is a call matched by name, not proved)".to_string());
    }
    let terms: BTreeSet<String> = LONG_WORD
        .find_iter(ctx.typed)
        .map(|found| found.as_str())
        .filter(|word| {
            !FRAME
                .iter()
                .any(|frame| word.to_lowercase().starts_with(frame))
        })
        .map(stem_chain)
        .collect();
    let uids: BTreeSet<&str> = best.iter().map(|hop| hop.symbol.uid.as_str()).collect();

    // Each hop at its call site, or whole when short.
    for (i, hop) in best.iter().enumerate() {
        let symbol = &hop.symbol;
        let (start, end) = (symbol.start_line as usize, symbol.end_line as usize);
        match best.get(i + 1) {
            None => {
                emitter.emit(
                    &symbol.path,
                    start,
                    start + HEAD_CAP - 1,
                    &format!("{} (constructor of {}), head", symbol.name, target.name),
                );
            }
            Some(next) if end - start < HOP_CAP => {
                emitter.emit(
                    &symbol.path,
                    start,
                    end,
                    &format!("{} calls {}", symbol.name, next.symbol.name),
                );
            }
            Some(next) => {
                let site = if next.call_line == 0 {
                    start
                } else {
                    next.call_line as usize
                };
                if site.saturating_sub(2) > start {
                    emitter.emit(
                        &symbol.path,
                        start,
                        start,
                        &format!("{} (declaration)", symbol.name),
                    );
                }
                emitter.emit(
                    &symbol.path,
                    start.max(site.saturating_sub(2)),
                    site + 4,
                    &format!("{} calls {}", symbol.name, next.symbol.name),
                );
            }
        }
    }
    // The other callees of the entry that take the target.
    if let Ok(target_name) = Regex::new(&format!(r"\b{}\b", regex::escape(&target.name))) {
        for callee in callees(ctx, &entry.uid) {
            if uids.contains(callee.uid.as_str()) || !FUNC_KINDS.contains(&callee.kind.as_str()) {
                continue;
            }
            let lines = lines_of(ctx, &callee.path);
            let signature = lines.get(usize::try_from(callee.start).unwrap_or(1).saturating_sub(1));
            if signature.is_some_and(|line| target_name.is_match(line)) {
                let first = callee.start as usize;
                emitter.emit(
                    &callee.path,
                    first,
                    first + 9,
                    &format!("{} receives the {}", callee.name, target.name),
                );
            }
        }
    }
    // At each hop, the callees whose names carry the question's words.
    for hop in &best[..best.len() - 1] {
        for c in ranked(ctx, &hop.symbol.uid, &terms, &uids) {
            if !emitter.body(
                &c,
                &format!(
                    "{} (called by {}, named by the question)",
                    c.name, hop.symbol.name
                ),
                BODY_CAP,
            ) {
                continue;
            }
            for d in ranked(ctx, &c.uid, &terms, &uids) {
                emitter.body(
                    &d,
                    &format!("{} (called by {}, named by the question)", d.name, c.name),
                    BODY_CAP,
                );
            }
            let mut far: BTreeMap<String, FlowNode> = BTreeMap::new();
            for e in callees(ctx, &c.uid) {
                if e.path != c.path
                    && FUNC_KINDS.contains(&e.kind.as_str())
                    && e.end.saturating_sub(e.start) >= 8
                    && !is_test_path(&e.path)
                {
                    far.entry(e.uid.clone()).or_insert(e);
                }
            }
            let mut far: Vec<FlowNode> = far.into_values().collect();
            far.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
            for d in far.into_iter().take(2) {
                emitter.body(&d, &format!("{} (called by {})", d.name, c.name), FAR_CAP);
            }
        }
    }
    let mut blocks = vec![Block::new("call trace, entry to target".to_string(), chain)];
    blocks.extend(emitter.blocks);
    if blocks.len() <= 2 {
        return false;
    }
    out.drops.push(Drop::CallTrace);
    out.rescuing = true;
    out.lead.extend(blocks);
    true
}

/// The callees of `uid` whose names carry the question's words, best first.
fn ranked(ctx: &Ctx, uid: &str, terms: &BTreeSet<String>, skip: &BTreeSet<&str>) -> Vec<FlowNode> {
    let mut found: Vec<(i64, String, u64, FlowNode)> = Vec::new();
    for c in callees(ctx, uid) {
        if skip.contains(c.uid.as_str())
            || !FUNC_KINDS.contains(&c.kind.as_str())
            || is_test_path(&c.path)
        {
            continue;
        }
        let hay = c.name.to_lowercase();
        let score = terms
            .iter()
            .filter(|term| hay.contains(term.as_str()))
            .count() as i64;
        if score > 0 && c.end.saturating_sub(c.start) >= 2 {
            found.push((-score, c.path.clone(), c.start, c));
        }
    }
    found.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    let mut seen = BTreeSet::new();
    found
        .into_iter()
        .filter(|(.., c)| seen.insert(c.uid.clone()))
        .map(|(.., c)| c)
        .take(MAX_RANKED / 2)
        .collect()
}

/// The entry a walkthrough names in code form: the first such name the graph
/// defines, as `(name, symbol)`.
fn named_entry(ctx: &Ctx) -> Option<(String, SymbolHit)> {
    let mut names: Vec<String> = BACKTICKED
        .captures_iter(ctx.typed)
        .map(|caps| caps[1].to_string())
        .collect();
    names.extend(
        DOTTED
            .find_iter(ctx.typed)
            .map(|found| found.as_str().to_string()),
    );
    names.extend(
        CODE_SHAPED
            .find_iter(ctx.typed)
            .map(|found| found.as_str().to_string()),
    );
    let mut seen = BTreeSet::new();
    for full in names.into_iter().filter(|name| seen.insert(name.clone())) {
        let parts: Vec<&str> = full.split('.').collect();
        let last = parts[parts.len() - 1];
        let Ok(hits) = ctx.evidence.symbols(last, ctx.deadline) else {
            continue;
        };
        let mut rows: Vec<SymbolHit> = hits
            .into_iter()
            .filter(|hit| hit.name.rsplit('.').next() == Some(last) && !is_test_path(&hit.path))
            .collect();
        if rows.is_empty() {
            continue;
        }
        let leading = &parts[..parts.len() - 1];
        rows.sort_by(|a, b| {
            let held = |hit: &SymbolHit| {
                leading
                    .iter()
                    .filter(|part| hit.path.to_lowercase().contains(&part.to_lowercase()))
                    .count()
            };
            held(b)
                .cmp(&held(a))
                .then((a.kind != "function").cmp(&(b.kind != "function")))
                .then(a.path.len().cmp(&b.path.len()))
                .then(a.path.cmp(&b.path))
        });
        return Some((last.to_string(), rows.remove(0)));
    }
    None
}

/// The ranked walkthrough of a named entry.
fn flow_rank(ctx: &Ctx, out: &mut Outcome) {
    let Some((name, entry)) = named_entry(ctx) else {
        return;
    };
    let lower = ctx.typed.to_lowercase();
    let mut seen = BTreeSet::new();
    let terms: Vec<String> = LONG_WORD
        .find_iter(&lower)
        .map(|found| found.as_str().to_string())
        .filter(|word| seen.insert(word.clone()) && !NOISE.contains(&word.as_str()))
        .collect();
    // uid -> (callee, depth)
    let mut pool: BTreeMap<String, (FlowNode, usize)> = BTreeMap::new();
    let weight = |term: &str| if GENERIC.contains(&term) { 0.35 } else { 1.2 };
    let scored = |pool: &BTreeMap<String, (FlowNode, usize)>| -> Vec<(f64, String, usize)> {
        let mut rows: Vec<(f64, String, usize)> = pool
            .iter()
            .map(|(uid, (symbol, depth))| {
                let (n, p) = (symbol.name.to_lowercase(), symbol.path.to_lowercase());
                let mut score: f64 = terms
                    .iter()
                    .filter(|t| n.contains(t.as_str()))
                    .map(|t| weight(t))
                    .sum::<f64>()
                    + terms
                        .iter()
                        .filter(|t| p.contains(t.as_str()))
                        .map(|t| 0.3 * weight(t))
                        .sum::<f64>();
                if p.contains("test") || p.contains("polyfill") {
                    score -= 9.0;
                }
                (score - 0.25 * (*depth as f64 - 1.0), uid.clone(), *depth)
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        rows
    };
    let mut frontier = vec![entry.uid.clone()];
    let mut kept: BTreeSet<String> = BTreeSet::new();
    for depth in 1..=3 {
        for uid in frontier.iter().take(6) {
            for callee in callees(ctx, uid) {
                if callee.uid != entry.uid && !pool.contains_key(&callee.uid) {
                    pool.insert(callee.uid.clone(), (callee, depth));
                }
            }
        }
        let rows = scored(&pool);
        let level: Vec<&(f64, String, usize)> = rows
            .iter()
            .filter(|row| row.2 == depth && row.0 > -1.0)
            .collect();
        let passing: Vec<&&(f64, String, usize)> =
            level.iter().filter(|row| row.0 > MIN_SCORE).collect();
        let chosen: Vec<String> = if passing.len() >= 2 {
            passing.iter().map(|row| row.1.clone()).collect()
        } else {
            level.iter().take(3).map(|row| row.1.clone()).collect()
        };
        kept.extend(chosen.iter().cloned());
        frontier = chosen;
    }
    let ranked: Vec<(f64, String, usize)> = scored(&pool)
        .into_iter()
        .filter(|row| row.0 > MIN_SCORE || kept.contains(&row.1))
        .take(RANK_MAX)
        .collect();
    if ranked.is_empty() {
        return;
    }
    let mut trace = vec![format!(
        "entry {name} — {}:{}-{}",
        entry.path, entry.start_line, entry.end_line
    )];
    for (_, uid, depth) in &ranked {
        let symbol = &pool[uid].0;
        trace.push(format!(
            "{}calls {} — {}:{}-{}",
            "  ".repeat(*depth),
            symbol.name,
            symbol.path,
            symbol.start,
            symbol.end
        ));
    }
    let mut blocks = vec![Block::new(
        "call trace, ranked by the question".to_string(),
        trace,
    )];
    let mut emitter = Emitter {
        ctx,
        emitted: Vec::new(),
        blocks: Vec::new(),
        used: 0,
    };
    emit_definition(&mut emitter, &node_of(&entry), ENTRY_LINES, &name, true);
    for (_, uid, _) in &ranked {
        let symbol = &pool[uid].0;
        emit_definition(&mut emitter, symbol, RANK_BODY_LINES, &symbol.name, false);
    }
    blocks.extend(emitter.blocks);
    out.drops.push(Drop::CallTrace);
    out.rescuing = true;
    out.lead.extend(blocks);
}

/// A symbol's definition, numbered, at most `cap` lines, and a note when it
/// continues.
fn emit_definition(emitter: &mut Emitter, symbol: &FlowNode, cap: usize, name: &str, entry: bool) {
    let lines = lines_of(emitter.ctx, &symbol.path);
    let first = usize::try_from(symbol.start).unwrap_or(1).max(1);
    let hi = usize::try_from(symbol.end)
        .unwrap_or(first)
        .min(lines.len());
    if first > hi {
        return;
    }
    let last = hi.min(first + cap - 1);
    let mut body: Vec<String> = (first..=last)
        .map(|n| {
            format!(
                "{n}| {}",
                lines[n - 1].chars().take(LINE_CHARS).collect::<String>()
            )
        })
        .collect();
    if last < hi {
        body.push(format!("… {name} continues past line {last}"));
    }
    let kind = if entry {
        "method".to_string()
    } else {
        symbol.kind.clone()
    };
    emitter.blocks.push(Block::new(
        format!(
            "{}:{first}-{last} — {name} definition ({kind})",
            symbol.path
        ),
        body,
    ));
}

#[cfg(test)]
mod tests;
