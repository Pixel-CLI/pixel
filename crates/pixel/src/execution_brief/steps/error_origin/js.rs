// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The default-handler boundary of `error-origin`: a JavaScript function that
//! takes `(req, res, callback)`, falls back to an imported dependency when the
//! callback is missing, and hands the chosen handler to a router as the last
//! argument, with the dependency declared in a tracked `package.json`. The
//! text is scanned, never run: strings and comments are blanked first and any
//! doubt (a regex literal, a local declaration of the binding) ends the
//! search.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::super::Ctx;
use super::super::walk::Walk;
use super::super::text::{re, splitlines_owned};
use super::{JS_EXT, is_test_path, show};

/// Bytes of tracked JavaScript that is scanned.
const MAX_HANDLER_SOURCE_BYTES: usize = 512_000;
/// Package manifests that are read, and their bytes.
const MAX_HANDLER_MANIFESTS: usize = 128;
const MAX_HANDLER_MANIFEST_BYTES: usize = 512_000;
/// Bytes of one manifest.
const MAX_MANIFEST_BYTES: u64 = 64_000;
/// Characters of a quoted manifest line.
const MAX_LINE_CHARS: usize = 900;


static DEFAULT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bdefault\b"));
static NOT_FOUND: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b404\b|\bnot[\s-]+found\b"));
static EXAMPLES: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(^|/)examples?/"));
static FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\bfunction(?:\s+[A-Za-z_$][\w$]*)?\s*\(([^)]*)\)\s*\{")
});
static REQUIRE: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*require\s*\("));
static IMPORT: LazyLock<Regex> =
    LazyLock::new(|| re(r"\bimport\s+([A-Za-z_$][\w$]*)\s+from\s*"));
static IDENT: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_$][\w$]*$"));
static REQUEST: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(?:req|request)$"));
static RESPONSE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(?:res|response)$"));
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b(?:(?:const|let|var)\s+)?([A-Za-z_$][\w$]*)\s*=\s*([A-Za-z_$][\w$]*)\s*\|\|\s*([A-Za-z_$][\w$]*)\s*\(",
    )
});
static CALL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*\.\s*([A-Za-z_$][\w$]*)\s*\(",
    )
});

