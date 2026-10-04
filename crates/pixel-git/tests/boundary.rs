// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The git boundary: `pixel-git` is the only crate that spawns `git` in
//! production code, and inside it only `runner.rs` does. Every other crate —
//! and every other module of `pixel-git` — goes through `GitRunner`, so
//! every call gets the wall-clock timeout, the stdout cap and stderr
//! redaction.
//!
//! Fourteen bare `Command::new("git")` sites had drifted into the other
//! crates (the guard hook, the task sandbox, `pixel status`, `pixel doctor`,
//! `pixel repo-state`, the sniper run) before this test existed; one of them
//! could hang an agent's tool call on a stuck `git status`. This test walks
//! every other crate's `src/` and fails on a spawn outside a `#[cfg(test)]
//! mod`. `pixel-git` is no exception on the inside: a second test walks its
//! own `src/` and fails on any production spawn outside `src/runner.rs`,
//! which is where the bounded primitive lives (`merge-file` used to be
//! spawned directly by `plumbing.rs`, with no timeout, cap or captured
//! stderr).
//!
//! Test code is exempt: fixtures drive real git directly by design (see
//! `.agents/rules/test-hygiene.md`). A `#[cfg(test)]` attribute followed by
//! a `mod` item starts the test region of a file, which runs to the end of
//! the file. That cut is only sound because every file keeps its test
//! modules after its last production item, which a third test enforces:
//! `pixel/src/main.rs` had twelve test modules interleaved with its
//! commands, so everything after the first one, about 5 800 production
//! lines, was exempt from the walk (#528). `pixel-cli`'s `docs_drift` test
//! cuts files at the same point and relies on the same order.

use std::path::{Path, PathBuf};

const SPAWN: &str = "Command::new(\"git\")";

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Byte offset where the file's test region starts: the first
/// `#[cfg(test)]` whose next non-attribute line declares a `mod`. A
/// `#[cfg(test)]` on a lone function does not open the region.
fn test_region_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[i + 1..]
                .iter()
                .find(|l| !l.trim_start().starts_with("#["))
                .map_or("", |l| l.trim_start());
            let item = next
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ");
            if item.starts_with("mod ") {
                return Some(offset);
            }
        }
        offset += line.len() + 1;
    }
    None
}

/// Production lines (1-based) of `text` that spawn git directly.
fn production_spawns(text: &str) -> Vec<usize> {
    let production = test_region_start(text).map_or(text, |end| &text[..end]);
    production
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(SPAWN))
        .map(|(i, _)| i + 1)
        .collect()
}

