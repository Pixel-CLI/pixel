// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The relevance gate of a plain-language prompt: does the repository talk
//! about what the prompt talks about?
//!
//! The daemon counts, per task keyword, the files it occurs in
//! (`facts.relevance`) and weighs the keywords and the files they meet in;
//! the meaning search returns the chunks closest to the prompt. This module
//! turns those two answers and the shape of the prompt into one decision with
//! no language model in it: a handful of [`Features`], each a small function
//! of the evidence, are scored by a fixed logistic model
//! (`intercept + sum(coef * (x - mean) / std)`, the constants in
//! [`super::gate_model`]) and the score falls in one of three [`Tier`]s by two
//! thresholds. A feature whose source did not answer contributes nothing, as
//! if it sat at its mean.
//!
//! The weights are the daemon's, never recomputed here
//! (`pixel_daemon::relevance::row_weight` per keyword, `CoFile::weight` per
//! file), so a change of weighting there moves the numbers and not the rules.

use super::gate_model;

/// Leads of the meaning search the agreement feature looks at.
const AGREEMENT_LEADS: usize = 3;

/// Openers of a question in English.
const QUESTION_OPENERS: &[&str] = &[
    "are", "can", "could", "do", "does", "how", "is", "should", "what", "when", "where", "which",
    "who", "why", "would",
];

/// How much one task keyword says about the repository.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeywordStat {
    pub(crate) keyword: String,
    /// The daemon's weight: `0.0` for a word found everywhere (or whose
    /// probe truncated), the cap for a word found nowhere.
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
    pub(crate) keywords: Vec<KeywordStat>,
    /// Best first.
    pub(crate) cofiles: Vec<CoFileStat>,
}

/// One lead of the meaning search: the file and how close the chunk was.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Lead {
    pub(crate) path: String,
    pub(crate) score: f64,
}

/// Everything the gate looks at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateInput<'a> {
    pub(crate) relevance: &'a RelevanceInput,
    /// Empty when the meaning search did not answer.
    pub(crate) leads: &'a [Lead],
    /// The typed prompt.
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

/// The value of every feature of one prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Features {
    /// Sum of every keyword's weight (Q).
    pub(crate) total_weight: f64,
    /// Weight the heaviest file holds (S).
    pub(crate) best_weight: f64,
    /// S / Q, at most 1.
    pub(crate) coverage: f64,
    /// The heaviest structural file's weight over Q, at most 1.
    pub(crate) structural_coverage: f64,
    /// Files with a keyword in a symbol or path name.
    pub(crate) structural_files: usize,
    /// Task keywords.
    pub(crate) keywords: usize,
    /// Keywords with a weight above zero.
    pub(crate) informative: usize,
    /// Informative keywords the heaviest file holds.
    pub(crate) shared: usize,
    /// Share of the first meaning leads that are also co-files; `None` when
    /// the meaning search gave no lead.
    pub(crate) agreement: Option<f64>,
    /// Score of the best meaning lead; `None` without one.
    pub(crate) top_lead: Option<f64>,
    /// Share of the keywords that name a git, release or deploy operation.
    pub(crate) ops_share: f64,
    /// The prompt is phrased as a question.
    pub(crate) question: bool,
    /// ln(1 + words of the prompt).
    pub(crate) length: f64,
}

/// A feature a model term can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    Coverage,
    StructuralCoverage,
    StructuralFiles,
    Keywords,
    Informative,
    Shared,
    Agreement,
    TopLead,
    OpsShare,
    Question,
    Length,
}

impl Feature {
    /// The feature's value, or `None` when its source did not answer.
    pub(crate) fn value(self, features: &Features) -> Option<f64> {
        match self {
            Self::Coverage => Some(features.coverage),
            Self::StructuralCoverage => Some(features.structural_coverage),
            Self::StructuralFiles => Some(features.structural_files as f64),
            Self::Keywords => Some(features.keywords as f64),
            Self::Informative => Some(features.informative as f64),
            Self::Shared => Some(features.shared as f64),
            Self::Agreement => features.agreement,
            Self::TopLead => features.top_lead,
            Self::OpsShare => Some(features.ops_share),
            Self::Question => Some(f64::from(u8::from(features.question))),
            Self::Length => Some(features.length),
        }
    }
}

/// One term of the model: a feature, its coefficient on the standardised
/// value, and the mean and standard deviation it was standardised with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Term {
    pub(crate) feature: Feature,
    pub(crate) coef: f64,
    pub(crate) mean: f64,
    pub(crate) std: f64,
}

