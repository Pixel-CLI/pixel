// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The prompt side of the brief: the text the user typed, how clearly it asks
//! about code (`Signal`), and the task a long paste leaves (`specs/brief.md`,
//! "Signal and gate").

use std::borrow::Cow;

use crate::chain::Anchors;
use crate::routes;

/// Words that name code or ask a question about it. A prompt with none of
/// them and no identifier asks for something else (git, a release, a
/// discussion), and a retrieval route for it is noise a model learns to skip.
const CODE_WORDS: &[&str] = &[
    "api",
    "bug",
    "bugs",
    "call",
    "called",
    "callee",
    "callees",
    "caller",
    "callers",
    "calls",
    "class",
    "classes",
    "codebase",
    "constant",
    "crash",
    "crashes",
    "crate",
    "crates",
    "declaration",
    "declared",
    "defined",
    "definition",
    "endpoint",
    "enum",
    "error",
    "errors",
    "exception",
    "failing",
    "field",
    "fields",
    "function",
    "functions",
    "handler",
    "handlers",
    "hook",
    "hooks",
    "implement",
    "implementation",
    "implemented",
    "implements",
    "import",
    "imports",
    "interface",
    "method",
    "methods",
    "module",
    "modules",
    "panic",
    "panics",
    "parameter",
    "parser",
    "refactor",
    "regression",
    "rename",
    "schema",
    "signature",
    "struct",
    "structs",
    "symbol",
    "symbols",
    "test",
    "tests",
    "trait",
    "variable",
];

/// Openings of a question about how the code works or where something is.
const CODE_QUESTIONS: &[&str] = &[
    "explain ",
    "find the ",
    "fix ",
    "how does ",
    "how is ",
    "trace ",
    "what calls ",
    "what does ",
    "where are ",
    "where do ",
    "where does ",
    "where is ",
    "which file ",
    "which function ",
    "who calls ",
    "why does ",
];

/// Source extensions that make a token a file name.
pub(crate) const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cpp", "cs", "css", "ex", "exs", "go", "h", "hpp", "html", "java", "js", "json", "jsx",
    "kt", "lua", "md", "php", "py", "rb", "rs", "sh", "sql", "swift", "toml", "ts", "tsx", "vue",
    "yaml", "yml",
];

const PASTE_OPEN: &str = "<pasted_content";
const PASTE_CLOSE: &str = "</pasted_content";

/// The part of a prompt the user typed: pasted blocks (`<pasted_content …>`
/// to its closing tag) are someone else's text and never the task itself, nor
/// is the file listing Codex writes above its request heading
/// ([`crate::continuation::user_request`]).
/// A removed block leaves a space, so the words on either side stay apart
/// (`where is<block>the parser` keeps its `where is ` opening).
pub fn typed_text(prompt: &str) -> String {
    let mut typed = String::new();
    let mut rest = crate::continuation::user_request(prompt);
    while let Some(start) = find_tag(rest, PASTE_OPEN) {
        typed.push_str(&rest[..start]);
        let after = &rest[start + PASTE_OPEN.len()..];
        let Some(close) = find_tag(after, PASTE_CLOSE) else {
            return typed;
        };
        let tail = &after[close + PASTE_CLOSE.len()..];
        rest = tail.find('>').map_or("", |end| &tail[end + 1..]);
        if !rest.is_empty() {
            typed.push(' ');
        }
    }
    typed.push_str(rest);
    typed
}

/// Whether a whitespace-free `word` is a link: `http://`, `https://` or
/// `www.`, behind any opening punctuation.
fn is_url(word: &str) -> bool {
    let word = word
        .trim_start_matches(['(', '<', '[', '"', '\'', '`'])
        .to_ascii_lowercase();
    word.starts_with("http://") || word.starts_with("https://") || word.starts_with("www.")
}

/// `text` without its links: the query string of a pasted URL
/// (`?utm_source=x`) is not a code identifier. Whitespace is kept as typed.
pub(crate) fn strip_urls(text: &str) -> String {
    fn flush(word: &mut String, out: &mut String) {
        if !is_url(word) {
            out.push_str(word);
        }
        word.clear();
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The first `tag` that ends at a tag boundary (`>` or whitespace), so a
/// longer name such as `</pasted_contentious>` is not taken for it.
fn find_tag(haystack: &str, tag: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = haystack[from..].find(tag) {
        let at = from + found;
        let next = haystack[at + tag.len()..].chars().next();
        if next.is_none_or(|ch| ch == '>' || ch.is_whitespace()) {
            return Some(at);
        }
        from = at + tag.len();
    }
    None
}

/// A token that can only be a name in code: `a::b`, `snake_case`, `camelCase`
/// or `PascalCase` with an inner capital, a path, or a file with a source
/// extension (in any case, so `README.MD` counts). Surrounding punctuation,
/// a sentence's closing period and backticks are not part of the token.
pub(crate) fn names_code(token: &str) -> bool {
    let token = token.trim_matches(|ch: char| "()[]{}<>,.;:!?\"'`".contains(ch));
    let inner = |separator: char| {
        token.split(separator).count() > 1
            && token
                .split(separator)
                .all(|part| part.chars().next().is_some_and(char::is_alphanumeric))
    };
    let camel = token
        .chars()
        .zip(token.chars().skip(1))
        .any(|(before, after)| before.is_lowercase() && after.is_uppercase());
    let extension = token.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    });
    token.contains("::") || inner('_') || inner('/') || camel || extension
}

/// How clearly the typed text of a prompt asks about code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A backticked identifier or a code-shaped token (`a::b`, `snake_case`,
    /// `camelCase`, a path, a source file): no model needed to say so.
    Strong,
    /// Only a code word or a code-question opener matched: close enough to
    /// ask, ambiguous enough that a model verdict can help.
    Weak,
    /// Plain language with at least [`MIN_PROSE_KEYWORDS`] content words that
    /// is not a repository operation. Whether it is about this repository is
    /// not something its shape can say: the evidence decides.
    Prose,
}

