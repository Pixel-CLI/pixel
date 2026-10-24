// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The terms the selector's BM25 matches: one tokeniser for the
//! index, which counts each file's terms when a generation is published,
//! and for the prompt, so that both sides meet on the same forms.

use std::collections::BTreeMap;

use crate::rows::sub_words;

/// English function words and the most generic verbs of a request: they
/// name nothing in a repository. Measured with and without on the selector
/// bench's development set: without, natural hit_at_5 fell from
/// 0.58 to 0.42 and identifier from 0.96 to 0.92.
const STOP: &[&str] = &[
    "a", "about", "above", "after", "again", "all", "also", "an", "and", "any", "are", "as", "at",
    "be", "been", "before", "being", "below", "between", "both", "but", "by", "can", "could",
    "did", "do", "does", "doesnt", "doing", "don", "dont", "down", "during", "each", "else", "few",
    "for", "from", "further", "get", "gets", "got", "had", "has", "have", "having", "he", "her",
    "here", "him", "his", "how", "i", "if", "im", "in", "into", "is", "isnt", "it", "its", "just",
    "like", "lot", "made", "make", "makes", "may", "me", "might", "more", "most", "must", "my",
    "no", "nor", "not", "of", "off", "on", "once", "one", "only", "or", "other", "our", "out",
    "over", "own", "please", "same", "shall", "she", "should", "so", "some", "such", "than",
    "thank", "thanks", "that", "the", "their", "them", "then", "there", "these", "they", "this",
    "those", "through", "to", "too", "two", "under", "up", "very", "want", "was", "we", "were",
    "what", "when", "where", "which", "while", "who", "whom", "whose", "why", "will", "with",
    "would", "you", "your",
];

/// The terms of `text`: its sub-words ([`sub_words`]), lower case, of two
/// characters or more, not in the stop list, each reduced by [`stem`].
pub fn terms(text: &str) -> Vec<String> {
    sub_words(text)
        .into_iter()
        .filter(|word| word.chars().count() > 1 && STOP.binary_search(&word.as_str()).is_err())
        .map(|word| stem(&word))
        .collect()
}

/// A word with its commonest English inflection taken off, so that
/// "renaming" meets `rename`, "sounds" `Sound` and "banned" `ban`:
/// one suffix of `ings`, `ing`, `ies` (to `y`), `ed`, `es` or `s` (not after
/// `s`, `u` or `i`: "class", "status", "analysis"), then a final `e`, then one of a doubled final consonant
/// other than `l` and `s`. A word of four characters or fewer stays as is
/// but for the `e` and the doubled consonant. Not a full stemmer: no
/// dependency, the same on both sides, and enough for the measured cases.
pub fn stem(word: &str) -> String {
    let mut w: Vec<char> = word.to_lowercase().chars().collect();
    for suffix in ["ings", "ing", "ies", "ed", "es", "s"] {
        let s: Vec<char> = suffix.chars().collect();
        let kept = suffix == "s" && matches!(w.iter().rev().nth(1), Some('s' | 'u' | 'i'));
        if w.len() > s.len() + 2 && w.ends_with(&s) && !kept {
            w.truncate(w.len() - s.len());
            if suffix == "ies" {
                w.push('y');
            }
            break;
        }
    }
    if w.len() > 3 && w.last() == Some(&'e') {
        w.pop();
    }
    let n = w.len();
    if n > 3 && w[n - 1] == w[n - 2] && !"aeiouls".contains(w[n - 1]) {
        w.pop();
    }
    w.into_iter().collect()
}

/// The language of a prompt's typed text: the index and the code are read
/// as English; a French prompt is mapped onto English terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    French,
}

/// Frequent French function words, accents folded ([`fold`]), rare in
/// English prose ("on", "a" and "as" are left out; "son" and "plus" stay,
/// since one alone never makes a prompt French): what tells a French
/// prompt.
const FRENCH: &[&str] = &[
    "apres", "au", "aussi", "aux", "avant", "avec", "avoir", "c", "ce", "ces", "cette", "chez",
    "comment", "d", "dans", "de", "deja", "des", "donc", "du", "elle", "elles", "encore", "entre",
    "est", "et", "etre", "eux", "faut", "il", "ils", "j", "jamais", "je", "l", "la", "le", "les",
    "leur", "leurs", "lorsque", "lui", "ma", "mais", "mes", "moi", "mon", "n", "ne", "nous", "ont",
    "ou", "parce", "pas", "plus", "pour", "pourquoi", "qu", "quand", "que", "quel", "quelle",
    "quelles", "quels", "qui", "quoi", "s", "sa", "sans", "se", "ses", "son", "sont", "sur", "toi",
    "toujours", "tous", "tout", "tres", "tu", "un", "une", "vous", "y",
];

/// English function words a prompt in English holds: a text is French only
/// when it holds more French ones than these.
const ENGLISH: &[&str] = &[
    "and", "are", "can", "do", "does", "for", "how", "i", "in", "is", "it", "my", "of", "should",
    "that", "the", "this", "to", "we", "what", "when", "where", "why", "with", "you",
];

