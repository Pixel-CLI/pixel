// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-k-code-sites`: the code a configuration question reads, not the
//! manual that describes it.
//!
//! Ported from `probes/cand-k-code-sites/probe`. Two shapes, both derived
//! from the prompt and the repository:
//!
//! * env: an ALL_CAPS_NAME of the prompt that a source file spells (not a
//!   doc) is a variable. The function that reads it is shown, then the
//!   functions of the same file it calls, then the function that calls it
//!   (who applies the loaded values). Blocks of the brief that are
//!   documentation are dropped when the code was found.
//! * sites: "which flags/options/settings govern X, and where are they
//!   used?": the prompt's rare identifier stems are searched in non-test
//!   code; every hit is mapped to its enclosing definition, the definitions
//!   that rank highest are shown, at most three per file.
//!
//! The script searched the working tree with `rg`; the step searches the
//! tracked code files, in the order `rg --sort path` printed them, skipping
//! hidden paths, binary files and `.gitignore` is not read: an untracked file
//! is not searched.

mod source;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use self::source::{
    Hit, Source, block, chain, decl_end, decls_of, fname, has_word, is_comment, is_decl,
    is_type_head, size, window, with_docs, with_type,
};
use super::text::{escape, indent, re, subwords};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-k-code-sites",
    when_kinds: &[],
    skip_kinds: &[],
    when_prompt: Some(
        r"(?i:\bwhere\b.*\b(appl\w+|used|handled|enforced|checked|read)\b)|\b[A-Z][A-Z0-9]*_[A-Z0-9_]+\b",
    ),
    when_gate_closed: true,
    budget_ms: 2500,
    mode: ProbeMode::Lead,
    run,
};

/// Lines of the definition that reads a variable.
const ENV_REGION: usize = 60;
/// Lines of a definition that names a stem, after the first four blocks.
const SITE_REGION: usize = 26;
/// Characters of output per mode.
const BUDGET_ENV: usize = 7000;
const BUDGET_SITES: usize = 7000;
/// What the brief's documentation blocks look like: dropped when the code was found.
const DOCY: &str = r"^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/";

const QUESTION_WORDS: &[&str] = &[
    "which", "where", "apply", "applies", "does", "what", "that", "this", "with", "from", "when",
    "they", "them", "their", "used", "uses", "using", "have", "flag", "flags", "option", "options",
    "setting", "settings", "enable", "enables", "disable", "disables", "affect", "affects",
    "decide", "decides", "turn", "turns", "toggle", "toggles", "switch", "parameter", "argument",
    "code", "value", "values", "handled", "handle", "handles", "found", "find", "finds", "read",
    "reads", "show", "shows", "make", "makes", "determine",
];


