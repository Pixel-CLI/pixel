// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-k-member-manifest`: a question about the content and the
//! requirements of one named workspace member ("what does the x crate contain
//! / depend on").
//!
//! Ported from `probes/cand-k-member-manifest/probe`. The member is found
//! from the manifests when the step runs: a tracked manifest (`Cargo.toml`,
//! `package.json`, `pyproject.toml`, `go.mod`) in a directory whose name
//! equals the word before "crate/package/module/library" in the prompt. It
//! prints that manifest (identity noise such as authors, urls and licence
//! lines cut) with its line numbers, so features, optional and dev
//! dependencies and deprecation comments are visible, and the tracked files
//! of the member's directory. It also asks to drop the brief's blocks outside
//! that directory: as in the script, the pattern is a negative lookahead,
//! which the host's regex engine does not read, so the host drops nothing.
//! Read-only, nothing printed when no member is named.

use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{escape, head, last_segment, splitlines, universal_newlines};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-k-member-manifest",
    when_kinds: &[],
    skip_kinds: &["history", "impact", "flow"],
    when_prompt: Some(
        r"(?i)\b(crates?|packages?|modules?|libraries|library|libs?)\b.*\b(contains?|depend\w*|dependenc\w+|features?|made of|exports?|provides?|includes?)\b|\b(contains?|depend\w*|features?)\b.*\b(crates?|packages?|modules?|libs?)\b",
    ),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Append,
    run,
};

const MANIFESTS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
const MAX_FILES: usize = 24;
const MAX_MANIFEST: usize = 70;
/// Bytes of a manifest that are read; the script has no cap.
const READ_LIMIT: u64 = 2_000_000;
/// Words before "crate" that name nothing.
const NOT_NAMES: [&str; 7] = ["this", "that", "each", "which", "other", "the", "a"];

static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:the\s+)?([A-Za-z][\w.-]*)\s+(?:crate|package|module|library|lib)\b")
        .expect("the named-member pattern is a literal")
});
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(authors|documentation|homepage|repository|keywords|categories|license|license-file|readme|exclude|include|publish|bugs|author|funding|edition(\.workspace)?|rust-version(\.workspace)?)\s*[=:]",
    )
    .expect("the identity-line pattern is a literal")
});

fn run(ctx: &Ctx) -> String {
    let Some(files) = ctx.files() else {
        return String::new();
    };
    let manifests: Vec<&String> = files
        .iter()
        .filter(|file| file.contains('/') && MANIFESTS.contains(&last_segment(file)))
        .collect();
    let names = NAMED
        .captures_iter(ctx.prompt)
        .map(|caps| caps[1].to_lowercase())
        .filter(|name| !NOT_NAMES.contains(&name.as_str()));
    for name in names {
        let Some(hit) = manifests
            .iter()
            .find(|path| last_segment(directory(path)).to_lowercase() == name)
        else {
            continue;
        };
        let member = directory(hit);
        let Some(text) = ctx.read(hit, READ_LIMIT) else {
            return String::new();
        };
        let text = universal_newlines(&text);
        let lines = splitlines(&text);
        let shown: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !NOISE.is_match(line))
            .take(MAX_MANIFEST)
            .map(|(index, line)| format!("{}| {}", index + 1, head(line, 140)))
            .collect();
        let prefix = format!("{member}/");
        let inside: Vec<&str> = files
            .iter()
            .filter_map(|file| file.strip_prefix(prefix.as_str()))
            .collect();
        let more = inside.len().saturating_sub(MAX_FILES);
        let mut listed: Vec<String> = inside[..inside.len() - more]
            .iter()
            .map(ToString::to_string)
            .collect();
        if more > 0 {
            listed.push(format!("+{more} more"));
        }
        return format!(
            "drop: ^(?!{}[/ ])\n### {hit}:1-{} — manifest of {name} (identity lines cut)\n{}\n### {member}/ — {} tracked files\n{}\n",
            escape(member),
            lines.len(),
            shown.join("\n"),
            inside.len(),
            listed.join(", ")
        );
    }
    String::new()
}


/// `os.path.dirname` of a path with a slash.
fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}


#[cfg(test)]
mod tests;