/// The language of `text`: French when at least two of its words are
/// French function words and they outnumber its English ones; English
/// otherwise. Measured on the selector bench's 72 development prompts:
/// none of the English ones reads as French.
pub fn language(text: &str) -> Language {
    let words: Vec<String> = sub_words(text).iter().map(|w| fold(w)).collect();
    let count = |list: &[&str]| {
        words
            .iter()
            .filter(|w| list.binary_search(&w.as_str()).is_ok())
            .count()
    };
    let french = count(FRENCH);
    if french >= 2 && french > count(ENGLISH) {
        Language::French
    } else {
        Language::English
    }
}

/// A word in lower case with the accents of French taken off, as the
/// lexicon's keys are.
pub fn fold(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.to_lowercase().chars() {
        match c {
            'à' | 'â' | 'ä' => out.push('a'),
            'ç' => out.push('c'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'î' | 'ï' => out.push('i'),
            'ô' | 'ö' => out.push('o'),
            'ù' | 'û' | 'ü' => out.push('u'),
            'ÿ' => out.push('y'),
            'œ' => out.push_str("oe"),
            'æ' => out.push_str("ae"),
            c => out.push(c),
        }
    }
    out
}

/// French words and the English words code names the same notion with
///; a word with none carries no meaning in a request ("donne",
/// "merci") and is dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lexicon {
    /// By the word as written, in lower case: accents tell "marché"
    /// (market) from "marche" (works).
    exact: BTreeMap<String, Vec<String>>,
    /// By the word with its accents folded, for a prompt typed without
    /// them: the English words of every form that folds to it, and none
    /// only when no such form has any.
    folded: BTreeMap<String, Vec<String>>,
}

impl Lexicon {
    /// Reads `<french word>\t<english words, or ->` lines; `#` starts a
    /// comment line. A line of another shape is skipped.
    pub fn parse(text: &str) -> Lexicon {
        let mut lexicon = Lexicon::default();
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
            let folded = lexicon.folded.entry(fold(&french)).or_default();
            for target in &targets {
                if !folded.contains(target) {
                    folded.push(target.clone());
                }
            }
            lexicon.exact.insert(french, targets);
        }
        lexicon
    }

    /// The English words a French word maps to, whole, as the lexicon
    /// writes them (`marketplace`, not its term): looked up in lower case
    /// as written first, so accents choose ("marché" against "marche"),
    /// then with its accents folded, where the words of every form that
    /// folds to it are merged in file order. Empty for a word the lexicon
    /// drops ("donne"), `None` for one it does not hold.
    ///
    /// [`prompt_terms`] reads the terms of these words; a caller that
    /// matches whole words (v1's execution brief, ) reads them
    /// here, so both read one lookup and cannot drift apart on a form.
    pub fn english_words(&self, word: &str) -> Option<&[String]> {
        let word = word.to_lowercase();
        self.exact
            .get(&word)
            .or_else(|| self.folded.get(&fold(&word)))
            .map(Vec::as_slice)
    }

    /// How many French words, as written, it maps or drops.
    pub fn len(&self) -> usize {
        self.exact.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }
}

