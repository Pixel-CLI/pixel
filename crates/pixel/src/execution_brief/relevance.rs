// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The relevance gate of a plain-language prompt: does the repository talk
//! about what the prompt talks about?
//!
//! The daemon counts, per task keyword, the files it occurs in
//! (`facts.relevance`) and weighs the keywords and the files they meet in.
//! This module turns that block and the typed prompt into one decision with
//! no language model in it: four [`Features`], each a small function of the
//! evidence, are scored by a fixed logistic model
//! (`intercept + sum(coef * feature)`, the constants in [`super::gate_model`])
//! and the score falls in one of three [`Tier`]s by two thresholds.
//!
//! The weights are the daemon's, never recomputed here
//! (`pixel_daemon::relevance::row_weight` per keyword, `CoFile::weight` per
//! file), so a change of weighting there moves the numbers and not the rules.
//! The model was fitted on English prompts and holds only with a code graph:
//! without one, or without a keyword to weigh, it is not applied.

use super::gate_model;

/// How much one task keyword says about the repository.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeywordStat {
    pub(crate) keyword: String,
    /// The daemon's weight: `0.0` for a general word or one found in over a
    /// quarter of the files, the cap for a word found nowhere.
    pub(crate) weight: f64,
    /// The word is French and the repository holds none of its translations:
    /// untranslated French is not evidence of off-topic, so it weighs
    /// nothing whatever the daemon said.
    pub(crate) french_only: bool,
}

/// A file that several keywords meet in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CoFileStat {
    pub(crate) path: String,
    /// The task keywords found in the file.
    pub(crate) keywords: Vec<String>,
    /// The daemon's weight of the file: what its keywords add up to.
    pub(crate) weight: f64,
    /// At least one keyword is a symbol name or a path word of the file, not
    /// only text inside it.
    pub(crate) structural: bool,
}

/// What the scorer reads of the daemon's relevance block, independent of the
/// wire type that carried it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelevanceInput {
    /// The code graph answered; without it no keyword can be structural.
    pub(crate) graph: bool,
    /// Indexed files the counts are drawn from.
    pub(crate) files_considered: usize,
    /// Files a keyword of positive weight matched by name or symbol, counted
    /// before the co-file list was cut.
    pub(crate) structural_files: usize,
    pub(crate) keywords: Vec<KeywordStat>,
    /// Heaviest first.
    pub(crate) cofiles: Vec<CoFileStat>,
}

/// Everything the gate looks at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateInput<'a> {
    pub(crate) relevance: &'a RelevanceInput,
    /// The typed prompt, pasted blocks removed: the text the block was
    /// computed from.
    pub(crate) typed: &'a str,
}

/// How much the brief trusts the prompt to be about this repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// The full brief.
    High,
    /// A compact brief that says it is a maybe.
    Low,
    /// Nothing.
    Off,
}

impl Tier {
    /// The tier as the decision log spells it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
            Self::Off => "off",
        }
    }
}

/// How a verdict came about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The model scored the prompt.
    Model,
    /// No keyword to weigh: nothing to score, the prompt is off.
    NoKeywords,
    /// The code graph did not answer: the model does not apply, so the
    /// verdict says nothing either way.
    NoGraph,
}

/// The four features of the model, and the counts the log and the
/// `confidence:` line add.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Features {
    /// `ln(1 + 1000 * structural_files / files_considered)`.
    pub(crate) struct_per_mille: f64,
    /// The prompt is phrased as a question.
    pub(crate) question: bool,
    /// Share of the keywords that name a git, release or CI operation.
    pub(crate) ops_share: f64,
    /// The heaviest structural co-file's weight over the keywords' weight.
    pub(crate) struct_ratio: f64,
    /// Sum of every keyword's weight (Q).
    pub(crate) total_weight: f64,
    /// Weight of the heaviest structural co-file (B).
    pub(crate) best_structural_weight: f64,
    pub(crate) files_considered: usize,
    pub(crate) structural_files: usize,
    /// Task keywords.
    pub(crate) keywords: usize,
    /// Keywords with a weight above zero.
    pub(crate) informative: usize,
    /// Informative keywords the heaviest co-file holds.
    pub(crate) shared: usize,
}