static ENV_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b[A-Z][A-Z0-9]*_[A-Z0-9_]*[A-Z0-9]\b"));
static WHERE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\bwhere\b.*\b(appl\w+|used|handled|enforced|checked|honou?red|read)\b")
});
static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(flags?|options?|settings?|switch(?:es)?|parameters?|arguments?|toggles?)\b")
});
static CALL: LazyLock<Regex> = LazyLock::new(|| re(r"\b([a-z_]\w*)\s*\("));
static CALL_ARGS: LazyLock<Regex> = LazyLock::new(|| re(r"\b([a-z_]\w*)\s*\(([^()]*)\)"));
static QUOTED: LazyLock<Regex> = LazyLock::new(|| re(r#""[^"]*""#));
static WORDS: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z]{4,}"));
static CONDITION: LazyLock<Regex> = LazyLock::new(|| re(r"\b(if|while|match|&&|\|\|)\b"));
static NAMES_STEMS: LazyLock<Regex> = LazyLock::new(|| re(r"names (\S+)"));

fn run(ctx: &Ctx) -> String {
    answer(ctx).unwrap_or_default()
}

/// A block of the env mode with its priority: the variable's reader first.
type Prioritised = (usize, Vec<String>);

fn answer(ctx: &Ctx) -> Option<String> {
    let mut src = Source::new(ctx);
    let mut env = env_mode(&mut src, ctx.prompt)?;
    let sites = if env.is_empty() {
        sites_mode(&mut src, ctx.prompt)?
    } else {
        Vec::new()
    };
    if env.is_empty() && sites.is_empty() {
        return None;
    }
    let mut out = String::new();
    let blocks: Vec<Vec<String>> = if env.is_empty() {
        let stems: BTreeSet<String> = sites
            .iter()
            .filter_map(|b| NAMES_STEMS.captures(&b[0]))
            .flat_map(|found| {
                found[1]
                    .replace('%', "")
                    .split('/')
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        let stems: Vec<String> = stems.iter().map(|stem| escape(stem)).collect();
        out.push_str(&format!(
            "drop: ^config registry|^(?!.*(?:{}))\n",
            stems.join("|")
        ));
        out.push_str(&format!("drop: {DOCY}\n"));
        sites
    } else {
        let mut order: Vec<usize> = (0..env.len()).collect();
        order.sort_by_key(|n| (env[*n].0, *n));
        let mut kept = HashSet::new();
        let mut used = 0;
        for n in order {
            let cost = size(&env[n].1);
            if used + cost <= BUDGET_ENV {
                kept.insert(n);
                used += cost;
            }
        }
        let mut shown = Vec::new();
        for (n, (_, b)) in env.drain(..).enumerate() {
            if kept.contains(&n) {
                shown.push(b);
            }
        }
        let paths: BTreeSet<&str> = shown
            .iter()
            .filter_map(|b| b[0].get(4..))
            .filter_map(|header| header.split(':').next())
            .collect();
        out.push_str(&format!("drop: {DOCY}\n"));
        for path in paths {
            out.push_str(&format!(
                "drop: ^{}:\\d+-\\d+ — (file named|whole file)\n",
                escape(path)
            ));
        }
        shown
    };
    for b in &blocks {
        out.push_str(&b.join("\n"));
        out.push('\n');
    }
    Some(out)
}

/// `os.path.splitext(os.path.basename(path))[0]`.
fn file_stem(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rfind('.') {
        Some(at) if base[..at].chars().any(|ch| ch != '.') => &base[..at],
        _ => base,
    }
}

/// The function that reads a variable named by the prompt, what it calls and
/// who calls it. `None` when the clock ran out.
fn env_mode(src: &mut Source, prompt: &str) -> Option<Vec<Prioritised>> {
    let mut tokens: Vec<&str> = Vec::new();
    for found in ENV_TOKEN.find_iter(prompt) {
        if !tokens.contains(&found.as_str()) {
            tokens.push(found.as_str());
        }
    }
    tokens.retain(|token| token.chars().count() > 5);
    let mut blocks: Vec<Prioritised> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    for token in tokens.iter().take(2) {
        let found = src.search(&|line| has_word(line, token))?;
        let mut sites: Vec<(String, usize)> = Vec::new();
        for hit in found {
            if !is_comment(&hit.text) && !src.is_test(&hit.path, hit.line) {
                sites.push((hit.path, hit.line));
            }
        }
        sites.sort();
        sites.dedup();
        for (path, i) in sites.into_iter().take(2) {
            let lines = src.lines(&path);
            let rows = &lines.rows;
            let decls = decls_of(rows);
            let mut ends = HashMap::new();
            let Some(&d) = chain(rows, &decls, &mut ends, i).first() else {
                continue;
            };
            let (lo, hi) = window(with_docs(rows, d), ends[&d], &[i], ENV_REGION);
            if !seen.insert((path.clone(), lo)) {
                continue;
            }
            blocks.push((0, block(rows, &path, lo, hi, &format!("reads {token}"))));
            let own = fname(rows, d);
            let label = own.as_deref().unwrap_or("None");
            let mut callees: Vec<usize> = Vec::new();
            let mut depth_of: HashMap<usize, usize> = HashMap::new();
            let mut frontier = vec![d];
            for depth in 0..2 {
                let mut found = Vec::new();
                for f in &frontier {
                    let end = *ends.entry(*f).or_insert_with(|| decl_end(rows, *f));
                    let body = rows[*f..=end].join("\n");
                    let mut names: Vec<&str> = Vec::new();
                    for call in CALL.captures_iter(&body) {
                        let name = call.get(1).map_or("", |m| m.as_str());
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    for name in names {
                        if own.as_deref() == Some(name) {
                            continue;
                        }
                        let callee = decls.iter().find(|e| {
                            fname(rows, **e).as_deref() == Some(name)
                                && !callees.contains(e)
                                && **e != d
                                && indent(&rows[**e]) <= indent(&rows[d]) + 4
                        });
                        if let Some(&e) = callee {
                            callees.push(e);
                            depth_of.insert(e, depth);
                            found.push(e);
                        }
                    }
                }
                frontier = found;
            }
            callees.sort_unstable();
            for e in callees.into_iter().take(4) {
                let end = *ends.entry(e).or_insert_with(|| decl_end(rows, e));
                let (clo, chi) = window(with_docs(rows, e), end, &[], 56);
                if seen.insert((path.clone(), clo)) {
                    blocks.push((
                        2 + depth_of[&e],
                        block(rows, &path, clo, chi, &format!("callee of {label}")),
                    ));
                }
            }
            let mut calls: Vec<Hit> = Vec::new();
            if let Some(own) = &own {
                let qualified = format!(
                    r"\b{}::{}\s*\(",
                    regex::escape(file_stem(&path)),
                    regex::escape(own)
                );
                if let Ok(pattern) = Regex::new(&qualified) {
                    calls = src.search(&|line| pattern.is_match(line))?;
                }
                if calls.is_empty()
                    && own.chars().count() > 5
                    && let Ok(pattern) = Regex::new(&format!(r"\b{}\s*\(", regex::escape(own)))
                {
                    calls = src
                        .search(&|line| pattern.is_match(line))?
                        .into_iter()
                        .filter(|hit| !is_decl(&hit.text) && !is_comment(&hit.text))
                        .collect();
                }
            }
            for hit in calls.iter().take(2) {
                let clines = src.lines(&hit.path);
                let crows = &clines.rows;
                let cdecls = decls_of(crows);
                let mut cends = HashMap::new();
                let Some(&e) = chain(crows, &cdecls, &mut cends, hit.line).first() else {
                    continue;
                };
                let (clo, chi) = window(with_docs(crows, e), cends[&e], &[hit.line], 56);
                if seen.insert((hit.path.clone(), clo)) {
                    blocks.push((
                        1,
                        block(
                            crows,
                            &hit.path,
                            clo,
                            chi,
                            &format!("calls {label}, where the loaded values take effect"),
                        ),
                    ));
                }
            }
        }
    }
    Some(blocks)
}

/// The prompt's identifier stems: `follows` -> `follow`.
fn stems_of(prompt: &str) -> Vec<String> {
    let mut words: Vec<&str> = Vec::new();
    for found in WORDS.find_iter(prompt) {
        if !words.contains(&found.as_str()) {
            words.push(found.as_str());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for word in words {
        let mut stem = word.to_lowercase();
        if QUESTION_WORDS.contains(&stem.as_str()) {
            continue;
        }
        for suffix in ["ing", "ed", "es", "s"] {
            if stem.len() > 5 && stem.ends_with(suffix) {
                stem.truncate(stem.len() - suffix.len());
                break;
            }
        }
        if !out.contains(&stem) {
            out.push(stem);
        }
    }
    out
}

/// `re.search(r"(?i)(?<![A-Za-z0-9_]){stem}(?![A-Za-z0-9_])", text)`.
fn names_stem(text: &str, stem: &str) -> bool {
    let text = text.to_lowercase();
    let plain = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
    let mut from = 0;
    while let Some(at) = text[from..].find(stem) {
        let start = from + at;
        let end = start + stem.len();
        let before = text[..start].chars().next_back().is_none_or(|ch| !plain(ch));
        let after = text[end..].chars().next().is_none_or(|ch| !plain(ch));
        if before && after {
            return true;
        }
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// How much one hit says about a definition applying the stem.
fn hit_weight(text: &str, stem: &str) -> f64 {
    let stem = regex::escape(stem);
    let text = text.trim();
    if re(&format!(
        r"(?i)^(pub(\([^)]*\))?\s+)?\w*{stem}\w*\s*:"
    ))
    .is_match(text)
    {
        return 0.3;
    }
    if re(&format!(r"(?i)\w*{stem}\w*\s*\(")).is_match(text) || CONDITION.is_match(text) {
        return 2.0;
    }
    1.0
}

/// One definition that the stems' hits land in.
#[derive(Default)]
struct Region {
    hits: BTreeSet<usize>,
    stems: BTreeSet<String>,
    end: usize,
    named: bool,
    ty: bool,
    weight: f64,
}

/// A hit that counts: code, not a comment, inside the file, not a test item.
fn counts(src: &mut Source, hit: &Hit) -> bool {
    !is_comment(&hit.text)
        && hit.line < src.lines(&hit.path).rows.len()
        && !src.is_test(&hit.path, hit.line)
}

/// "Which flags control X, where are they applied?": the definitions that
/// name or apply the question's stems. `None` when the clock ran out.
fn sites_mode(src: &mut Source, prompt: &str) -> Option<Vec<Vec<String>>> {
    if !(WHERE.is_match(prompt) && SUBJECT.is_match(prompt)) {
        return Some(Vec::new());
    }
    let ncode = src.code_files().max(1);
    let stems = stems_of(prompt);
    if stems.is_empty() {
        return Some(Vec::new());
    }
    let found = src.search(&|line| {
        let line = line.to_lowercase();
        stems.iter().any(|stem| line.contains(stem.as_str()))
    })?;
    let mut keep: Vec<(String, Vec<Hit>)> = Vec::new();
    for stem in &stems {
        let mut hits: Vec<Hit> = Vec::new();
        for hit in found.iter().filter(|hit| hit.text.to_lowercase().contains(stem.as_str())) {
            if !counts(src, hit) {
                continue;
            }
            let parts: HashSet<String> = subwords(&hit.text).into_iter().collect();
            if parts.contains(stem)
                || parts.contains(&format!("{stem}s"))
                || parts.contains(&format!("{stem}ed"))
            {
                hits.push(hit.clone());
            }
        }
        let files: HashSet<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
        let escaped = regex::escape(stem);
        let declared = re(&format!(
            r"(?i)\b(?:fn|struct|enum|trait|class|def|const|static|let|var|type|func|function)\s+(?:mut\s+)?\w*{escaped}|^\s*(?:pub(?:\([^)]*\))?\s+)?\w*{escaped}\w*\s*:\s*[A-Za-z&]"
        ));
        if !hits.iter().any(|hit| declared.is_match(&hit.text)) {
            continue;
        }
        if hits.len() >= 2 && !files.is_empty() && files.len() as f64 <= 4.0_f64.max(ncode as f64 * 0.15)
        {
            keep.push((stem.clone(), hits));
        }
    }
    if keep.is_empty() {
        return Some(Vec::new());
    }
    let mut snapshot = keep.clone();
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    for (stem, hits) in &snapshot {
        let mut hop: BTreeSet<String> = BTreeSet::new();
        for hit in hits {
            let text = QUOTED.replace_all(&hit.text, "\"\"");
            for call in CALL_ARGS.captures_iter(&text) {
                if names_stem(&call[2], stem) {
                    hop.insert(call[1].to_owned());
                }
            }
        }
        for name in hop {
            let mut extra = Vec::new();
            for hit in src.search(&|line| has_word(line, &name))? {
                if counts(src, &hit) {
                    extra.push(hit);
                }
            }
            let key = format!("%{name}");
            match keep.iter_mut().find(|(known, _)| *known == key) {
                Some((_, rows)) => rows.extend(extra),
                None => keep.push((key, extra)),
            }
        }
    }
    let mut regions: BTreeMap<(String, usize), Region> = BTreeMap::new();
    let mut defs: HashMap<String, (Vec<usize>, HashMap<usize, usize>)> = HashMap::new();
    for (key, hits) in &keep {
        let stem = key.trim_start_matches('%');
        for hit in hits {
            let lines = src.lines(&hit.path);
            let rows = &lines.rows;
            let (decls, ends) = defs
                .entry(hit.path.clone())
                .or_insert_with(|| (decls_of(rows), HashMap::new()));
            let found = chain(rows, decls, ends, hit.line);
            let Some(&innermost) = found.first() else {
                continue;
            };
            // The outermost definition that names the stem and is small enough.
            let pick = found
                .iter()
                .rev()
                .find(|d| rows[**d].to_lowercase().contains(stem) && ends[*d] - **d < 48)
                .copied()
                .unwrap_or(innermost);
            let region = regions
                .entry((hit.path.clone(), pick))
                .or_insert_with(|| Region {
                    end: ends[&pick],
                    ..Region::default()
                });
            region.hits.insert(hit.line);
            region.stems.insert(stem.to_owned());
            region.weight += hit_weight(&hit.text, stem);
            if rows[pick].to_lowercase().contains(stem) {
                region.named = true;
                region.ty = is_type_head(&rows[pick]);
            }
        }
    }
    let rank = |key: &(String, usize), region: &Region| -> f64 {
        let small = if region.end - key.1 < 10 && !region.ty { 3.0 } else { 0.0 };
        let kind = if region.ty { 3.0 } else { 0.0 };
        -(region.weight - small + kind + 0.5 * region.stems.len() as f64)
    };
    let mut ranked: Vec<(&(String, usize), &Region)> = regions.iter().collect();
    ranked.sort_by(|a, b| {
        rank(a.0, a.1)
            .partial_cmp(&rank(b.0, b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut per_file: HashMap<&str, usize> = HashMap::new();
    let mut used = 0;
    for ((path, d), region) in ranked {
        if per_file.get(path.as_str()).copied().unwrap_or(0) >= 3
            || blocks.len() >= 8
            || region.end - d < 2
        {
            continue;
        }
        let lines = src.lines(path);
        let rows = &lines.rows;
        let hits: Vec<usize> = region.hits.iter().copied().collect();
        let size_cap = if region.named {
            40
        } else if blocks.len() < 4 {
            SITE_REGION
        } else {
            16
        };
        let (lo, hi) = window(with_type(rows, with_docs(rows, *d)), region.end, &hits, size_cap);
        let stems: Vec<&str> = region.stems.iter().map(String::as_str).collect();
        let b = block(rows, path, lo, hi, &format!("names {}", stems.join("/")));
        if used + size(&b) > BUDGET_SITES {
            continue;
        }
        used += size(&b);
        *per_file.entry(path.as_str()).or_insert(0) += 1;
        blocks.push(b);
    }
    Some(blocks)
}

#[cfg(test)]
mod tests;
