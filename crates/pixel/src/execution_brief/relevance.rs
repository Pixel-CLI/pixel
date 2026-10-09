// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The relevance gate of a plain-language prompt: does the repository talk
//! about what the prompt talks about?
//!
//! The daemon counts, per task keyword, the files it occurs in
//! (`facts.relevance`); this module turns those counts into one decision with
//! no model in it. A keyword that occurs in few files carries weight, one that
//! occurs everywhere carries none, and a keyword the repository lacks weighs
//! the most: a prompt about a weather forecast is not covered by any file,
//! and the missing words say so. The prompt is on topic when one file holds
//! enough of that weight: at least two informative keywords, one of them a
//! symbol or file name rather than a bare text match, and at least
//! [`MIN_COVERAGE`] of the whole prompt's weight.
//!
//! The weights are the daemon's, never recomputed here
//! (`pixel_daemon::relevance::row_weight` per keyword, `CoFile::weight` per
//! file): the thresholds below sit on one side of the wire and the formula on
//! the other, so a change of weighting there moves the numbers and not the
//! rules.

/// Share of the prompt's total keyword weight one file must hold to cover
/// it. Tuned on the dev split of the brief-gate set (#883), never the test
/// split: from 0.3 to 0.5 the dev F1 does not move once a file needs
/// [`MIN_SHARED_KEYWORDS`] words, and above 0.5 it only loses recall.
pub(crate) const MIN_COVERAGE: f64 = 0.5;
/// Informative keywords one file must hold. With two, a git tool's own
/// vocabulary opened for 15 of 20 ops prompts and 58% of the off-topic dev
/// prompts; with four the dev F1 gives up 4 points (74.6 to 70.2) for 31
/// points less false positives (58 to 27%). Tuned on the dev split only.
pub(crate) const MIN_SHARED_KEYWORDS: usize = 4;
/// Share of the weight from which the brief says its confidence is high: on
/// the dev split the first file of a brief at or above it held an expected
/// file for 73% of the prompts, below it for 22%.
pub(crate) const HIGH_COVERAGE: f64 = 0.9;

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

/// What the scorer reads, independent of the wire type that carried it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelevanceInput {
    /// The code graph answered; without it no keyword can be structural.
    pub(crate) graph: bool,
    pub(crate) keywords: Vec<KeywordStat>,
    /// Best first.
    pub(crate) cofiles: Vec<CoFileStat>,
}

/// Why a decision came out as it did; logged next to every brief.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Features {
    /// Sum of every keyword's weight (Q).
    pub(crate) total_weight: f64,
    /// Weight the best file holds (S).
    pub(crate) covered_weight: f64,
    /// Task keywords.
    pub(crate) keywords: usize,
    /// Keywords with a weight above zero.
    pub(crate) informative: usize,
    /// Informative keywords the best file holds.
    pub(crate) shared: usize,
    /// The best file is structural.
    pub(crate) structural: bool,
}

/// The decision and the file that carried it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) on_topic: bool,
    /// The best file's share of the total weight, S / Q.
    pub(crate) score: f64,
    /// The file that decided: the best qualifying one when on topic, else the
    /// one holding the most weight.
    pub(crate) best_file: Option<String>,
    pub(crate) features: Features,
}

/// A keyword carries evidence when its weight is above zero.
fn is_informative(weight: f64) -> bool {
    weight > 0.0
}

/// A file shares enough informative keywords to be a topic.
fn shares_enough(shared: usize) -> bool {
    shared >= MIN_SHARED_KEYWORDS
}

/// A file holds enough of the prompt's weight to cover it.
fn covers(covered: f64, total: f64) -> bool {
    covered >= MIN_COVERAGE * total
}

/// A keyword living in a symbol or path name counts once the graph answered;
/// without a graph no file can show it, so the requirement is waived.
fn structural_ok(structural: bool, graph: bool) -> bool {
    structural || !graph
}

