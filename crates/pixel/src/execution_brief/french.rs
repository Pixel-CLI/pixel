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
//!   are written as they are. `mvp_model::Lexicon` hands out stems only (its
//!   lookup is private at the vendored commit), so [`EnglishWords`] reads the
//!   same file for the words.
//! - **Code as typed.** A code-shaped token (`names_code`) or a backticked one
//!   is kept whole: it is a name in the repository, in any language.
//! - **The question word.** The gate reads an English question word among the
//!   first three words of the prompt (`relevance::is_question`); a French one
//!   in that place is written as its English opener, so a French question is
//!   still a question there.
//!
//! The prompt's language is decided on the typed text, so an English prompt
//! is never rewritten: its brief stays byte for byte what it was.

use std::collections::BTreeMap;
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

/// The English words of each French form of the lexicon, as written there:
/// what [`Lexicon`] holds, without its stemming. A word is looked up as typed
/// in lower case first, so accents choose ("marché" against "marche"), then
/// with its accents folded, where the words of every form that folds to it
/// are merged in file order.
#[derive(Debug, Default)]
struct EnglishWords {
    exact: BTreeMap<String, Vec<String>>,
    folded: BTreeMap<String, Vec<String>>,
}

impl EnglishWords {
    /// Reads `<french>\t<english words, or ->` lines; `#` starts a comment and
    /// a line of another shape is skipped, as `Lexicon::parse` does.
    fn parse(text: &str) -> Self {
        let mut words = Self::default();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let Some((french, english)) = line.split_once('\t') else {
                continue;
            };
            let english = english.trim();
            let targets: Vec<String> = if english == "-" {
                Vec::new()
            } else {
                english.split_whitespace().map(str::to_owned).collect()
            };
            let french = french.trim().to_lowercase();
            let merged = words.folded.entry(fold(&french)).or_default();
            for target in &targets {
                if !merged.contains(target) {
                    merged.push(target.clone());
                }
            }
            words.exact.insert(french, targets);
        }
        words
    }

    /// The English words of `word`, `None` when the lexicon does not hold it.
    fn get(&self, word: &str) -> Option<&[String]> {
        let word = word.to_lowercase();
        self.exact
            .get(&word)
            .or_else(|| self.folded.get(&fold(&word)))
            .map(Vec::as_slice)
    }
}

fn english_words() -> &'static EnglishWords {
    static READ: OnceLock<EnglishWords> = OnceLock::new();
    READ.get_or_init(|| EnglishWords::parse(LEXICON))
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
    match english_words().get(word) {
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

    #[test]
    fn the_reader_should_let_accents_choose_and_merge_the_folded_forms() {
        let words = EnglishWords::parse(
            "# a comment\twith a tab\nmarche\t-\nmarché\tmarket marketplace\npasse\tpassword pass\npassé\t-\nPassé\tpast\nno tab here\n",
        );
        assert_eq!(words.get("# a comment"), None);
        assert_eq!(words.get("no tab here"), None);
        assert_eq!(words.get("marche"), Some(&[][..]));
        assert_eq!(
            words.get("MARCHÉ"),
            Some(&["market".to_string(), "marketplace".to_string()][..])
        );
        // Typed without its accent, a word the lexicon holds only with one
        // gets the words of every form that folds to it, once each.
        assert_eq!(
            words.get("passe"),
            Some(&["password".to_string(), "pass".to_string()][..])
        );
        assert_eq!(
            words.folded.get("passe").map(Vec::as_slice),
            Some(
                &[
                    "password".to_string(),
                    "pass".to_string(),
                    "past".to_string()
                ][..]
            )
        );
        assert_eq!(words.get("marchés"), None);
    }

    // The words are the lexicon's own: read for words here, the file gives
    // the very terms pixel-mvp's reader maps each form to, so the two readers
    // cannot drift apart on a form.
    #[test]
    fn every_form_should_carry_the_words_whose_terms_pixel_mvp_maps_it_to() {
        let words = english_words();
        let mvp = mvp_lexicon();
        assert_eq!(words.exact.len(), mvp.len());
        let mut compared = 0;
        for french in words.exact.keys() {
            let mvp_terms = prompt_terms(french, Language::French, mvp);
            if sub_words(french).len() != 1 || mvp_terms.is_empty() {
                continue;
            }
            let terms: Vec<String> = words
                .get(french)
                .unwrap()
                .iter()
                .flat_map(|word| mvp_model::terms(word))
                .collect();
            assert_eq!(terms, mvp_terms, "{french}");
            compared += 1;
        }
        assert!(compared > 4_000, "{compared} forms compared");
    }
}
