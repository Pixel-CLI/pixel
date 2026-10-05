// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Conservative routing for bounded Codex caller facts.

/// Return one explicit, bare symbol only for a caller/impact question.
///
/// Qualified names, paths, multiple candidate names, and uncertain prompts
/// abstain so ordinary Codex retrieval stays byte-for-byte native.
pub(crate) fn classify(prompt: &str) -> Option<String> {
    let typed = crate::execution_brief::typed_text(prompt);
    let lower = typed.to_lowercase();
    if explicitly_native_only(&lower) || !asks_about_call_graph(&lower) {
        return None;
    }

    single_bare_symbol(&typed)
}

fn asks_about_call_graph(prompt: &str) -> bool {
    prompt.starts_with("rename ")
        || [
            "callers of",
            "direct callers",
            "who calls",
            "called by",
            "call chain",
            "call path",
            "call graph",
            "dependency graph",
            "callers",
            "callee",
            "blast radius",
            "fan-out",
            "dependents of",
            "impact",
        ]
        .iter()
        .any(|signal| prompt.contains(signal))
}

fn explicitly_native_only(prompt: &str) -> bool {
    [
        "do not use pixel",
        "don't use pixel",
        "dont use pixel",
        "without using pixel",
        "without pixel",
        "no pixel",
        "skip pixel",
        "avoid pixel",
        "native-only",
        "native only",
        "only native tools",
        "only native search",
        "use native tools only",
        "use native search only",
        "only use native tools",
        "only use native search",
        "use grep only",
        "use rg only",
        "use grep instead of pixel",
        "use rg instead of pixel",
    ]
    .iter()
    .any(|signal| prompt.contains(signal))
}

fn single_bare_symbol(prompt: &str) -> Option<String> {
    let segments: Vec<_> = prompt.split('`').collect();
    let quoted_count = segments.len().saturating_sub(1) / 2;
    if segments.len() % 2 == 0 || quoted_count > 1 {
        return None;
    }

    let mut candidates = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if index % 2 == 1 {
            if !is_bare_identifier(segment) {
                return None;
            }
            candidates.push(*segment);
            continue;
        }

        for (start, word) in ascii_identifier_tokens(segment) {
            if is_code_identifier(word) && !is_path_or_qualified_token(segment, start, word.len()) {
                candidates.push(word);
            }
        }
    }

    if candidates.len() != 1 {
        return None;
    }
    Some(candidates[0].to_owned())
}

fn ascii_identifier_tokens(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut start = None;
    for index in 0..=bytes.len() {
        let is_identifier_byte = bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        match (start, is_identifier_byte) {
            (None, true) => start = Some(index),
            (Some(begin), false) => {
                tokens.push((begin, &text[begin..index]));
                start = None;
            }
            _ => {}
        }
    }
    tokens
}

fn is_path_or_qualified_token(text: &str, start: usize, length: usize) -> bool {
    let bytes = text.as_bytes();
    let end = start + length;
    let previous = start.checked_sub(1).and_then(|index| bytes.get(index));
    let next = bytes.get(end);
    if previous == Some(&b'/')
        || previous == Some(&b'\\')
        || next == Some(&b'/')
        || next == Some(&b'\\')
    {
        return true;
    }
    if (previous == Some(&b':') && start >= 2 && bytes[start - 2] == b':')
        || (next == Some(&b':') && bytes.get(end + 1) == Some(&b':'))
    {
        return true;
    }
    let dotted_before =
        previous == Some(&b'.') && start >= 2 && bytes[start - 2].is_ascii_alphanumeric();
    let dotted_after =
        next == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_alphanumeric);
    dotted_before || dotted_after
}

fn is_bare_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_code_identifier(value: &str) -> bool {
    is_bare_identifier(value)
        && (value.contains('_')
            || value
                .chars()
                .skip(1)
                .any(|character| character.is_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn generic_repository_and_history_questions_abstain() {
        for prompt in [
            "How is locale routing configured?",
            "Research the API retry behavior",
            "If the ApplicationModal UI were renamed, which files reference it?",
            "If ApplicationModal were renamed, which files reference it?",
            "Add a regression test for `foo`.",
            "Who introduced this idea?",
            "Show git history.",
            "Which commit introduced `Foo`?",
            "Trace the inline multiword description `locale routing behavior`.",
            "Who calls `Foo!`?",
            "Who calls ``?",
            "Who calls Foo::bar?",
            "Who calls Foo.bar?",
            "Who calls src/router.rs::handler?",
            "What is the impact of `src/router`?",
            "Rename src/foo.rs to src/bar.rs.",
            "Who calls `foo_bar` or `getUserById`?",
            "Who calls `foo_bar` and getUserById?",
            "Do not use Pixel; show native-only callers for FooBar.",
            "Who calls FooBar? Use native search only.",
        ] {
            assert_eq!(classify(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn one_bare_symbol_routes_explicit_caller_and_impact_questions() {
        for (prompt, expected) in [
            (
                "What are the direct callers and call path around transferPageToGhost?",
                "transferPageToGhost",
            ),
            ("Who calls get_user_by_id?", "get_user_by_id"),
            ("Who calls `Foo`?", "Foo"),
            (
                "Who calls transferPageToGhost in apps/notion-to-ghost?",
                "transferPageToGhost",
            ),
            (
                "What is the blast radius of changing ApplicationModal?",
                "ApplicationModal",
            ),
        ] {
            assert_eq!(classify(prompt).as_deref(), Some(expected), "{prompt}");
        }
    }
}
