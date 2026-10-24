// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `cand-j-callers` (`specs/brief.md`, "Recipes"): "where is X defined, who
//! calls it, which tests cover it" needs the callers, not the callees.
//!
//! X is the identifier-shaped word of the question that the symbol graph
//! resolves to a function. The step shows the callers (their body when short,
//! else the window around the call) and the call sites of those callers, the
//! tests among the callers, the helpers named after X (`X_raw`), a unit test
//! beside each file X calls into, and the tests that set what X reads: the
//! `ALLCAPS` environment names in its body and doc comment, looked up in the
//! test files. The blocks of the pack that are about other functions are
//! dropped.
//!
//! Read-only and deterministic. The script also carried resolution for
//! JavaScript re-exports and for two particular frameworks' request contexts;
//! those worked around a graph that did not resolve imports, and are not
//! ported (the replay says whether a caller question still misses callers).

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::chain::{CallerHit, SymbolHit};
use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Drop, Outcome, Step};

const BODY_CAP: usize = 40;
const SITE_CONTEXT: usize = 8;
const MAX_CALLERS: usize = 6;
const MAX_SITES: usize = 3;
const MAX_TESTS: usize = 3;
const MAX_FILES: usize = 4_000;
const LINE_CHARS: usize = 200;
const HELPER_LINES: usize = 22;
const ENV_TEST_LINES: usize = 30;
/// Rows of the search for an environment variable's name.
const ENV_ROWS: usize = 400;
/// How far back from a line the declaration around it is looked for.
const UNIT_REACH: usize = 80;
/// How far forward from a declaration its closing brace is looked for.
const UNIT_SCAN: usize = 300;
const CALLEE_DEPTH: usize = 1;
const CALLEE_LIMIT: usize = 24;

static ASKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(callers?|who calls|(functions?|methods?) (that )?(calls?|invokes?|uses?)|called (by|from)|call sites?|calls it)\b|\bwhich\b.{0,80}\b(call|calls|invoke|invokes|uses?)\b",
    )
    .expect("the callers-question pattern is a literal")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(tests?|__tests__|spec|testdata)/|[._-](test|spec)s?\.\w+$|(^|/)test_\w+\.\w+$|_tests?\.\w+$",
    )
    .expect("the test-path pattern is a literal")
});
static SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.(rs|py|js|jsx|ts|tsx|go|java|kt|c|h|cc|cpp|hpp|rb|php|swift|cs|scala|sh|lua)$")
        .expect("the source pattern is a literal")
});
static DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+|extern\s+"[^"]*"\s+|static\s+|export\s+|default\s+)*(?:fn|def|function|func)\s+([A-Za-z_]\w*)|^\s*(?:[A-Za-z_]\w*!)\(\s*([A-Za-z_]\w*)\s*,\s*(?:\||move\s*\||\{|fn\b)|^\s*(?:test|it|describe)\(\s*['"`]([^'"`]+)"#,
    )
    .expect("the declaration pattern is a literal")
});
static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"`([A-Za-z_]\w*)(?:\(\))?`|\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+)\b|\b([a-z]+[A-Z][A-Za-z0-9]*)\b|\b([A-Z][a-z0-9]+(?:[A-Z][a-z0-9]*)+)\b",
    )
    .expect("the identifier pattern is a literal")
});
static ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b").expect("the env pattern is a literal")
});
static COMMENT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(//|#\[|#\s|/\*|\*|@)").expect("the comment pattern is a literal")
});
static DOC_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(//|#|/\*|\*|@)").expect("the doc pattern is a literal"));
static STRINGS_AND_COMMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])'|//.*$"#).expect("the mask pattern is a literal")
});
static TEST_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(#\[test\]|def test_|(it|test)\()")
        .expect("the test-start pattern is a literal")
});

/// The declaration around line `n` (1-based): `(first, last)`, or `None` when
/// none is within reach. The last line is at most `cap` lines after the first.
fn unit_around(lines: &[String], n: usize, cap: usize) -> Option<(usize, usize)> {
    let start = (n.saturating_sub(UNIT_REACH - 1)..n)
        .rev()
        .find(|i| lines.get(*i).is_some_and(|line| DECL.is_match(line)))?;
    let mut first = start;
    while first > 0 && COMMENT_LINE.is_match(&lines[first - 1]) {
        first -= 1;
    }
    let (mut depth, mut seen, mut end) = (0_i64, false, None);
    for (j, line) in lines
        .iter()
        .enumerate()
        .take((start + UNIT_SCAN).min(lines.len()))
        .skip(start)
    {
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
            end = Some(j);
            break;
        }
    }
    let end = end.filter(|end| end + 1 >= n)?;
    Some((first + 1, (end + 1).min(first.saturating_add(cap))))
}

/// Source lines with an emit that never repeats a range.
struct Emitter<'a> {
    ctx: &'a Ctx<'a>,
    emitted: Vec<(String, usize, usize)>,
    blocks: Vec<Block>,
}

