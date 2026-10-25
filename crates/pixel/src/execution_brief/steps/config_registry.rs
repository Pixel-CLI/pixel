// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `config-registry`: a setting question ("where do I set X", "which
//! environment variables", "what port") is answered by a registry built over
//! the working tree: key, value or fallback, where it is declared, where it is
//! read.
//!
//! It reads env reads in JS/TS/Python/Rust/Go/Ruby, typed env declarations,
//! committed `.env` templates and the git-tracked `.env.development` /
//! `.env.production` (values that look like secrets are masked; untracked
//! `.env*` are never read), exported default constants and dev-server port
//! settings. Five narrower lanes answer first when their shape is present: a
//! CLI flag and the function that derives its runtime value, the browser
//! storage key a preference persists under, a JavaScript setting and the
//! compiler and getter that consume it, a Python configuration loader and the
//! literal `set("key", value)` calls. Read-only and deterministic.
//!
//! Ported from `probes/config-registry/probe`. The script ran `rg` over the
//! working tree, `rg -l` for the narrow lanes and Python's `ast` and
//! `tokenize` for the loader; the step walks the tree with the `ignore` crate
//! `rg` is built on (its ignore rules, a NUL in the first 64 KiB buffer
//! skipping the file) and parses Python with tree-sitter.

use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{head, last_segment};
use super::{Ctx, Step};

mod browser;
mod cli_setting;
mod compiled;
mod evidence;
mod loader;
mod rank;
mod registry;
mod repo;
mod scan;
mod scopes;
mod search;
mod words;

use evidence::{
    SECRETISH, comment_marks, declaration_candidates, evidence_block, is_env_path, mask,
    setter_evidence,
};
use rank::{rank, rank_bound};
use registry::{Def, Entry, registry};
use repo::Repo;
use scan::scan;
use words::words_of;

pub(super) const STEP: Step = Step {
    name: "config-registry",
    when_kinds: &["config"],
    skip_kinds: &[],
    when_prompt: Some(
        r"(?i)\b(config\w*|settings?|env(ironment)?|variables?|flags?|options?|defaults?|ports?|overrid\w+|max(?:imum)?|min(?:imum)?|bounds?)\b",
    ),
    when_gate_closed: false,
    budget_ms: 2000,
    mode: ProbeMode::Lead,
    run,
};

/// The characters of the output: the narrow lanes' and the registry's share.
const OUT_CAP: usize = 9000;
const ENV_FILES_SHOWN: usize = 6;
const WORDS_SHOWN: usize = 6;

static INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(config\w*|settings?|env(ironment)?(\s+var\w*)?|variables?|flags?|options?|defaults?|ports?|overrid\w+|where do i set|set (that|this|it)|max(?:imum)?|min(?:imum)?|bounds?)\b",
    )
    .expect("the intent pattern is a literal")
});
static BOUND_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(max(?:imum)?|min(?:imum)?|bounds?)\b")
        .expect("the bound pattern is a literal")
});
static EXISTING_CONFIG_INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(config\w*|settings?|env(ironment)?(\s+var\w*)?|variables?|flags?|options?|defaults?|ports?|overrid\w+|where do i set|set (that|this|it))\b",
    )
    .expect("the config-intent pattern is a literal")
});
static MASKED_ASSIGNMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"=(.*)$").expect("the assignment pattern is a literal"));

fn run(ctx: &Ctx) -> String {
    emit(&Repo::new(ctx), ctx.prompt)
}

fn emit(repo: &Repo, prompt: &str) -> String {
    if !INTENT.is_match(prompt) {
        return String::new();
    }
    let words = words_of(prompt);
    let scoped = cli_setting::evidence(repo, prompt) + &browser::evidence(repo, prompt);
    if !scoped.is_empty() {
        return head(&scoped, OUT_CAP).to_owned();
    }
    let loader = loader::evidence(repo, prompt);
    let Some(rows) = scan(repo) else {
        return String::new();
    };
    let compiled = compiled::evidence(repo, prompt, &rows);
    if !compiled.is_empty() {
        return head(&compiled, OUT_CAP).to_owned();
    }
    let reg = registry(&rows);
    let bound_only = BOUND_QUERY.is_match(prompt) && !EXISTING_CONFIG_INTENT.is_match(prompt);
    let keys = if bound_only {
        rank_bound(repo, &reg, prompt)
    } else {
        let all: Vec<(&String, &Entry)> = reg.iter().collect();
        rank(repo, &all, prompt)
    };
    let setters = setter_evidence(repo, &rows, prompt);
    let prefix = head(&(loader + &setters), OUT_CAP).to_owned();
    if keys.is_empty() {
        return prefix;
    }
    let registry_output = registry_output(repo, &reg, &keys, &words);
    let remaining = OUT_CAP.saturating_sub(prefix.chars().count());
    prefix + head(&registry_output, remaining)
}

