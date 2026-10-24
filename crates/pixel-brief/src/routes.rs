// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The question-kind router of the prompt-start brief.
//!
//! After the fixed prefix (literal search, concept fallback, symbol pick),
//! the chain spends the ops it has left on the evidence shape the prompt
//! actually asked for: a call path, covering tests, a file's signature,
//! history, or the definition's body. [`QuestionKind`] names that shape; it
//! comes from the typed text, by word lists, never from a model.

use crate::chain::Anchors;

/// The evidence shape a prompt asks for. `Lookup` is the literal-lookup
/// brief the chain always ran; the other kinds compose daemon or local
/// ops after the prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuestionKind {
    /// The default: a file search, a symbol pick, its first source line.
    Lookup,
    /// How one symbol reaches another (`evaluate`/`trace` witness hops).
    Flow,
    /// Which test files exercise a symbol (`uses` callers, test-filtered).
    Tests,
    /// Configuration, install or hook questions: JSON paths stay admitted
    /// (the generated filter skips only this kind), plus `list-signatures`
    /// on a named file.
    Config,
    /// Why or when code came to be: a facts-freshness probe, then a
    /// read-only history search on the phrase.
    Rationale,
    /// Bugfix, refactor and review prompts: `impact` has already run under
    /// change intent; this adds the definition's bounded body
    /// (`pack-context`) so the `defined` line carries code, not a guess.
    Bugfix,
    /// How the repository is organised: a tour, the lay of the land, what
    /// each crate or module is for. The evidence is the repository itself
    /// (its map, the head of its architecture document), not a symbol.
    Overview,
    /// When, why or by whom something changed, asked in so many words:
    /// "which commit", "when did", "who added". The evidence is the
    /// commits of the history index that touched it.
    History,
    /// A review of a change: the latest commit, a diff, a branch, the
    /// working tree. The evidence is the change and the code around it.
    Review,
}

