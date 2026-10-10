// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The question-kind router of the prompt-start brief.
//!
//! After the fixed prefix (literal search, concept fallback, symbol pick),
//! the chain spends the ops it has left on the evidence shape the prompt
//! actually asked for: a call path, covering tests, a file's signature,
//! history, the definition's body, or task facts. [`QuestionKind`] names
//! that shape; it comes from a `pixel classify` verdict when the warm local
//! judge answered with a label that names an evidence kind, else from a
//! typed-text heuristic.

use super::chain::Anchors;

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
    /// New behaviour to place: `targets_facts` from the daemon.
    Feature,
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
/// Words that report a defect: a strong prompt names its symbol, so a
/// bugfix brief can come from the heuristic too, not only from a verdict.
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
    /// The kind a verdict label names, or `None` for labels that name a
    /// task rather than an evidence shape: `investigate`, `question` and
    /// `ops` still route through [`Self::heuristic`].
    pub(crate) fn of_verdict(label: &str) -> Option<Self> {
        match label {
            "bugfix" | "refactor" | "review" => Some(Self::Bugfix),
            "feature" => Some(Self::Feature),
            _ => None,
        }
    }

    /// The kind the typed text asks for when no verdict routed it. Words
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
        let flow = (asks(FLOW_WORDS)
            && (symbol_anchors >= 2 || (symbol_anchors == 1 && !asks(BUGFIX_WORDS))))
            || (symbol_anchors == 0
                && asks(PROSE_FLOW_WORDS)
                && !asks(TEST_WORDS)
                && !asks(BUGFIX_WORDS));
        if flow {
            Self::Flow
        } else if asks(TEST_WORDS) {
            Self::Tests
        } else if asks(CONFIG_WORDS) {
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
            Self::Feature => "feature",
        }
    }
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

    #[test]
    fn of_verdict_should_map_change_and_feature_labels_and_defer_the_rest() {
        for label in ["bugfix", "refactor", "review"] {
            assert_eq!(
                QuestionKind::of_verdict(label),
                Some(QuestionKind::Bugfix),
                "{label}"
            );
        }
        assert_eq!(
            QuestionKind::of_verdict("feature"),
            Some(QuestionKind::Feature)
        );
        for label in ["investigate", "question", "ops", "none", "anything"] {
            assert_eq!(QuestionKind::of_verdict(label), None, "{label}");
        }
    }

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
    fn as_str_should_name_every_kind() {
        let names: Vec<&str> = [
            QuestionKind::Lookup,
            QuestionKind::Flow,
            QuestionKind::Tests,
            QuestionKind::Config,
            QuestionKind::Rationale,
            QuestionKind::Bugfix,
            QuestionKind::Feature,
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
                "feature"
            ]
        );
    }

    #[test]
    fn q_should_single_quote_and_escape_inner_quotes() {
        assert_eq!(q("handleError"), "'handleError'");
        assert_eq!(q("it's"), "'it'\\''s'");
    }
}