/// `text` with the strings and comments blanked, byte for byte: offsets and
/// line breaks stay where they were.
fn mask_noncode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize, keep_breaks: bool| {
        for byte in &mut out[from..to.min(bytes.len())] {
            if !(keep_breaks && *byte == b'\n') {
                *byte = b' ';
            }
        }
    };
    let find = |needle: &[u8], from: usize| {
        bytes
            .get(from..)
            .and_then(|rest| rest.windows(needle.len()).position(|window| window == needle))
            .map(|at| from + at)
    };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            let end = bytes[i + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |at| i + 2 + at);
            blank(&mut out, i, end, false);
            i = end;
        } else if bytes[i..].starts_with(b"/*") {
            let end = find(b"*/", i + 2).map_or(bytes.len(), |at| at + 2);
            blank(&mut out, i, end, true);
            i = end;
        } else if matches!(bytes[i], b'\'' | b'"' | b'`') {
            let quote = bytes[i];
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == b'\\' {
                    end += 2;
                } else if bytes[end] == quote {
                    end += 1;
                    break;
                } else {
                    end += 1;
                }
            }
            blank(&mut out, i, end, true);
            i = end;
        } else {
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `}` that closes the `{` at `start`.
fn matching_brace(code: &str, start: usize) -> Option<usize> {
    let mut depth = 0_i64;
    for (at, byte) in code.bytes().enumerate().skip(start) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// A `function ... (params) {` of the code: where it starts, where its body
/// opens and closes, and the plain parameter names.
struct Span {
    start: usize,
    open: usize,
    close: usize,
    params: Vec<String>,
}

fn function_spans(code: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for found in FUNCTION.captures_iter(code) {
        let (Some(whole), Some(params)) = (found.get(0), found.get(1)) else {
            continue;
        };
        let open = whole.end() - 1;
        let Some(close) = matching_brace(code, open) else {
            continue;
        };
        spans.push(Span {
            start: whole.start(),
            open,
            close,
            params: params
                .as_str()
                .split(',')
                .map(|part| part.trim().to_owned())
                .filter(|part| IDENT.is_match(part))
                .collect(),
        });
    }
    spans
}

/// The package a module specifier names (`@scope/name`, `name`); none for a
/// relative path, an absolute one or a `node:` builtin.
fn package_name(specifier: &str) -> Option<String> {
    if specifier.is_empty() || specifier.starts_with(['.', '/']) || specifier.starts_with("node:") {
        return None;
    }
    let parts: Vec<&str> = specifier.split('/').collect();
    if specifier.starts_with('@') {
        return (parts.len() >= 2).then(|| parts[..2].join("/"));
    }
    parts.first().map(|part| (*part).to_owned())
}

/// The quoted specifier that follows an import or a `require(`, closing paren
/// included for a `require`.
fn specifier_after(rest: &str, closing_paren: bool) -> Option<&str> {
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|ch| matches!(ch, '\'' | '"'))?;
    let body = &rest[1..];
    let end = body.find(['\'', '"'])?;
    if end == 0 || !body[end..].starts_with(quote) {
        return None;
    }
    let after = &body[end + 1..];
    (!closing_paren || after.trim_start().starts_with(')')).then_some(&body[..end])
}

/// Bound name -> (package, offset of the import).
fn imports(text: &str, code: &str) -> HashMap<String, Vec<(String, usize)>> {
    let mut found: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for (index, pattern) in [&*REQUIRE, &*IMPORT].into_iter().enumerate() {
        for matched in pattern.captures_iter(code) {
            let (Some(whole), Some(name)) = (matched.get(0), matched.get(1)) else {
                continue;
            };
            let dependency = text
                .get(whole.end()..)
                .and_then(|rest| specifier_after(rest, index == 0))
                .and_then(package_name);
            if let Some(dependency) = dependency {
                found
                    .entry(name.as_str().to_owned())
                    .or_default()
                    .push((dependency, whole.start()));
            }
        }
    }
    found
}

/// A tracked regular file below `root` with no symbolic link on its way, no
/// larger than `limit`: its size.
fn safe_size(root: &Path, relative: &str, limit: u64) -> Option<u64> {
    let path = Path::new(relative);
    if path.is_absolute() || relative.is_empty() {
        return None;
    }
    let mut current = root.to_path_buf();
    for part in path.components() {
        match part {
            std::path::Component::Normal(name) => current.push(name),
            _ => return None,
        }
        if current.symlink_metadata().ok()?.is_symlink() {
            return None;
        }
    }
    let info = current.symlink_metadata().ok()?;
    (info.is_file() && info.len() <= limit).then_some(info.len())
}

/// A package manifest: its lines and its declared dependencies' names.
struct Manifest {
    lines: Vec<String>,
    dependencies: Vec<String>,
}

/// The tracked `package.json` files, read within bounds, by path.
fn manifests(ctx: &Ctx) -> HashMap<String, Manifest> {
    let mut paths: Vec<&String> = ctx
        .files()
        .unwrap_or_default()
        .iter()
        .filter(|path| path.rsplit('/').next() == Some("package.json"))
        .collect();
    paths.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let listing: usize = paths.iter().map(|path| path.len() + 1).sum();
    if listing > MAX_HANDLER_MANIFEST_BYTES || paths.len() > MAX_HANDLER_MANIFESTS {
        return HashMap::new();
    }
    let mut found = HashMap::new();
    let mut used = 0_u64;
    for path in paths {
        let Some(size) = safe_size(ctx.root(), path, MAX_MANIFEST_BYTES) else {
            continue;
        };
        used += size;
        if used > MAX_HANDLER_MANIFEST_BYTES as u64 {
            return HashMap::new();
        }
        let Some(text) = ctx.read(path, MAX_MANIFEST_BYTES) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut dependencies = Vec::new();
        for section in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(entries) = data.get(section).and_then(serde_json::Value::as_object) {
                for (name, version) in entries {
                    if version.is_string() {
                        dependencies.push(name.clone());
                    }
                }
            }
        }
        found.insert(
            path.clone(),
            Manifest {
                lines: splitlines_owned(&text),
                dependencies,
            },
        );
    }
    found
}

