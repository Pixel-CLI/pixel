// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A French prompt's words, put into the English words code names things
//! with, before the brief searches and gates on them: pixel-mvp's ADR-0032,
//! moved into this brief under ADR-0036 of the pixel-mvp research
//! repository.
//!
//! The rules are pixel-mvp's, read from its vendored copy (`mvp-model`,
//! never edited here) and not restated: `mvp_model::language` says whether
//! the typed text is French, `mvp_model::sub_words` splits it into words,
//! and `mvp_model::prompt_terms` says which words carry nothing (a French
//! function word, or a filler the lexicon maps to `-`). The lexicon is its
//! data, vendored byte for byte (`crates/mvp-lexicon/fr-en.tsv`).
//!
//! What is this brief's own, and why:
//!
//! - **Words, not stems.** pixel-mvp ranks on its stemmed terms; this brief's
//!   searches match whole words (`pixel_daemon::relevance` counts a word-bounded
//!   content match, a symbol name, a path word), so the lexicon's English words
//!   are written as they are: `Lexicon::english_words`, the lookup
//!   `prompt_terms` reads the terms of, so the brief's words and pixel-mvp's
//!   terms come from one lookup and cannot drift apart on a form.
//! - **Code as typed.** A code-shaped token (`names_code`) or a backticked one
//!   is kept whole: it is a name in the repository, in any language.
//! - **The question word.** The gate reads an English question word among the
//!   first three words of the prompt (`relevance::is_question`); a French one
//!   in that place is written as its English opener, so a French question is
//!   still a question there.
//!
//! The prompt's language is decided on the typed text, so an English prompt
//! is never rewritten: its brief stays byte for byte what it was.

use std::sync::OnceLock;

use mvp_model::{Language, Lexicon, fold, language, prompt_terms, sub_words};

/// The switch: on unless set to `0`, `false` or `off`, which leaves a French
/// prompt as typed. Measured on `eval/brief-gate/` before it went on
/// (`docs/bench/brief-gate.md`, "French prompts through the lexicon").
pub(crate) const FRENCH_ENV: &str = "PIXEL_BRIEF_FRENCH";

/// pixel-mvp's lexicon, vendored (`crates/mvp-lexicon/VENDORED.md`).
const LEXICON: &str = include_str!("../../../mvp-lexicon/fr-en.tsv");

/// The French question words that open a question, and the English opener
/// the gate reads for each (`gate_model::QUESTION_WORDS`).
const QUESTION_OPENERS: [(&str, &str); 9] = [
    ("comment", "how"),
    ("ou", "where"),
    ("pourquoi", "why"),
    ("quand", "when"),
    ("quel", "which"),
    ("quelle", "which"),
    ("quelles", "which"),
    ("quels", "which"),
    ("quoi", "what"),
];

/// How many words of the text the gate looks at for a question word: the words
/// written (`out.len()`), not the words typed, because the gate reads the
/// text this module hands it.
const QUESTION_WINDOW: usize = 3;

/// Whether [`FRENCH_ENV`]'s value turns the mapping on.
pub(crate) fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// The lexicon as pixel-mvp reads it: which words carry nothing.
fn mvp_lexicon() -> &'static Lexicon {
    static READ: OnceLock<Lexicon> = OnceLock::new();
    READ.get_or_init(|| Lexicon::parse(LEXICON))
}