/// One top-level item of a Rust file.
#[derive(Debug, PartialEq)]
struct TopItem {
    /// 1-based line of the item's first token, after its attributes.
    line: usize,
    /// Whether `#[cfg(test)]` is among its outer attributes.
    cfg_test: bool,
    /// Whether the item is a `mod`, inline or declared (`mod x;`).
    is_mod: bool,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Index just past the literal or comment that starts at `i`, and the
/// newlines it spans; `None` when nothing skippable starts there. Covers
/// `//` and nested `/* */` comments, `"…"` strings with escapes, raw
/// strings (`r#"…"#`, `br"…"`) and char literals (`'{'`, `'\''`), so a
/// bracket or a line inside one is never read as code. A lifetime (`'a`)
/// and a raw identifier (`r#type`) are not literals.
fn skip_literal(chars: &[char], i: usize) -> Option<(usize, usize)> {
    let at = |k: usize| chars.get(k).copied();
    let newlines = |from: usize, to: usize| chars[from..to].iter().filter(|&&c| c == '\n').count();
    let prev_is_ident = i > 0 && is_ident_char(chars[i - 1]);
    match chars[i] {
        '/' if at(i + 1) == Some('/') => {
            let end = (i..chars.len())
                .find(|&k| chars[k] == '\n')
                .unwrap_or(chars.len());
            Some((end, 0))
        }
        '/' if at(i + 1) == Some('*') => {
            let mut depth = 0usize;
            let mut k = i;
            while k < chars.len() {
                if chars[k] == '/' && at(k + 1) == Some('*') {
                    depth += 1;
                    k += 2;
                } else if chars[k] == '*' && at(k + 1) == Some('/') {
                    depth -= 1;
                    k += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            Some((k, newlines(i, k.min(chars.len()))))
        }
        '"' => {
            let mut k = i + 1;
            while k < chars.len() && chars[k] != '"' {
                k += if chars[k] == '\\' { 2 } else { 1 };
            }
            let end = (k + 1).min(chars.len());
            Some((end, newlines(i, end)))
        }
        'r' | 'b' if !prev_is_ident => {
            let start = if chars[i] == 'b' && at(i + 1) == Some('r') {
                i + 2
            } else if chars[i] == 'r' {
                i + 1
            } else {
                return None;
            };
            let hashes = chars[start..].iter().take_while(|&&c| c == '#').count();
            if at(start + hashes) != Some('"') {
                return None;
            }
            let body = start + hashes + 1;
            let closing: Vec<char> = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            let end = (body..chars.len())
                .find(|&k| chars[k..].starts_with(&closing))
                .map_or(chars.len(), |k| k + closing.len());
            Some((end, newlines(i, end)))
        }
        '\'' if at(i + 1) == Some('\\') => {
            let end = (i + 3..chars.len())
                .find(|&k| chars[k] == '\'')
                .map_or(chars.len(), |k| k + 1);
            Some((end, 0))
        }
        '\'' if at(i + 2) == Some('\'') => Some((i + 3, 0)),
        _ => None,
    }
}

/// The top-level items of `text`, in source order. Brackets are counted
/// outside literals and comments only (see [`skip_literal`]), so a Rust
/// fixture held in a raw string inside a test module, with its own `fn`
/// and `}` lines at column 0, stays part of that module. An item starts at
/// the first token after the previous one ended: on a `;` or a closing
/// `}` back at depth 0. Attributes before it are collected, not counted as
/// items.
fn top_level_items(text: &str) -> Vec<TopItem> {
    let chars: Vec<char> = text.chars().collect();
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut between_items = true;
    let mut attr_start: Option<usize> = None;
    let mut cfg_test = false;
    let mut line = 1;
    let mut i = 0;
    while i < chars.len() {
        if let Some((end, lines)) = skip_literal(&chars, i) {
            line += lines;
            i = end;
            continue;
        }
        let c = chars[i];
        if depth == 0 && between_items && attr_start.is_none() {
            if c == '#' {
                attr_start = Some(i);
            } else if is_ident_char(c) {
                let head: String = chars[i..].iter().take_while(|&&ch| ch != '\n').collect();
                let item = head
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub(super) ")
                    .trim_start_matches("pub ");
                items.push(TopItem {
                    line,
                    cfg_test,
                    is_mod: item.starts_with("mod "),
                });
                between_items = false;
                cfg_test = false;
            }
        }
        match c {
            '\n' => line += 1,
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(start) = attr_start.take() {
                        let attr: String = chars[start..=i]
                            .iter()
                            .filter(|ch| !ch.is_whitespace())
                            .collect();
                        cfg_test |= attr == "#[cfg(test)]";
                    } else if c == '}' {
                        between_items = true;
                    }
                }
            }
            ';' if depth == 0 => between_items = true,
            _ => {}
        }
        i += 1;
    }
    items
}

/// Lines (1-based) of the production items that follow the first
/// `#[cfg(test)] mod` of `text`.
fn production_after_test_modules(text: &str) -> Vec<usize> {
    let items = top_level_items(text);
    let Some(first_test_mod) = items.iter().position(|item| item.cfg_test && item.is_mod) else {
        return Vec::new();
    };
    items[first_test_mod..]
        .iter()
        .filter(|item| !item.cfg_test)
        .map(|item| item.line)
        .collect()
}

#[test]
fn test_modules_follow_every_production_item() {
    // The two walks in this file and `docs_drift` in pixel-cli read a file
    // up to its first test module and skip the rest. A production item
    // placed after that point is invisible to all three.
    let crates = crates_dir();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    assert!(files.len() > 50, "walked too few files: {}", files.len());

    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for line in production_after_test_modules(&text) {
            offenders.push(format!(
                "{}:{line}",
                file.strip_prefix(&crates).unwrap().display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "production items after the file's first #[cfg(test)] mod; move the test modules to the end of the file:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn production_after_a_test_module_is_reported_by_line() {
    let text = [
        "use std::process::Command;",
        "#[cfg(test)]",
        "static LOCK: () = ();",
        "fn before() {}",
        "#[cfg(test)]",
        "#[path = \"x_tests.rs\"]",
        "mod x_tests;",
        "pub(crate) fn after(",
        "    a: u8,",
        ") -> u8 {",
        "    a",
        "}",
        "#[cfg(test)]",
        "mod tests {}",
        "const LAST: &[&str] = &[",
        "    \"x\",",
        "];",
    ]
    .join("\n");
    // The cfg(test) static before the first test module is not an offender;
    // the multi-line signature counts once, at its first line.
    assert_eq!(production_after_test_modules(&text), vec![8, 15]);

    let ordered = [
        "fn prod() {}",
        "#[cfg(test)]",
        "mod tests {}",
        "#[cfg(test)]",
        "fn helper() {}",
        "#[cfg(test)]",
        "pub(crate) mod testutil;",
    ]
    .join("\n");
    assert_eq!(production_after_test_modules(&ordered), Vec::<usize>::new());
    assert_eq!(
        production_after_test_modules("fn only() {}\n"),
        Vec::<usize>::new()
    );
}

#[test]
fn literals_and_comments_inside_a_test_module_are_not_items() {
    let text = [
        "#[cfg(test)]",
        "mod tests {",
        "    const SRC: &str = r#\"",
        "fn main() {",
        "}",
        "fn not_an_item() {}",
        "\"#;",
        "    const OPEN: char = '{';",
        "    const QUOTE: char = '\\'';",
        "    const ESCAPED: &str = \"}\\\"}\";",
        "    fn borrow<'a>(s: &'a str) -> &'a str { s }",
        "    /* } /* nested } */ fn hidden() {} */",
        "    // }",
        "    const BYTES: &[u8] = br\"}\";",
        "}",
        "#[cfg(test)]",
        "mod more {}",
    ]
    .join("\n");
    assert_eq!(
        top_level_items(&text),
        vec![
            TopItem {
                line: 2,
                cfg_test: true,
                is_mod: true
            },
            TopItem {
                line: 17,
                cfg_test: true,
                is_mod: true
            },
        ]
    );
}

#[test]
fn only_pixel_git_spawns_git_in_production_code() {
    let crates = crates_dir();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap() {
        let krate = entry.unwrap().path();
        let name = krate.file_name().unwrap().to_string_lossy().into_owned();
        // The bench crate is not shipped; pixel-git is the boundary itself.
        if name == "pixel-git" || name == "pixel-bench" {
            continue;
        }
        let src = krate.join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    assert!(files.len() > 50, "walked too few files: {}", files.len());

    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for line in production_spawns(&text) {
            offenders.push(format!(
                "{}:{line}",
                file.strip_prefix(&crates).unwrap().display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "git spawned outside pixel-git; use pixel_git::GitRunner (timeout, output cap, redaction):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn pixel_git_spawns_git_only_in_the_runner() {
    // The crate is the boundary for everyone else; its own boundary is
    // `GitRunner`'s bounded primitive in `runner.rs`. A bare spawn in
    // another module has no timeout, no output cap and no captured stderr,
    // and the cross-crate test above (which skips `pixel-git` entirely)
    // could not see it.
    let krate = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = krate.join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("runner.rs")),
        "walked {files:?} under {}: the walk is broken, not the code",
        src.display()
    );

    let mut offenders = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        if name == "runner.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for line in production_spawns(&text) {
            offenders.push(format!("src/{name}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "git spawned outside src/runner.rs; use GitRunner (timeout, output cap, redaction):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn a_test_module_exempts_only_what_follows_it() {
    let text = [
        "fn prod() {",
        "    Command::new(\"git\");",
        "}",
        "#[cfg(test)]",
        "fn helper_only_in_tests() {",
        "    Command::new(\"git\");",
        "}",
        "#[cfg(test)]",
        "mod tests {",
        "    fn fixture() { Command::new(\"git\"); }",
        "}",
    ]
    .join("\n");
    // The lone cfg(test) fn at line 5 is still production for this test's
    // purposes (it is not a module); only the `mod tests` region is exempt.
    assert_eq!(production_spawns(&text), vec![2, 6]);

    let no_tests = "fn prod() {\n    Command::new(\"git\");\n}\n";
    assert_eq!(production_spawns(no_tests), vec![2]);

    let pub_mod = [
        "#[cfg(test)]",
        "pub(crate) mod testutil {",
        "    fn git() { Command::new(\"git\"); }",
        "}",
    ]
    .join("\n");
    assert_eq!(production_spawns(&pub_mod), Vec::<usize>::new());
}