/// The manifest of the closest directory at or above the source file.
fn manifest_for<'a>(
    relative: &str,
    manifests: &'a HashMap<String, Manifest>,
) -> Option<(String, &'a Manifest)> {
    let mut directory = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
    loop {
        let name = if directory.is_empty() {
            "package.json".to_owned()
        } else {
            format!("{directory}/package.json")
        };
        if let Some(found) = manifests.get(&name) {
            return Some((name, found));
        }
        if directory.is_empty() {
            return None;
        }
        directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

/// The arguments of the call whose `(` is at `open`, split at top-level commas.
fn arguments(body: &str, open: usize) -> Vec<String> {
    let mut depth = 0_i64;
    let mut close = None;
    for (at, byte) in body.bytes().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(at);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return Vec::new();
    };
    let inner = &body[open + 1..close];
    let (mut args, mut start) = (Vec::new(), 0);
    let (mut parens, mut braces, mut brackets) = (0_i64, 0_i64, 0_i64);
    for (at, byte) in inner.bytes().enumerate() {
        match byte {
            b'(' => parens += 1,
            b')' => parens -= 1,
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b',' if parens == 0 && braces == 0 && brackets == 0 => {
                args.push(inner[start..at].trim().to_owned());
                start = at + 1;
            }
            _ => {}
        }
    }
    args.push(inner[start..].trim().to_owned());
    args
}

/// Whether `name` stands alone in `text` (`\bname\b`).
fn mentions(text: &str, name: &str) -> bool {
    Regex::new(&format!(r"\b{}\b", regex::escape(name))).is_ok_and(|found| found.is_match(text))
}

/// A router-shaped call that receives the chosen handler as its last
/// argument next to the request and the response: offset and `receiver.method`.
fn call_uses(body: &str, result: &str, request: &str, response: &str) -> Option<(usize, String)> {
    for found in CALL.captures_iter(body) {
        let (Some(whole), Some(receiver), Some(method)) = (found.get(0), found.get(1), found.get(2))
        else {
            continue;
        };
        let receiver = receiver.as_str().trim();
        let tail = receiver.rsplit('.').next().unwrap_or("").trim().to_lowercase();
        let role = tail.rsplit('_').next().unwrap_or("").trim_matches('$');
        if !["router", "route", "routes", "dispatcher", "dispatch"].contains(&role)
            || !["handle", "dispatch", "route", "complete", "finish", "next"]
                .contains(&method.as_str().to_lowercase().as_str())
        {
            continue;
        }
        let args = arguments(body, whole.end() - 1);
        if args.len() < 3 || args.last().map(String::as_str) != Some(result) {
            continue;
        }
        let prefix = args[..args.len() - 1].join(" ");
        if mentions(&prefix, request) && mentions(&prefix, response) {
            return Some((whole.start(), format!("{receiver}.{}", method.as_str())));
        }
    }
    None
}

/// Whether the body holds something shaped like a regex literal
/// (`(?<![\w$])/(?:\\.|[^/\n]){1,180}/[a-z]*`), which this scanner reads too
/// poorly to trust.
fn has_regex_literal(body: &str) -> bool {
    let bytes = body.as_bytes();
    for (start, _) in body.match_indices('/') {
        let before = body[..start].chars().next_back();
        if before.is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$') {
            continue;
        }
        let mut reach = vec![start + 1];
        for _ in 0..180 {
            let mut next: Vec<usize> = Vec::new();
            for at in &reach {
                let Some(byte) = bytes.get(*at) else {
                    continue;
                };
                if *byte == b'\\' && bytes.get(at + 1).is_some_and(|after| *after != b'\n') {
                    next.push(at + 2);
                }
                if *byte != b'/' && *byte != b'\n' {
                    next.push(at + 1);
                }
            }
            next.sort_unstable();
            next.dedup();
            if next.iter().any(|at| bytes.get(*at) == Some(&b'/')) {
                return true;
            }
            if next.is_empty() {
                break;
            }
            reach = next;
        }
    }
    false
}

/// One function that completes the pattern.
struct Candidate {
    path: String,
    source: String,
    dependency: String,
    import_at: usize,
    fallback_at: usize,
    use_at: usize,
    callee: String,
    manifest_path: String,
    manifest_lines: Vec<String>,
    manifest_line: usize,
}

