// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A named JavaScript setting, the function that compiles it when it is set,
//! and the getter that reads the compiled value (`trust proxy` and `req.ip`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::evidence::{is_js, setter_key};
use super::repo::{MAX_FILE_BYTES, Repo, resolve};
use super::scan::{Row, SCAN_MAX_COLUMNS};
use super::scopes::{
    active_match, back_chars, forward_chars, js_code_mask, js_function_ranges, redact_source_range,
    source_range,
};
use super::search::scoped_source_files;
use super::words::{BOUND_WORDS, WEAK, letters, stems, words_of};

const SCOPED_SCAN_MAX_MS: u64 = 450;
const BYTES_READ_MAX: u64 = 4 * 1024 * 1024;
const JS_SOURCE_SUFFIXES: [&str; 8] = [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"];
const SETTINGS_TRIED: usize = 3;

macro_rules! pattern {
    ($name:ident, $text:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($text).expect("a pattern of the compiled lane is a literal"));
    };
}

pattern!(
    INTENT,
    r"(?i)\b(compil\w*|implement\w*|use\w*|consumer|getter)\b"
);
pattern!(DOTTED, r"\b([a-z_$][\w$]*)\.([a-z_$][\w$]*)\b");
pattern!(CASE, r#"case\s*(?:'(.*?)'|"(.*?)")\s*:"#);
pattern!(CASE_MASKED, r"\bcase\s*:");
pattern!(
    COMPILE_CALL,
    r#"this\s*\.\s*set\s*\(\s*(?:'(.*?)'|"(.*?)")\s*,\s*([A-Za-z_$][\w$]*)\s*\(\s*[A-Za-z_$][\w$]*\s*\)"#
);
pattern!(COMPILE_MASKED, r"\bthis\s*\.\s*set\s*\(\s*,\s*[A-Za-z_$][\w$]*\s*\(");
pattern!(DEFINE_GETTER, r"\bdefineGetter\s*\(");
pattern!(
    KEY_ACCESS,
    r#"\.\s*get\s*\(\s*(?:'(.*?)'|"(.*?)")\s*\)"#
);
pattern!(KEY_ACCESS_MASKED, r"\.\s*get\s*\(\s*\)");

fn group<'t>(caps: &regex::Captures<'t>, one: usize, two: usize) -> &'t str {
    caps.get(one).or_else(|| caps.get(two)).map_or("", |found| found.as_str())
}

/// A path relative to the repository as a `/`-separated string, when it is inside.
fn relative(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root).ok()?.to_str().map(|rel| rel.replace('\\', "/"))
}

/// The setting's compiler binding and the function range of its `case`.
fn compiler_binding(source: &str, setting: &str) -> Option<(String, (usize, usize))> {
    let compiled_key = format!("{setting} fn");
    for (start, end, body_start) in js_function_ranges(source) {
        let body = &source[body_start..end - 1];
        let masked_body = js_code_mask(body);
        for case in CASE.captures_iter(body) {
            let whole = case.get(0).expect("a match has a whole");
            if group(&case, 1, 2) != setting
                || !CASE_MASKED.is_match(&masked_body[whole.start()..whole.end()])
            {
                continue;
            }
            let call = active_match(
                body,
                &COMPILE_CALL,
                |segment| COMPILE_MASKED.is_match(segment),
                |caps| group(caps, 1, 2) == compiled_key,
            );
            let Some(call) = call else {
                continue;
            };
            let call_start = call.get(0).expect("a match has a whole").start();
            if call_start < whole.end() {
                continue;
            }
            let between = &body[whole.end()..call_start];
            let intervening = active_match(between, &CASE, |segment| CASE_MASKED.is_match(segment), |_| true);
            if intervening.is_some() {
                continue;
            }
            return Some((call[3].to_owned(), (start, end)));
        }
    }
    None
}