/// The terms of a prompt's text in `language`. English: [`terms`]. French:
/// a French function word, or one the lexicon maps to nothing, dropped;
/// one the lexicon knows (as written, else with its accents folded)
/// replaced by the terms of its English words; any other kept, folded, as
/// [`terms`] reads it, so that a word
/// French and code share ("jobs", "webhook") still meets its name.
pub fn prompt_terms(text: &str, language: Language, lexicon: &Lexicon) -> Vec<String> {
    if language == Language::English {
        return terms(text);
    }
    let mut out = Vec::new();
    for word in sub_words(text) {
        if FRENCH.binary_search(&fold(&word).as_str()).is_ok() {
            continue;
        }
        match lexicon.english_words(&word) {
            Some(english) => {
                for target in english {
                    out.extend(terms(target));
                }
            }
            None => out.extend(terms(&fold(&word))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_lists_are_sorted_for_their_binary_search() {
        for list in [STOP, FRENCH, ENGLISH] {
            assert!(
                list.windows(2).all(|pair| pair[0] < pair[1]),
                "unsorted near {list:?}"
            );
        }
    }

    // 's measured misses: the prompt inflects, the code does not.
    #[test]
    fn inflections_meet_the_names_they_come_from() {
        for (prompt, name) in [
            ("renaming", "rename"),
            ("sounds", "sound"),
            ("banned", "ban"),
            ("uninstalling", "uninstall"),
            ("shipped", "ship"),
            ("refunded", "refund"),
            ("processes", "process"),
            ("policies", "policy"),
        ] {
            assert_eq!(stem(prompt), stem(name), "{prompt} / {name}");
        }
    }

    #[test]
    fn words_that_are_not_inflections_keep_their_shape() {
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("bus"), "bus");
        assert_ne!(stem("ship"), stem("shop"));
    }

    #[test]
    fn a_prompt_and_a_name_split_into_the_same_terms() {
        assert_eq!(
            terms("Shipment#mark_shipped"),
            terms("shipment mark shipped")
        );
        assert_eq!(
            terms("How do I mark an order as shipped?"),
            ["mark", "order", "ship"]
        );
        assert_eq!(
            terms("app/models/user/bannable.rb"),
            ["app", "model", "user", "bannabl", "rb"]
        );
    }

    // none of the selector bench's English prompts may read as
    // French, or their minimaps would change; these are shaped like them.
    #[test]
    fn english_prompts_with_a_french_looking_word_stay_english() {
        for prompt in [
            "how do I make it so only admins can create rooms?",
            "where is the list of sounds you can play in a room?",
            "the son of a parent node is not set on the tree",
            "why does `pixel status` say de-indexed for a file that is there?",
        ] {
            assert_eq!(language(prompt), Language::English, "{prompt}");
        }
    }

    #[test]
    fn french_prompts_read_as_french_with_or_without_accents() {
        for prompt in [
            "pourquoi les rappels sont envoyés deux fois ?",
            "pourquoi les rappels sont envoyes deux fois",
            "Donne moi un prompt pour le prochain chantier",
            "quand un admin supprime un salon, les messages restent",
        ] {
            assert_eq!(language(prompt), Language::French, "{prompt}");
        }
    }

    // The failure that started "Donne" read as English stems to
    // `done`'s term. Read as French with the lexicon, it is a filler word.
    #[test]
    fn a_french_prompt_maps_its_words_and_drops_its_filler() {
        let lexicon =
            Lexicon::parse("# test\ncommande\torder command\ndonne\t-\nremboursées\trefund\n");
        let prompt = "Donne moi les commandes remboursées";
        assert!(terms(prompt).contains(&stem("done")));
        let french = prompt_terms(prompt, language(prompt), &lexicon);
        assert!(!french.contains(&stem("done")), "{french:?}");
        assert!(french.contains(&"refund".to_owned()), "{french:?}");
        // "commandes" is not in this lexicon: kept, as the English side
        // reads it, so a word French and code share still meets its name.
        assert!(french.contains(&stem("commandes")), "{french:?}");
    }

    // Folded, "marche" (works) and "marché" (market) are one key; as
    // written, accents tell them apart, and a word typed without accents
    // still finds every English word of its forms.
    #[test]
    fn accents_choose_the_word_and_their_absence_finds_every_form() {
        let lexicon = Lexicon::parse(
            "marche\t-\nmarché\tmarket marketplace\npasse\tpassword pass\npassé\t-\n",
        );
        let french = |text: &str| prompt_terms(text, Language::French, &lexicon);
        assert_eq!(french("marche"), Vec::<String>::new());
        assert_eq!(french("marché"), ["market", "marketplac"]);
        assert_eq!(french("passe"), ["password", "pass"]);
        assert_eq!(french("MARCHÉ"), ["market", "marketplac"]);
        assert_eq!(lexicon.len(), 4);
    }

    // A caller that matches whole words (v1's brief searches a word-bounded
    // content match, a symbol name) needs the words as written: a term
    // ("marketplac") is not a word the code holds.
    #[test]
    fn english_words_are_whole_and_their_terms_are_the_prompts() {
        let lexicon = Lexicon::parse("marché\tmarket marketplace\ndonne\t-\n");
        assert_eq!(
            lexicon.english_words("MARCHÉ"),
            Some(&["market".to_owned(), "marketplace".to_owned()][..])
        );
        let terms_of_words: Vec<String> = lexicon
            .english_words("marché")
            .unwrap()
            .iter()
            .flat_map(|word| terms(word))
            .collect();
        assert_eq!(
            terms_of_words,
            prompt_terms("marché", Language::French, &lexicon)
        );
        assert_eq!(lexicon.english_words("donne"), Some(&[][..]));
        assert_eq!(lexicon.english_words("marchés"), None);
    }

    // Typed without accents, a word the lexicon holds only with them gets
    // the words of every form that folds to it, once each, in file order:
    // which form was meant cannot be told, so none is lost. Typed with its
    // accents, it gets its own form's words only.
    #[test]
    fn a_folded_word_merges_the_words_of_every_form() {
        let lexicon = Lexicon::parse("créé\tcreate\ncrée\tcreate creator\ncréée\tcreated\n");
        assert_eq!(
            lexicon.english_words("cree"),
            Some(&["create".to_owned(), "creator".to_owned()][..])
        );
        assert_eq!(
            lexicon.english_words("creé"),
            Some(&["create".to_owned(), "creator".to_owned()][..])
        );
        assert_eq!(
            lexicon.english_words("Créé"),
            Some(&["create".to_owned()][..])
        );
        assert_eq!(
            lexicon.english_words("creee"),
            Some(&["created".to_owned()][..])
        );
    }

    #[test]
    fn folding_takes_off_french_accents_only() {
        assert_eq!(fold("Expédiée"), "expediee");
        assert_eq!(fold("cœur"), "coeur");
        assert_eq!(fold("Straße"), "straße");
    }
}