/// The block lines of the default handler boundary a question about a default
/// 404 asks for; empty unless exactly one function matches.
pub(super) fn boundary(ctx: &Ctx, walk: &mut Walk, prompt: &str, paths: &[String]) -> Vec<String> {
    if !DEFAULT.is_match(prompt) || !NOT_FOUND.is_match(prompt) {
        return Vec::new();
    }
    let manifests = manifests(ctx);
    if manifests.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut used = 0;
    for path in paths {
        if !JS_EXT.iter().any(|ext| path.ends_with(ext)) || is_test_path(path) || EXAMPLES.is_match(path)
        {
            continue;
        }
        let source = walk.lines(path).join("\n");
        used += source.len();
        if used > MAX_HANDLER_SOURCE_BYTES {
            return Vec::new();
        }
        let code = mask_noncode(&source);
        let imported = imports(&source, &code);
        let Some((manifest_path, manifest)) = manifest_for(path, &manifests) else {
            continue;
        };
        if imported.is_empty() {
            continue;
        }
        let spans = function_spans(&code);
        for span in &spans {
            let body_start = span.open + 1;
            let mut body: Vec<u8> = code.as_bytes()[body_start..span.close].to_vec();
            for nested in &spans {
                if nested.start > span.open && nested.close < span.close {
                    for index in nested.start - body_start..=nested.close - body_start {
                        if let Some(byte) = body.get_mut(index)
                            && *byte != b'\n'
                        {
                            *byte = b' ';
                        }
                    }
                }
            }
            let body = String::from_utf8_lossy(&body).into_owned();
            if has_regex_literal(&body) {
                continue;
            }
            let request = span.params.iter().find(|name| REQUEST.is_match(name));
            let response = span.params.iter().find(|name| RESPONSE.is_match(name));
            let (Some(request), Some(response)) = (request, response) else {
                continue;
            };
            for found in ASSIGNMENT.captures_iter(&body) {
                let (Some(whole), Some(result), Some(callback), Some(binding)) =
                    (found.get(0), found.get(1), found.get(2), found.get(3))
                else {
                    continue;
                };
                let (result, callback, binding) =
                    (result.as_str(), callback.as_str(), binding.as_str());
                if !span.params.iter().any(|param| param == callback)
                    || span.params.iter().any(|param| param == binding)
                {
                    continue;
                }
                let local = format!(
                    r"\b(?:const|let|var|class|function)\s+{}\b",
                    regex::escape(binding)
                );
                if Regex::new(&local).is_ok_and(|local| local.is_match(&body)) {
                    continue;
                }
                let Some([(dependency, import_at)]) = imported.get(binding).map(Vec::as_slice)
                else {
                    continue;
                };
                if !manifest.dependencies.contains(dependency) {
                    continue;
                }
                let fallback_args = arguments(&body, whole.end() - 1);
                if !fallback_args.iter().any(|arg| mentions(arg, request))
                    || !fallback_args.iter().any(|arg| mentions(arg, response))
                {
                    continue;
                }
                let Some((use_at, callee)) =
                    call_uses(&body[whole.end()..], result, request, response)
                else {
                    continue;
                };
                let declared = Regex::new(&format!(r#""{}"\s*:"#, regex::escape(dependency)))
                    .ok()
                    .map(|declared| {
                        manifest
                            .lines
                            .iter()
                            .enumerate()
                            .filter(|(_, line)| declared.is_match(line))
                            .map(|(i, _)| i + 1)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let [manifest_line] = declared[..] else {
                    continue;
                };
                candidates.push(Candidate {
                    path: path.clone(),
                    source: source.clone(),
                    dependency: dependency.clone(),
                    import_at: *import_at,
                    fallback_at: body_start + whole.start(),
                    use_at: body_start + whole.end() + use_at,
                    callee,
                    manifest_path: manifest_path.clone(),
                    manifest_lines: manifest.lines.clone(),
                    manifest_line,
                });
            }
        }
    }
    let [found] = &candidates[..] else {
        return Vec::new();
    };
    let line_of = |at: usize| found.source[..at].matches('\n').count() + 1;
    let source_lines = splitlines_owned(&found.source);
    let (import_line, fallback_line, use_line) =
        (line_of(found.import_at), line_of(found.fallback_at), line_of(found.use_at));
    let callee = &found.callee;
    let mut out = vec![format!(
        "### default handler callback candidate — optional callback falls back to an imported dependency; the handler receives request/response and is passed as the final argument to {callee}; callee behavior and external response text are not inspected"
    )];
    let (import, fallback, uses) = (import_line as i64, fallback_line as i64, use_line as i64);
    out.extend(show(&found.path, &source_lines, import, import, "imported dependency binding"));
    out.extend(show(&found.path, &source_lines, fallback - 1, fallback, "optional callback fallback"));
    out.extend(show(
        &found.path,
        &source_lines,
        uses,
        uses,
        &format!("passes the selected handler to {callee}; call behavior unverified"),
    ));
    let line = found.manifest_line;
    out.push(format!(
        "### {}:{line}-{line} — declared non-development dependency: {}",
        found.manifest_path, found.dependency
    ));
    out.push(format!(
        "{line}| {}",
        crate::execution_brief::steps::text::head(&found.manifest_lines[line - 1], MAX_LINE_CHARS)
    ));
    out
}
