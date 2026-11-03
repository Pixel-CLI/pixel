// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A browser preference a prompt names: the `localStorage` or `sessionStorage`
//! key it is read from and written to, and where an imported key constant is
//! declared.

use std::collections::BTreeSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::super::text::{splitlines, head};
use super::repo::{MAX_FILE_BYTES, Repo, resolve};
use super::scopes::{SCOPED_BLOCK_MAX_LINES, SCOPED_EVIDENCE_MAX, rust_scopes, source_range};
use super::search::scoped_source_files;
use super::words::{BOUND_WORDS, PERSISTENCE_WORDS, WEAK, key_tokens, stems, words_of};

const SCOPED_SCAN_MAX_MS: u64 = 450;
const JS_SUFFIXES: [&str; 8] = [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"];
const KEY_EXTENSIONS: [&str; 6] = [".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"];
const SENSITIVE_STORAGE_WORDS: [&str; 9] = [
    "key", "secret", "token", "password", "passwd", "credential", "private", "signature", "config",
];

macro_rules! pattern {
    ($name:ident, $text:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($text).expect("a pattern of the browser lane is a literal"));
    };
}

pattern!(
    INTENT,
    r"(?i)\b(persist\w*|session\w*|remember\w*|restore\w*|startup|initial|load\w*)\b"
);
pattern!(
    STORAGE,
    r"\b(?:localStorage|sessionStorage)\.(?:getItem|setItem)\s*\("
);
pattern!(
    STORAGE_CALL,
    r"(?i)\b(localStorage|sessionStorage)\.(getItem|setItem)\s*\(\s*([^,\n)]+)"
);
pattern!(AS_KEYWORD, r"(?i)\bas\b");
pattern!(MEMBER, r"\b([A-Z][A-Z0-9_]*)\.([A-Z][A-Z0-9_]*)\b");
pattern!(IMPORT, r#"import\s*\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#);

/// Whether a text has a token that names a key, a secret or the like.
fn is_sensitive(text: &str) -> bool {
    key_tokens(text)
        .iter()
        .any(|token| SENSITIVE_STORAGE_WORDS.contains(&token.as_str()))
}

fn key_words(expression: &str) -> BTreeSet<String> {
    let head = AS_KEYWORD.splitn(expression, 2).next().unwrap_or("");
    stems(
        head.split(|ch: char| !ch.is_ascii_alphabetic())
            .filter(|run| !run.is_empty())
            .map(str::to_lowercase),
    )
}

/// `Path.with_suffix(ext)` of the last component.
fn with_suffix(name: &str, ext: &str) -> String {
    let dot = name.rfind('.');
    match dot {
        Some(at) if at > 0 && at + 1 < name.len() => format!("{}{ext}", &name[..at]),
        _ => format!("{name}{ext}"),
    }
}

/// The declaration of `OBJ.MEMBER` in the file `OBJ` is imported from, when
/// that import is relative and the file tracked.
fn key_declaration(repo: &Repo, source_path: &str, source: &str, expression: &str) -> Vec<String> {
    let Some(member) = MEMBER.captures(expression) else {
        return Vec::new();
    };
    let (object, name) = (member[1].to_owned(), member[2].to_owned());
    let Some(import) = IMPORT.captures(source) else {
        return Vec::new();
    };
    let imported = Regex::new(&format!(r"\b{}\b", regex::escape(&object)));
    if !imported.is_ok_and(|rx| rx.is_match(&import[1])) {
        return Vec::new();
    }
    let module = import[2].to_owned();
    if !module.starts_with('.') {
        return Vec::new();
    }
    let Some(inventory) = repo.inventory() else {
        return Vec::new();
    };
    // `(root / source_path).parent / module`: `.` and empty parts vanish, `..` stays.
    let mut parts: Vec<&str> = source_path.split('/').collect();
    parts.pop();
    parts.extend(module.split('/').filter(|part| !part.is_empty() && *part != "."));
    let Some(last) = parts.pop() else {
        return Vec::new();
    };
    let Ok(scope) = Regex::new(&format!(r"(?m)\b(?:const|let)\s+{}\s*=\s*\{{", regex::escape(&object))) else {
        return Vec::new();
    };
    let Ok(entry) = Regex::new(&format!(r"(?m)^\s*{}\s*:\s*([^,\n]+)", regex::escape(&name))) else {
        return Vec::new();
    };
    for ext in KEY_EXTENSIONS {
        let mut rel_parts = parts.clone();
        let file = with_suffix(last, ext);
        rel_parts.push(&file);
        let rel = rel_parts.join("/");
        let target = repo.root.join(&rel);
        if !resolve(&target).starts_with(&repo.root) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if meta.file_type().is_symlink() || !target.is_file() {
            continue;
        }
        if !inventory.contains(&rel) || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(raw) = std::fs::read(&target) else {
            continue;
        };
        let text = super::super::text::universal_newlines(&String::from_utf8_lossy(&raw));
        for (_, end, body_start) in rust_scopes(&text, &scope) {
            let body = &text[body_start..end - 1];
            if let Some(found) = entry.find(body) {
                let absolute = body_start + found.start();
                let lines = source_range(
                    &rel,
                    &text,
                    absolute,
                    absolute + found.len(),
                    "imported storage-key declaration",
                );
                if !is_sensitive(&name) {
                    return lines;
                }
                return lines
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| {
                        if index == 0 {
                            line
                        } else {
                            format!("{}| <masked>", line.split('|').next().unwrap_or(""))
                        }
                    })
                    .collect();
            }
        }
    }
    Vec::new()
}

