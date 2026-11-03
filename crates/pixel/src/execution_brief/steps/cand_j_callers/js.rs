// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What the caller fallbacks of `cand-j-callers` know about JavaScript and
//! TypeScript source: a mask that blanks comments and strings, brace
//! matching, which names a barrel re-exports and which imports bind a
//! factory to the module that defines it.
//!
//! Every function mirrors the script's of the same name. Masked text keeps the
//! byte offsets of the source: a blanked character becomes as many spaces as
//! it has bytes, so an offset found in the mask is one in the source.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::super::text::suffix;

/// `re.escape` for a pattern of the regex crate.
fn escaped(text: &str) -> String {
    regex::escape(text)
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the pattern is built from escaped text")
}

/// Extensions tried after a module path without one.
const SOURCE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".rs", ".py", ".go", ".rb",
];
/// Extensions of the files whose lines are read masked.
const SCRIPT_EXTENSIONS: &[&str] = &[".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"];
/// Completed export walks kept.
const MAX_EXPORT_CACHE_ENTRIES: usize = 8_192;

static IMPORT_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"(?s)\bimport\s*\{([^}]+)\}\s*from\s*(?:'([^'"]+)'|"([^'"]+)")"#)
});
static IMPORT_AT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?s)^import\s*\{([^}]+)\}\s*from\s*"));
static AS: LazyLock<Regex> = LazyLock::new(|| pattern(r"\s+as\s+"));
static NAMED_EXPORT: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)\bexport\s*\{([^}]+)\}"));
static EXPORT_FROM: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?s)\bexport\s*\{([^}]+)\}\s*from\b"));
static EXPORT_STAR: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bexport\s*\*\s*from\b"));
static FROM_AT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*from\b"));
static QUOTED_AT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"^\s*(?:'([^'"]+)'|"([^'"]+)")"#));
static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[A-Za-z_$][\w$]*$"));
static CONFIG_NAME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(^|/)(?:tsconfig|jsconfig)[^/]*\.json$"));
static HELPER: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\bfunction\s+([A-Za-z_$][\w$]*)\s*\(\s*([A-Za-z_$][\w$]*)\s*,\s*([A-Za-z_$][\w$]*)\s*,\s*([A-Za-z_$][\w$]*)\s*\)\s*\{",
    )
});

/// The blank-or-keep decision for each character of `source`, spelled as the
/// script spells its state machine.
pub(super) fn code_mask(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let count = chars.len();
    let mut blank = vec![false; count];
    let (mut at, mut state) = (0, 'c');
    let starts = |at: usize, a: char, b: char| chars.get(at) == Some(&a) && chars.get(at + 1) == Some(&b);
    while at < count {
        match state {
            'c' => {
                if starts(at, '/', '/') {
                    (blank[at], blank[at + 1]) = (true, true);
                    state = 'l';
                    at += 2;
                    continue;
                }
                if starts(at, '/', '*') {
                    (blank[at], blank[at + 1]) = (true, true);
                    state = 'b';
                    at += 2;
                    continue;
                }
                if matches!(chars[at], '"' | '\'' | '`') {
                    state = chars[at];
                    blank[at] = true;
                    at += 1;
                    continue;
                }
            }
            'l' => {
                if chars[at] == '\n' {
                    state = 'c';
                } else {
                    blank[at] = true;
                }
            }
            'b' => {
                if starts(at, '*', '/') {
                    (blank[at], blank[at + 1]) = (true, true);
                    state = 'c';
                    at += 2;
                    continue;
                }
                if chars[at] != '\n' {
                    blank[at] = true;
                }
            }
            quote => {
                if chars[at] == '\\' {
                    blank[at] = true;
                    if chars.get(at + 1).is_some_and(|next| *next != '\n') {
                        blank[at + 1] = true;
                    }
                    at += 2;
                    continue;
                }
                if chars[at] == quote {
                    state = 'c';
                } else if chars[at] != '\n' {
                    blank[at] = true;
                }
            }
        }
        at += 1;
    }
    let mut out = String::with_capacity(source.len());
    for (ch, blanked) in chars.into_iter().zip(blank) {
        if blanked && ch != '\n' {
            out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
        } else {
            out.push(ch);
        }
    }
    out
}

/// Opening brace offset -> its closing brace offset.
pub(super) fn braces(mask: &str) -> HashMap<usize, usize> {
    let (mut stack, mut matches) = (Vec::new(), HashMap::new());
    for (at, ch) in mask.char_indices() {
        if ch == '{' {
            stack.push(at);
        } else if ch == '}'
            && let Some(open) = stack.pop()
        {
            matches.insert(open, at);
        }
    }
    matches
}