impl Emitter<'_> {
    fn emit(&mut self, path: &str, a: usize, b: usize, label: &str) {
        let Some(text) = self.ctx.read(path) else {
            return;
        };
        let lines: Vec<&str> = text.lines().collect();
        let (a, b) = (a.max(1), b.min(lines.len()));
        if a > b
            || self
                .emitted
                .iter()
                .any(|(p, x, y)| p == path && *x <= a && b <= *y)
        {
            return;
        }
        self.emitted.push((path.to_string(), a, b));
        let body = (a..=b)
            .map(|n| {
                let line: String = lines[n - 1].chars().take(LINE_CHARS).collect();
                format!("{n}| {line}")
            })
            .collect();
        self.blocks
            .push(Block::new(format!("{path}:{a}-{b} — {label}"), body));
    }
}

fn lines_of(ctx: &Ctx, path: &str) -> Vec<String> {
    ctx.read(path)
        .map(|text| text.lines().map(ToString::to_string).collect())
        .unwrap_or_default()
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::CandJCallers,
        &[
            QuestionKind::Overview,
            QuestionKind::Review,
            QuestionKind::History,
        ],
    ) || !ASKS.is_match(ctx.typed)
    {
        return;
    }
    let _timer = ctx.timer(Step::CandJCallers);
    let Some(sym) = resolve(ctx) else {
        return;
    };
    let Some(files) = ctx.files() else {
        return;
    };
    let dlines = lines_of(ctx, &sym.path);
    let (start, end) = (sym.start_line as usize, sym.end_line as usize);
    if start < 1 || end > dlines.len() || start > end {
        return;
    }
    let mut top = start;
    while top > 1 && DOC_LINE.is_match(&dlines[top - 2]) {
        top -= 1;
    }
    let envs: Vec<String> = ENV
        .find_iter(&dlines[top - 1..end].join("\n"))
        .map(|found| found.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut emitter = Emitter {
        ctx,
        emitted: Vec::new(),
        blocks: Vec::new(),
    };
    let sources: Vec<&String> = files
        .iter()
        .filter(|file| SOURCE.is_match(file))
        .take(MAX_FILES)
        .collect();
    callers_of(ctx, &sym, &mut emitter);
    helpers_named_after(ctx, &sym, &mut emitter);
    unit_tests_beside_callees(ctx, &sym, &sources, &mut emitter);
    tests_that_set(ctx, &envs, &mut emitter);
    if emitter.blocks.is_empty() {
        return;
    }
    let mut names = vec![sym.name.clone()];
    names.extend(envs);
    out.drops.push(Drop::UnrelatedTest(names));
    out.drops.push(Drop::OtherDefinitions(sym.name));
    out.rescuing = true;
    out.lead.extend(emitter.blocks);
}