/// The head lines and excerpts of the chosen keys.
fn registry_output(repo: &Repo, reg: &registry::Registry, keys: &[String], words: &[String]) -> String {
    let mut env_files: Vec<&str> = repo
        .tracked()
        .iter()
        .filter(|file| file.starts_with(".env") || file.contains("/.env"))
        .flat_map(|file| file.split_whitespace())
        .filter(|file| last_segment(file).starts_with(".env"))
        .collect();
    let mut head = vec![format!(
        "### config registry — keys matching {} (key | value or fallback | declared | read at)",
        words.iter().take(WORDS_SHOWN).cloned().collect::<Vec<_>>().join(", ")
    )];
    let mut blocks: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<(String, usize)> = std::collections::HashSet::new();
    for key in keys {
        let entry = &reg[key];
        // Env files first, then by path; the sort is stable.
        let mut defs: Vec<&Def> = entry.defs.iter().collect();
        defs.sort_by_key(|def| (!is_env_path(&def.path), def.path.clone()));
        if defs.len() > 1 {
            let (heads, excerpts) = declaration_candidates(repo, key, &defs);
            head.extend(heads);
            blocks.extend(excerpts);
            for read in entry.reads.iter().take(2) {
                blocks.extend(evidence_block(repo, key, &read.path, read.line, 1, 2));
            }
            continue;
        }
        let first = defs.first();
        let file_value = defs
            .iter()
            .find(|def| is_env_path(&def.path) && !def.value.is_empty())
            .map(|def| (def.path.as_str(), def.line, mask(key, &def.value)));
        let fallback = mask(
            key,
            entry
                .reads
                .iter()
                .map(|read| read.default.as_str())
                .find(|default| !default.is_empty())
                .or_else(|| {
                    defs.iter()
                        .find(|def| !def.value.is_empty() && !is_env_path(&def.path))
                        .map(|def| def.value.as_str())
                })
                .unwrap_or(""),
        );
        let is_env = !entry.reads.is_empty()
            || defs
                .iter()
                .any(|def| def.path.ends_with("env.d.ts") || is_env_path(&def.path));
        let value = match (&file_value, fallback.is_empty()) {
            (Some((path, line, value)), _) => {
                let mut text = format!("set in {path}:{line} = {value}");
                if !fallback.is_empty() {
                    text.push_str(&format!(" (code fallback {fallback})"));
                }
                text
            }
            (None, false) if is_env => format!("code fallback {fallback}; an env file may override"),
            (None, false) => format!("constant default {fallback}"),
            (None, true) => "no value in indexed files".to_owned(),
        };
        let reads: Vec<String> = entry
            .reads
            .iter()
            .take(3)
            .map(|read| format!("{}:{}", read.path, read.line))
            .collect();
        let declared = first.map_or("not declared".to_owned(), |def| format!("{}:{}", def.path, def.line));
        let mut line = format!(
            "- {key} | {value} | {declared} | read at {}",
            if reads.is_empty() { "no code read".to_owned() } else { reads.join(", ") }
        );
        if entry.reads.len() > 3 {
            line.push_str(&format!(" (+{} more)", entry.reads.len() - 3));
        }
        head.push(line);
        if let Some(def) = first
            && seen.insert((def.path.clone(), def.line))
        {
            let span = comment_marks(repo, &def.path, def.line).min(3);
            let mut excerpt = if key == "dev-server port" {
                evidence_block(repo, key, &def.path, def.line, 1, 1)
            } else {
                evidence_block(repo, key, &def.path, def.line, span, 0)
            };
            if is_env_path(&def.path) {
                for text in &mut excerpt {
                    let name = text.split('=').next().unwrap_or("");
                    let name = name.rsplit('|').next().unwrap_or("");
                    if SECRETISH.is_match(name) || SECRETISH.is_match(key) {
                        *text = MASKED_ASSIGNMENT.replace(text, "=<masked>").into_owned();
                    }
                }
            }
            blocks.extend(excerpt);
        }
        for read in entry.reads.iter().take(2) {
            if seen.insert((read.path.clone(), read.line)) {
                blocks.extend(evidence_block(repo, key, &read.path, read.line, 1, 2));
            }
        }
    }
    if !env_files.is_empty() {
        env_files.sort_unstable();
        head.push(format!(
            "env files in the repo (names only; local .env* are never indexed): {}",
            env_files.iter().take(ENV_FILES_SHOWN).copied().collect::<Vec<_>>().join(", ")
        ));
    }
    head.extend(blocks);
    head.join("\n") + "\n"
}

#[cfg(test)]
mod tests;