impl Term {
    /// What the term adds to the score; nothing when the feature is absent.
    pub(crate) fn contribution(&self, features: &Features) -> f64 {
        self.feature
            .value(features)
            .map_or(0.0, |value| self.coef * (value - self.mean) / self.std)
    }
}

/// The decision and the file that carried it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) tier: Tier,
    /// The model's score; the thresholds cut it into tiers.
    pub(crate) score: f64,
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

/// Sum of the keywords' weights.
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

/// `part` of `whole`, at most the whole and nothing of an empty one.
fn share(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        (part / whole).min(1.0)
    } else {
        0.0
    }
}

/// The heaviest of `files`; the first of equals, which is the one the daemon
/// ranked first.
fn heaviest<'a>(files: impl Iterator<Item = &'a CoFileStat>) -> Option<&'a CoFileStat> {
    files.fold(None, |best: Option<&CoFileStat>, file| match best {
        Some(held) if held.weight >= file.weight => Some(held),
        _ => Some(file),
    })
}

/// Informative keywords `file` holds, each counted once.
fn shared_terms(file: &CoFileStat, keywords: &[KeywordStat]) -> usize {
    keywords
        .iter()
        .filter(|stat| is_informative(weight_of(stat)) && file.keywords.contains(&stat.keyword))
        .count()
}

/// Files with a keyword in a symbol or path name.
fn structural_files(files: &[CoFileStat]) -> usize {
    files.iter().filter(|file| file.structural).count()
}

/// Share of the first [`AGREEMENT_LEADS`] meaning leads whose file the
/// keywords also met in; `None` without a lead.
fn agreement(leads: &[Lead], files: &[CoFileStat]) -> Option<f64> {
    let taken = &leads[..leads.len().min(AGREEMENT_LEADS)];
    if taken.is_empty() {
        return None;
    }
    let agreeing = taken
        .iter()
        .filter(|lead| files.iter().any(|file| file.path == lead.path))
        .count();
    Some(agreeing as f64 / taken.len() as f64)
}

/// The score of the first lead.
fn top_lead(leads: &[Lead]) -> Option<f64> {
    leads.first().map(|lead| lead.score)
}

/// Share of the keywords that name an operation of the repository.
fn ops_share(keywords: &[KeywordStat]) -> f64 {
    if keywords.is_empty() {
        return 0.0;
    }
    let ops = keywords
        .iter()
        .filter(|stat| super::OPS_WORDS.contains(&stat.keyword.as_str()))
        .count();
    ops as f64 / keywords.len() as f64
}

/// Whether the prompt asks a question: it ends on a question mark or opens
/// with a question word.
fn is_question(typed: &str) -> bool {
    let typed = typed.trim();
    typed.ends_with('?')
        || typed
            .split_whitespace()
            .next()
            .is_some_and(|first| QUESTION_OPENERS.contains(&first.to_lowercase().as_str()))
}

/// ln(1 + the words of the prompt): length without letting a long paste
/// outweigh everything else.
fn length(typed: &str) -> f64 {
    (1.0 + typed.split_whitespace().count() as f64).ln()
}

/// Every feature of `input`.
fn extract(input: &GateInput) -> Features {
    let relevance = input.relevance;
    let total = total_weight(&relevance.keywords);
    let best = heaviest(relevance.cofiles.iter());
    let best_structural = heaviest(relevance.cofiles.iter().filter(|file| file.structural));
    let best_weight = best.map_or(0.0, |file| file.weight);
    Features {
        total_weight: total,
        best_weight,
        coverage: share(best_weight, total),
        structural_coverage: share(best_structural.map_or(0.0, |file| file.weight), total),
        structural_files: structural_files(&relevance.cofiles),
        keywords: relevance.keywords.len(),
        informative: informative_count(&relevance.keywords),
        shared: best.map_or(0, |file| shared_terms(file, &relevance.keywords)),
        agreement: agreement(input.leads, &relevance.cofiles),
        top_lead: top_lead(input.leads),
        ops_share: ops_share(&relevance.keywords),
        question: is_question(input.typed),
        length: length(input.typed),
    }
}

/// `intercept` plus what each of `terms` adds.
fn linear(features: &Features, intercept: f64, terms: &[Term]) -> f64 {
    intercept
        + terms
            .iter()
            .map(|term| term.contribution(features))
            .sum::<f64>()
}

