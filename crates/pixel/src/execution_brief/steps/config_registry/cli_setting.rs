// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A prompt-named CLI setting (a `Flag` implementation of a Rust command-line
//! tool) and the function that derives its runtime value.

use std::collections::BTreeSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::super::text::head;
use super::repo::Repo;
use super::scopes::{
    SCOPED_EVIDENCE_MAX, back_chars, forward_chars, rust_code_mask, rust_scopes, source_range,
};
use super::search::scoped_source_files;
use super::words::{BOUND_WORDS, WEAK, letters, stems, words_of};

const SCOPED_SCAN_MAX_MS: u64 = 450;

macro_rules! pattern {
    ($name:ident, $text:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($text).expect("a pattern of the CLI lane is a literal"));
    };
}

pattern!(
    INTENT,
    r"(?i)\b(config\w*|settings?|flags?|options?|defaults?|overrid\w+|where do i set|set (that|this|it))\b"
);
pattern!(IMPL_FLAG, r"impl\s+Flag\s+for");
pattern!(
    TEST_MODULE,
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{)"
);
pattern!(FLAG_IMPL, r"(?m)^\s*impl\s+Flag\s+for\s+\w+\s*\{");
pattern!(NAME_LONG, r"\bfn\s+name_long\s*\(");
pattern!(NAME_SHORT, r"\bfn\s+name_short\s*\(");
pattern!(UPDATE, r"\bfn\s+update\s*\(");
pattern!(NAME_LITERAL, r#"(?i)"([a-z][a-z0-9_-]*)""#);
pattern!(
    SHORT_BYTE,
    r#"Some\s*\(\s*b(?:'([A-Za-z0-9])'|"([A-Za-z0-9])")\s*\)"#
);
pattern!(SHORT_MASKED, r"\bSome\s*\(\s*b\s+\s*\)");
pattern!(ARGS_FIELD, r"\bargs\.([a-z_][a-z0-9_]*)\s*=");
pattern!(
    CONSUMER_FN,
    r"(?m)^\s*(?:(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn)\s+\w+\s*(?:<[^>]*>)?\s*\("
);
pattern!(
    RUNTIME_SHAPE,
    r"(?i)\b(parallelism|available_[a-z_]+|unwrap_or(?:_else)?|if\s+let|match)\b"
);
pattern!(ATTRIBUTES, r"(?:^|\n)(\s*#\[[^\n]+\]\s*)+$");
pattern!(CFG_TEST, r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]");

/// The `-j` style short options the prompt names, lowercase: a dash, one
/// letter, and no word character or dash on either side.
fn requested_short(prompt: &str) -> BTreeSet<String> {
    let chars: Vec<char> = prompt.chars().collect();
    let word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '-';
    let mut out = BTreeSet::new();
    let mut at = 0;
    while at + 1 < chars.len() {
        let letter = chars[at + 1];
        if chars[at] == '-'
            && letter.is_ascii_alphabetic()
            && (at == 0 || !word(chars[at - 1]))
            && chars.get(at + 2).is_none_or(|next| !word(*next))
        {
            out.insert(letter.to_lowercase().to_string());
            at += 2;
        } else {
            at += 1;
        }
    }
    out
}

/// Whether an item at `item_start` is `#[cfg(test)]` or inside a test module.
fn rust_test_only(source: &str, item_start: usize, test_modules: &[(usize, usize)]) -> bool {
    let prefix = &source[..item_start];
    if let Some(caps) = ATTRIBUTES.captures(prefix)
        && CFG_TEST.is_match(&caps[1])
    {
        return true;
    }
    test_modules.iter().any(|(start, end)| *start < item_start && item_start < *end)
}

/// What a `Flag` implementation declares: where it is, the field its `update`
/// sets and how well the prompt names it.
struct Selected {
    rel: String,
    source: String,
    start: usize,
    end: usize,
    field: String,
}

fn select(repo: &Repo, prompt: &str, deadline: Instant) -> Option<Selected> {
    let weak_or_bound = |word: &String| !WEAK.contains(word) && !BOUND_WORDS.contains(word);
    let prompt_terms: BTreeSet<String> = stems(words_of(prompt)).into_iter().filter(weak_or_bound).collect();
    if prompt_terms.is_empty() {
        return None;
    }
    let requested = requested_short(prompt);
    for rel in scoped_source_files(repo, &IMPL_FLAG, &[".rs"], deadline) {
        if Instant::now() >= deadline {
            break;
        }
        let Some(source) = repo.source_text(&rel) else {
            continue;
        };
        let test_modules: Vec<(usize, usize)> = rust_scopes(&source, &TEST_MODULE)
            .into_iter()
            .map(|(start, end, _)| (start, end))
            .collect();
        for (start, end, body_start) in rust_scopes(&source, &FLAG_IMPL) {
            if rust_test_only(&source, start, &test_modules) {
                continue;
            }
            let body = &source[body_start..end - 1];
            let name_method = rust_scopes(body, &NAME_LONG).into_iter().next();
            let short_method = rust_scopes(body, &NAME_SHORT).into_iter().next();
            let Some(update_method) = rust_scopes(body, &UPDATE).into_iter().next() else {
                continue;
            };
            let name_match = name_method.and_then(|(_, close, open)| {
                NAME_LITERAL.captures(&body[open..close - 1]).map(|caps| caps[1].to_lowercase())
            });
            let option_tokens = name_match.map(|name| stems(letters(&name))).unwrap_or_default();
            let overlap = option_tokens.intersection(&prompt_terms).count();
            let mut short_char = None;
            if let Some((_, close, open)) = short_method {
                let short_text = &body[open..close - 1];
                let masked = rust_code_mask(short_text);
                for caps in SHORT_BYTE.captures_iter(short_text) {
                    let whole = caps.get(0).expect("a match has a whole");
                    if SHORT_MASKED.is_match(&masked[whole.start()..whole.end()]) {
                        let letter = caps.get(1).or_else(|| caps.get(2)).expect("one quote form matched");
                        short_char = Some(letter.as_str().to_lowercase());
                        break;
                    }
                }
            }
            let short_match = short_char.is_some_and(|letter| requested.contains(&letter));
            // An explicit short option outranks nearby prose: "command-line"
            // must not select --line-buffered when the prompt asks about -j.
            if !requested.is_empty() {
                if !short_match {
                    continue;
                }
            } else if overlap == 0 {
                continue;
            }
            let update_text = &body[update_method.2..update_method.1 - 1];
            let Some(field) = ARGS_FIELD.captures(update_text).map(|caps| caps[1].to_owned()) else {
                continue;
            };
            return Some(Selected { rel, source, start, end, field });
        }
    }
    None
}

/// The function that consumes the flag's field and the flag that sets it.
pub(super) fn evidence(repo: &Repo, prompt: &str) -> String {
    if !INTENT.is_match(prompt) {
        return String::new();
    }
    let mut deadline = Instant::now() + Duration::from_millis(SCOPED_SCAN_MAX_MS);
    let Some(selected) = select(repo, prompt, deadline) else {
        return String::new();
    };
    let Selected { rel, source: impl_source, start, end, field } = selected;
    deadline = deadline.max(Instant::now() + Duration::from_millis(200));
    let Ok(field_use) = Regex::new(&format!(r"\blow\.{}\b", regex::escape(&field))) else {
        return String::new();
    };
    let needle = format!("low.{field}");
    for consumer in scoped_source_files(repo, &escaped(&field), &[".rs"], deadline) {
        if Instant::now() >= deadline {
            break;
        }
        let Some(source) = repo.source_text(&consumer) else {
            continue;
        };
        // Most hits are comments, docs or unrelated field mentions: parse
        // only a file that holds the exact receiver.
        if !source.contains(&needle) {
            continue;
        }
        for (_, fn_end, fn_body) in rust_scopes(&source, &CONSUMER_FN) {
            let body = &source[fn_body..fn_end - 1];
            let masked = rust_code_mask(body);
            if !field_use.is_match(&masked) || !RUNTIME_SHAPE.is_match(&masked) {
                continue;
            }
            let positions: Vec<usize> = field_use.find_iter(&masked).map(|found| found.start()).collect();
            let (Some(first), Some(last)) = (positions.first(), positions.last()) else {
                continue;
            };
            let lo = back_chars(&source, fn_body + first, 140).max(fn_body);
            let hi = forward_chars(&source, fn_body + last, 180).min(fn_end - 1);
            let mut blocks = source_range(
                &consumer,
                &source,
                lo,
                hi,
                "matched setting consumed within one function",
            );
            blocks.extend(source_range(
                &rel,
                &impl_source,
                start,
                end,
                "matching CLI setting declaration and override",
            ));
            let text = blocks.join("\n") + "\n";
            return head(&text, SCOPED_EVIDENCE_MAX).to_owned();
        }
    }
    String::new()
}

fn escaped(field: &str) -> Regex {
    Regex::new(&regex::escape(field)).expect("an escaped field is a pattern")
}