impl Signal {
    /// The signal as the decision log spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Weak => "weak",
            Self::Prose => "prose",
        }
    }
}

/// The typed text of `prompt` and how strongly it asks about code, or `None`
/// when it asks for something else (git and release requests, pasted chat
/// threads, discussion). Only the typed text counts; pasted blocks never
/// steer the search.
pub fn code_signal(prompt: &str) -> Option<(String, Signal)> {
    let typed = strip_urls(&typed_text(prompt));
    let typed = typed.trim();
    if explicit_identifier(typed).is_some() || typed.split_whitespace().any(names_code) {
        return Some((typed.to_string(), Signal::Strong));
    }
    let lower = typed.to_lowercase();
    let weak = lower
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| CODE_WORDS.contains(&word))
        || lower
            .split(['.', '?', '!', ':', ';', ',', '\n'])
            .map(str::trim_start)
            .any(|clause| {
                CODE_QUESTIONS
                    .iter()
                    .any(|opening| clause.starts_with(opening))
            });
    weak.then(|| (typed.to_string(), Signal::Weak))
}

/// Typed text longer than this many characters, over at least
/// [`PASTE_TAIL_LINES`] lines, is a prompt with an untagged paste in front of
/// the question: only its last paragraph is the task.
const PASTE_TAIL_CHARS: usize = 600;
const PASTE_TAIL_LINES: usize = 4;
/// Characters of the first paragraph's head that a prompt made only of a
/// pasted block keeps (see [`pasted_task`]).
const PASTED_HEAD_CHARS: usize = 300;
/// Characters of the last paragraph's tail kept beside that head: a character
/// short of it, so that the line break joining the two leaves the whole within
/// [`PASTE_TAIL_CHARS`], which [`brief_task`] and the decision log do not cut.
const PASTED_TAIL_CHARS: usize = 299;
const _: () = assert!(PASTED_HEAD_CHARS + 1 + PASTED_TAIL_CHARS <= PASTE_TAIL_CHARS);
/// Content words a plain-language prompt needs before the relevance gate is
/// asked: one word is a reply, not a task.
const MIN_PROSE_KEYWORDS: usize = 2;

/// Words that name a repository operation (git, a pull request or an issue,
/// a release, a deploy, CI) in English and French, folded to ASCII as the
/// tokenizer folds them; the words that steer the agent's own run rather
/// than ask about code (`go next`, `stop`, `try again`); and the few function
/// and address words a short prompt leaves behind when its language is not
/// detected (`les`, `peux`, `you`). A prompt made only of these asks to
/// operate the repository or the agent, not to read the code: the #1032
/// replay briefed "Merge les PRs", "Tu peux les merger en --admin" and
/// "go next". Deliberately short: anything it misses meets the relevance
/// gate, which decides on evidence.
const OPS_WORDS: &[&str] = &[
    "actuel",
    "admin",
    "again",
    "ahead",
    "ai",
    "amend",
    "avec",
    "back",
    "bascule",
    "basculer",
    "branch",
    "branche",
    "branches",
    "bump",
    "can",
    "checkout",
    "cherry",
    "ci",
    "clone",
    "clore",
    "close",
    "commit",
    "commite",
    "commiter",
    "commits",
    "create",
    "creer",
    "current",
    "deploie",
    "deploy",
    "deployed",
    "deployer",
    "deux",
    "dis",
    "est",
    "fais",
    "ferme",
    "fermer",
    "fetch",
    "fini",
    "finished",
    "fusionne",
    "fusionner",
    "git",
    "go",
    "green",
    "head",
    "issue",
    "issues",
    "laisse",
    "les",
    "main",
    "master",
    "merge",
    "merged",
    "merger",
    "moi",
    "next",
    "non",
    "one",
    "order",
    "ordre",
    "origin",
    "oui",
    "pause",
    "peux",
    "pick",
    "pousse",
    "pousser",
    "pr",
    "prod",
    "production",
    "prs",
    "publie",
    "publier",
    "publish",
    "pull",
    "push",
    "rebase",
    "relance",
    "relancer",
    "release",
    "releases",
    "remote",
    "rerun",
    "reset",
    "retry",
    "revert",
    "ship",
    "squash",
    "staging",
    "stash",
    "stop",
    "suivant",
    "suivante",
    "sur",
    "switch",
    "tag",
    "tags",
    "termine",
    "tire",
    "toi",
    "try",
    "two",
    "upstream",
    "vas",
    "vers",
    "version",
    "versions",
    "verte",
    "worktree",
    "worktrees",
    "you",
];

/// Words that acknowledge, greet or judge the last answer. They are content
/// words to the tokenizer and carry nothing about the repository.
const CHATTER_WORDS: &[&str] = &[
    "awesome",
    "bonjour",
    "bravo",
    "cheers",
    "cool",
    "excellent",
    "fine",
    "genial",
    "good",
    "great",
    "hello",
    "looks",
    "merci",
    "nice",
    "okay",
    "parfait",
    "perfect",
    "salut",
    "sorry",
    "super",
    "sure",
    "thank",
    "thanks",
    "thx",
    "works",
    "worked",
    "yeah",
    "yep",
];

/// The part of the typed text that is the task: the last paragraph when the
/// text is long and spread over lines (an untagged paste before the
/// question), the whole text otherwise. Text with no blank line is one
/// paragraph and stays whole.
pub fn brief_task(typed: &str) -> &str {
    let typed = typed.trim();
    if typed.chars().count() > PASTE_TAIL_CHARS && typed.lines().count() >= PASTE_TAIL_LINES {
        last_paragraph(typed)
    } else {
        typed
    }
}

