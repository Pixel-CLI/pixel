// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The tracked source files that hold a narrow shape: what `rg -l` over the
//! working tree gave the script, restricted to the files it kept (tracked,
//! regular, inside the repository, at most 400 KiB).

use std::time::Instant;

use ignore::overrides::{Override, OverrideBuilder};
use regex::Regex;

use super::repo::Repo;
use super::scan::searchable;

const SCOPED_SCAN_MAX_FILES: usize = 160;
const EXCLUDES: [&str; 7] = [
    "!.git",
    "!target",
    "!node_modules",
    "!tests",
    "!test",
    "!__tests__",
    "!*.test.*",
];

/// Whether `rel`, or a directory above it, is left out by the globs.
fn excluded(globs: &Override, rel: &str) -> bool {
    rel.match_indices('/')
        .any(|(at, _)| globs.matched(&rel[..at], true).is_ignore())
        || globs.matched(rel, false).is_ignore()
}

/// Whether one line of `bytes` (as ripgrep would search them) matches.
pub(super) fn any_line_matches(bytes: &[u8], pattern: &Regex) -> bool {
    searchable(bytes)
        .split(|byte| *byte == b'\n')
        .any(|line| pattern.is_match(&String::from_utf8_lossy(line)))
}

/// The paths, sorted, of at most 160 tracked files ending in one of `suffixes`
/// with a line matching `pattern`; empty when the tracked listing is too
/// large, and cut short at `deadline`.
pub(super) fn scoped_source_files(
    repo: &Repo,
    pattern: &Regex,
    suffixes: &[&str],
    deadline: Instant,
) -> Vec<String> {
    if repo.inventory().is_none() {
        return Vec::new();
    }
    let mut builder = OverrideBuilder::new(&repo.root);
    let suffix_globs: Vec<String> = suffixes.iter().map(|suffix| format!("*{suffix}")).collect();
    for glob in EXCLUDES.iter().copied().chain(suffix_globs.iter().map(String::as_str)) {
        if builder.add(glob).is_err() {
            return Vec::new();
        }
    }
    let Ok(globs) = builder.build() else {
        return Vec::new();
    };
    let mut paths: Vec<&str> = repo.tracked().iter().map(String::as_str).collect();
    paths.sort_unstable();
    let mut found = Vec::new();
    for rel in paths {
        if Instant::now() >= deadline || repo.timed_out() || found.len() >= SCOPED_SCAN_MAX_FILES {
            break;
        }
        if !suffixes.iter().any(|suffix| rel.ends_with(suffix)) || excluded(&globs, rel) {
            continue;
        }
        let Some((resolved, _)) = repo.checked(rel) else {
            continue;
        };
        if std::fs::read(resolved).is_ok_and(|bytes| any_line_matches(&bytes, pattern)) {
            found.push(rel.to_owned());
        }
    }
    found
}