/// The keyword's weight: the daemon's, except that a French word the
/// repository lacks weighs nothing, because it names a thing the codebase
/// spells in English.
fn weight_of(stat: &KeywordStat) -> f64 {
    if stat.french_only { 0.0 } else { stat.weight }
}

/// The best file's share of the weight as a word: `high`, `medium` or `low`.
fn level(score: f64) -> &'static str {
    if score >= HIGH_COVERAGE {
        "high"
    } else if score >= MIN_COVERAGE {
        "medium"
    } else {
        "low"
    }
}

impl Verdict {
    /// The `confidence:` line of a brief built on this decision. A prompt
    /// the repository does not cover still gets the line when the brief was
    /// asked for anyway, and says to verify.
    pub(crate) fn confidence_line(&self) -> String {
        let features = &self.features;
        let terms = format!(
            "{}/{} key terms covered",
            features.shared, features.informative
        );
        let advice = if self.on_topic {
            "start with the first file"
        } else {
            "verify with rg before relying on these files"
        };
        format!("confidence: {} — {terms}; {advice}", level(self.score))
    }
}

/// One co-file measured against the prompt.
struct Candidate<'a> {
    path: &'a str,
    qualifies: bool,
    covered: f64,
    shared: usize,
    structural: bool,
}

/// Decide whether the prompt behind `input` is about this repository.
pub(crate) fn judge(input: &RelevanceInput) -> Verdict {
    let weights: Vec<(&str, f64)> = input
        .keywords
        .iter()
        .map(|stat| (stat.keyword.as_str(), weight_of(stat)))
        .collect();
    let total: f64 = weights.iter().map(|(_, weight)| weight).sum();
    let informative = weights
        .iter()
        .filter(|(_, weight)| is_informative(*weight))
        .count();
    let mut best: Option<Candidate> = None;
    for cofile in &input.cofiles {
        let shared = weights
            .iter()
            .filter(|(keyword, weight)| {
                is_informative(*weight) && cofile.keywords.iter().any(|held| held == keyword)
            })
            .count();
        let covered = cofile.weight;
        // Two informative keywords already mean a total above zero, so a
        // prompt with no weight at all qualifies no file.
        let qualifies = shares_enough(shared)
            && structural_ok(cofile.structural, input.graph)
            && covers(covered, total);
        // A qualifying file beats any other; among equals the heavier wins
        // and a tie keeps the file the daemon ranked first.
        let better = best
            .as_ref()
            .is_none_or(|held| (qualifies, covered) > (held.qualifies, held.covered));
        if better {
            best = Some(Candidate {
                path: &cofile.path,
                qualifies,
                covered,
                shared,
                structural: cofile.structural,
            });
        }
    }
    let covered_weight = best.as_ref().map_or(0.0, |held| held.covered);
    Verdict {
        on_topic: best.as_ref().is_some_and(|held| held.qualifies),
        // A file may weigh more than the prompt (a boost for structure):
        // the share is a share, at most the whole.
        score: if total > 0.0 {
            (covered_weight / total).min(1.0)
        } else {
            0.0
        },
        best_file: best.as_ref().map(|held| held.path.to_string()),
        features: Features {
            total_weight: total,
            covered_weight,
            keywords: input.keywords.len(),
            informative,
            shared: best.as_ref().map_or(0, |held| held.shared),
            structural: best.as_ref().is_some_and(|held| held.structural),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words every fixture is made of: `w0`, `w1`, ...
    fn word(index: usize) -> String {
        format!("w{index}")
    }

    /// A prompt of `keywords` (word, weight) and the files its words meet in
    /// (path, words, structural); a file weighs what the daemon says it does:
    /// the sum of the weights of its words.
    fn input(keywords: &[(String, f64)], cofiles: &[(&str, Vec<String>, bool)]) -> RelevanceInput {
        RelevanceInput {
            graph: true,
            keywords: keywords
                .iter()
                .map(|(word, weight)| KeywordStat {
                    keyword: word.clone(),
                    weight: *weight,
                    french_only: false,
                })
                .collect(),
            cofiles: cofiles
                .iter()
                .map(|(path, words, structural)| CoFileStat {
                    path: (*path).into(),
                    keywords: words.clone(),
                    weight: keywords
                        .iter()
                        .filter(|(word, _)| words.contains(word))
                        .map(|(_, weight)| weight)
                        .sum(),
                    structural: *structural,
                })
                .collect(),
        }
    }

    /// `count` words of weight `weight`, numbered from `from`.
    fn words(from: usize, count: usize, weight: f64) -> Vec<(String, f64)> {
        (from..from + count).map(|n| (word(n), weight)).collect()
    }

    /// The names of the words `from..from + count`.
    fn names(from: usize, count: usize) -> Vec<String> {
        (from..from + count).map(word).collect()
    }

    /// Every word of the shared minimum, at weight 2.
    const K: usize = MIN_SHARED_KEYWORDS;

    #[test]
    fn shares_enough_should_need_the_minimum_of_informative_keywords() {
        assert!(!shares_enough(MIN_SHARED_KEYWORDS - 1));
        assert!(shares_enough(MIN_SHARED_KEYWORDS));
        assert!(shares_enough(MIN_SHARED_KEYWORDS + 1));
        assert_eq!(MIN_SHARED_KEYWORDS, 4);
    }

    #[test]
    fn covers_should_accept_a_share_at_the_threshold() {
        // 1 of 2 is exactly MIN_COVERAGE: covered, not "more than".
        assert!(!covers(0.99, 2.0));
        assert!(covers(1.0, 2.0));
        assert!(covers(1.01, 2.0));
        assert!(
            covers(0.0, 0.0),
            "an empty prompt is not covered by this fn"
        );
    }

    #[test]
    fn structural_ok_should_waive_the_requirement_without_a_graph() {
        assert!(structural_ok(true, true));
        assert!(!structural_ok(false, true));
        assert!(structural_ok(false, false));
        assert!(structural_ok(true, false));
    }

    #[test]
    fn is_informative_should_need_a_weight_above_zero() {
        assert!(!is_informative(0.0));
        assert!(is_informative(f64::MIN_POSITIVE));
        assert!(!is_informative(-1.0));
    }

    #[test]
    fn level_should_name_the_share_with_both_edges_inclusive() {
        assert_eq!(level(HIGH_COVERAGE), "high");
        assert_eq!(level(HIGH_COVERAGE - 0.01), "medium");
        assert_eq!(level(MIN_COVERAGE), "medium");
        assert_eq!(level(MIN_COVERAGE - 0.01), "low");
        assert_eq!(level(1.0), "high");
        assert_eq!(level(0.0), "low");
    }

    #[test]
    fn weight_of_should_give_an_untranslated_french_word_no_weight() {
        let word = |weight: f64, french_only: bool| KeywordStat {
            keyword: "mot".into(),
            weight,
            french_only,
        };
        assert!(weight_of(&word(4.0, true)).abs() < f64::EPSILON);
        assert!((weight_of(&word(4.0, false)) - 4.0).abs() < f64::EPSILON);
        assert!((weight_of(&word(2.0, false)) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_take_a_prompt_one_file_covers_as_on_topic() {
        let verdict = judge(&input(
            &words(0, K, 2.0),
            &[("daemon.rs", names(0, K), true)],
        ));
        assert!(verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("daemon.rs"));
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
        let total = 2.0 * K as f64;
        assert_eq!(
            verdict.features,
            Features {
                total_weight: total,
                covered_weight: total,
                keywords: K,
                informative: K,
                shared: K,
                structural: true,
            }
        );
    }

    #[test]
    fn judge_should_cover_a_prompt_at_exactly_the_minimum_share() {
        // The file holds K words of weight 2; the prompt has K more of them:
        // exactly half of the weight, and half covers.
        let at = judge(&input(
            &words(0, 2 * K, 2.0),
            &[("f.rs", names(0, K), true)],
        ));
        assert!(at.on_topic, "exactly half of the weight is covered");
        assert!((at.score - 0.5).abs() < f64::EPSILON);
        // One more word the file lacks and the share is just under half.
        let below = judge(&input(
            &words(0, 2 * K + 1, 2.0),
            &[("f.rs", names(0, K), true)],
        ));
        assert!(!below.on_topic);
        assert!(below.score < MIN_COVERAGE, "{}", below.score);
    }

    #[test]
    fn judge_should_reject_a_prompt_the_repository_does_not_talk_about() {
        // Two words found nowhere, one in a lot of files: the weather.
        let verdict = judge(&input(
            &[
                ("weather".into(), 3.0),
                ("tomorrow".into(), 3.0),
                ("like".into(), 0.0),
            ],
            &[],
        ));
        assert!(!verdict.on_topic);
        assert_eq!(verdict.best_file, None);
        assert!(verdict.score.abs() < f64::EPSILON);
        assert!((verdict.features.total_weight - 6.0).abs() < f64::EPSILON);
        assert_eq!(verdict.features.informative, 2);
    }

    #[test]
    fn judge_should_reject_a_prompt_with_no_weight_at_all() {
        // Every word is everywhere (or French and absent): Q == 0, even
        // with a file that holds all of them.
        let verdict = judge(&input(&words(0, K, 0.0), &[("f.rs", names(0, K), true)]));
        assert!(!verdict.on_topic);
        assert!(verdict.features.total_weight.abs() < f64::EPSILON);
        assert!(verdict.score.abs() < f64::EPSILON);
        let mut french = input(&[("fichier".into(), 4.0)], &[]);
        french.keywords[0].french_only = true;
        let only_french = judge(&french);
        assert!(!only_french.on_topic);
        assert!(only_french.features.total_weight.abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_not_count_a_ubiquitous_keyword_toward_the_words_a_file_needs() {
        // The file holds K - 1 informative words and a ubiquitous one.
        let mut short = words(0, K - 1, 2.0);
        short.push((word(K - 1), 0.0));
        let verdict = judge(&input(&short, &[("f.rs", names(0, K), true)]));
        assert!(!verdict.on_topic);
        assert_eq!(verdict.features.shared, K - 1);
        // The same file with one more informative word qualifies.
        let mut enough = short.clone();
        enough.push((word(K), 2.0));
        let two = judge(&input(&enough, &[("f.rs", names(0, K + 1), true)]));
        assert!(two.on_topic);
        assert_eq!(two.features.shared, K);
    }

    #[test]
    fn judge_should_refuse_a_file_one_word_short_of_the_minimum() {
        let verdict = judge(&input(
            &words(0, K - 1, 2.0),
            &[("f.rs", names(0, K - 1), true)],
        ));
        assert!(
            !verdict.on_topic,
            "{K} words are a topic, fewer a coincidence"
        );
        assert_eq!(verdict.best_file.as_deref(), Some("f.rs"));
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_need_a_structural_file_only_when_the_graph_answered() {
        let keywords = words(0, K, 2.0);
        let flat = [("notes.md", names(0, K), false)];
        let with_graph = judge(&input(&keywords, &flat));
        assert!(
            !with_graph.on_topic,
            "text-only matches never open the gate"
        );
        assert!(!with_graph.features.structural);
        let mut without = input(&keywords, &flat);
        without.graph = false;
        assert!(judge(&without).on_topic);
    }

    #[test]
    fn judge_should_prefer_the_qualifying_file_over_one_holding_more_weight() {
        // `a.md` holds more weight but is not structural: `b.rs` decides.
        let verdict = judge(&input(
            &words(0, K + 1, 2.0),
            &[
                ("a.md", names(0, K + 1), false),
                ("b.rs", names(0, K), true),
            ],
        ));
        assert!(verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("b.rs"));
        assert!((verdict.score - K as f64 / (K as f64 + 1.0)).abs() < 1e-9);
        assert!(verdict.features.structural);
    }

    #[test]
    fn judge_should_keep_the_first_of_two_equal_files() {
        let verdict = judge(&input(
            &words(0, K, 2.0),
            &[
                ("first.rs", names(0, K), true),
                ("second.rs", names(0, K), true),
            ],
        ));
        assert_eq!(verdict.best_file.as_deref(), Some("first.rs"));
    }

    #[test]
    fn judge_should_name_the_heaviest_file_when_none_qualifies() {
        let mut keywords = words(0, K, 2.0);
        keywords.extend(words(K, 2, 3.0));
        let verdict = judge(&input(
            &keywords,
            &[
                ("small.rs", names(0, 1), true),
                ("heavy.rs", names(0, K - 1), true),
            ],
        ));
        assert!(!verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("heavy.rs"));
        assert!((verdict.features.covered_weight - 2.0 * (K - 1) as f64).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_count_a_keyword_a_file_lists_twice_once() {
        let verdict = judge(&input(
            &words(0, K, 2.0),
            &[("f.rs", vec![word(0), word(0), word(0)], true)],
        ));
        assert_eq!(verdict.features.shared, 1);
    }

    #[test]
    fn judge_should_weigh_a_missing_french_word_as_nothing_not_as_off_topic() {
        // "modifie" is French and absent: it neither lifts Q nor stops a file
        // that holds the other words from covering the prompt.
        let mut keywords = words(0, K, 2.0);
        keywords.push(("modifie".into(), 3.0));
        let held = [("envfile.rs", names(0, K), true)];
        let mut french = input(&keywords, &held);
        french.keywords[K].french_only = true;
        let verdict = judge(&french);
        assert!(verdict.on_topic);
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
        assert_eq!(verdict.features.informative, K);
        // The same word, English, would have weighed 3 and lowered the share.
        let english = judge(&input(&keywords, &held));
        assert!(english.on_topic);
        assert!((english.score - 2.0 * K as f64 / (2.0 * K as f64 + 3.0)).abs() < 1e-9);
    }

    #[test]
    fn judge_should_take_the_file_weight_from_the_daemon_and_not_recompute_it() {
        let total = 2.0 * K as f64;
        let mut prompt = input(&words(0, K, 2.0), &[("f.rs", names(0, K), true)]);
        prompt.cofiles[0].weight = 0.75 * total;
        let verdict = judge(&prompt);
        assert!((verdict.features.covered_weight - 0.75 * total).abs() < 1e-9);
        assert!((verdict.score - 0.75).abs() < 1e-9);
        // A file the daemon weighs above the whole prompt scores the whole.
        prompt.cofiles[0].weight = 1.5 * total;
        let boosted = judge(&prompt);
        assert!((boosted.features.covered_weight - 1.5 * total).abs() < 1e-9);
        assert!((boosted.score - 1.0).abs() < f64::EPSILON);
        // And a file the daemon weighs too low no longer covers the prompt.
        prompt.cofiles[0].weight = 0.49 * total;
        assert!(!judge(&prompt).on_topic);
    }

    #[test]
    fn confidence_line_should_say_how_much_of_the_prompt_the_file_covers() {
        // K words held, one more the file lacks, weight 3: medium.
        let mut keywords = words(0, K, 2.0);
        keywords.push((word(K), 3.0));
        let medium = judge(&input(&keywords, &[("f.rs", names(0, K), true)]));
        assert!(medium.on_topic);
        assert_eq!(
            medium.confidence_line(),
            format!(
                "confidence: medium — {K}/{} key terms covered; start with the first file",
                K + 1
            )
        );
        let whole = judge(&input(&words(0, K, 2.0), &[("f.rs", names(0, K), true)]));
        assert_eq!(
            whole.confidence_line(),
            format!("confidence: high — {K}/{K} key terms covered; start with the first file")
        );
        let off = judge(&input(
            &[(word(0), 2.0), (word(1), 3.0), (word(2), 3.0)],
            &[("f.rs", names(0, 1), true)],
        ));
        assert!(!off.on_topic);
        assert_eq!(
            off.confidence_line(),
            "confidence: low — 1/3 key terms covered; verify with rg before relying on these files"
        );
    }
}