/// The text the brief searches and gates on for the typed text `typed`:
/// `None` when it is not French, or when nothing in it names anything.
///
/// Each whitespace-separated token: a code-shaped or backticked one is kept as
/// typed; any other is split into words, and each word is dropped when it
/// carries nothing (a French question word among the first
/// [`QUESTION_WINDOW`] words becomes its English opener instead), replaced
/// by its English words when the lexicon holds it, kept with its accents
/// folded otherwise ("daemon", "webhook" and "jobs" still meet their names).
/// A text that ends on `?` still does.
pub(crate) fn english(typed: &str) -> Option<String> {
    if language(typed) != Language::French {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for token in typed.split_whitespace() {
        if token.contains('`') || super::names_code(token) {
            out.push(token.to_string());
            continue;
        }
        for word in sub_words(token) {
            push_word(&word, &mut out);
        }
    }
    if out.is_empty() {
        return None;
    }
    let mut text = out.join(" ");
    if typed.trim_end().ends_with('?') {
        text.push('?');
    }
    Some(text)
}

/// The English of one French `word`, onto `out`.
fn push_word(word: &str, out: &mut Vec<String>) {
    if prompt_terms(word, Language::French, mvp_lexicon()).is_empty() {
        let folded = fold(word);
        if out.len() < QUESTION_WINDOW
            && let Some((_, opener)) = QUESTION_OPENERS
                .iter()
                .find(|(french, _)| *french == folded)
        {
            out.push((*opener).to_string());
        }
        return;
    }
    match mvp_lexicon().english_words(word) {
        Some(english) => out.extend(english.iter().cloned()),
        None => out.push(fold(word)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_should_be_on_unless_it_is_turned_off() {
        // `yes` was off while the switch was opt-in: with the default on, only
        // the three opt-outs leave a French prompt as typed.
        for value in [
            None,
            Some(""),
            Some("1"),
            Some("true"),
            Some("on"),
            Some("yes"),
        ] {
            assert!(enabled(value), "{value:?}");
        }
        for value in ["0", "false", "off"] {
            assert!(!enabled(Some(value)), "{value}");
        }
    }

    // The concept the prompt names is spelled in English in the code: the
    // brief must search the English words, or a French prompt finds nothing.
    #[test]
    fn a_french_prompt_should_be_searched_by_the_english_words_code_uses() {
        assert_eq!(
            english("pourquoi le daemon redémarre quand je change de branche").as_deref(),
            Some("why daemon restart change branch")
        );
        assert_eq!(
            english("pourquoi les abonnements expirés ne sont pas remboursés ?").as_deref(),
            Some("why subscription expire expiration refund?")
        );
    }

    // ADR-0032's own case: "donne" carries nothing, it is not `done`.
    #[test]
    fn a_filler_word_should_be_dropped_and_not_searched() {
        let english = english("donne moi les commandes remboursées").unwrap();
        assert_eq!(english, "order command refund");
    }

    #[test]
    fn a_word_the_lexicon_lacks_should_be_kept_without_its_accents() {
        // "déduplication" is not in the lexicon; `deduplication` is how the
        // code spells it, and the folded word meets it.
        assert_eq!(
            english("la déduplication du webhook plante dans le worktree").as_deref(),
            Some("deduplication webhook crash worktree")
        );
    }

    #[test]
    fn a_code_shaped_token_should_be_kept_as_typed() {
        assert_eq!(
            english(
                "pourquoi `tokenize_task` ne garde pas les accents de crates/pixel-rank/src/lib.rs"
            )
            .as_deref(),
            Some("why `tokenize_task` keep accents crates/pixel-rank/src/lib.rs")
        );
        // Backticks name code even around a plain word.
        assert_eq!(
            english("pourquoi le `brief` ne se lance pas").as_deref(),
            Some("why `brief` launch run start")
        );
    }

    // The gate reads a question word among the first three only: a French one
    // further on is a word of the sentence, not an opener.
    #[test]
    fn only_an_opening_question_word_should_become_english() {
        assert_eq!(
            english("euh comment je lance les tests").as_deref(),
            Some("how launch run start tests")
        );
        // Three words are written before "comment": it is the fourth.
        assert_eq!(
            english("le daemon du hook pixel comment").as_deref(),
            Some("daemon hook pixel")
        );
        assert_eq!(
            english("le daemon de pixel et le hook de claude disent comment le lancer")
                .as_deref()
                .map(|text| text.split(' ').any(|word| word == "how")),
            Some(false)
        );
    }

    // The gate reads the first three words of the text it is handed, so the
    // window counts the words written, not the words typed: the French words
    // that open a question ("tu peux") are dropped and do not push the
    // question word out, while a translation that writes more words than were
    // typed pushes it past the third, where the gate would not see it anyway.
    #[test]
    fn the_question_window_should_count_the_words_written() {
        assert_eq!(
            english("tu peux regarder pourquoi le webhook plante dans le système").as_deref(),
            Some("why webhook crash system")
        );
        assert_eq!(
            english("abonnements comment les rembourser dans le système").as_deref(),
            Some("subscription how refund system")
        );
        assert_eq!(
            english("abonnements expirés comment les rembourser dans le système").as_deref(),
            Some("subscription expire expiration refund system")
        );
    }

    // An English prompt is never rewritten, French-looking words or not: its
    // brief must stay byte for byte what it was.
    #[test]
    fn an_english_prompt_should_not_be_rewritten() {
        for prompt in [
            "why does the daemon restart when I switch branches?",
            "the son of a parent node is not set on the tree",
            "where is the list of sounds you can play in a room?",
            "merci",
            "",
        ] {
            assert_eq!(english(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn a_french_prompt_of_filler_only_should_be_left_as_typed() {
        assert_eq!(english("merci beaucoup pour tout ça"), None);
    }

    // The brief matches whole words, so it must write the lexicon's words
    // as written ("marketplace", not its term), and let accents choose
    // between two forms: "marché" is a market, "marche" a filler.
    #[test]
    fn the_lexicons_whole_words_should_be_written_and_accents_should_choose() {
        assert_eq!(
            english("le marché du vendeur ne marche pas").as_deref(),
            english("le marché du vendeur").as_deref()
        );
        let written = english("le marché du vendeur").unwrap();
        assert!(written.split(' ').any(|w| w == "marketplace"), "{written}");
        assert!(!written.contains("marche"), "{written}");
    }
}