/// The decision and the file that carried it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) tier: Tier,
    pub(crate) basis: Basis,
    /// The model's score; the thresholds cut it into tiers. `None` when the
    /// model was not applied.
    pub(crate) score: Option<f64>,
    /// The heaviest co-file, the one the brief starts from.
    pub(crate) best_file: Option<String>,
    pub(crate) features: Features,
}

/// A keyword carries evidence when its weight is above zero.
fn is_informative(weight: f64) -> bool {
    weight > 0.0
}

/// The keyword's weight: the daemon's, except that a French word the
/// repository lacks weighs nothing, because it names a thing the codebase
/// spells in English.
fn weight_of(stat: &KeywordStat) -> f64 {
    if stat.french_only { 0.0 } else { stat.weight }
}

/// Sum of the keywords' weights (Q).
fn total_weight(keywords: &[KeywordStat]) -> f64 {
    keywords.iter().map(weight_of).sum()
}

/// Keywords that carry weight.
fn informative_count(keywords: &[KeywordStat]) -> usize {
    keywords
        .iter()
        .filter(|stat| is_informative(weight_of(stat)))
        .count()
}

/// `ln(1 + 1000 * structural / considered)`: how widely the prompt's rare
/// words name files and symbols, as a share of the repository; `0` for an
/// empty one.
fn struct_per_mille(structural_files: usize, files_considered: usize) -> f64 {
    if files_considered == 0 {
        return 0.0;
    }
    (1000.0 * structural_files as f64 / files_considered as f64).ln_1p()
}

/// The lowercase runs of `[a-z0-9_]` in `text`: everything else separates
/// words.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))
        .filter(|word| !word.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Whether the prompt is phrased as a question: it ends on a question mark,
/// or one of its first three words opens a question.
fn is_question(typed: &str) -> bool {
    typed.trim().ends_with('?')
        || words(typed)
            .iter()
            .take(3)
            .any(|word| gate_model::QUESTION_WORDS.contains(&word.as_str()))
}

/// Share of the keywords that name an operation; `0` without any.
fn ops_share(keywords: &[KeywordStat]) -> f64 {
    if keywords.is_empty() {
        return 0.0;
    }
    let ops = keywords
        .iter()
        .filter(|stat| gate_model::OPS_VOCAB.contains(&stat.keyword.as_str()))
        .count();
    ops as f64 / keywords.len() as f64
}

/// The heaviest of `files`; the first of equals, which is the one the daemon
/// ranked first.
fn heaviest<'a>(files: impl Iterator<Item = &'a CoFileStat>) -> Option<&'a CoFileStat> {
    files.fold(None, |best: Option<&CoFileStat>, file| match best {
        Some(held) if held.weight >= file.weight => Some(held),
        _ => Some(file),
    })
}

/// B: the weight of the heaviest structural co-file, `0` when there is none.
fn best_structural_weight(files: &[CoFileStat]) -> f64 {
    heaviest(files.iter().filter(|file| file.structural)).map_or(0.0, |file| file.weight)
}

/// B over Q: how strongly the rare words, taken together, land on a file
/// named for them; `0` when the keywords weigh nothing.
fn struct_ratio(best: f64, total: f64) -> f64 {
    if total > 0.0 { best / total } else { 0.0 }
}

/// Informative keywords `file` holds, each counted once.
fn shared_terms(file: &CoFileStat, keywords: &[KeywordStat]) -> usize {
    keywords
        .iter()
        .filter(|stat| is_informative(weight_of(stat)) && file.keywords.contains(&stat.keyword))
        .count()
}

