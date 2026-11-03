// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-j-callers`: "where is X defined, who calls it, which tests cover
//! it" needs the callers, not the callees.
//!
//! Ported from `probes/cand-j-callers/probe`. X is the identifier-shaped word
//! of the question that the symbol graph resolves to a function. The step
//! shows the callers (their body when short, else the window around the
//! call) and the call sites of those callers, the tests among the callers,
//! the test helpers named after X (`X_raw`), and the tests that set what X
//! reads: the ALLCAPS environment names in its body and doc comment, looked
//! up in the test files. Unrelated test blocks of the brief are dropped.
//! When the graph resolves no name, the source-level fallbacks of
//! [`semantic`] answer instead. Read-only, deterministic, no network.

mod js;
mod semantic;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::graph::{Edge, Role, Sym};
use super::text::{BRACE_NOISE, COMMENT_START, DECL, SRC, TESTPATH, escape, head, lines_of, re};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-j-callers",
    when_kinds: &[],
    skip_kinds: &["overview", "review", "history"],
    when_prompt: Some(
        r"(?i)\b(callers?|who calls|(functions?|methods?) (that )?(calls?|invokes?|uses?)|called (by|from)|call sites?|calls it|which (?:library )?(?:code|modules?|parts?|functions?|methods?) (?:of (?:the )?(?:(?:lib(?:rary)?|modules?|packages?|components?|repos?(?:itory)?|code|classes?|services?|servers?|clients?|apis?|request|response|context)(?: [\w./-]+){0,2}|[\w./-]+\.(?:js|ts|tsx|jsx|py|rs|go)) )?(call|calls|invoke|uses?)|locate\b.{0,80}\bentry points? (?:that |which )?(?:call|calls|invoke|uses?)\b)\b",
    ),
    when_gate_closed: true,
    budget_ms: 2500,
    mode: ProbeMode::Lead,
    run,
};

/// Lines of a caller shown whole.
const BODY_CAP: i64 = 40;
/// Lines around a call in a long caller.
const SITE_CONTEXT: i64 = 8;
/// Callers shown, call sites of each, environment tests.
const MAX_CALLERS: usize = 6;
const MAX_SITES: usize = 3;
const MAX_TESTS: usize = 3;
/// Files read.
const MAX_FILES: usize = 4000;
/// Bytes of one file.
const MAX_FILE_BYTES: u64 = 300_000;
/// Callers and callees are read this far into their pages.
const EDGE_OFFSET: usize = 100;