/// `find-code`-style words that ask how one symbol reaches another.
const FLOW_WORDS: &[&str] = &[
    "reach", "reaches", "reached", "reaching", "flow", "flows", "path", "paths", "through", "call",
    "calls", "called", "calling", "caller", "callers", "callee", "callees",
];
/// Words that ask about test coverage.
const TEST_WORDS: &[&str] = &[
    "test",
    "tests",
    "tested",
    "testing",
    "spec",
    "specs",
    "assert",
    "asserts",
    "asserted",
    "assertion",
    "assertions",
];
/// Words that explicitly ask about a setting: a configuration, an
/// environment variable, a flag or option, a default value. A topic that
/// merely sits near configuration (`install`, `hook`) does not route here.
const CONFIG_WORDS: &[&str] = &[
    "config",
    "configs",
    "configure",
    "configured",
    "configures",
    "configuring",
    "configuration",
    "setting",
    "settings",
    "env",
    "environment",
    "flag",
    "flags",
    "option",
    "options",
    "default",
    "defaults",
];
/// Flow words a plain-language question may carry without naming a symbol:
/// the narrower list, since `path`, `through` and the noun `flow` ("the
/// login flow") are everyday words.
const PROSE_FLOW_WORDS: &[&str] = &[
    "reach", "reaches", "reached", "reaching", "call", "calls", "called", "calling", "caller",
    "callers", "callee", "callees", "trace", "traces", "traced", "tracing",
];
/// Words that ask why code exists or when it arrived.
const RATIONALE_WORDS: &[&str] = &[
    "why",
    "history",
    "decision",
    "decisions",
    "introduced",
    "introduces",
    "when",
    "commit",
    "commits",
    "changelog",
    "removed",
    "removes",
];
/// Phrases that ask how the whole is put together; they need a noun for the
/// whole (see [`WHOLE_WORDS`]), since `how is the config structured` is
/// about a part.
const OVERVIEW_HOW: &[&str] = &[
    "organised",
    "organized",
    "structured",
    "laid out",
    "architecture",
];
/// Phrases that ask for a tour of something; any noun of [`SCOPE_WORDS`]
/// will do.
const OVERVIEW_CUES: &[&str] = &[
    "tour",
    "overview",
    "high level",
    "big picture",
    "main modules",
    "main crates",
    "main packages",
    "main components",
    "main directories",
    "main folders",
    "each crate",
    "each package",
    "each module",
    "each directory",
    "each folder",
    "new to this",
    "onboard",
];
/// Phrases that are a request for the whole picture whatever else is said.
const OVERVIEW_WHOLE: &[&str] = &["lay of the land", "fit together"];
/// Nouns for the parts of a workspace, and the phrases that ask which of them
/// there are (`which crates make up the workspace`): together they ask for
/// the member list, which the manifests answer.
const MEMBER_NOUNS: &[&str] = &["crates", "packages", "members", "modules", "libraries"];
const MEMBER_VERBS: &[&str] = &[
    "make up",
    "makes up",
    "made up",
    "consist of",
    "consists of",
    "composed of",
    "in the workspace",
    "of the workspace",
    "in this workspace",
    "of this workspace",
    "depend on",
    "depends on",
    "depend upon",
];
/// Nouns for the whole repository.
const WHOLE_WORDS: &[&str] = &[
    "repo",
    "repository",
    "codebase",
    "code base",
    "project",
    "monorepo",
    "code",
];
/// Nouns for the whole or for its parts: what a tour is about.
const SCOPE_WORDS: &[&str] = &[
    "repo",
    "repository",
    "codebase",
    "code base",
    "project",
    "monorepo",
    "crate",
    "crates",
    "package",
    "packages",
    "module",
    "modules",
    "app",
    "library",
];
/// Words that ask for a review.
const REVIEW_CUES: &[&str] = &[
    "review",
    "reviews",
    "audit",
    "critique",
    "sanity check",
    "look over",
    "look at",
    "check over",
    "risky",
    "check",
    "regression",
    "regressions",
    "missing tests",
    "anything wrong",
    "anything off",
    "what could break",
    "safe to merge",
];
/// Phrases that name a change: a commit, a diff, a branch, the working tree.
const CHANGE_NOUNS: &[&str] = &[
    "latest commit",
    "last commit",
    "most recent commit",
    "previous commit",
    "recent commit",
    "this commit",
    "the commit",
    "my commit",
    "my changes",
    "these changes",
    "the changes",
    "this change",
    "the diff",
    "my diff",
    "this diff",
    "this pr",
    "pull request",
    "this branch",
    "my branch",
    "uncommitted",
    "unstaged",
    "staged",
    "working tree",
    "what i changed",
    "my work",
    "my patch",
    "head~1",
];
/// Phrases that ask to be walked through what happens, whatever the
/// symbols involved.
const WALKTHROUGH: &[&str] = &[
    "walk me through",
    "walk through",
    "what happens when",
    "what happens if",
    "what happens after",
    "what happens from",
    "end to end",
    "end-to-end",
    "step by step",
    "life cycle",
    "lifecycle",
    "life of a",
    "journey of",
];
/// Phrases that ask about the history of the code in so many words; the
/// plain `when` or `why` of a rationale question is not enough.
const HISTORY_PHRASES: &[&str] = &[
    "which commit",
    "what commit",
    "which pr",
    "when did",
    "since when",
    "since which",
    "who added",
    "who wrote",
    "who introduced",
    "who changed",
    "who removed",
    "who broke",
    "commit titled",
    "commit called",
    "commit named",
    "what did the commit",
    "what did commit",
    "git log",
    "git blame",
    "blame",
];
/// Words that report a defect or ask for a change to a named symbol.
const BUGFIX_WORDS: &[&str] = &[
    "fix",
    "fixes",
    "fixed",
    "fixing",
    "bug",
    "bugs",
    "crash",
    "crashes",
    "crashed",
    "error",
    "errors",
    "panic",
    "panics",
    "broken",
    "fail",
    "fails",
    "failed",
    "failing",
    "failure",
    "failures",
    "regression",
    "regressions",
    "exception",
    "exceptions",
];