/// Every feature of `input`.
fn extract(input: &GateInput) -> Features {
    let relevance = input.relevance;
    let total = total_weight(&relevance.keywords);
    let best = best_structural_weight(&relevance.cofiles);
    Features {
        struct_per_mille: struct_per_mille(relevance.structural_files, relevance.files_considered),
        question: is_question(input.typed),
        ops_share: ops_share(&relevance.keywords),
        struct_ratio: struct_ratio(best, total),
        total_weight: total,
        best_structural_weight: best,
        files_considered: relevance.files_considered,
        structural_files: relevance.structural_files,
        keywords: relevance.keywords.len(),
        informative: informative_count(&relevance.keywords),
        shared: heaviest(relevance.cofiles.iter())
            .map_or(0, |file| shared_terms(file, &relevance.keywords)),
    }
}

/// The model's score of `features`.
fn linear(features: &Features) -> f64 {
    gate_model::INTERCEPT
        + gate_model::STRUCT_PER_MILLE * features.struct_per_mille
        + gate_model::QUESTION * f64::from(u8::from(features.question))
        + gate_model::OPS_SHARE * features.ops_share
        + gate_model::STRUCT_RATIO * features.struct_ratio
}

/// The tier a score falls in: strictly above `high` is high, strictly above
/// `low` is low.
fn tier_of(score: f64, high: f64, low: f64) -> Tier {
    if score > high {
        Tier::High
    } else if score > low {
        Tier::Low
    } else {
        Tier::Off
    }
}

impl Verdict {
    /// The `confidence:` line of a brief built on this decision; `None` when
    /// there is no brief.
    pub(crate) fn confidence_line(&self) -> Option<String> {
        match (self.basis, self.tier) {
            (Basis::Model, Tier::High) => Some(format!(
                "confidence: high — {}/{} key terms covered; start with the first file",
                self.features.shared, self.features.informative
            )),
            (Basis::Model, Tier::Low) => {
                Some("confidence: low — possibly related; verify before relying on it".to_string())
            }
            _ => None,
        }
    }
}