/// The tier a score falls in: at or above `high` is high, at or above `low`
/// is low.
fn tier_of(score: f64, high: f64, low: f64) -> Tier {
    if score >= high {
        Tier::High
    } else if score >= low {
        Tier::Low
    } else {
        Tier::Off
    }
}

impl Verdict {
    /// The `confidence:` line of a brief built on this decision; `None` when
    /// there is no brief.
    pub(crate) fn confidence_line(&self) -> Option<String> {
        match self.tier {
            Tier::High => Some(format!(
                "confidence: high — {}/{} key terms covered; start with the first file",
                self.features.shared, self.features.informative
            )),
            Tier::Low => {
                Some("confidence: low — possibly related; verify before relying on it".to_string())
            }
            Tier::Off => None,
        }
    }
}

/// Decide whether the prompt behind `input` is about this repository, and
/// how sure the brief should sound. The one place the model is applied, so
/// a different model replaces this function and nothing around it.
pub(crate) fn judge(input: &GateInput) -> Verdict {
    let features = extract(input);
    let score = linear(&features, gate_model::INTERCEPT, &gate_model::TERMS);
    Verdict {
        tier: tier_of(score, gate_model::HIGH, gate_model::LOW),
        score,
        best_file: heaviest(input.relevance.cofiles.iter()).map(|file| file.path.clone()),
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

    fn lead(path: &str, score: f64) -> Lead {
        Lead {
            path: path.into(),
            score,
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
    fn share_should_be_a_fraction_of_the_whole_and_never_more() {
        assert!(near(share(1.0, 2.0), 0.5));
        assert!(near(share(2.0, 2.0), 1.0));
        assert!(near(share(3.0, 2.0), 1.0), "a boosted file is the whole");
        assert!(
            share(1.0, 0.0).abs() < f64::EPSILON,
            "nothing of an empty prompt"
        );
        assert!(share(1.0, -1.0).abs() < f64::EPSILON);
        assert!(share(0.0, 2.0).abs() < f64::EPSILON);
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
            heaviest(files.iter().filter(|f| f.structural)).map(|f| f.path.as_str()),
            Some("b.rs")
        );
        assert_eq!(
            heaviest(files.iter().filter(|f| !f.structural)).map(|f| f.path.as_str()),
            Some("c.rs")
        );
        assert!(heaviest([].iter()).is_none());
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

    #[test]
    fn structural_files_should_count_the_files_with_a_symbol_or_path_match() {
        let files = [
            file("a.rs", &[], 1.0, true),
            file("b.md", &[], 1.0, false),
            file("c.rs", &[], 1.0, true),
        ];
        assert_eq!(structural_files(&files), 2);
        assert_eq!(structural_files(&[]), 0);
    }

    #[test]
    fn agreement_should_be_the_share_of_the_first_leads_the_keywords_also_met_in() {
        let files = [file("a.rs", &[], 1.0, true), file("b.rs", &[], 1.0, true)];
        let leads = [
            lead("a.rs", 0.9),
            lead("x.rs", 0.8),
            lead("b.rs", 0.7),
            lead("a.rs", 0.6),
        ];
        // Only the first three count: a.rs and b.rs of a.rs, x.rs, b.rs.
        assert!(near(agreement(&leads, &files).unwrap(), 2.0 / 3.0));
        assert!(near(agreement(&leads[..1], &files).unwrap(), 1.0));
        assert!(agreement(&leads[1..2], &files).unwrap().abs() < f64::EPSILON);
        assert_eq!(agreement(&[], &files), None);
        assert!(agreement(&leads, &[]).unwrap().abs() < f64::EPSILON);
    }

    #[test]
    fn top_lead_should_be_the_score_of_the_first_lead() {
        assert_eq!(top_lead(&[lead("a.rs", 0.9), lead("b.rs", 0.1)]), Some(0.9));
        assert_eq!(top_lead(&[]), None);
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
    }

    #[test]
    fn is_question_should_follow_the_mark_or_the_opening_word() {
        for typed in [
            "does the daemon start",
            "How is it built",
            "  where is the cache",
            "the cache is built where?",
            "WHY",
        ] {
            assert!(is_question(typed), "{typed}");
        }
        for typed in [
            "bump the version",
            "whatever the daemon does",
            "the daemon",
            "",
        ] {
            assert!(!is_question(typed), "{typed}");
        }
    }

    #[test]
    fn length_should_be_the_log_of_one_plus_the_words() {
        assert!(length("").abs() < f64::EPSILON);
        assert!(near(length("one"), 2.0_f64.ln()));
        assert!(near(length("how does the daemon start"), 6.0_f64.ln()));
        assert!(near(length("  two   words "), 3.0_f64.ln()));
    }

    fn sample() -> RelevanceInput {
        RelevanceInput {
            graph: true,
            keywords: vec![
                stat("push", 2.0),
                stat("daemon", 3.0),
                stat("watch", 1.0),
                stat("the", 0.0),
            ],
            cofiles: vec![
                file("docs/notes.md", &["daemon", "watch"], 4.0, false),
                file("src/daemon.rs", &["daemon", "watch", "push"], 6.0, true),
                file("src/other.rs", &["daemon"], 3.0, true),
            ],
        }
    }

    #[test]
    fn extract_should_read_every_feature_from_the_evidence() {
        let input = sample();
        let leads = [lead("src/daemon.rs", 0.8), lead("elsewhere.rs", 0.6)];
        let features = extract(&GateInput {
            relevance: &input,
            leads: &leads,
            typed: "does the daemon push?",
        });
        assert_eq!(
            features,
            Features {
                total_weight: 6.0,
                best_weight: 6.0,
                coverage: 1.0,
                structural_coverage: 1.0,
                structural_files: 2,
                keywords: 4,
                informative: 3,
                shared: 3,
                agreement: Some(0.5),
                top_lead: Some(0.8),
                ops_share: 0.25,
                question: true,
                length: 5.0_f64.ln(),
            }
        );
    }

    #[test]
    fn extract_should_leave_the_meaning_features_absent_without_a_lead_and_zero_without_a_file() {
        let empty = RelevanceInput {
            graph: true,
            keywords: vec![stat("weather", 3.0)],
            cofiles: Vec::new(),
        };
        let features = extract(&GateInput {
            relevance: &empty,
            leads: &[],
            typed: "weather",
        });
        assert_eq!(features.agreement, None);
        assert_eq!(features.top_lead, None);
        assert!(features.coverage.abs() < f64::EPSILON);
        assert!(features.best_weight.abs() < f64::EPSILON);
        assert_eq!((features.shared, features.structural_files), (0, 0));
        assert!(!features.question);
    }

    #[test]
    fn extract_should_measure_the_structural_file_apart_from_the_heaviest() {
        let input = RelevanceInput {
            graph: true,
            keywords: vec![stat("a", 2.0), stat("b", 2.0)],
            cofiles: vec![
                file("notes.md", &["a", "b"], 4.0, false),
                file("code.rs", &["a"], 2.0, true),
            ],
        };
        let features = extract(&GateInput {
            relevance: &input,
            leads: &[],
            typed: "a b",
        });
        assert!(near(features.coverage, 1.0));
        assert!(near(features.structural_coverage, 0.5));
        assert_eq!(features.shared, 2, "the heaviest file decides `shared`");
    }

    #[test]
    fn feature_value_should_name_each_feature_and_leave_the_unanswered_ones_absent() {
        let features = Features {
            total_weight: 8.0,
            best_weight: 6.0,
            coverage: 0.75,
            structural_coverage: 0.5,
            structural_files: 2,
            keywords: 5,
            informative: 4,
            shared: 3,
            agreement: Some(0.25),
            top_lead: Some(0.9),
            ops_share: 0.2,
            question: true,
            length: 1.5,
        };
        for (feature, expected) in [
            (Feature::Coverage, 0.75),
            (Feature::StructuralCoverage, 0.5),
            (Feature::StructuralFiles, 2.0),
            (Feature::Keywords, 5.0),
            (Feature::Informative, 4.0),
            (Feature::Shared, 3.0),
            (Feature::Agreement, 0.25),
            (Feature::TopLead, 0.9),
            (Feature::OpsShare, 0.2),
            (Feature::Question, 1.0),
            (Feature::Length, 1.5),
        ] {
            assert_eq!(feature.value(&features), Some(expected), "{feature:?}");
        }
        let bare = Features {
            agreement: None,
            top_lead: None,
            question: false,
            ..features
        };
        assert_eq!(Feature::Agreement.value(&bare), None);
        assert_eq!(Feature::TopLead.value(&bare), None);
        assert_eq!(Feature::Question.value(&bare), Some(0.0));
    }

    #[test]
    fn a_term_should_standardise_the_feature_and_add_nothing_when_it_is_absent() {
        let term = Term {
            feature: Feature::Agreement,
            coef: 2.0,
            mean: 0.5,
            std: 0.25,
        };
        let mut features = Features {
            agreement: Some(0.75),
            ..Features::default()
        };
        assert!(
            near(term.contribution(&features), 2.0),
            "2 * (0.75 - 0.5) / 0.25"
        );
        features.agreement = Some(0.25);
        assert!(near(term.contribution(&features), -2.0));
        features.agreement = None;
        assert!(term.contribution(&features).abs() < f64::EPSILON);
    }

    #[test]
    fn linear_should_add_the_intercept_and_every_term() {
        let terms = [
            Term {
                feature: Feature::Shared,
                coef: 1.0,
                mean: 2.0,
                std: 1.0,
            },
            Term {
                feature: Feature::Keywords,
                coef: -0.5,
                mean: 4.0,
                std: 2.0,
            },
        ];
        let features = Features {
            shared: 4,
            keywords: 8,
            ..Features::default()
        };
        // 0.25 + 1 * (4 - 2) / 1 + -0.5 * (8 - 4) / 2
        assert!(near(linear(&features, 0.25, &terms), 1.25));
        assert!(near(linear(&features, 0.25, &[]), 0.25));
    }

    #[test]
    fn tier_of_should_put_a_score_at_a_threshold_in_the_higher_tier() {
        let (high, low) = (1.5, 0.5);
        assert_eq!(tier_of(high + 0.01, high, low), Tier::High);
        assert_eq!(tier_of(high, high, low), Tier::High);
        assert_eq!(tier_of(high - 0.01, high, low), Tier::Low);
        assert_eq!(tier_of(low, high, low), Tier::Low);
        assert_eq!(tier_of(low - 0.01, high, low), Tier::Off);
        assert_eq!(tier_of(f64::NEG_INFINITY, high, low), Tier::Off);
    }

    #[test]
    fn the_model_thresholds_should_leave_room_for_a_low_tier() {
        assert!(std::hint::black_box(gate_model::HIGH) > gate_model::LOW);
    }

    #[test]
    fn a_tier_should_name_itself_for_the_decision_log() {
        assert_eq!(Tier::High.as_str(), "high");
        assert_eq!(Tier::Low.as_str(), "low");
        assert_eq!(Tier::Off.as_str(), "off");
    }

    fn verdict(tier: Tier) -> Verdict {
        Verdict {
            tier,
            score: 0.0,
            best_file: None,
            features: Features {
                informative: 5,
                shared: 4,
                ..Features::default()
            },
        }
    }

    #[test]
    fn confidence_line_should_state_the_tier_and_nothing_for_off() {
        assert_eq!(
            verdict(Tier::High).confidence_line().as_deref(),
            Some("confidence: high — 4/5 key terms covered; start with the first file")
        );
        assert_eq!(
            verdict(Tier::Low).confidence_line().as_deref(),
            Some("confidence: low — possibly related; verify before relying on it")
        );
        assert_eq!(verdict(Tier::Off).confidence_line(), None);
    }

    #[test]
    fn judge_should_apply_the_model_to_the_features_and_name_the_heaviest_file() {
        let input = sample();
        let leads = [lead("src/daemon.rs", 0.8)];
        let gate = GateInput {
            relevance: &input,
            leads: &leads,
            typed: "does the daemon push?",
        };
        let verdict = judge(&gate);
        let features = extract(&gate);
        let score = linear(&features, gate_model::INTERCEPT, &gate_model::TERMS);
        assert_eq!(verdict.features, features);
        assert!(near(verdict.score, score));
        assert_eq!(
            verdict.tier,
            tier_of(score, gate_model::HIGH, gate_model::LOW)
        );
        assert_eq!(verdict.best_file.as_deref(), Some("src/daemon.rs"));
    }

    #[test]
    fn judge_should_name_no_file_for_a_prompt_no_file_holds() {
        let input = RelevanceInput {
            graph: true,
            keywords: vec![stat("weather", 3.0)],
            cofiles: Vec::new(),
        };
        let verdict = judge(&GateInput {
            relevance: &input,
            leads: &[],
            typed: "weather",
        });
        assert_eq!(verdict.best_file, None);
        assert_eq!(verdict.tier, Tier::Off);
    }
}