static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"`([A-Za-z_]\w*)(?:\(\))?`|\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+)\b|\b([a-z]+[A-Z][A-Za-z0-9]*)\b|\b([A-Z][a-z0-9]+(?:[A-Z][a-z0-9]*)+)\b",
    )
});
static SHOWN: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^### (\S+?):\d+-\d+"));
static ENV: LazyLock<Regex> = LazyLock::new(|| re(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b"));
static DOC_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(//|#|/\*|\*|@)"));
static TEST_START: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s*(#\[test\]|def test_|(it|test)\()"));
static TEST_DROP: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^### (\S+?:\d+-\d+) — (.*\(test\).*|.*\(a test of .*)$"));
static TESTS_MATCHING: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^### (tests matching [^\n]*)$"));
static DEFINITION_DROP: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^### (\S+?:\d+-\d+) — (\w+) definition \((?:method|function|variant|struct)\)$")
});

fn run(ctx: &Ctx) -> String {
    Callers::new(ctx).answer().unwrap_or_default()
}

/// The files of one run, read once.
struct Callers<'a, 'b> {
    ctx: &'a Ctx<'b>,
    files: HashMap<String, Rc<Vec<String>>>,
    emitted: Vec<(String, i64, i64)>,
    out: Vec<String>,
}

impl<'a, 'b> Callers<'a, 'b> {
    fn new(ctx: &'a Ctx<'b>) -> Self {
        Self { ctx, files: HashMap::new(), emitted: Vec::new(), out: Vec::new() }
    }

    /// The lines of a tracked file; none when it cannot be read.
    fn read(&mut self, path: &str) -> Rc<Vec<String>> {
        let ctx = self.ctx;
        Rc::clone(
            self.files
                .entry(path.to_owned())
                .or_insert_with(|| Rc::new(lines_of(ctx, path, MAX_FILE_BYTES))),
        )
    }

    /// The function the question names: the first identifier-shaped word that
    /// the graph resolves to a function or method of that name.
    fn resolve(&self) -> Option<Sym> {
        let mut seen: Vec<&str> = Vec::new();
        for found in IDENT.captures_iter(self.ctx.prompt) {
            if let Some(word) = (1..=4).find_map(|k| found.get(k)).map(|m| m.as_str())
                && !seen.contains(&word)
            {
                seen.push(word);
            }
        }
        let shown: BTreeSet<&str> = SHOWN
            .captures_iter(self.ctx.brief)
            .filter_map(|caps| caps.get(1).map(|m| m.as_str()))
            .collect();
        for word in seen {
            let mut rows: Vec<Sym> = self
                .ctx
                .symbols_named(word)
                .into_iter()
                .filter(|sym| ["function", "method"].contains(&sym.kind.as_str()) && sym.name == word)
                .collect();
            if !rows.is_empty() {
                rows.sort_by(|a, b| {
                    (!shown.contains(a.path.as_str()), &a.path, a.start_line)
                        .cmp(&(!shown.contains(b.path.as_str()), &b.path, b.start_line))
                });
                return rows.into_iter().next();
            }
            if self.ctx.timed_out() {
                return None;
            }
        }
        None
    }

    /// `emit` of the script: one block of numbered lines, unless an earlier
    /// block holds the range.
    fn emit(&mut self, path: &str, first: i64, last: i64, label: &str) {
        let lines = self.read(path);
        let len = i64::try_from(lines.len()).unwrap_or(i64::MAX);
        let (first, last) = (first.max(1), last.min(len));
        if first > last
            || self
                .emitted
                .iter()
                .any(|(known, lo, hi)| known == path && *lo <= first && last <= *hi)
        {
            return;
        }
        self.emitted.push((path.to_owned(), first, last));
        let mut block = vec![format!("### {path}:{first}-{last} — {label}")];
        for n in first..=last {
            let line = usize::try_from(n - 1).ok().and_then(|at| lines.get(at)).map_or("", String::as_str);
            block.push(format!("{n}| {}", head(line, 200)));
        }
        self.out.extend(block);
    }

    fn answer(&mut self) -> Option<String> {
        let Some(sym) = self.resolve() else {
            return self.fallback();
        };
        let ctx = self.ctx;
        let definition = self.read(&sym.path);
        let mut top = sym.start_line;
        if top < 1 || sym.end_line > i64::try_from(definition.len()).unwrap_or(i64::MAX) {
            return None;
        }
        while top > 1 && DOC_LINE.is_match(&definition[usize::try_from(top - 2).ok()?]) {
            top -= 1;
        }
        let body = definition
            .get(usize::try_from(top - 1).ok()?..usize::try_from(sym.end_line).ok()?)?
            .join("\n");
        let envs: BTreeSet<String> = ENV.find_iter(&body).map(|m| m.as_str().to_owned()).collect();

        let callers = by_place(ctx.call_edges(&sym.uid, Role::Callers, EDGE_OFFSET));
        for edge in callers.iter().take(MAX_CALLERS) {
            let c = &edge.symbol;
            let site = site_of(edge);
            let label = if TESTPATH.is_match(&c.path) { "a test that calls" } else { "calls" };
            let text = format!("{} {label} {}", c.name, sym.name);
            if c.end_line - c.start_line < BODY_CAP {
                self.emit(&c.path, c.start_line, c.end_line, &text);
            } else {
                self.emit(&c.path, site - SITE_CONTEXT, site + SITE_CONTEXT, &text);
            }
            let ups = by_place(ctx.call_edges(&c.uid, Role::Callers, EDGE_OFFSET));
            for up in ups.iter().take(MAX_SITES) {
                let u = &up.symbol;
                let usite = site_of(up);
                if u.start_line >= usite - 1 {
                    self.emit(&u.path, u.start_line, usite + 1, &format!("{} calls {}", u.name, c.name));
                } else {
                    self.emit(&u.path, u.start_line, u.start_line + 1, &format!("{} (declaration)", u.name));
                    self.emit(&u.path, usite - 1, usite + 1, &format!("{} calls {}", u.name, c.name));
                }
            }
            if ctx.timed_out() {
                return None;
            }
        }

        let helper = re(&format!(
            r"\bfn\s+({name}_\w+|\w+_{name})\b|\bdef\s+({name}_\w+)",
            name = regex::escape(&sym.name)
        ));
        let mut files: Vec<String> = ctx
            .files()?
            .iter()
            .filter(|path| SRC.is_match(path))
            .cloned()
            .collect();
        files.sort();
        files.truncate(MAX_FILES);
        for file in &files {
            if ctx.timed_out() {
                return None;
            }
            let lines = self.read(file);
            for (i, line) in lines.iter().enumerate() {
                if let Some(found) = helper.captures(line) {
                    if let Some((first, last)) = unit_around(&lines, i + 1, 22) {
                        let name = found.get(1).or_else(|| found.get(2)).map_or("", |m| m.as_str());
                        self.emit(file, first, last, &format!("{name} (helper named after {})", sym.name));
                    }
                    break;
                }
            }
        }

        let callee_files: BTreeSet<String> = ctx
            .call_edges(&sym.uid, Role::Callees, EDGE_OFFSET)
            .into_iter()
            .map(|edge| edge.symbol.path)
            .filter(|path| *path != sym.path)
            .collect();
        for file in callee_files.iter().take(MAX_FILES) {
            if TESTPATH.is_match(file) {
                continue;
            }
            let lines = self.read(file);
            for (i, line) in lines.iter().enumerate() {
                if TEST_START.is_match(line) {
                    let at = if line.trim_start().starts_with("#[") { i + 2 } else { i + 1 };
                    if let Some((first, last)) = unit_around(&lines, at, 22) {
                        let name = file.rsplit('/').next().unwrap_or(file);
                        self.emit(file, first, last, &format!("a unit test beside {name}, which {} calls into", sym.name));
                    }
                    break;
                }
            }
        }

        let mut found = 0;
        for token in &envs {
            for file in &files {
                if !TESTPATH.is_match(file) || found >= MAX_TESTS {
                    continue;
                }
                let lines = self.read(file);
                if let Some(i) = lines.iter().position(|line| line.contains(token.as_str()))
                    && let Some((first, last)) = unit_around(&lines, i + 1, 30)
                {
                    self.emit(file, first, last, &format!("a test that sets {token}"));
                    found += 1;
                }
            }
        }
        if self.out.is_empty() {
            return None;
        }

        let drops = brief_drops(self.ctx.brief, &sym.name, &envs);
        let mut text = String::new();
        if !drops.is_empty() {
            text.push_str(&format!("drop: ^(?:{})\n", drops.join("|")));
        }
        text.push_str(&self.out.join("\n"));
        text.push('\n');
        Some(text)
    }

    /// The source-level fallbacks, for a question whose names the graph does
    /// not resolve.
    fn fallback(&self) -> Option<String> {
        if !semantic::wants_semantic_callers(self.ctx.prompt) {
            return None;
        }
        let (sources, partial) = semantic::tracked_source(self.ctx, self.ctx.prompt);
        let text = semantic::semantic_caller_blocks(self.ctx, &sources, partial);
        (!text.is_empty()).then(|| format!("{text}\n"))
    }
}

/// Edges sorted by the place of the symbol at the other end.
fn by_place(mut edges: Vec<Edge>) -> Vec<Edge> {
    edges.sort_by(|a, b| {
        (&a.symbol.path, a.symbol.start_line).cmp(&(&b.symbol.path, b.symbol.start_line))
    });
    edges
}

/// The line of a call: the edge's site, else where the caller starts.
fn site_of(edge: &Edge) -> i64 {
    edge.site_line.filter(|site| *site != 0).unwrap_or(edge.symbol.start_line)
}

/// The brief blocks the answer replaces: tests that do not name the symbol,
/// and definitions of other functions.
fn brief_drops(brief: &str, name: &str, envs: &BTreeSet<String>) -> Vec<String> {
    let mut drops = Vec::new();
    let body_after = |from: usize| {
        let end = brief[from..].find("\n### ").map_or(brief.len(), |at| from + at);
        &brief[from..end]
    };
    for found in TEST_DROP.captures_iter(brief) {
        let body = body_after(found.get(0).map_or(0, |m| m.end()));
        if !body.contains(name) && !envs.iter().any(|token| body.contains(token.as_str())) {
            drops.push(escape(&found[1]));
        }
    }
    for found in TESTS_MATCHING.captures_iter(brief) {
        if !body_after(found.get(0).map_or(0, |m| m.end())).contains(name) {
            drops.push(escape(&found[1]));
        }
    }
    for found in DEFINITION_DROP.captures_iter(brief) {
        if &found[2] != name {
            drops.push(escape(&found[1]));
        }
    }
    drops
}

/// The declaration around line `n` (1-based): `(first, last)`, or `None` when
/// none is within reach.
fn unit_around(lines: &[String], n: usize, cap: usize) -> Option<(i64, i64)> {
    let low = n.saturating_sub(79);
    let start = (low..n).rev().find(|i| lines.get(*i).is_some_and(|line| DECL.is_match(line)))?;
    let mut first = start;
    while first > 0 && COMMENT_START.is_match(&lines[first - 1]) {
        first -= 1;
    }
    let (mut depth, mut seen, mut end) = (0_i64, false, None);
    'scan: for (j, row) in lines.iter().enumerate().take(lines.len().min(start + 300)).skip(start) {
        for ch in BRACE_NOISE.replace_all(row, "").chars() {
            match ch {
                '{' => (depth, seen) = (depth + 1, true),
                '}' => depth -= 1,
                _ => {}
            }
        }
        if seen && depth <= 0 {
            end = Some(j);
            break 'scan;
        }
    }
    let end = end.filter(|end| *end + 1 >= n)?;
    let to_i = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
    Some((to_i(first + 1), to_i(end + 1).min(to_i(first + cap))))
}

#[cfg(test)]
mod tests;