struct Best {
    rel: String,
    key: String,
    source: String,
    first: usize,
    last: usize,
}

/// One subject-matched storage key that is both read and written, quoted.
pub(super) fn evidence(repo: &Repo, prompt: &str) -> String {
    if !INTENT.is_match(prompt) {
        return String::new();
    }
    let subject: BTreeSet<String> = stems(words_of(prompt))
        .into_iter()
        .filter(|word| {
            !WEAK.contains(word) && !PERSISTENCE_WORDS.contains(word) && !BOUND_WORDS.contains(word)
        })
        .collect();
    if subject.is_empty() {
        return String::new();
    }
    let deadline = Instant::now() + Duration::from_millis(SCOPED_SCAN_MAX_MS);
    // The candidate with the smallest `(path, first call, key)`.
    let mut best: Option<Best> = None;
    for rel in scoped_source_files(repo, &STORAGE, &JS_SUFFIXES, deadline) {
        if Instant::now() >= deadline {
            break;
        }
        let Some(source) = repo.source_text(&rel) else {
            continue;
        };
        // `(storage, mode, key, offset)` of each call whose key names the subject.
        let mut calls: Vec<(String, String, String, usize)> = Vec::new();
        for caps in STORAGE_CALL.captures_iter(&source) {
            let key = caps[3].trim().to_owned();
            if key_words(&key).is_disjoint(&subject) {
                continue;
            }
            calls.push((
                caps[1].to_lowercase(),
                caps[2].to_lowercase(),
                key,
                caps.get(0).expect("a match has a whole").start(),
            ));
        }
        let storages: BTreeSet<&String> = calls.iter().map(|call| &call.0).collect();
        for storage in storages {
            let keys: BTreeSet<&String> =
                calls.iter().filter(|call| &call.0 == storage).map(|call| &call.2).collect();
            for key in keys {
                let same: Vec<&(String, String, String, usize)> =
                    calls.iter().filter(|call| &call.0 == storage && &call.2 == key).collect();
                let reads = same.iter().any(|call| call.1 == "getitem");
                let writes = same.iter().any(|call| call.1 == "setitem");
                if !(reads && writes) {
                    continue;
                }
                let first = same.iter().map(|call| call.3).min().unwrap_or(0);
                let last = same.iter().map(|call| call.3).max().unwrap_or(0);
                let better = best
                    .as_ref()
                    .is_none_or(|b| (&rel, first, key) < (&b.rel, b.first, &b.key));
                if better {
                    best = Some(Best {
                        rel: rel.clone(),
                        key: key.clone(),
                        source: source.clone(),
                        first,
                        last,
                    });
                }
            }
        }
    }
    let Some(Best { rel, key, source, first, last }) = best else {
        return String::new();
    };
    let lines = splitlines(&source);
    let first_line = source[..first].matches('\n').count() + 1;
    let last_line = source[..last].matches('\n').count() + 1;
    let lo = first_line.saturating_sub(3).max(1);
    let mut hi = lines.len().min(last_line + 5);
    if hi + 1 > lo + SCOPED_BLOCK_MAX_LINES {
        hi = lo + SCOPED_BLOCK_MAX_LINES - 1;
    }
    let mut out = vec![format!(
        "### {rel}:{lo}-{hi} — browser preference read/write using the same subject-matched storage key; active runtime conditions unknown"
    )];
    let sensitive_key = is_sensitive(&key);
    for number in lo..=hi {
        let line = head(lines[number - 1], 150);
        out.push(format!(
            "{number}| {}",
            if sensitive_key || is_sensitive(line) { "<masked>" } else { line }
        ));
    }
    out.extend(key_declaration(repo, &rel, &source, &key));
    head(&(out.join("\n") + "\n"), SCOPED_EVIDENCE_MAX).to_owned()
}