impl QuestionKind {
    /// The kind the typed text asks for. Words
    /// match exactly — the lists name each inflection, so `spec` does not
    /// take `specific` nor `env` take `envelope`; precedence is the order
    /// the kinds are checked below. Words that are segments of an anchor —
    /// `decisions` inside `build_decisions_request` — name the target of
    /// the question, not its shape, and never route it.
    pub(crate) fn heuristic(typed: &str, anchors: &Anchors) -> Self {
        let anchored = anchors.segment_words();
        let symbol_anchors = anchors.names().count();
        let asks = |words: &[&str]| {
            typed
                .split(|ch: char| !ch.is_alphanumeric())
                .map(str::to_lowercase)
                .filter(|word| !anchored.contains(word))
                .any(|word| words.contains(&word.as_str()))
        };
        // Two named endpoints make a from→to question. One anchor with a
        // flow word is still a reach question — the route derives the
        // destination from the prose or answers with the endpoint's
        // callers — unless a defect word makes it a bug report.
        // A question that names no symbol still asks for a flow when it says
        // so in plain words; the chain then borrows the anchor from the
        // meaning search's best chunk.
        if asks_overview(typed) && symbol_anchors == 0 && anchors.paths().is_empty() {
            return Self::Overview;
        }
        if asks_review(typed) {
            return Self::Review;
        }
        if asks_phrase(typed, HISTORY_PHRASES) && !asks(TEST_WORDS) {
            return Self::History;
        }
        let walkthrough =
            asks_phrase(typed, WALKTHROUGH) && !asks(TEST_WORDS) && !asks(BUGFIX_WORDS);
        let flow = walkthrough
            || (asks(FLOW_WORDS)
                && (symbol_anchors >= 2 || (symbol_anchors == 1 && !asks(BUGFIX_WORDS))))
            || (symbol_anchors == 0
                && asks(PROSE_FLOW_WORDS)
                && !asks(TEST_WORDS)
                && !asks(BUGFIX_WORDS));
        if flow {
            Self::Flow
        } else if asks(TEST_WORDS) {
            Self::Tests
        } else if asks(CONFIG_WORDS) || names_env_var(typed) {
            Self::Config
        } else if asks(RATIONALE_WORDS) {
            Self::Rationale
        } else if asks(BUGFIX_WORDS) {
            Self::Bugfix
        } else {
            Self::Lookup
        }
    }

    /// The label the `kind:` line renders.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::Flow => "flow",
            Self::Tests => "tests",
            Self::Config => "config",
            Self::Rationale => "rationale",
            Self::Bugfix => "bugfix",
            Self::Overview => "overview",
            Self::Review => "review",
            Self::History => "history",
        }
    }
}