/// The statement-level `Regex` of an imported binding.
fn import_pattern(binding: &str) -> Option<Regex> {
    Regex::new(&format!(
        r#"\b(?:var|let|const)\s+{}\s*=\s*require\(\s*(?:'(\.[^'"]+)'|"(\.[^'"]+)")\s*\)\.([A-Za-z_$][\w$]*)"#,
        regex::escape(binding)
    ))
    .ok()
}

pub(super) fn evidence(repo: &Repo, prompt: &str, rows: &[Row]) -> String {
    if !INTENT.is_match(prompt) {
        return String::new();
    }
    let Some(dotted) = DOTTED.captures(prompt) else {
        return String::new();
    };
    let (receiver, property_name) = (dotted[1].to_owned(), dotted[2].to_owned());
    let prompt_words: BTreeSet<String> = stems(words_of(prompt))
        .into_iter()
        .filter(|word| !WEAK.contains(word) && !BOUND_WORDS.contains(word))
        .collect();
    let Some(inventory) = repo.inventory() else {
        return String::new();
    };
    // `(-overlap, words in the setting, path, setting)`, deduplicated and sorted.
    let mut settings: BTreeSet<(std::cmp::Reverse<usize>, usize, String, String)> = BTreeSet::new();
    for Row { path, text, .. } in rows {
        if !inventory.contains(path) || !is_js(path) {
            continue;
        }
        let head = text.trim_start();
        if head.starts_with("//")
            || head.starts_with("/*")
            || head.starts_with('*')
            || text.len() > SCAN_MAX_COLUMNS
        {
            continue;
        }
        let Some(setting) = setter_key(text) else {
            continue;
        };
        let setting_words = stems(letters(&setting));
        let overlap = prompt_words.intersection(&setting_words).count();
        if overlap > 0 {
            settings.insert((std::cmp::Reverse(overlap), setting_words.len(), path.clone(), setting));
        }
    }
    if settings.is_empty() {
        return String::new();
    }
    let deadline = Instant::now() + Duration::from_millis(SCOPED_SCAN_MAX_MS);
    let mut bytes_read = 0u64;
    for (_, _, setting_path, setting) in settings.into_iter().take(SETTINGS_TRIED) {
        if Instant::now() >= deadline {
            break;
        }
        let Some((_, size)) = repo.checked(&setting_path) else {
            continue;
        };
        let Some(setting_source) = repo.source_text(&setting_path) else {
            continue;
        };
        bytes_read += size;
        if bytes_read > BYTES_READ_MAX {
            break;
        }
        let Some((binding, setter_span)) = compiler_binding(&setting_source, &setting) else {
            continue;
        };
        let compiled_key = format!("{setting} fn");
        let Some(pattern) = import_pattern(&binding) else {
            continue;
        };
        let binding_masked = Regex::new(&format!(
            r"\b(?:var|let|const)\s+{}\s*=\s*require\s*\(\s*\)\s*\.\s*[A-Za-z_$][\w$]*",
            regex::escape(&binding)
        ));
        let Ok(binding_masked) = binding_masked else {
            continue;
        };
        let imported = active_match(
            &setting_source,
            &pattern,
            |segment| binding_masked.is_match(segment),
            |caps| caps[3] == binding,
        );
        let Some(imported) = imported else {
            continue;
        };
        let module = group(&imported, 1, 2).to_owned();
        let setting_dir = repo.root.join(&setting_path);
        let module_base = resolve(&setting_dir.parent().unwrap_or(&repo.root).join(&module));
        let mut compiler: Option<(String, String)> = None;
        for suffix in JS_SOURCE_SUFFIXES {
            let mut name = module_base.clone().into_os_string();
            name.push(suffix);
            let target = PathBuf::from(name);
            let Ok(meta) = std::fs::symlink_metadata(&target) else {
                continue;
            };
            if meta.file_type().is_symlink() || !resolve(&target).starts_with(&repo.root) {
                continue;
            }
            let Some(rel) = relative(&repo.root, &target) else {
                continue;
            };
            let Ok(target_meta) = std::fs::metadata(&target) else {
                continue;
            };
            if !inventory.contains(&rel) || !target_meta.is_file() || target_meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(raw) = std::fs::read(&target) else {
                continue;
            };
            bytes_read += target_meta.len();
            if bytes_read > BYTES_READ_MAX {
                break;
            }
            let text = super::super::text::universal_newlines(&String::from_utf8_lossy(&raw));
            compiler = Some((rel, text));
            break;
        }
        let Some((compiler_path, compiler_source)) = compiler else {
            continue;
        };
        let exports = Regex::new(&format!(
            r"\bexports\s*\.\s*{}\s*=\s*$",
            regex::escape(&binding)
        ));
        let declared = Regex::new(&format!(r"\bfunction\s+{}\s*\(", regex::escape(&binding)));
        let (Ok(exports), Ok(declared)) = (exports, declared) else {
            continue;
        };
        let compiler_ranges: Vec<(usize, usize)> = js_function_ranges(&compiler_source)
            .into_iter()
            .filter(|(start, end, _)| {
                let from = back_chars(&compiler_source, *start, 100);
                exports.is_match(&compiler_source[from..*start])
                    || declared.is_match(&compiler_source[*start..*end])
            })
            .map(|(start, end, _)| (start, end))
            .collect();
        let [(compiler_start, compiler_end)] = compiler_ranges[..] else {
            continue;
        };
        let getter_pattern = Regex::new(&format!(
            r#"defineGetter\s*\(\s*{}\s*,\s*(?:'{}'|"{}")\s*,\s*function(?:\s+[A-Za-z_$][\w$]*)?\s*\("#,
            regex::escape(&receiver),
            regex::escape(&property_name),
            regex::escape(&property_name)
        ));
        let getter_masked = Regex::new(&format!(
            r"\bdefineGetter\s*\(\s*{}\s*,\s*,\s*function(?:\s+[A-Za-z_$][\w$]*)?\s*\(",
            regex::escape(&receiver)
        ));
        let (Ok(getter_pattern), Ok(getter_masked)) = (getter_pattern, getter_masked) else {
            continue;
        };
        for consumer in scoped_source_files(repo, &DEFINE_GETTER, &JS_SOURCE_SUFFIXES, deadline) {
            if Instant::now() >= deadline {
                break;
            }
            let Some((_, size)) = repo.checked(&consumer) else {
                continue;
            };
            bytes_read += size;
            if bytes_read > BYTES_READ_MAX {
                break;
            }
            let Some(source) = repo.source_text(&consumer) else {
                continue;
            };
            let getter = active_match(&source, &getter_pattern, |segment| getter_masked.is_match(segment), |_| true);
            let Some(getter) = getter else {
                continue;
            };
            let whole = getter.get(0).expect("a match has a whole");
            let window_end = forward_chars(&source, whole.end(), 20);
            for (fn_start, fn_end, _) in js_function_ranges(&source) {
                if fn_start < whole.start() || fn_start > window_end {
                    continue;
                }
                let body = &source[fn_start..fn_end];
                let access = active_match(
                    body,
                    &KEY_ACCESS,
                    |segment| KEY_ACCESS_MASKED.is_match(segment),
                    |caps| group(caps, 1, 2) == compiled_key,
                );
                if access.is_none() {
                    continue;
                }
                let getter_lines = redact_source_range(
                    source_range(&consumer, &source, whole.start(), fn_end, "named getter reads the matching compiled setting"),
                    &setting,
                );
                let setter_lines = redact_source_range(
                    source_range(&setting_path, &setting_source, setter_span.0, setter_span.1, "setting branch installs a compiled function"),
                    &setting,
                );
                let compiler_lines = redact_source_range(
                    source_range(&compiler_path, &compiler_source, compiler_start, compiler_end, "exact imported setting compiler implementation"),
                    &setting,
                );
                let all: Vec<String> = setter_lines.into_iter().chain(compiler_lines).chain(getter_lines).collect();
                return all.join("\n") + "\n";
            }
        }
    }
    String::new()
}
