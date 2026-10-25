// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The lines that may define or read a configuration key: the script ran one
//! `rg` over the tree; the step walks it with the `ignore` crate `rg` is built
//! on, and reads the git-tracked environment templates beside it.

use std::path::Path;
use std::sync::LazyLock;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::Regex;

use super::super::text::{splitlines, universal_newlines};
use super::repo::{MAX_FILE_BYTES, Repo};

/// A line of a file: its path, its number (counted on `\n`) and its text.
pub(super) struct Row {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
}

/// ripgrep's `--max-columns`, in bytes, the line terminator included.
pub(super) const SCAN_MAX_COLUMNS: usize = 300;
/// ripgrep reads a file in buffers of this size and drops the one holding a NUL.
const BUFFER: usize = 64 * 1024;

pub(super) const CODE_EXT: [&str; 14] = [
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".py", ".rs", ".go", ".rb",
    ".vue", ".svelte",
];
const OTHER_EXT: [&str; 8] = [
    ".json", ".mts", ".yml", ".yaml", ".toml", ".mdx", ".md", ".d.ts",
];
const GLOBS: [&str; 17] = [
    "!.git",
    "!.pixel",
    "!.env*",
    "!*.env",
    "!node_modules",
    "!target",
    "!dist",
    "!build",
    "!vendor",
    "!*.lock",
    "!yarn.lock",
    "!package-lock.json",
    "!*.min.js",
    "!*.map",
    "!*.snap",
    "!*.svg",
    "!*.json5",
];
const PATTERNS: [&str; 13] = [
    r"process\.env|import\.meta\.env",
    r"(?:VITE|REACT_APP|NEXT_PUBLIC|NUXT|EXPO_PUBLIC)_[A-Z0-9_]{2,}",
    r"os\.(?:environ|getenv)",
    r#"env::var|\benv\s*=\s*"|env!\("#,
    r"os\.(?:Getenv|LookupEnv)",
    r"\bENV(?:\[|\.fetch)",
    r"^\s*[A-Z][A-Z0-9_]{2,}\??\s*:\s*[A-Za-z<>\[\]| ]+;",
    r"^(?:export\s+)?[A-Z][A-Z0-9_]*=",
    r"^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:export\s+)?(?:const|static)\s+[A-Z][A-Z0-9_]{2,}\b",
    r"^[A-Z][A-Z0-9_]{2,}\s*(?::[^=]+)?=",
    r"\bport\s*[:=]|--port|\bPORT=",
    r#"\b[\w.]+\.(?:set|setDefault|set_default)\(\s*['"][\w .-]+['"]\s*,"#,
    r"\b(?:localStorage|sessionStorage)\.(?:getItem|setItem)\s*\(",
];

static LINE: LazyLock<Regex> = LazyLock::new(|| {
    let union: Vec<String> = PATTERNS.iter().map(|pattern| format!("(?:{pattern})")).collect();
    Regex::new(&union.join("|")).expect("the scan patterns are literals")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(test|tests|__tests__|fixtures?|node_modules)/|\.(test|spec)\.")
        .expect("the test-path pattern is a literal")
});
static TEMPLATE_ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\.env\.(?:example|sample|template|dist|defaults?)|(?:example|sample)\.env)$")
        .expect("the template pattern is a literal")
});
static MODE_ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\.env\.(?:development|production|local\.example)$")
        .expect("the env-mode pattern is a literal")
});
pub(super) static ENV_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:export\s+)?([A-Z][A-Z0-9_]*)=(.*)$").expect("the env line is a literal")
});

/// The text of a line as the script saw it: its trailing `\r` gone, cut at the
/// first character `str.splitlines()` ends a line at.
fn row_text(line: &str) -> String {
    let text = line.replace("\r\n", "\n").replace('\r', "\n");
    splitlines(&text).first().copied().unwrap_or("").to_owned()
}

/// The bytes ripgrep searches: a NUL in its first buffer ends the file before
/// any match; a later one drops the buffer holding it and what follows.
pub(super) fn searchable(bytes: &[u8]) -> &[u8] {
    let Some(nul) = bytes.iter().position(|byte| *byte == 0) else {
        return bytes;
    };
    if nul < BUFFER {
        return &[];
    }
    let head = &bytes[..nul / BUFFER * BUFFER];
    head.iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[], |end| &head[..=end])
}

/// Rows of one file's bytes that match [`PATTERNS`], long lines left out.
pub(super) fn matching_rows(path: &str, bytes: &[u8], rows: &mut Vec<Row>) {
    for (index, raw) in searchable(bytes).split(|byte| *byte == b'\n').enumerate() {
        if raw.len() + 1 > SCAN_MAX_COLUMNS {
            continue;
        }
        let line = String::from_utf8_lossy(raw);
        if LINE.is_match(&line) {
            rows.push(Row {
                path: path.to_owned(),
                line: index + 1,
                text: row_text(&line),
            });
        }
    }
}

/// Every candidate row of the tree, sorted by path and line; `None` when the
/// brief's time is up.
pub(super) fn scan(repo: &Repo) -> Option<Vec<Row>> {
    let root = &repo.root;
    let mut globs = OverrideBuilder::new(root);
    for glob in GLOBS {
        globs.add(glob).ok()?;
    }
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .max_filesize(Some(MAX_FILE_BYTES))
        .overrides(globs.build().ok()?)
        .add_custom_ignore_filename(".rgignore");
    let mut rows = Vec::new();
    for entry in walk.build() {
        if repo.timed_out() {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(rel) = entry.path().strip_prefix(root).ok().and_then(Path::to_str) else {
            continue;
        };
        // Only these extensions reach the registry, and no test path does.
        if !(CODE_EXT.iter().chain(&OTHER_EXT)).any(|ext| rel.ends_with(ext))
            || TEST_PATH.is_match(rel)
        {
            continue;
        }
        if let Ok(bytes) = std::fs::read(entry.path()) {
            matching_rows(rel, &bytes, &mut rows);
        }
    }
    rows.extend(env_rows(repo));
    rows.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Some(rows)
}

/// The tracked environment templates and mode files, read like the script:
/// regular files of at most [`MAX_FILE_BYTES`], lines of at most
/// [`SCAN_MAX_COLUMNS`] bytes that assign an upper-case name.
fn env_rows(repo: &Repo) -> Vec<Row> {
    let mut rows = Vec::new();
    for rel in repo.tracked() {
        let base = rel.rsplit('/').next().unwrap_or(rel);
        if !TEMPLATE_ENV.is_match(base) && !MODE_ENV.is_match(base) {
            continue;
        }
        let full = repo.root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if meta.file_type().is_symlink() || !full.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(raw) = std::fs::read(&full) else {
            continue;
        };
        let text = universal_newlines(&String::from_utf8_lossy(&raw));
        for (index, line) in splitlines(&text).into_iter().enumerate() {
            if line.len() <= SCAN_MAX_COLUMNS && ENV_LINE.is_match(line) {
                rows.push(Row {
                    path: rel.clone(),
                    line: index + 1,
                    text: line.to_owned(),
                });
            }
        }
    }
    rows
}