/// `typed` in lower case with its quotes and runs of spaces plain: what the
/// phrase lists are matched against.
pub(crate) fn plain(typed: &str) -> String {
    let text: String = typed
        .to_lowercase()
        .chars()
        .map(|ch| match ch {
            '\u{2019}' | '\u{2018}' | '`' => '\'',
            ch if ch.is_whitespace() || ch == ',' || ch == ';' => ' ',
            ch => ch,
        })
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the lower-cased text holds `phrase` as whole words.
pub(crate) fn has_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, found)| {
        let before = text[..at].chars().next_back();
        let after = text[at + found.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether `typed` holds any of `phrases` as whole words.
fn asks_phrase(typed: &str, phrases: &[&str]) -> bool {
    let text = plain(typed);
    phrases.iter().any(|phrase| has_phrase(&text, phrase))
}

/// Whether `typed` names an environment variable or a constant in its own
/// shape: an upper-case token of two or more `_`-joined parts
/// (`RIPGREP_CONFIG_PATH`). Its segment words are anchor segments and never
/// route by themselves, so the shape does.
pub(crate) fn names_env_var(typed: &str) -> bool {
    typed
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|token| {
            let parts: Vec<&str> = token.split('_').collect();
            parts.len() >= 2
                && parts.iter().all(|part| {
                    !part.is_empty()
                        && part
                            .chars()
                            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
                })
                && parts
                    .iter()
                    .any(|part| part.chars().any(|ch| ch.is_ascii_uppercase()))
        })
}

/// Whether `typed` asks how the repository is organised: a cue of a tour with
/// a noun for the whole or its parts beside it, a question of how it is put
/// together about the whole, or a request for the whole picture.
pub(crate) fn asks_overview(typed: &str) -> bool {
    let text = plain(typed);
    let any = |list: &[&str]| list.iter().any(|phrase| has_phrase(&text, phrase));
    any(OVERVIEW_WHOLE)
        || (any(MEMBER_NOUNS) && any(MEMBER_VERBS))
        || (any(OVERVIEW_CUES) && any(SCOPE_WORDS))
        || (any(OVERVIEW_HOW) && any(WHOLE_WORDS))
        || asks_whole_purpose(&text)
}

/// Whether `text` asks what the whole does or is: a `what does … <noun> do`
/// or `what is … <noun> for/about` question whose subject is a noun of
/// [`WHOLE_WORDS`] directly followed by the verb. A word between the noun and
/// the verb (`what does the project config do`) names a part, not the whole,
/// and a named symbol (`what does build_decisions_request do`) is a lookup.
fn asks_whole_purpose(text: &str) -> bool {
    // A question about the whole names it: "what does/is …" at the start or
    // anywhere in the text ("tell me what this repository is for").
    if !has_phrase(text, "what") {
        return false;
    }
    let asked = |noun: &str, tail: &str| has_phrase(text, &format!("{noun} {tail}"));
    // `what does the code do with the value` asks about the value, not the
    // whole: only a bare `<noun> do` (end of the clause, or `do?`) is the whole.
    let does = WHOLE_WORDS
        .iter()
        .any(|noun| asked(noun, "do") && !asked(noun, "do with") && !asked(noun, "do to"));
    let is = WHOLE_WORDS.iter().any(|noun| {
        asked(noun, "is for")
            || asked(noun, "is about")
            || asked(noun, "for")
            || asked(noun, "about")
    });
    let purpose = has_phrase(text, "purpose of")
        && WHOLE_WORDS.iter().any(|noun| {
            ["", "the ", "this "]
                .iter()
                .any(|article| has_phrase(text, &format!("purpose of {article}{noun}")))
        });
    does || is || purpose
}

/// Whether `typed` asks for a review of a change: a cue of review beside a
/// noun for the change, or a change noun beside a worry about it.
pub(crate) fn asks_review(typed: &str) -> bool {
    let text = plain(typed);
    let any = |list: &[&str]| list.iter().any(|phrase| has_phrase(&text, phrase));
    any(CHANGE_NOUNS) && any(REVIEW_CUES)
}

/// Whether `word` is a flow word (exact match): the chain uses it to
/// find the destination a one-anchor flow prompt names in prose after
/// the flow word.
pub(crate) fn asks_flow(word: &str) -> bool {
    FLOW_WORDS.contains(&word)
}

/// `text` as one single-quoted shell word, for the `next:` command.
pub(crate) fn q(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind a prompt routes to under the heuristic, with the anchors
    /// the chain would extract from it.
    fn kind_of(typed: &str) -> QuestionKind {
        QuestionKind::heuristic(typed, &Anchors::from_text(typed))
    }

    #[test]
    fn heuristic_should_pick_flow_with_symbol_anchors_and_a_flow_word() {
        assert_eq!(
            kind_of("how does login_flow reach logout_handler"),
            QuestionKind::Flow
        );
        // One anchor with a flow word is still a reach question.
        assert_eq!(
            kind_of("how does start_brief reach the render"),
            QuestionKind::Flow
        );
        assert_eq!(kind_of("who calls retry_loop"), QuestionKind::Flow);
        assert_eq!(
            kind_of("how does the login flow work"),
            QuestionKind::Lookup
        );
        // A defect word makes the single anchor a bug report, not a path.
        assert_eq!(
            kind_of("the call in retry_loop is broken"),
            QuestionKind::Bugfix
        );
    }

    #[test]
    fn heuristic_should_ignore_words_that_are_only_anchor_segments() {
        // `decisions` inside `build_decisions_request` must not read as a
        // rationale stem; the word names the asked-about target.
        assert_eq!(
            kind_of("what does build_decisions_request do"),
            QuestionKind::Lookup
        );
        assert_eq!(
            kind_of("where is `config/app.json` read"),
            QuestionKind::Lookup
        );
    }

    #[test]
    fn heuristic_should_pick_tests_config_rationale_then_lookup() {
        assert_eq!(kind_of("which tests cover retry_loop"), QuestionKind::Tests);
        assert_eq!(
            kind_of("where is the spec for the parser"),
            QuestionKind::Tests
        );
        for prompt in [
            "how is the hook configured",
            "which env setting controls it",
            "what is the default value of the timeout",
            "which flag turns the brief off",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Config, "{prompt}");
        }
        // A topic near configuration is not a question about a setting.
        for prompt in [
            "how does install handle existing claude files",
            "how does the hook decide what to inject",
            "why does the install fail",
        ] {
            assert_ne!(kind_of(prompt), QuestionKind::Config, "{prompt}");
        }
        for prompt in [
            "when was retry_loop introduced",
            "why was the cache removed",
            "history of the parser decision",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Rationale, "{prompt}");
        }
        // Tests and config outrank rationale.
        assert_eq!(kind_of("which tests were added when"), QuestionKind::Tests);
        assert_eq!(kind_of("where is fetchUser defined"), QuestionKind::Lookup);
    }

    #[test]
    fn heuristic_should_route_a_plain_reach_question_to_flow_without_a_symbol() {
        for prompt in [
            "how does the prompt hook reach the brief renderer",
            "trace what happens when a prompt is submitted",
            "what calls the watchdog",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Flow, "{prompt}");
        }
        // Tests and defects keep their own routes.
        assert_eq!(kind_of("which tests call the parser"), QuestionKind::Tests);
        assert_ne!(kind_of("the call crashes on startup"), QuestionKind::Flow);
    }

    #[test]
    fn heuristic_should_not_route_words_that_only_contain_a_word() {
        // Prefix stems took `specific` for `spec`, `envelope` for `env`,
        // `fixture` for `fix` and `whenever` for `when`; whole-word
        // matching keeps each in its own meaning.
        for prompt in [
            "which specific function parses the input",
            "how is the envelope built",
            "the fixture in main.rs returns empty",
            "whenever the watcher fires the graph updates",
            "the callback path is pathological",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Lookup, "{prompt}");
        }
    }

    #[test]
    fn heuristic_should_pick_bugfix_for_defect_words_below_rationale() {
        for prompt in [
            "fix the crash in expire",
            "the parser is broken",
            "debug why this fails", // `why` outranks the defect word
        ] {
            let expected = if prompt.contains("why") {
                QuestionKind::Rationale
            } else {
                QuestionKind::Bugfix
            };
            assert_eq!(kind_of(prompt), expected, "{prompt}");
        }
        assert_eq!(kind_of("fix the crash in expire"), QuestionKind::Bugfix);
    }

    #[test]
    fn heuristic_should_route_a_what_does_the_whole_do_question_to_overview() {
        for prompt in [
            "What does this project do?",
            "what does this repo do",
            "what does this codebase do",
            "what does this monorepo do",
            "what is this project for",
            "what is this project about",
            "what does the code do",
            "tell me what this repository is for",
            "what is the purpose of this project",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Overview, "{prompt}");
        }
        // A word between the whole noun and the verb names a part, not the
        // whole; a named symbol is a lookup.
        assert_eq!(
            kind_of("what does the project config do"),
            QuestionKind::Config
        );
        assert_eq!(
            kind_of("what does build_decisions_request do"),
            QuestionKind::Lookup
        );
        assert_eq!(kind_of("what does the parser do"), QuestionKind::Lookup);
    }

    #[test]
    fn heuristic_should_route_a_tour_of_the_repository_to_overview() {
        for prompt in [
            "I'm new to this repo. give me a quick tour of how it's organised and what each crate is for",
            "give me the lay of the land here, what are the main modules and how do they fit together",
            "what is the overall architecture of this codebase",
            "how is this project structured",
            "can you give me an overview of the repo",
            "what are the main crates in this monorepo",
            "which crates make up the workspace and what does each do",
            "what packages is this repository made up of",
            "list the members of the workspace",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Overview, "{prompt}");
        }
        // A tour of something else, a part of the repository, or a question
        // that names a symbol or a path is not a tour of the whole.
        for prompt in [
            "give me a tour of Paris",
            "how is the config structured",
            "overview of `retry_loop` and what it does",
            "give me an overview of crates/pixel-graph/src/store.rs",
            "how is the file organised",
            "what does the alpha crate contain",
            "which modules make up `retry_loop`",
        ] {
            assert_ne!(kind_of(prompt), QuestionKind::Overview, "{prompt}");
        }
    }

    #[test]
    fn heuristic_should_route_a_review_of_a_change_to_review() {
        for prompt in [
            "review the latest commit and tell me if anything looks risky or is missing tests",
            "can you review my changes",
            "look over the diff before I push",
            "is this PR safe to merge",
            "check the uncommitted changes for regressions",
            "audit the last commit",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Review, "{prompt}");
        }
        // A review needs a change to look at, and a change needs a worry.
        for prompt in [
            "review the parser design",
            "write the commit message for my changes",
            "commit my changes and push",
            "what changed in the parser",
        ] {
            assert_ne!(kind_of(prompt), QuestionKind::Review, "{prompt}");
        }
        // Whole words only: `staged` is not in `stage`.
        assert_ne!(
            kind_of("review the stage of the pipeline that last commits"),
            QuestionKind::Review
        );
    }

    #[test]
    fn heuristic_should_route_a_walkthrough_to_flow_with_or_without_a_symbol() {
        for prompt in [
            "walk me through what happens when someone drops an image file onto the canvas, from the drop event until the image is rendered",
            "what happens when a prompt is submitted",
            "walk me through the login",
            "explain the lifecycle of a request step by step",
            "what happens after `submit_prompt` returns",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::Flow, "{prompt}");
        }
        // Tests and defects keep their own routes.
        assert_eq!(
            kind_of("which tests walk through what happens when the cache expires"),
            QuestionKind::Tests
        );
        assert_ne!(
            kind_of("walk me through why it crashes when the cache expires"),
            QuestionKind::Flow
        );
    }

    #[test]
    fn as_str_should_name_every_kind() {
        let names: Vec<&str> = [
            QuestionKind::Lookup,
            QuestionKind::Flow,
            QuestionKind::Tests,
            QuestionKind::Config,
            QuestionKind::Rationale,
            QuestionKind::Bugfix,
            QuestionKind::Overview,
            QuestionKind::Review,
            QuestionKind::History,
        ]
        .into_iter()
        .map(QuestionKind::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "lookup",
                "flow",
                "tests",
                "config",
                "rationale",
                "bugfix",
                "overview",
                "review",
                "history"
            ]
        );
    }

    #[test]
    fn q_should_single_quote_and_escape_inner_quotes() {
        assert_eq!(q("handleError"), "'handleError'");
        assert_eq!(q("it's"), "'it'\\''s'");
    }

    #[test]
    fn heuristic_should_route_a_question_about_history_in_so_many_words_to_history() {
        for prompt in [
            "when did ignore start skipping unreachable ignore files, which commit was it and what did it change",
            "who added the retry loop",
            "since which commit does the cache expire",
            "when did the daemon idle timeout change",
            "git blame on the parser",
        ] {
            assert_eq!(kind_of(prompt), QuestionKind::History, "{prompt}");
        }
        // A plain `why` or `when` stays a rationale question, a tests question
        // stays one, and a walkthrough is not history.
        assert_eq!(
            kind_of("why was the cache removed"),
            QuestionKind::Rationale
        );
        assert_eq!(
            kind_of("which tests cover the change when it was added"),
            QuestionKind::Tests
        );
        assert_eq!(
            kind_of("what happens when a prompt is submitted"),
            QuestionKind::Flow
        );
    }

    #[test]
    fn heuristic_should_not_read_what_does_the_code_do_with_a_value_as_a_tour() {
        for prompt in [
            "Which flag sets the DFA size limit, and what does the code do with the value it parses?",
            "what does the code do to the buffer it reads",
        ] {
            assert_ne!(kind_of(prompt), QuestionKind::Overview, "{prompt}");
        }
        assert_eq!(
            kind_of("what does this codebase do"),
            QuestionKind::Overview
        );
        assert_eq!(kind_of("what does the code do?"), QuestionKind::Overview);
    }

    #[test]
    fn heuristic_should_route_a_named_commit_question_to_history_and_a_review_to_review() {
        assert_eq!(
            kind_of(
                "What did the commit titled 'ignore,globset: increase pool capacity' change, and which files changed with it?"
            ),
            QuestionKind::History
        );
        assert_eq!(
            kind_of("Review the commit titled 'ci: attest build provenance': what does it change?"),
            QuestionKind::Review
        );
        assert_eq!(
            kind_of("which tests cover the commit titled 'x'"),
            QuestionKind::Tests
        );
    }

    #[test]
    fn heuristic_should_route_which_crates_a_crate_depends_on_to_the_overview() {
        assert_eq!(
            kind_of("What does the grep crate contain, and which other crates does it depend on?"),
            QuestionKind::Overview
        );
    }

    #[test]
    fn names_env_var_should_need_an_upper_case_token_of_two_parts() {
        assert!(names_env_var("how is RIPGREP_CONFIG_PATH read"));
        assert!(names_env_var("the value of NO_COLOR, if set"));
        assert!(names_env_var("RUST_LOG2 matters"));
        assert!(!names_env_var("how is retry_limit read"));
        assert!(!names_env_var("the HTTP status"));
        assert!(!names_env_var("a _ b and __ and A_"));
    }

    #[test]
    fn heuristic_should_route_a_question_that_names_an_env_var_to_config() {
        assert_eq!(
            kind_of("How does ripgrep find and read the file named by RIPGREP_CONFIG_PATH?"),
            QuestionKind::Config
        );
    }
}