/// Decide whether the prompt behind `input` is about this repository, and
/// how sure the brief should sound. The one place the model is applied, so
/// a different model replaces this function and nothing around it.
pub(crate) fn judge(input: &GateInput) -> Verdict {
    let features = extract(input);
    let best_file = heaviest(input.relevance.cofiles.iter()).map(|file| file.path.clone());
    let (tier, basis, score) = if input.relevance.keywords.is_empty() {
        (Tier::Off, Basis::NoKeywords, None)
    } else if !input.relevance.graph {
        (Tier::Off, Basis::NoGraph, None)
    } else {
        let score = linear(&features);
        (
            tier_of(score, gate_model::HIGH, gate_model::LOW),
            Basis::Model,
            Some(score),
        )
    };
    Verdict {
        tier,
        basis,
        score,
        best_file,
        features,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(keyword: &str, weight: f64) -> KeywordStat {
        KeywordStat {
            keyword: keyword.into(),
            weight,
            french_only: false,
        }
    }

    fn file(path: &str, words: &[&str], weight: f64, structural: bool) -> CoFileStat {
        CoFileStat {
            path: path.into(),
            keywords: words.iter().map(ToString::to_string).collect(),
            weight,
            structural,
        }
    }

    fn near(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn is_informative_should_need_a_weight_above_zero() {
        assert!(!is_informative(0.0));
        assert!(is_informative(f64::MIN_POSITIVE));
        assert!(!is_informative(-1.0));
    }

    #[test]
    fn weight_of_should_give_an_untranslated_french_word_no_weight() {
        let word = |weight: f64, french_only: bool| KeywordStat {
            keyword: "mot".into(),
            weight,
            french_only,
        };
        assert!(weight_of(&word(4.0, true)).abs() < f64::EPSILON);
        assert!(near(weight_of(&word(4.0, false)), 4.0));
        assert!(near(weight_of(&word(2.0, false)), 2.0));
    }

    #[test]
    fn total_weight_should_add_the_weights_and_skip_untranslated_french() {
        let mut french = stat("fichier", 4.0);
        french.french_only = true;
        assert!(near(
            total_weight(&[stat("a", 2.0), stat("b", 0.0), stat("c", 3.0), french]),
            5.0
        ));
        assert!(total_weight(&[]).abs() < f64::EPSILON);
    }

    #[test]
    fn informative_count_should_count_keywords_with_weight() {
        let mut french = stat("fichier", 4.0);
        french.french_only = true;
        assert_eq!(
            informative_count(&[stat("a", 2.0), stat("b", 0.0), stat("c", 3.0), french]),
            2
        );
        assert_eq!(informative_count(&[]), 0);
    }

    #[test]
    fn struct_per_mille_should_be_the_log_of_the_structural_share_per_thousand() {
        // 30 of 1000 files: ln(1 + 30).
        assert!(near(struct_per_mille(30, 1000), 31.0_f64.ln()));
        assert!(near(struct_per_mille(1, 2), 501.0_f64.ln()));
        assert!(struct_per_mille(0, 1000).abs() < f64::EPSILON);
        assert!(
            struct_per_mille(5, 0).abs() < f64::EPSILON,
            "an empty repository has no share"
        );
        // The value does not move with the size of the repository.
        assert!(near(struct_per_mille(3, 100), struct_per_mille(30, 1000)));
    }

    #[test]
    fn words_should_be_the_lowercase_runs_of_letters_digits_and_underscores() {
        assert_eq!(
            words("How does Foo_bar2 work? (really)"),
            ["how", "does", "foo_bar2", "work", "really"]
        );
        assert_eq!(words("  "), Vec::<String>::new());
        assert_eq!(words("l'index"), ["l", "index"]);
        assert_eq!(words("café"), ["caf"], "only ASCII letters make a word");
    }

    #[test]
    fn is_question_should_follow_the_mark_or_one_of_the_first_three_words() {
        for typed in [
            "does the daemon start",
            "How is it built",
            "  where is the cache",
            "so how is it built",
            "so, then, how is it built",
            "the cache is built where?",
            "  the cache is built where?  ",
            "WHY",
        ] {
            assert!(is_question(typed), "{typed}");
        }
        for typed in [
            "bump the version",
            "whatever the daemon does",
            "the daemon",
            "ok so then how is it built",
            "",
        ] {
            assert!(!is_question(typed), "{typed}");
        }
    }

    #[test]
    fn ops_share_should_be_the_share_of_keywords_naming_an_operation() {
        let keywords = [
            stat("push", 1.0),
            stat("branch", 1.0),
            stat("parser", 1.0),
            stat("daemon", 1.0),
        ];
        assert!(near(ops_share(&keywords), 0.5));
        assert!(ops_share(&keywords[2..]).abs() < f64::EPSILON);
        assert!(near(ops_share(&keywords[..2]), 1.0));
        assert!(ops_share(&[]).abs() < f64::EPSILON);
        // The model's own list: `changelog` and `workflow` count here, not in
        // the shape filter.
        assert!(near(
            ops_share(&[stat("changelog", 1.0), stat("workflow", 1.0)]),
            1.0
        ));
        assert!(
            ops_share(&[stat("Push", 1.0)]).abs() < f64::EPSILON,
            "exact words"
        );
    }

    #[test]
    fn heaviest_should_take_the_heaviest_and_the_first_of_equals() {
        let files = [
            file("a.rs", &[], 2.0, false),
            file("b.rs", &[], 5.0, true),
            file("c.rs", &[], 5.0, false),
            file("d.rs", &[], 1.0, true),
        ];
        assert_eq!(
            heaviest(files.iter()).map(|f| f.path.as_str()),
            Some("b.rs")
        );
        assert_eq!(
            heaviest(files.iter().filter(|f| !f.structural)).map(|f| f.path.as_str()),
            Some("c.rs")
        );
        assert!(heaviest([].iter()).is_none());
    }

    #[test]
    fn best_structural_weight_should_ignore_the_files_without_a_symbol_or_path_match() {
        let files = [
            file("notes.md", &[], 9.0, false),
            file("a.rs", &[], 2.0, true),
            file("b.rs", &[], 4.0, true),
        ];
        assert!(near(best_structural_weight(&files), 4.0));
        assert!(best_structural_weight(&files[..1]).abs() < f64::EPSILON);
        assert!(best_structural_weight(&[]).abs() < f64::EPSILON);
    }

    #[test]
    fn struct_ratio_should_divide_by_the_weight_and_be_zero_without_any() {
        assert!(near(struct_ratio(3.0, 4.0), 0.75));
        assert!(
            near(struct_ratio(5.0, 4.0), 1.25),
            "a ratio, not a share: not clamped"
        );
        assert!(struct_ratio(3.0, 0.0).abs() < f64::EPSILON);
        assert!(struct_ratio(3.0, -1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn shared_terms_should_count_each_informative_keyword_the_file_holds_once() {
        let keywords = [
            stat("a", 2.0),
            stat("b", 0.0),
            stat("c", 3.0),
            stat("d", 1.0),
        ];
        let held = file("f.rs", &["a", "a", "b", "c"], 5.0, true);
        assert_eq!(
            shared_terms(&held, &keywords),
            2,
            "a and c; b has no weight"
        );
        assert_eq!(shared_terms(&file("g.rs", &[], 0.0, false), &keywords), 0);
        assert_eq!(shared_terms(&held, &[]), 0);
    }

    fn sample() -> RelevanceInput {
        RelevanceInput {
            graph: true,
            files_considered: 1000,
            structural_files: 30,
            keywords: vec![
                stat("push", 2.0),
                stat("daemon", 3.0),
                stat("watch", 1.0),
                stat("the", 0.0),
            ],
            cofiles: vec![
                file("docs/notes.md", &["daemon", "watch"], 9.0, false),
                file("src/daemon.rs", &["daemon", "watch", "push"], 6.0, true),
                file("src/other.rs", &["daemon"], 3.0, true),
            ],
        }
    }

    #[test]
    fn extract_should_read_every_feature_from_the_evidence() {
        let input = sample();
        let features = extract(&GateInput {
            relevance: &input,
            typed: "does the daemon push?",
        });
        assert_eq!(
            features,
            Features {
                struct_per_mille: 31.0_f64.ln(),
                question: true,
                ops_share: 0.25,
                struct_ratio: 1.0,
                total_weight: 6.0,
                best_structural_weight: 6.0,
                files_considered: 1000,
                structural_files: 30,
                keywords: 4,
                informative: 3,
                // The heaviest file overall is the note, which holds two.
                shared: 2,
            }
        );
    }

    #[test]
    fn linear_should_add_the_intercept_and_each_weighted_feature() {
        let features = Features {
            struct_per_mille: 2.0,
            question: true,
            ops_share: 0.5,
            struct_ratio: 0.25,
            ..Features::default()
        };
        let expected = gate_model::INTERCEPT
            + gate_model::STRUCT_PER_MILLE * 2.0
            + gate_model::QUESTION
            + gate_model::OPS_SHARE * 0.5
            + gate_model::STRUCT_RATIO * 0.25;
        assert!(near(linear(&features), expected));
        assert!(near(linear(&Features::default()), gate_model::INTERCEPT));
        let plain = Features {
            question: false,
            ..features
        };
        assert!(near(linear(&plain), expected - gate_model::QUESTION));
    }

    #[test]
    fn tier_of_should_put_a_score_at_a_threshold_in_the_lower_tier() {
        let (high, low) = (1.5, 0.5);
        assert_eq!(tier_of(high + 0.01, high, low), Tier::High);
        assert_eq!(tier_of(high, high, low), Tier::Low, "strictly above");
        assert_eq!(tier_of(high - 0.01, high, low), Tier::Low);
        assert_eq!(tier_of(low + 0.01, high, low), Tier::Low);
        assert_eq!(tier_of(low, high, low), Tier::Off, "strictly above");
        assert_eq!(tier_of(low - 0.01, high, low), Tier::Off);
        assert_eq!(tier_of(f64::NEG_INFINITY, high, low), Tier::Off);
    }

    #[test]
    fn a_tier_should_name_itself_for_the_decision_log() {
        assert_eq!(Tier::High.as_str(), "high");
        assert_eq!(Tier::Low.as_str(), "low");
        assert_eq!(Tier::Off.as_str(), "off");
    }

    fn verdict(tier: Tier, basis: Basis) -> Verdict {
        Verdict {
            tier,
            basis,
            score: Some(0.0),
            best_file: None,
            features: Features {
                informative: 5,
                shared: 4,
                ..Features::default()
            },
        }
    }

    #[test]
    fn confidence_line_should_state_the_tier_and_nothing_for_off_or_an_unscored_prompt() {
        assert_eq!(
            verdict(Tier::High, Basis::Model)
                .confidence_line()
                .as_deref(),
            Some("confidence: high — 4/5 key terms covered; start with the first file")
        );
        assert_eq!(
            verdict(Tier::Low, Basis::Model)
                .confidence_line()
                .as_deref(),
            Some("confidence: low — possibly related; verify before relying on it")
        );
        assert_eq!(verdict(Tier::Off, Basis::Model).confidence_line(), None);
        assert_eq!(verdict(Tier::Off, Basis::NoGraph).confidence_line(), None);
        assert_eq!(
            verdict(Tier::Off, Basis::NoKeywords).confidence_line(),
            None
        );
    }

    #[test]
    fn judge_should_apply_the_model_to_the_features_and_name_the_heaviest_file() {
        let input = sample();
        let gate = GateInput {
            relevance: &input,
            typed: "does the daemon push?",
        };
        let verdict = judge(&gate);
        let features = extract(&gate);
        let score = linear(&features);
        assert_eq!(verdict.features, features);
        assert!(near(verdict.score.unwrap(), score));
        assert_eq!(verdict.basis, Basis::Model);
        assert_eq!(
            verdict.tier,
            tier_of(score, gate_model::HIGH, gate_model::LOW)
        );
        assert_eq!(verdict.best_file.as_deref(), Some("docs/notes.md"));
    }

    #[test]
    fn judge_should_put_a_prompt_with_no_keyword_off_without_a_score() {
        let input = RelevanceInput {
            keywords: Vec::new(),
            ..sample()
        };
        let verdict = judge(&GateInput {
            relevance: &input,
            typed: "how does it work?",
        });
        assert_eq!(
            (verdict.tier, verdict.basis),
            (Tier::Off, Basis::NoKeywords)
        );
        assert_eq!(verdict.score, None);
    }

    #[test]
    fn judge_should_not_apply_the_model_without_a_code_graph() {
        let input = RelevanceInput {
            graph: false,
            ..sample()
        };
        let verdict = judge(&GateInput {
            relevance: &input,
            typed: "does the daemon push?",
        });
        assert_eq!((verdict.tier, verdict.basis), (Tier::Off, Basis::NoGraph));
        assert_eq!(verdict.score, None);
        // The features are still read, for the log.
        assert_eq!(verdict.features.structural_files, 30);
        assert_eq!(verdict.best_file.as_deref(), Some("docs/notes.md"));
    }

    #[test]
    fn judge_should_put_a_prompt_the_repository_does_not_talk_about_off() {
        let input = RelevanceInput {
            graph: true,
            files_considered: 1000,
            structural_files: 0,
            keywords: vec![stat("weather", 4.0), stat("tomorrow", 4.0)],
            cofiles: Vec::new(),
        };
        let verdict = judge(&GateInput {
            relevance: &input,
            typed: "what is the weather tomorrow",
        });
        assert_eq!((verdict.tier, verdict.basis), (Tier::Off, Basis::Model));
        assert_eq!(verdict.best_file, None);
        // intercept + question
        assert!(near(
            verdict.score.unwrap(),
            gate_model::INTERCEPT + gate_model::QUESTION
        ));
    }
}
