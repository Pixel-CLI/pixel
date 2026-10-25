// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `history-fallback`: a history question whose brief has no history block (no
//! history index, a lookup that missed the window) is answered from git itself.
//!
//! Ported from `probes/history-fallback/probe`. The commits naming a version
//! the prompt quotes, else `git log --grep` on a quoted title, else the
//! commits whose subjects share several words with the prompt, else
//! `log --grep` on the prompt's rarest words and a `-S` pickaxe on the
//! rarest; then the top commit's message, stat, patch (a merge against its
//! first parent) and first tag. Read-only git, deterministic.

use pixel_plugin::ProbeMode;

use super::commit_lookup::{
    self as lookup, by_subject, by_version, git, greps, id_of, quoted, rare_words, subject_words,
};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "history-fallback",
    when_kinds: &["history"],
    skip_kinds: &[],
    when_prompt: Some(
        r"(?i)\b(commits?|changelog|git history|introduced|blame|released?|tagged?)\b",
    ),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Append,
    run,
};

/// Lines of the commit's message, stat and patch shown at most.
const PATCH_LINES: usize = 90;
/// Lines of the message among them.
const MESSAGE_LINES: usize = 10;
/// Characters of one line shown.
const LINE_CHARS: usize = 160;
/// Prompt words the header names.
const HEADER_WORDS: usize = 3;
/// Rare words a `--all-match` search starts with, then one fewer, down to one.
const MATCH_WORDS: [usize; 3] = [3, 2, 1];
/// Commits a search shows at most.
const SHOWN: &str = "6";
/// Commits a pickaxe shows at most.
const PICKED: &str = "5";

fn run(ctx: &Ctx) -> String {
    if ctx
        .brief
        .split('\n')
        .any(|line| line.starts_with("### history"))
    {
        return String::new();
    }
    let prompt = ctx.prompt;
    let named = by_version(ctx, prompt);
    if let Some((_, first)) = named.first() {
        let shown: Vec<&str> = named.iter().map(|(_, line)| line.as_str()).collect();
        return id_of(first).map_or_else(String::new, |sha| {
            show(ctx, &lookup::versions(prompt), &shown, sha)
        });
    }
    for phrase in quoted(prompt) {
        let out = git(
            ctx,
            &[
                "log",
                "-i",
                "--fixed-strings",
                "-n",
                SHOWN,
                "--format=%h %as %s",
                &format!("--grep={phrase}"),
            ],
        );
        let shown: Vec<&str> = out.trim().lines().collect();
        if let Some(sha) = shown.first().and_then(|first| id_of(first)) {
            return show(ctx, &subject_words(&phrase), &shown, sha);
        }
    }
    let named = by_subject(ctx, prompt);
    if let Some((sha, _)) = named.first() {
        let shown: Vec<&str> = named.iter().map(|(_, line)| line.as_str()).collect();
        return show(ctx, &subject_words(prompt), &shown, sha);
    }
    let words = rare_words(ctx, prompt);
    let Some(rarest) = words.first() else {
        return String::new();
    };
    let mut last = 0;
    let mut found = String::new();
    for count in MATCH_WORDS {
        let count = count.min(words.len());
        if count == last {
            continue;
        }
        last = count;
        let greps = greps(&words, count);
        let mut args = vec![
            "log",
            "-i",
            "--all-match",
            "-n",
            SHOWN,
            "--format=%h %as %s",
        ];
        args.extend(greps.iter().map(String::as_str));
        found = git(ctx, &args).trim().to_owned();
        if !found.is_empty() {
            break;
        }
    }
    if found.is_empty() {
        found = git(
            ctx,
            &[
                "log",
                &format!("-S{rarest}"),
                "-n",
                PICKED,
                "--format=%h %as %s",
                "--",
                "*.rs",
                "*.py",
                "*.ts",
                "*.js",
                "*.go",
            ],
        )
        .trim()
        .to_owned();
    }
    let shown: Vec<&str> = found.lines().collect();
    shown
        .first()
        .and_then(|first| id_of(first))
        .map_or_else(String::new, |sha| show(ctx, &words, &shown, sha))
}

/// The blocks of the answer: the commits `shown`, then `sha`'s message, files
/// and patch, and the first tag that holds it.
fn show(ctx: &Ctx, words: &[String], shown: &[&str], sha: &str) -> String {
    let message = git(
        ctx,
        &[
            "show",
            "-s",
            "--format=%h %ad %an%n%B",
            "--date=short",
            sha,
        ],
    );
    let patch = git(
        ctx,
        &[
            "show",
            "--format=",
            "--stat=100",
            "--patch",
            "-U2",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            sha,
        ],
    );
    let tags = git(ctx, &["tag", "--contains", sha, "--sort=creatordate"]);
    let named: Vec<&str> = words
        .iter()
        .take(HEADER_WORDS)
        .map(String::as_str)
        .collect();
    let body: Vec<String> = message
        .trim()
        .lines()
        .take(MESSAGE_LINES)
        .chain(patch.lines())
        .take(PATCH_LINES)
        .map(|line| line.chars().take(LINE_CHARS).collect())
        .collect();
    let mut out = format!(
        "### history — commits matching {}\n{}\n### commit {sha} — message, files and patch\n{}\n",
        named.join(", "),
        shown.join("\n"),
        body.join("\n"),
    );
    if let Some(tag) = tags.split('\n').next().filter(|tag| !tag.is_empty()) {
        out.push_str(&format!("first tag containing it: {tag}\n"));
    }
    out
}

#[cfg(test)]
mod tests;