/// The text after the last blank line.
fn last_paragraph(text: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.trim().is_empty() {
            start = offset;
        }
    }
    text[start..].trim()
}

/// The text before the first blank line.
fn first_paragraph(text: &str) -> &str {
    let text = text.trim();
    let mut end = text.len();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            end = offset;
            break;
        }
        offset += line.len();
    }
    text[..end].trim()
}

/// The first `max` characters of `text`.
fn head_chars(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(end, _)| &text[..end])
}

/// The last `max` characters of `text`.
fn tail_chars(text: &str, max: usize) -> &str {
    let skip = text.chars().count().saturating_sub(max);
    text.char_indices()
        .nth(skip)
        .map_or("", |(start, _)| &text[start..])
}

/// The text inside every pasted block of `prompt`, in order: what
/// [`typed_text`] leaves out, between the same tags. A block that never closes
/// runs to the end of the prompt, and an opening tag cut off by the end of the
/// prompt holds nothing.
fn pasted_bodies(prompt: &str) -> Vec<&str> {
    let mut bodies = Vec::new();
    let mut rest = prompt;
    while let Some(start) = find_tag(rest, PASTE_OPEN) {
        let after = &rest[start + PASTE_OPEN.len()..];
        let close = find_tag(after, PASTE_CLOSE);
        // The body starts after the `>` that ends the opening tag.
        if let Some(from) = after.find('>').map(|end| end + 1) {
            let to = close.unwrap_or(after.len());
            if from <= to {
                bodies.push(&after[from..to]);
            }
        }
        let Some(close) = close else {
            break;
        };
        let tail = &after[close + PASTE_CLOSE.len()..];
        rest = tail.find('>').map_or("", |end| &tail[end + 1..]);
    }
    bodies
}

/// What a prompt made only of pasted blocks asks: the head of the first
/// paragraph of the paste and, when it has more than one paragraph, the tail
/// of the last, links dropped. A thread or a log opens with what it is about
/// and closes on what is wanted of it; the middle is the bulk that would swamp
/// the words of the task. `None` when the paste holds no text.
fn pasted_task(prompt: &str) -> Option<String> {
    let pasted = strip_urls(&pasted_bodies(prompt).join("\n\n"));
    let pasted = pasted.trim();
    let (first, last) = (first_paragraph(pasted), last_paragraph(pasted));
    let head = head_chars(first, PASTED_HEAD_CHARS).trim_end();
    if head.is_empty() {
        return None;
    }
    if first == last {
        return Some(head.to_string());
    }
    let tail = tail_chars(last, PASTED_TAIL_CHARS).trim_start();
    Some(format!("{head}\n{tail}"))
}

/// The content words of a task: the tokenizer's keywords without the
/// acknowledgements and greetings of a conversation. A plural acronym is one
/// word (see [`fold_plural_acronyms`]).
pub(crate) fn brief_keywords(task: &str) -> Vec<String> {
    crate::tokens::tokenize_task(&fold_plural_acronyms(task))
        .map(|query| query.keywords)
        .unwrap_or_default()
        .into_iter()
        .filter(|word| !CHATTER_WORDS.contains(&word.as_str()))
        .collect()
}

/// `task` with each plural acronym (`PRs`, `APIs`, `IDs`: two capitals or
/// more and a final `s`) written in capitals, so the tokenizer keeps it whole.
/// Split at its case change, `PRs` became the keywords `p` and `rs`, and `rs`
/// is a short technical keyword (the Rust extension) that a merge request
/// carried into the relevance gate.
fn fold_plural_acronyms(task: &str) -> Cow<'_, str> {
    let plural = |word: &str| {
        word.len() >= 3
            && word.ends_with('s')
            && word[..word.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_uppercase())
    };
    if !task
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(plural)
    {
        return Cow::Borrowed(task);
    }
    let mut folded = String::with_capacity(task.len());
    let mut word = String::new();
    for ch in task.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() {
            word.push(ch);
            continue;
        }
        if plural(&word) {
            word.make_ascii_uppercase();
        }
        folded.push_str(&word);
        word.clear();
        folded.push(ch);
    }
    folded.pop();
    Cow::Owned(folded)
}