/// Brace depth before each byte offset, never below zero (`top_level`).
struct Depths(Vec<u32>);

impl Depths {
    fn new(mask: &str) -> Self {
        let mut depth = 0_u32;
        let mut out = Vec::with_capacity(mask.len() + 1);
        for byte in mask.bytes() {
            out.push(depth);
            match byte {
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        out.push(depth);
        Self(out)
    }

    fn top_level(&self, at: usize) -> bool {
        self.0.get(at).copied() == Some(0)
    }
}

/// A function the script found: name, start and end offsets, masked body.
pub(super) struct Assigned {
    pub(super) name: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) body: String,
}

/// Direct top-level `receiver.method = function` bodies.
pub(super) fn assigned_functions(source: &str, receiver: &str) -> Vec<Assigned> {
    let mask = code_mask(source);
    let braces = braces(&mask);
    let depths = Depths::new(&mask);
    let receiver = escaped(receiver);
    let finder = pattern(&format!(
        r"\b{receiver}\s*\.\s*([A-Za-z_$][\w$]*)\s*=\s*(?:{receiver}\s*\.\s*[A-Za-z_$][\w$]*\s*=\s*)?function\b"
    ));
    let mut out = Vec::new();
    for found in finder.captures_iter(&mask) {
        let whole = found.get(0).expect("group 0");
        if !depths.top_level(whole.start()) {
            continue;
        }
        let Some(open) = mask[whole.end()..].find('{').map(|at| whole.end() + at) else {
            continue;
        };
        let Some(close) = braces.get(&open) else {
            continue;
        };
        out.push(Assigned {
            name: found[1].to_owned(),
            start: whole.start(),
            end: close + 1,
            body: mask[open + 1..*close].to_owned(),
        });
    }
    out
}

/// Getter bodies defined on `receiver` by a local helper that proves it:
/// `(name, start, end)`.
pub(super) fn proven_getter_functions(
    source: &str,
    receiver: &str,
    queried: &str,
) -> Vec<(String, usize, usize)> {
    let mask = code_mask(source);
    let braces = braces(&mask);
    let depths = Depths::new(&mask);
    let mut helpers: Vec<String> = Vec::new();
    for found in HELPER.captures_iter(&mask) {
        let whole = found.get(0).expect("group 0");
        if !depths.top_level(whole.start()) {
            continue;
        }
        let Some(open) = mask[whole.end() - 1..].find('{').map(|at| whole.end() - 1 + at) else {
            continue;
        };
        let Some(close) = braces.get(&open) else {
            continue;
        };
        let (object, name, getter) = (&found[2], &found[3], &found[4]);
        let defines = pattern(&format!(
            r"(?s)Object\s*\.\s*defineProperty\s*\(\s*{}\s*,\s*{}\s*,\s*\{{.*?\bget\s*:\s*{}\b",
            escaped(object),
            escaped(name),
            escaped(getter)
        ));
        if defines.is_match(&mask[open + 1..*close]) {
            helpers.push(found[1].to_owned());
        }
    }
    let mut out = Vec::new();
    let receiver = escaped(receiver);
    let uses = pattern(&format!(r"\bthis\s*\.\s*{}\s*\(", escaped(queried)));
    for helper in helpers {
        let helper = escaped(&helper);
        let call = pattern(&format!(
            r#"\b{helper}\s*\(\s*{receiver}\s*,\s*(?:'([A-Za-z_$][\w$]*)'|"([A-Za-z_$][\w$]*)")\s*,\s*function\b"#
        ));
        let code_call = pattern(&format!(r"^\b{helper}\s*\(\s*{receiver}\s*,\s*"));
        for found in call.captures_iter(source) {
            let whole = found.get(0).expect("group 0");
            let name = found.get(1).or_else(|| found.get(2)).map_or("", |m| m.as_str());
            if !code_call.is_match(&mask[whole.start()..])
                || !depths.top_level(whole.start())
                || name == queried
            {
                continue;
            }
            let Some(open) = mask[whole.end()..].find('{').map(|at| whole.end() + at) else {
                continue;
            };
            if let Some(close) = braces.get(&open)
                && uses.is_match(&mask[open + 1..*close])
            {
                out.push((name.to_owned(), whole.start(), close + 1));
            }
        }
    }
    out
}

/// Source name -> the public names a named re-export gives it, in files of
/// script extensions (raw text: a superset filter, not proof).
pub(super) fn export_name_edges(
    texts: &BTreeMap<String, String>,
    order: &[String],
) -> HashMap<String, BTreeSet<String>> {
    let mut edges: HashMap<String, BTreeSet<String>> = HashMap::new();
    for path in order {
        let Some(source) = texts.get(path) else {
            continue;
        };
        if !has_script_extension(path) || !source.contains("export") {
            continue;
        }
        for found in NAMED_EXPORT.captures_iter(source) {
            for entry in found[1].split(',') {
                let names: Vec<&str> = AS.split(entry.trim()).collect();
                let (source_name, public_name) = (names[0].trim(), names[names.len() - 1].trim());
                if IDENTIFIER.is_match(source_name) && IDENTIFIER.is_match(public_name) {
                    edges
                        .entry(source_name.to_owned())
                        .or_default()
                        .insert(public_name.to_owned());
                }
            }
        }
    }
    edges
}

/// The names `factory` is public under through renames: a lexical superset.
pub(super) fn export_name_candidates(
    factory: &str,
    edges: &HashMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::from([factory.to_owned()]);
    let mut pending = vec![factory.to_owned()];
    while let Some(source_name) = pending.pop() {
        for public in edges.get(&source_name).into_iter().flatten() {
            if reachable.insert(public.clone()) {
                pending.push(public.clone());
            }
        }
    }
    reachable
}

/// Whether `path` is read masked (`.suffix.lower()` in the script's list).
pub(super) fn has_script_extension(path: &str) -> bool {
    let lower = suffix(path).to_lowercase();
    SCRIPT_EXTENSIONS.contains(&lower.as_str())
}


/// `os.path.normpath` for a relative or rooted POSIX path.
fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// `os.path.join(base, tail)`.
fn join(base: &str, tail: &str) -> String {
    if tail.starts_with('/') || base.is_empty() {
        tail.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{tail}")
    } else {
        format!("{base}/{tail}")
    }
}

fn dirname(path: &str) -> &str {
    path.rfind('/').map_or("", |at| {
        let head = &path[..at];
        if head.is_empty() { "/" } else { head }
    })
}

/// The tracked files a module path names: itself, with an extension, or its
/// index.
fn module_candidates(path: &str, texts: &BTreeMap<String, String>) -> Vec<String> {
    let mut options = vec![path.to_owned()];
    if suffix(path).is_empty() {
        options.extend(SOURCE_EXTENSIONS.iter().map(|extension| format!("{path}{extension}")));
        options.extend(
            SOURCE_EXTENSIONS
                .iter()
                .map(|extension| format!("{path}/index{extension}")),
        );
    }
    let mut seen = HashSet::new();
    options
        .into_iter()
        .filter(|candidate| seen.insert(candidate.clone()) && texts.contains_key(candidate))
        .collect()
}

/// The tracked files a specifier names from `importer`: relative imports and
/// the path aliases of the tracked `tsconfig`/`jsconfig` files.
pub(super) fn resolve_module_import(
    importer: &str,
    specifier: &str,
    texts: &BTreeMap<String, String>,
) -> Vec<String> {
    if specifier.starts_with('.') {
        let path = normpath(&join(dirname(importer), specifier));
        return module_candidates(&path, texts);
    }
    let mut found: Vec<String> = Vec::new();
    for (config_path, text) in texts {
        if !CONFIG_NAME.is_match(config_path) {
            continue;
        }
        let Ok(config) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        let options = config.get("compilerOptions");
        let aliases = options
            .and_then(|options| options.get("paths"))
            .and_then(serde_json::Value::as_object);
        let base_url = options
            .and_then(|options| options.get("baseUrl"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let base = normpath(&join(dirname(config_path), base_url));
        for (alias, replacements) in aliases.into_iter().flatten() {
            let middle = if let Some((before, after)) = alias.split_once('*') {
                if !specifier.starts_with(before) || !specifier.ends_with(after) {
                    continue;
                }
                let end = specifier.len() - after.len();
                Some(specifier.get(before.len()..end.max(before.len())).unwrap_or(""))
            } else if specifier == alias {
                None
            } else {
                continue;
            };
            for replacement in replacements.as_array().into_iter().flatten() {
                let Some(replacement) = replacement.as_str() else {
                    continue;
                };
                let target = replacement.replace('*', middle.unwrap_or(""));
                found.extend(module_candidates(&normpath(&join(&base, &target)), texts));
            }
        }
    }
    let mut seen = HashSet::new();
    found.retain(|candidate| seen.insert(candidate.clone()));
    found
}

/// What the export walk needs about the factory: where it lives and its name.
pub(super) struct Factory<'a> {
    pub(super) path: &'a str,
    pub(super) name: &'a str,
}

/// Completed export walks, by (module, exported name, factory path, factory).
pub(super) type ExportCache = HashMap<(String, String, String, String), bool>;

/// Whether the exported name of `path` is, through source export statements,
/// the factory itself.
pub(super) fn module_exports_name(
    path: &str,
    exported: &str,
    factory: &Factory,
    texts: &BTreeMap<String, String>,
    cache: &mut ExportCache,
) -> bool {
    let key = (
        path.to_owned(),
        exported.to_owned(),
        factory.path.to_owned(),
        factory.name.to_owned(),
    );
    if let Some(known) = cache.get(&key) {
        return *known;
    }
    let result = walk(path, exported, factory, texts, &mut HashSet::new());
    if cache.len() < MAX_EXPORT_CACHE_ENTRIES {
        cache.insert(key, result);
    }
    result
}

fn walk(
    path: &str,
    exported: &str,
    factory: &Factory,
    texts: &BTreeMap<String, String>,
    seen: &mut HashSet<(String, String)>,
) -> bool {
    if path == factory.path {
        let code = code_mask(texts.get(path).map_or("", String::as_str));
        let direct = exported == factory.name
            && pattern(&format!(
                r"\bexport\s+(?:const|let|var|function|class)\s+{}\b",
                escaped(factory.name)
            ))
            .is_match(&code);
        for found in NAMED_EXPORT.captures_iter(&code) {
            let end = found.get(0).expect("group 0").end();
            if FROM_AT.is_match(&code[end..]) {
                continue;
            }
            for entry in found[1].split(',') {
                let names: Vec<&str> = AS.split(entry.trim()).collect();
                if names[0].trim() == factory.name && names[names.len() - 1].trim() == exported {
                    return true;
                }
            }
        }
        return direct;
    }
    let state = (path.to_owned(), exported.to_owned());
    let Some(source) = texts.get(path) else {
        return false;
    };
    if seen.contains(&state) {
        return false;
    }
    seen.insert(state);
    let code = code_mask(source);
    for found in EXPORT_FROM.captures_iter(&code) {
        let end = found.get(0).expect("group 0").end();
        let Some(specifier) = QUOTED_AT.captures(&source[end..]) else {
            continue;
        };
        let specifier = specifier.get(1).or_else(|| specifier.get(2)).map_or("", |m| m.as_str());
        for entry in found[1].split(',') {
            let parts: Vec<&str> = AS.split(entry.trim()).collect();
            let (source_name, public_name) = (parts[0].trim(), parts[parts.len() - 1].trim());
            if public_name != exported {
                continue;
            }
            for target in resolve_module_import(path, specifier, texts) {
                if walk(&target, source_name, factory, texts, seen) {
                    return true;
                }
            }
        }
    }
    for found in EXPORT_STAR.find_iter(&code) {
        let Some(specifier) = QUOTED_AT.captures(&source[found.end()..]) else {
            continue;
        };
        let specifier = specifier.get(1).or_else(|| specifier.get(2)).map_or("", |m| m.as_str());
        for target in resolve_module_import(path, specifier, texts) {
            if walk(&target, exported, factory, texts, seen) {
                return true;
            }
        }
    }
    false
}

/// The local names of `path` whose import resolves to the factory.
pub(super) fn imported_local_names(
    path: &str,
    source: &str,
    code: &str,
    factory: &Factory,
    texts: &BTreeMap<String, String>,
    cache: &mut ExportCache,
) -> BTreeSet<String> {
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for found in IMPORT_BLOCK.captures_iter(source) {
        let whole = found.get(0).expect("group 0");
        let Some(masked) = IMPORT_AT.captures(&code[whole.start()..]) else {
            continue;
        };
        spans.push((whole.start(), whole.end()));
        let specifier = found.get(2).or_else(|| found.get(3)).map_or("", |m| m.as_str());
        let targets = resolve_module_import(path, specifier, texts);
        for entry in masked[1].split(',') {
            let names: Vec<&str> = AS.split(entry.trim()).collect();
            let (imported, local) = (names[0].trim(), names[names.len() - 1].trim());
            let proven = targets
                .iter()
                .any(|target| module_exports_name(target, imported, factory, texts, cache));
            if proven {
                aliases.insert(local.to_owned());
            }
        }
    }
    if aliases.is_empty() {
        return aliases;
    }
    let mut blanked = source.to_owned();
    spans.sort_unstable_by(|a, b| b.cmp(a));
    for (start, end) in spans {
        blanked.replace_range(start..end, &" ".repeat(end - start));
    }
    aliases.retain(|alias| {
        !pattern(&format!(
            r"\b(?:const|let|var|function|class)\s+{}\b",
            escaped(alias)
        ))
        .is_match(&blanked)
    });
    aliases
}