/// The function the question names: the first identifier-shaped word that the
/// graph resolves to a function or method of that name, preferring a file the
/// brief already shows.
fn resolve(ctx: &Ctx) -> Option<SymbolHit> {
    let mut seen: Vec<String> = Vec::new();
    for caps in IDENT.captures_iter(ctx.typed) {
        let Some(word) = (1..=4).find_map(|group| caps.get(group)) else {
            continue;
        };
        if !seen.iter().any(|held| held == word.as_str()) {
            seen.push(word.as_str().to_string());
        }
    }
    for word in seen {
        let Ok(found) = ctx.evidence.symbols(&word, ctx.deadline) else {
            continue;
        };
        let mut rows: Vec<SymbolHit> = found
            .into_iter()
            .filter(|hit| (hit.kind == "function" || hit.kind == "method") && hit.name == word)
            .collect();
        rows.sort_by(|a, b| {
            let shown = |hit: &SymbolHit| !ctx.near.contains(&hit.path);
            shown(a)
                .cmp(&shown(b))
                .then(a.path.cmp(&b.path))
                .then(a.start_line.cmp(&b.start_line))
        });
        if let Some(first) = rows.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// The declaration a caller hit points at: the symbol of that name in that
/// file whose span holds the call.
fn caller_symbol(ctx: &Ctx, hit: &CallerHit) -> Option<SymbolHit> {
    let found = ctx.evidence.symbols(&hit.via, ctx.deadline).ok()?;
    found.into_iter().find(|symbol| {
        symbol.path == hit.path && symbol.start_line <= hit.line && hit.line <= symbol.end_line
    })
}

fn callers_of(ctx: &Ctx, sym: &SymbolHit, emitter: &mut Emitter) {
    let Ok(mut callers) = ctx.evidence.callers(&sym.uid, ctx.deadline) else {
        return;
    };
    callers.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    for hit in callers.iter().take(MAX_CALLERS) {
        let site = hit.line as usize;
        let (first, last, uid) = match caller_symbol(ctx, hit) {
            Some(symbol) => (
                symbol.start_line as usize,
                symbol.end_line as usize,
                Some(symbol.uid),
            ),
            None => {
                let lines = lines_of(ctx, &hit.path);
                match unit_around(&lines, site, usize::MAX / 4) {
                    Some((a, b)) => (a, b, None),
                    None => (site.saturating_sub(SITE_CONTEXT), site + SITE_CONTEXT, None),
                }
            }
        };
        let label = if TEST_PATH.is_match(&hit.path) {
            "a test that calls"
        } else {
            "calls"
        };
        let text = format!("{} {label} {}", hit.via, sym.name);
        if last - first < BODY_CAP {
            emitter.emit(&hit.path, first, last, &text);
        } else {
            emitter.emit(
                &hit.path,
                site.saturating_sub(SITE_CONTEXT),
                site + SITE_CONTEXT,
                &text,
            );
        }
        let Some(uid) = uid else {
            continue;
        };
        let Ok(mut ups) = ctx.evidence.callers(&uid, ctx.deadline) else {
            continue;
        };
        ups.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        for up in ups.iter().take(MAX_SITES) {
            let usite = up.line as usize;
            let Some(upper) = caller_symbol(ctx, up) else {
                continue;
            };
            let ustart = upper.start_line as usize;
            if ustart + 1 >= usite {
                emitter.emit(
                    &up.path,
                    ustart,
                    usite + 1,
                    &format!("{} calls {}", up.via, hit.via),
                );
            } else {
                emitter.emit(
                    &up.path,
                    ustart,
                    ustart + 1,
                    &format!("{} (declaration)", up.via),
                );
                emitter.emit(
                    &up.path,
                    usite - 1,
                    usite + 1,
                    &format!("{} calls {}", up.via, hit.via),
                );
            }
        }
    }
}

/// The first helper per file named after the function: `X_raw`, `raw_X`.
fn helpers_named_after(ctx: &Ctx, sym: &SymbolHit, emitter: &mut Emitter) {
    let name = regex::escape(&sym.name);
    let Ok(helper) = Regex::new(&format!(
        r"\bfn\s+({name}_\w+|\w+_{name})\b|\bdef\s+({name}_\w+)"
    )) else {
        return;
    };
    let Ok(found) = ctx.evidence.grep(helper.as_str(), 400, ctx.deadline) else {
        return;
    };
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for hit in found.hits.iter().filter(|hit| SOURCE.is_match(&hit.path)) {
        let Some(text) = hit.text.as_deref() else {
            continue;
        };
        let Some(caps) = helper.captures(text) else {
            continue;
        };
        if !done.insert(&hit.path) {
            continue;
        }
        let lines = lines_of(ctx, &hit.path);
        if let Some((a, b)) = unit_around(&lines, hit.line as usize, HELPER_LINES) {
            let found_name = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map_or("", |m| m.as_str());
            emitter.emit(
                &hit.path,
                a,
                b,
                &format!("{found_name} (helper named after {})", sym.name),
            );
        }
    }
}

/// A unit test in each non-test file the function calls into.
fn unit_tests_beside_callees(
    ctx: &Ctx,
    sym: &SymbolHit,
    sources: &[&String],
    emitter: &mut Emitter,
) {
    let Ok(tree) = ctx
        .evidence
        .flow_tree(&sym.uid, CALLEE_DEPTH, CALLEE_LIMIT, ctx.deadline)
    else {
        return;
    };
    let files: BTreeSet<&str> = tree
        .iter()
        .map(|node| node.path.as_str())
        .filter(|path| *path != sym.path && !TEST_PATH.is_match(path))
        .collect();
    for file in files.into_iter().take(MAX_FILES) {
        if !sources.iter().any(|source| source.as_str() == file) {
            continue;
        }
        let lines = lines_of(ctx, file);
        let Some(index) = lines.iter().position(|line| TEST_START.is_match(line)) else {
            continue;
        };
        let at = if lines[index].trim_start().starts_with("#[") {
            index + 2
        } else {
            index + 1
        };
        if let Some((a, b)) = unit_around(&lines, at, HELPER_LINES) {
            let name = file.rsplit('/').next().unwrap_or(file);
            emitter.emit(
                file,
                a,
                b,
                &format!("a unit test beside {name}, which {} calls into", sym.name),
            );
        }
    }
}

/// The tests that set what the function reads: the first line per test file
/// that names an environment variable of its body.
fn tests_that_set(ctx: &Ctx, envs: &[String], emitter: &mut Emitter) {
    let mut found = 0;
    for token in envs {
        let Ok(rows) = ctx
            .evidence
            .grep(&regex::escape(token), ENV_ROWS, ctx.deadline)
        else {
            continue;
        };
        let mut done: BTreeSet<&str> = BTreeSet::new();
        for row in rows.hits.iter().filter(|row| TEST_PATH.is_match(&row.path)) {
            if found >= MAX_TESTS {
                return;
            }
            if !done.insert(&row.path) {
                continue;
            }
            let lines = lines_of(ctx, &row.path);
            if let Some((a, b)) = unit_around(&lines, row.line as usize, ENV_TEST_LINES) {
                emitter.emit(&row.path, a, b, &format!("a test that sets {token}"));
                found += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