/// A version number as the tokenizer leaves one: digits, or `v` and digits.
fn is_version_shaped(word: &str) -> bool {
    let digits = word.strip_prefix('v').unwrap_or(word);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

/// Every keyword names an operation or a version: git, a release, a deploy.
fn is_ops_request(keywords: &[String]) -> bool {
    keywords
        .iter()
        .all(|word| OPS_WORDS.contains(&word.as_str()) || is_version_shaped(word))
}

/// The task of `prompt` and why it may deserve a brief, or `None` when it
/// cannot: [`code_signal`] first, and failing that [`Signal::Prose`] for
/// plain language with at least [`MIN_PROSE_KEYWORDS`] content words that is
/// not a repository operation. Only the typed text counts, and of a long
/// text with a paste in front only its last paragraph. A prompt with nothing
/// typed at all but a pasted block is judged on [`pasted_task`] instead, and
/// only ever as plain language: the identifiers of someone else's log do not
/// make it code-shaped, so the relevance gate decides it.
pub fn brief_signal(prompt: &str) -> Option<(String, Signal)> {
    if let Some(found) = code_signal(prompt) {
        return Some(found);
    }
    let typed = strip_urls(&typed_text(prompt));
    let typed = if typed.trim().is_empty() {
        pasted_task(prompt)?
    } else {
        typed
    };
    let task = brief_task(&typed);
    // A whole-purpose question (`what is this project for?`) carries too few
    // content words to clear MIN_PROSE_KEYWORDS, but routing knows what to do
    // with it; never refuse it before it can be classified.
    let anchors = Anchors::from_text(task);
    if routes::asks_overview(task) && anchors.names().next().is_none() && anchors.paths().is_empty()
    {
        return Some((task.to_string(), Signal::Prose));
    }
    let keywords = brief_keywords(task);
    (keywords.len() >= MIN_PROSE_KEYWORDS && !is_ops_request(&keywords))
        .then(|| (task.to_string(), Signal::Prose))
}

fn explicit_identifier(task: &str) -> Option<String> {
    let (_, tail) = task.split_once('`')?;
    let (identifier, _) = tail.split_once('`')?;
    let identifier = identifier.trim();
    (!identifier.is_empty()
        && identifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_:-./".contains(ch)))
    .then(|| identifier.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typed text of `prompt` when it asks about code, whichever strength.
    fn code_request(prompt: &str) -> Option<String> {
        code_signal(prompt).map(|(typed, _)| typed)
    }

    #[test]
    fn code_signal_should_take_only_prompts_that_ask_about_code() {
        // Prompts from real sessions on 2026-10-04 that received a route.
        for prompt in [
            "go to branch main and pull",
            "commit and push",
            "release 0.6.2",
            "This is a good question, and I'm not sure about the answer because I would say yes, it should rewrite.",
            "So basically we should also do it with codex, of course, because codex does not necessarily give the same result.",
            "<pasted_content id=\"4f44\">\nLe model il s'en fout de pixel. `grep` src/main.rs\n</pasted_content id=\"4f44\">\n\nWe have only one remaining task: coherence.",
            "",
        ] {
            assert_eq!(code_request(prompt), None, "{prompt}");
        }
        for (prompt, typed) in [
            ("Trace callers of `Foo::bar`", "Trace callers of `Foo::bar`"),
            (
                "where is retrieval_route called",
                "where is retrieval_route called",
            ),
            (
                "How does task preparation refresh stale source evidence?",
                "How does task preparation refresh stale source evidence?",
            ),
            ("make TaskHookEvent cheaper", "make TaskHookEvent cheaper"),
            ("look at pixel_task::digest", "look at pixel_task::digest"),
            ("open crates/pixel/src/guard", "open crates/pixel/src/guard"),
            ("check guard.rs", "check guard.rs"),
            ("fix the failing test", "fix the failing test"),
            (
                "explain the guard's precedence rules",
                "explain the guard's precedence rules",
            ),
            (
                "fix the hook so a prompt keeps the task",
                "fix the hook so a prompt keeps the task",
            ),
            (
                "What does the watchdog guarantee?",
                "What does the watchdog guarantee?",
            ),
            ("Ok. Who calls the daemon?", "Ok. Who calls the daemon?"),
            // Each signal on its own: a backticked plain word, a code word,
            // a code question after a sentence break.
            ("ask about `config`", "ask about `config`"),
            (
                "the parser panics on empty input",
                "the parser panics on empty input",
            ),
            (
                "Thanks. Where is the config loaded?",
                "Thanks. Where is the config loaded?",
            ),
            (
                "<pasted_content id=\"1\">\nchat\n</pasted_content id=\"1\">\nWhy does the parser panic?",
                "Why does the parser panic?",
            ),
            // A code question after any clause break, not only ". ".
            (
                "Hey, where is the config loaded?",
                "Hey, where is the config loaded?",
            ),
            (
                "Context:\nwhy does the daemon stall",
                "Context:\nwhy does the daemon stall",
            ),
            ("Done! where is the config", "Done! where is the config"),
            // A block glued to the words around it does not fuse them.
            (
                "where is<pasted_content>x</pasted_content>the config",
                "where is the config",
            ),
        ] {
            assert_eq!(code_request(prompt).as_deref(), Some(typed), "{prompt}");
        }
    }

    #[test]
    fn strip_urls_should_drop_links_and_keep_the_rest_as_typed() {
        assert_eq!(
            strip_urls("see https://x.io/a?utm_source=b now"),
            "see  now"
        );
        assert_eq!(strip_urls("(www.example.com/p) and HTTP://Y.org"), " and ");
        assert_eq!(strip_urls("a\n<https://x.io>\nb"), "a\n\nb");
        assert_eq!(
            strip_urls("fix handle_error in src/a.rs"),
            "fix handle_error in src/a.rs"
        );
        assert_eq!(strip_urls("wwwhere is www_thing"), "wwwhere is www_thing");
    }

    #[test]
    fn a_link_alone_should_never_be_taken_for_a_code_identifier() {
        for prompt in [
            "https://example.com/post?utm_source=newsletter&utm_medium=email",
            "look at www.example.com/landing?utm_source=x",
            "<https://github.com/org/repo/pull/12?utm_source=slack>",
        ] {
            assert_eq!(code_signal(prompt), None, "{prompt}");
            assert_eq!(brief_signal(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn a_link_beside_a_real_identifier_should_leave_the_identifier_alone_strong() {
        let (typed, signal) =
            code_signal("why does retry_loop fail, see https://x.io/p?utm_source=y").unwrap();
        assert_eq!(signal, Signal::Strong);
        assert!(
            !typed.contains("utm_source") && !typed.contains("x.io"),
            "{typed}"
        );
        assert!(typed.contains("retry_loop"), "{typed}");
        // The link alone would have been the only code-shaped token.
        assert!(code_signal("see https://x.io/p?utm_source=y").is_none());
    }

    #[test]
    fn typed_text_should_drop_every_pasted_block_and_an_unclosed_one() {
        assert_eq!(
            typed_text(
                "a<pasted_content id=\"1\">x</pasted_content id=\"1\">b<pasted_content>y</pasted_content>c"
            ),
            "a b c"
        );
        // A longer tag name neither opens nor closes a block.
        assert_eq!(
            typed_text("<pasted_content>q </pasted_contentious>check guard.rs</pasted_content>x"),
            " x"
        );
        assert_eq!(
            typed_text("see <pasted_contents> here"),
            "see <pasted_contents> here"
        );
        // A tag cut off by the end of the prompt still opens a block.
        assert_eq!(typed_text("a <pasted_content"), "a ");
        // A block that ends the prompt leaves no trailing separator.
        assert_eq!(typed_text("x<pasted_content>y</pasted_content>"), "x");
        assert_eq!(
            typed_text("keep<pasted_content id=\"2\">never closed"),
            "keep"
        );
        assert_eq!(typed_text("x<pasted_content>y</pasted_content"), "x");
        assert_eq!(typed_text("plain"), "plain");
    }

    #[test]
    fn names_code_should_accept_code_shapes_and_reject_prose() {
        for token in [
            "a::b",
            "snake_case",
            "(snake_case)",
            "camelCase",
            "PascalCase",
            "src/main",
            "a.rs",
            "Cargo.toml",
            "x.tsx,",
            "guard.rs.",
            "`guard.rs`",
            "README.MD",
        ] {
            assert!(names_code(token), "{token}");
        }
        for token in [
            "Hello",
            "word",
            "_private",
            "trailing_",
            "a//b",
            "https://",
            "e.g.",
            ".rs",
            "v0.6",
            "ALLCAPS",
            "x.unknown",
            "/",
        ] {
            assert!(!names_code(token), "{token}");
        }
    }

    /// `pad` padded so the whole text is `len` characters over four lines,
    /// the last two forming the final paragraph.
    fn pasted_text(len: usize) -> String {
        let rest = "\n\nwhy is the daemon slow\nafter startup";
        format!("{}{rest}", "p".repeat(len - rest.len()))
    }

    #[test]
    fn brief_task_should_keep_the_whole_text_up_to_the_character_and_line_bounds() {
        let tail = "why is the daemon slow\nafter startup";
        // Exactly 600 characters over four lines is not past the bound.
        let at = pasted_text(PASTE_TAIL_CHARS);
        assert_eq!(at.chars().count(), 600);
        assert_eq!(at.lines().count(), 4);
        assert_eq!(brief_task(&at), at);
        // One more character and the paste in front is dropped.
        let over = pasted_text(PASTE_TAIL_CHARS + 1);
        assert_eq!(brief_task(&over), tail);
        assert_eq!(brief_task(&pasted_text(1000)), tail);
        // Three lines are one paragraph and a question: still whole.
        let three = format!("{}\n\nwhy is it slow", "p".repeat(PASTE_TAIL_CHARS));
        assert_eq!(three.lines().count(), PASTE_TAIL_LINES - 1);
        assert_eq!(brief_task(&three), three);
        // Four lines is the lower bound that counts.
        let four = format!("{}\n\nwhy is\nit slow", "p".repeat(PASTE_TAIL_CHARS));
        assert_eq!(four.lines().count(), PASTE_TAIL_LINES);
        assert_eq!(brief_task(&four), "why is\nit slow");
    }

    #[test]
    fn brief_task_should_trim_and_keep_a_long_text_without_a_blank_line_whole() {
        let block = "log line\n".repeat(80);
        assert_eq!(brief_task(&block), block.trim());
        assert_eq!(brief_task("  short question  "), "short question");
        assert_eq!(brief_task(""), "");
    }

    #[test]
    fn last_paragraph_should_start_after_the_last_blank_line_of_any_kind() {
        assert_eq!(last_paragraph("a\n\nb"), "b");
        assert_eq!(last_paragraph("a\n   \nb\nc"), "b\nc");
        assert_eq!(last_paragraph("a\r\n\r\nb"), "b");
        assert_eq!(last_paragraph("a\n\nb\n\nc"), "c");
        assert_eq!(last_paragraph("a\nb"), "a\nb");
    }

    #[test]
    fn brief_signal_should_return_a_code_shaped_prompt_exactly_as_code_signal_does() {
        for prompt in [
            "Trace callers of `Foo::bar`",
            "where is retrieval_route called",
            "fix the failing test",
            "How does task preparation refresh stale source evidence?",
            "make TaskHookEvent cheaper",
        ] {
            assert!(code_signal(prompt).is_some(), "{prompt}");
            assert_eq!(brief_signal(prompt), code_signal(prompt), "{prompt}");
        }
    }

    #[test]
    fn brief_signal_should_take_plain_language_as_prose() {
        for prompt in [
            "the metrics line is missing when i run the pie harness",
            "so it would be nice if search content could print how many files matched",
            "comment on modifie un fichier point env sans perdre aucune cle",
            "relance la ci qui a plante",
            // Nothing in the shape says these are off topic: the evidence does.
            "what's the weather going to be like tomorrow",
            "quel temps fera-t-il demain",
        ] {
            assert_eq!(
                brief_signal(prompt),
                Some((prompt.to_string(), Signal::Prose)),
                "{prompt}"
            );
        }
    }

    #[test]
    fn brief_signal_should_take_a_whole_purpose_question_however_few_content_words() {
        // "what is this project for" has one content word (`for` is a stop
        // word), below MIN_PROSE_KEYWORDS, but it names the whole and the
        // router answers it (issue #988).
        for prompt in [
            "what is this project for",
            "what is this repo for",
            "tell me what this repository is for",
            "what is the purpose of this project",
        ] {
            assert_eq!(
                brief_signal(prompt),
                Some((prompt.to_string(), Signal::Prose)),
                "{prompt}"
            );
        }
        // A question about a part is not the whole and still passes on its
        // own content words, unchanged.
        assert!(brief_signal("what is the project config for").is_some());
    }

    #[test]
    fn brief_signal_should_refuse_operations_acknowledgements_and_one_word_replies() {
        for prompt in [
            "commit and push",
            "go to branch main and pull",
            "release 0.6.2",
            "pousse sur main",
            "merge main and push it",
            "deploy v12 to production",
            "thanks, that works",
            "thanks, that looks good",
            "merci parfait",
            "daemon",
            "thanks daemon",
            "",
            "   ",
        ] {
            assert_eq!(brief_signal(prompt), None, "{prompt:?}");
        }
    }

    #[test]
    fn brief_signal_should_need_two_content_words_and_not_one() {
        assert_eq!(MIN_PROSE_KEYWORDS, 2);
        assert_eq!(
            brief_signal("daemon startup").map(|found| found.1),
            Some(Signal::Prose)
        );
        assert_eq!(brief_signal("daemon"), None);
        assert_eq!(brief_signal("the daemon"), None);
        // An operation word beside a content word is a content word's prompt.
        assert_eq!(
            brief_signal("push daemon startup").map(|found| found.1),
            Some(Signal::Prose)
        );
    }

    #[test]
    fn brief_signal_should_ignore_a_pasted_block_and_judge_the_last_paragraph_of_a_paste() {
        let tagged = "<pasted_content id=\"1\">\nthe daemon startup race watch\n</pasted_content id=\"1\">\nthanks";
        assert_eq!(brief_signal(tagged), None);
        // An untagged paste of a thread, then "thoughts?": one content word.
        let untagged = format!(
            "{}\n\nthoughts?",
            "slack message about the daemon startup\n".repeat(30)
        );
        assert!(untagged.chars().count() > PASTE_TAIL_CHARS);
        assert_eq!(brief_signal(&untagged), None);
        // The same paste with a real question after it.
        let asked = format!(
            "{}\n\nwhy is the daemon slow after startup",
            "chat line\n".repeat(80)
        );
        assert_eq!(
            brief_signal(&asked),
            Some((
                "why is the daemon slow after startup".to_string(),
                Signal::Prose
            ))
        );
    }

    /// A prompt that is only a pasted block holding `lines`.
    fn paste_only(lines: &[&str]) -> String {
        let mut all = vec!["<pasted_content id=\"1\">"];
        all.extend_from_slice(lines);
        all.push("</pasted_content id=\"1\">");
        all.join("\n")
    }

    #[test]
    fn brief_signal_should_judge_a_paste_only_prompt_on_its_first_and_last_paragraphs() {
        let prompt = paste_only(&[
            "the daemon misses changes made during startup",
            "after a rebase the index looks stale",
            "",
            "logs from the second run follow below",
            "",
            "so can we make the watcher wait for the first scan",
        ]);
        // Nothing is typed, and the code-shaped signal never reads the paste.
        assert_eq!(typed_text(&prompt), "");
        assert_eq!(code_signal(&prompt), None);
        assert_eq!(code_request(&prompt), None);
        assert_eq!(
            brief_signal(&prompt),
            Some((
                [
                    "the daemon misses changes made during startup",
                    "after a rebase the index looks stale",
                    "so can we make the watcher wait for the first scan",
                ]
                .join("\n"),
                Signal::Prose
            ))
        );
    }

    #[test]
    fn brief_signal_should_keep_a_one_paragraph_paste_to_its_head_and_not_repeat_it() {
        let one = paste_only(&["the daemon misses changes", "made during startup"]);
        assert_eq!(
            brief_signal(&one),
            Some((
                "the daemon misses changes\nmade during startup".to_string(),
                Signal::Prose
            ))
        );
        // The same text opening and closing the paste is read once.
        let twice = [
            "<pasted_content>the daemon startup race</pasted_content>",
            "<pasted_content>the daemon startup race</pasted_content>",
        ]
        .join("\n");
        assert_eq!(
            brief_signal(&twice),
            Some(("the daemon startup race".to_string(), Signal::Prose))
        );
    }

    #[test]
    fn brief_signal_should_join_the_first_block_head_to_the_last_block_tail() {
        let two = [
            "<pasted_content id=\"1\">",
            "the daemon misses changes made during startup",
            "</pasted_content id=\"1\">",
            "<pasted_content id=\"2\">",
            "so make the watcher wait for the first scan",
            "</pasted_content id=\"2\">",
        ]
        .join("\n");
        assert_eq!(
            brief_signal(&two),
            Some((
                [
                    "the daemon misses changes made during startup",
                    "so make the watcher wait for the first scan",
                ]
                .join("\n"),
                Signal::Prose
            ))
        );
    }

    /// The task of a prompt that is a pasted block of two paragraphs.
    fn pasted_two(first: &str, last: &str) -> String {
        brief_signal(&paste_only(&[first, "", last]))
            .expect("a paste with a task")
            .0
    }

    #[test]
    fn brief_signal_should_cap_a_paste_to_a_head_and_a_tail_the_task_bound_leaves_whole() {
        // 15 and 13 characters of words, then a filler to the length asked.
        let head = |len: usize| format!("daemon startup {}", "x".repeat(len - 15));
        let tail = |len: usize| format!("{} watcher scan", "y".repeat(len - 13));
        let kept = format!("{}\n{}", head(PASTED_HEAD_CHARS), tail(PASTED_TAIL_CHARS));
        assert_eq!(kept.chars().count(), PASTE_TAIL_CHARS);
        // At the bound, one character over it, and far over it.
        for (first, last) in [(300, 299), (301, 300), (5_000, 5_000)] {
            assert_eq!(
                pasted_two(&head(first), &tail(last)),
                kept,
                "{first} {last}"
            );
        }
        // One character under the bound loses nothing.
        assert_eq!(
            pasted_two(&head(299), &tail(298)),
            format!("{}\n{}", head(299), tail(298))
        );
        // What the brief judges is what the later stages keep whole.
        assert_eq!(brief_task(&kept), kept);
    }

    #[test]
    fn brief_signal_should_not_leave_a_blank_at_either_end_of_a_cut_paste() {
        // The head's 300th character is a space, and so is the first of the
        // 299 the tail keeps.
        let head = format!("daemon startup {} {}", "x".repeat(284), "z".repeat(40));
        assert_eq!(head.chars().nth(PASTED_HEAD_CHARS - 1), Some(' '));
        let tail = format!("{} {}", "y".repeat(50), "z".repeat(298));
        let skipped = tail.chars().count() - PASTED_TAIL_CHARS;
        assert_eq!(tail.chars().nth(skipped), Some(' '));
        assert_eq!(
            pasted_two(&head, &tail),
            format!("{}\n{}", &head[..PASTED_HEAD_CHARS - 1], "z".repeat(298))
        );
    }

    #[test]
    fn brief_signal_should_read_an_unclosed_paste_to_the_end_of_the_prompt() {
        let unclosed = [
            "<pasted_content id=\"2\">",
            "the daemon misses changes made during startup",
            "",
            "so can we make the watcher wait",
        ]
        .join("\n");
        assert_eq!(typed_text(&unclosed), "");
        assert_eq!(
            brief_signal(&unclosed),
            Some((
                [
                    "the daemon misses changes made during startup",
                    "so can we make the watcher wait",
                ]
                .join("\n"),
                Signal::Prose
            ))
        );
        // An opening tag cut off by the end of the prompt holds nothing to read.
        assert_eq!(brief_signal("<pasted_content"), None);
        assert_eq!(brief_signal("<pasted_content id=\"2\""), None);
        // Typed words beside the paste keep the paste out of the task.
        assert_eq!(
            brief_signal("<pasted_content>the daemon startup race</pasted_content> ok"),
            None
        );
    }

    #[test]
    fn brief_signal_should_still_decline_a_paste_that_asks_for_no_task() {
        for lines in [
            &["git commit and push", "", "release 0.6.2"][..],
            &["daemon"][..],
            &["thanks, that works"][..],
            &["https://example.com/a?utm_source=newsletter"][..],
            &["   ", ""][..],
            &[][..],
        ] {
            let prompt = paste_only(lines);
            assert_eq!(brief_signal(&prompt), None, "{prompt}");
        }
        assert_eq!(brief_signal("<pasted_content></pasted_content>"), None);
        assert_eq!(brief_signal(""), None);
        assert_eq!(brief_signal("  \n "), None);
    }

    #[test]
    fn brief_signal_should_never_take_a_pasted_log_for_code_shaped() {
        let prompt = paste_only(&[
            "thread 'main' panicked at src/fetch_user.rs:42 on `Option::unwrap()`",
            "",
            "why does fetchUser fail on an empty cache",
        ]);
        assert_eq!(code_signal(&prompt), None);
        assert_eq!(
            brief_signal(&prompt),
            Some((
                [
                    "thread 'main' panicked at src/fetch_user.rs:42 on `Option::unwrap()`",
                    "why does fetchUser fail on an empty cache",
                ]
                .join("\n"),
                Signal::Prose
            ))
        );
    }

    #[test]
    fn brief_signal_should_drop_the_links_of_a_paste_before_it_cuts_it() {
        let prompt = paste_only(&[
            "https://example.com/a?utm_source=x the daemon misses changes",
            "",
            "see https://x.io/b then make the watcher wait",
        ]);
        assert_eq!(
            brief_signal(&prompt),
            Some((
                "the daemon misses changes\nsee  then make the watcher wait".to_string(),
                Signal::Prose
            ))
        );
    }

    #[test]
    fn pasted_bodies_should_return_what_typed_text_leaves_out() {
        let none: Vec<&str> = Vec::new();
        assert_eq!(pasted_bodies("plain"), none);
        assert_eq!(
            pasted_bodies(
                "a<pasted_content id=\"1\">x</pasted_content id=\"1\">b<pasted_content>y</pasted_content>c"
            ),
            ["x", "y"]
        );
        // A longer tag name neither opens nor closes a block.
        assert_eq!(
            pasted_bodies(
                "<pasted_content>q </pasted_contentious>check guard.rs</pasted_content>x"
            ),
            ["q </pasted_contentious>check guard.rs"]
        );
        assert_eq!(pasted_bodies("see <pasted_contents> here"), none);
        // A tag cut off by the end of the prompt holds nothing; a block that
        // never closes runs to the end; a closing tag may be cut off too.
        assert_eq!(pasted_bodies("a <pasted_content"), none);
        assert_eq!(pasted_bodies("a <pasted_content id=\"2\""), none);
        assert_eq!(
            pasted_bodies("keep<pasted_content id=\"2\">never closed"),
            ["never closed"]
        );
        assert_eq!(pasted_bodies("x<pasted_content>y</pasted_content"), ["y"]);
        assert_eq!(pasted_bodies("<pasted_content></pasted_content>"), [""]);
        // A `>` that only comes with the closing tag ends no opening tag.
        assert_eq!(pasted_bodies("<pasted_content </pasted_content>z"), none);
    }

    #[test]
    fn pasted_task_should_be_none_when_the_paste_holds_no_text() {
        for prompt in [
            "",
            "plain",
            "<pasted_content",
            "<pasted_content></pasted_content>",
            "<pasted_content>  \n </pasted_content>",
            "<pasted_content>https://example.com/a?utm_source=x</pasted_content>",
        ] {
            assert_eq!(pasted_task(prompt), None, "{prompt:?}");
        }
        assert_eq!(
            pasted_task("<pasted_content>one two</pasted_content>").as_deref(),
            Some("one two")
        );
    }

    #[test]
    fn first_paragraph_should_stop_at_the_first_blank_line_of_any_kind() {
        assert_eq!(first_paragraph("a\n\nb"), "a");
        assert_eq!(first_paragraph("a\nb\n   \nc"), "a\nb");
        assert_eq!(first_paragraph("a\r\nb\r\n\r\nc"), "a\r\nb");
        assert_eq!(first_paragraph("\n\n  a\n\nb\n\n"), "a");
        assert_eq!(first_paragraph("a\nb"), "a\nb");
        assert_eq!(first_paragraph(""), "");
    }

    #[test]
    fn head_chars_and_tail_chars_should_count_characters_and_keep_a_short_text_whole() {
        assert_eq!(head_chars("héllo", 2), "hé");
        assert_eq!(tail_chars("héllo", 2), "lo");
        assert_eq!(tail_chars("héllo", 4), "éllo");
        for max in [5, 6, 100] {
            assert_eq!(head_chars("héllo", max), "héllo");
            assert_eq!(tail_chars("héllo", max), "héllo");
        }
        assert_eq!(head_chars("héllo", 0), "");
        assert_eq!(tail_chars("héllo", 0), "");
        assert_eq!(head_chars("", 3), "");
        assert_eq!(tail_chars("", 3), "");
    }

    #[test]
    fn is_ops_request_should_need_every_keyword_to_be_an_operation_or_a_version() {
        let words = |text: &[&str]| text.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(is_ops_request(&words(&["commit", "push"])));
        assert!(is_ops_request(&words(&["release", "2026"])));
        assert!(is_ops_request(&words(&["deploy", "v12"])));
        assert!(is_ops_request(&words(&["pousse", "sur", "main"])));
        assert!(!is_ops_request(&words(&["commit", "parser"])));
        assert!(!is_ops_request(&words(&["parser", "push"])));
        assert!(!is_ops_request(&words(&["parser", "daemon"])));
    }

    #[test]
    fn is_version_shaped_should_take_digits_with_an_optional_leading_v() {
        for word in ["2026", "v12", "7", "v7"] {
            assert!(is_version_shaped(word), "{word}");
        }
        for word in ["", "v", "vx", "12a", "release", "v1x"] {
            assert!(!is_version_shaped(word), "{word}");
        }
    }

    #[test]
    fn brief_keywords_should_drop_conversation_and_keep_content_words() {
        assert_eq!(
            brief_keywords("thanks, the daemon startup works"),
            ["daemon", "startup"]
        );
        assert!(brief_keywords("merci, parfait").is_empty());
        assert!(brief_keywords("").is_empty());
    }

    /// The briefed prompts of the #1032 replay that asked to operate a pull
    /// request, an issue or the agent itself, in both languages: none of them
    /// is about the code, and each costs the precision of the brief.
    #[test]
    fn brief_signal_should_refuse_a_pull_request_an_issue_or_a_steer_of_the_agent() {
        for prompt in [
            "Merge the PRs",
            "Merge les PRs",
            "Tu peux les merger en --admin",
            "Oui, merge #1122 avec --admin si sa CI est verte sur le head actuel.",
            "Merge mes PRs dans l'ordre avec --admin",
            "Creer les issues et PRs",
            "La CI de 1180 est termine",
            "stop j'ai merge 1189",
            "go next",
            "Can you try again",
            "Vas y je te laisse faire fais tout ca dans des worktrees",
            "## My request:\nMerge les PRs en --admin",
        ] {
            assert_eq!(brief_signal(prompt), None, "{prompt:?}");
        }
    }

    /// The same words beside one about the code leave the prompt to the
    /// relevance gate: only a prompt made of nothing else is refused.
    #[test]
    fn brief_signal_should_keep_an_operation_word_beside_a_word_about_the_code() {
        for prompt in [
            "the PRs dashboard rejects drafts",
            "close the socket when the daemon stops",
            "merge the lexer into the grammar",
            "Can you handle now https://github.com/o/r/issues/982 about the tokenizer cache",
            "pourquoi le head du lexique est faux",
            "## My request:\nhow the lexer grammar loads",
        ] {
            assert_eq!(
                brief_signal(prompt).map(|found| found.1),
                Some(Signal::Prose),
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn fold_plural_acronyms_should_capitalise_a_plural_acronym_and_nothing_else() {
        assert_eq!(fold_plural_acronyms("Merge the PRs."), "Merge the PRS.");
        assert_eq!(
            fold_plural_acronyms("APIs, IDs and URLs"),
            "APIS, IDS and URLS"
        );
        for text in [
            "Is it macOS?",
            "CSS is fine",
            "Os",
            "the PRS",
            "fix PR 12",
            "",
        ] {
            assert_eq!(fold_plural_acronyms(text), text, "{text:?}");
            assert!(
                matches!(fold_plural_acronyms(text), Cow::Borrowed(_)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn brief_keywords_should_keep_a_plural_acronym_whole() {
        // Split at its case change, `PRs` was `p` and the Rust extension `rs`.
        assert_eq!(brief_keywords("Merge the PRs"), ["merge", "prs"]);
        assert_eq!(brief_keywords("list the APIs"), ["list", "apis"]);
    }

    #[test]
    fn typed_text_should_drop_the_file_listing_above_a_codex_request() {
        let prompt = "# Files mentioned by the user:\n## shot.png: /tmp/shot.png\n## My request:\nwhere is the parser";
        assert_eq!(typed_text(prompt), "where is the parser");
    }

    #[test]
    fn signal_should_name_itself_for_the_decision_log() {
        assert_eq!(Signal::Strong.as_str(), "strong");
        assert_eq!(Signal::Weak.as_str(), "weak");
        assert_eq!(Signal::Prose.as_str(), "prose");
    }

    #[test]
    fn audited_paste_scanning_should_keep_every_adjacent_body_and_the_unicode_tail() {
        assert_eq!(
            pasted_bodies(
                "<pasted_content>first</pasted_content><pasted_content>déjà</pasted_content><pasted_content kind='log'>third</pasted_content>"
            ),
            ["first", "déjà", "third"]
        );
    }
}
