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
//! The weight of one keyword is the daemon's single spelling
//! (`pixel_daemon::relevance::keyword_weight`, lands with #885): the scorer
//! takes it as a [`WeightFn`] so the thresholds here and the formula there
//! cannot drift, and a test can drive the scorer with a plain table.

/// The weight of one keyword: the files it occurs in, the files considered
/// and whether the content probe stopped at its cap. `0.0` for a keyword
/// that occurs everywhere, more for a rarer one.
pub(crate) type WeightFn = fn(usize, usize, bool) -> f64;

/// Share of the prompt's total keyword weight one file must hold to cover
/// it. Tuned on the dev split of the brief-gate set (#883), never the test
/// split.
pub(crate) const MIN_COVERAGE: f64 = 0.5;
/// Informative keywords one file must hold: a single shared word is a
/// coincidence, two are a topic.
pub(crate) const MIN_SHARED_KEYWORDS: usize = 2;
/// Share of the weight from which the brief says its confidence is high.
pub(crate) const HIGH_COVERAGE: f64 = 0.8;

/// How widely one task keyword occurs, as the daemon counted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeywordStat {
    pub(crate) keyword: String,
    /// Files with a word-bounded content match.
    pub(crate) df: usize,
    /// The content probe stopped at its cap, so `df` undercounts.
    pub(crate) truncated: bool,
    /// The word is French and the repository holds none of its translations:
    /// untranslated French is not evidence of off-topic.
    pub(crate) french_only: bool,
}

/// A file that several keywords meet in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoFileStat {
    pub(crate) path: String,
    /// The task keywords found in the file.
    pub(crate) keywords: Vec<String>,
    /// At least one keyword is a symbol name or a path word of the file, not
    /// only text inside it.
    pub(crate) structural: bool,
}

/// What the scorer reads, independent of the wire type that carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelevanceInput {
    /// Indexed files the counts are drawn from.
    pub(crate) files_considered: usize,
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
fn weight_of(stat: &KeywordStat, files_considered: usize, weight: WeightFn) -> f64 {
    if stat.df == 0 && stat.french_only {
        0.0
    } else {
        weight(stat.df, files_considered, stat.truncated)
    }
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
pub(crate) fn judge(input: &RelevanceInput, weight: WeightFn) -> Verdict {
    let weights: Vec<(&str, f64)> = input
        .keywords
        .iter()
        .map(|stat| {
            (
                stat.keyword.as_str(),
                weight_of(stat, input.files_considered, weight),
            )
        })
        .collect();
    let total: f64 = weights.iter().map(|(_, weight)| weight).sum();
    let informative = weights
        .iter()
        .filter(|(_, weight)| is_informative(*weight))
        .count();
    let mut best: Option<Candidate> = None;
    for cofile in &input.cofiles {
        let mut covered = 0.0;
        let mut shared = 0;
        for (keyword, weight) in &weights {
            if is_informative(*weight) && cofile.keywords.iter().any(|held| held == keyword) {
                covered += weight;
                shared += 1;
            }
        }
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
        score: if total > 0.0 {
            covered_weight / total
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

    /// A table, not the daemon's formula: a keyword in ten files or more is
    /// everywhere, one the repository lacks weighs 3, any other 2. Round
    /// numbers keep every share exact.
    fn table(df: usize, _files: usize, truncated: bool) -> f64 {
        if truncated || df >= 10 {
            0.0
        } else if df == 0 {
            3.0
        } else {
            2.0
        }
    }

    fn stat(keyword: &str, df: usize) -> KeywordStat {
        KeywordStat {
            keyword: keyword.into(),
            df,
            truncated: false,
            french_only: false,
        }
    }

    fn cofile(path: &str, keywords: &[&str], structural: bool) -> CoFileStat {
        CoFileStat {
            path: path.into(),
            keywords: keywords.iter().map(ToString::to_string).collect(),
            structural,
        }
    }

    fn input(keywords: Vec<KeywordStat>, cofiles: Vec<CoFileStat>) -> RelevanceInput {
        RelevanceInput {
            files_considered: 100,
            graph: true,
            keywords,
            cofiles,
        }
    }

    #[test]
    fn shares_enough_should_need_two_informative_keywords() {
        assert!(!shares_enough(MIN_SHARED_KEYWORDS - 1));
        assert!(shares_enough(MIN_SHARED_KEYWORDS));
        assert!(shares_enough(MIN_SHARED_KEYWORDS + 1));
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
        let word = |df: usize, french_only: bool| KeywordStat {
            keyword: "mot".into(),
            df,
            truncated: false,
            french_only,
        };
        // Absent and French: no evidence either way.
        assert!(weight_of(&word(0, true), 100, table).abs() < f64::EPSILON);
        // Absent and English: the repository lacks the word, the most weight.
        assert!((weight_of(&word(0, false), 100, table) - 3.0).abs() < f64::EPSILON);
        // French but present (through a synonym): weighs as any present word.
        assert!((weight_of(&word(4, true), 100, table) - 2.0).abs() < f64::EPSILON);
        assert!((weight_of(&word(4, false), 100, table) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn weight_of_should_pass_the_counts_to_the_weight_function() {
        fn probe(df: usize, files: usize, truncated: bool) -> f64 {
            // Encodes all three arguments so a swapped one changes the value.
            df as f64 * 1000.0 + files as f64 + if truncated { 0.5 } else { 0.0 }
        }
        let truncated = KeywordStat {
            keyword: "k".into(),
            df: 7,
            truncated: true,
            french_only: false,
        };
        assert!((weight_of(&truncated, 42, probe) - 7042.5).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_take_a_prompt_one_file_covers_as_on_topic() {
        let verdict = judge(
            &input(
                vec![stat("watch", 3), stat("daemon", 4), stat("startup", 2)],
                vec![cofile("daemon.rs", &["watch", "daemon", "startup"], true)],
            ),
            table,
        );
        assert!(verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("daemon.rs"));
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            verdict.features,
            Features {
                total_weight: 6.0,
                covered_weight: 6.0,
                keywords: 3,
                informative: 3,
                shared: 3,
                structural: true,
            }
        );
    }

    #[test]
    fn judge_should_cover_a_prompt_at_exactly_the_minimum_share() {
        // Weights 2 + 2 held of 2 + 2 + 3 + 3 = 10: a share of 0.4, below.
        let keywords = |missing: usize| {
            let mut all = vec![stat("a", 1), stat("b", 1)];
            all.extend((0..missing).map(|n| stat(&format!("gone{n}"), 0)));
            all
        };
        let held = vec![cofile("f.rs", &["a", "b"], true)];
        let below = judge(&input(keywords(2), held.clone()), table);
        assert!(!below.on_topic);
        assert!((below.score - 0.4).abs() < 1e-9, "{}", below.score);
        // The equality edge: a and b held (4) of a, b, c and d (8) is exactly
        // half, and half covers.
        let at = judge(
            &input(
                vec![stat("a", 1), stat("b", 1), stat("c", 1), stat("d", 1)],
                vec![cofile("f.rs", &["a", "b"], true)],
            ),
            table,
        );
        assert!(at.on_topic, "exactly half of the weight is covered");
        assert!((at.score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_reject_a_prompt_the_repository_does_not_talk_about() {
        // Two words found nowhere, one in a lot of files: the weather.
        let verdict = judge(
            &input(
                vec![stat("weather", 0), stat("tomorrow", 0), stat("like", 50)],
                vec![],
            ),
            table,
        );
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
        let verdict = judge(
            &input(
                vec![stat("file", 90), stat("line", 80)],
                vec![cofile("f.rs", &["file", "line"], true)],
            ),
            table,
        );
        assert!(!verdict.on_topic);
        assert!(verdict.features.total_weight.abs() < f64::EPSILON);
        assert!(verdict.score.abs() < f64::EPSILON);
        let french = KeywordStat {
            french_only: true,
            ..stat("fichier", 0)
        };
        let only_french = judge(&input(vec![french], vec![]), table);
        assert!(!only_french.on_topic);
        assert!(only_french.features.total_weight.abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_not_count_a_ubiquitous_keyword_toward_the_two_a_file_needs() {
        // `file` is everywhere: the file holds it and one informative word.
        let verdict = judge(
            &input(
                vec![stat("watch", 2), stat("file", 90)],
                vec![cofile("f.rs", &["watch", "file"], true)],
            ),
            table,
        );
        assert!(!verdict.on_topic);
        assert_eq!(verdict.features.shared, 1);
        // The same file with a second informative word qualifies.
        let two = judge(
            &input(
                vec![stat("watch", 2), stat("file", 90), stat("daemon", 2)],
                vec![cofile("f.rs", &["watch", "file", "daemon"], true)],
            ),
            table,
        );
        assert!(two.on_topic);
        assert_eq!(two.features.shared, 2);
    }

    #[test]
    fn judge_should_refuse_a_single_shared_keyword() {
        let verdict = judge(
            &input(
                vec![stat("watch", 2)],
                vec![cofile("f.rs", &["watch"], true)],
            ),
            table,
        );
        assert!(!verdict.on_topic, "one keyword is a coincidence");
        assert_eq!(verdict.best_file.as_deref(), Some("f.rs"));
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_need_a_structural_file_only_when_the_graph_answered() {
        let keywords = vec![stat("watch", 2), stat("daemon", 2)];
        let flat = vec![cofile("notes.md", &["watch", "daemon"], false)];
        let with_graph = judge(&input(keywords.clone(), flat.clone()), table);
        assert!(
            !with_graph.on_topic,
            "text-only matches never open the gate"
        );
        assert!(!with_graph.features.structural);
        let mut without = input(keywords, flat);
        without.graph = false;
        assert!(judge(&without, table).on_topic);
    }

    #[test]
    fn judge_should_prefer_the_qualifying_file_over_one_holding_more_weight() {
        // `a.md` holds more weight but is not structural: `b.rs` decides.
        let verdict = judge(
            &input(
                vec![stat("a", 1), stat("b", 1), stat("c", 1)],
                vec![
                    cofile("a.md", &["a", "b", "c"], false),
                    cofile("b.rs", &["a", "b"], true),
                ],
            ),
            table,
        );
        assert!(verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("b.rs"));
        assert!((verdict.score - 4.0 / 6.0).abs() < 1e-9);
        assert!(verdict.features.structural);
    }

    #[test]
    fn judge_should_keep_the_first_of_two_equal_files() {
        let verdict = judge(
            &input(
                vec![stat("a", 1), stat("b", 1)],
                vec![
                    cofile("first.rs", &["a", "b"], true),
                    cofile("second.rs", &["a", "b"], true),
                ],
            ),
            table,
        );
        assert_eq!(verdict.best_file.as_deref(), Some("first.rs"));
    }

    #[test]
    fn judge_should_name_the_heaviest_file_when_none_qualifies() {
        let verdict = judge(
            &input(
                vec![stat("a", 1), stat("b", 1), stat("c", 0), stat("d", 0)],
                vec![
                    cofile("small.rs", &["a"], true),
                    cofile("heavy.rs", &["a", "b"], true),
                ],
            ),
            table,
        );
        assert!(!verdict.on_topic);
        assert_eq!(verdict.best_file.as_deref(), Some("heavy.rs"));
        assert!((verdict.features.covered_weight - 4.0).abs() < f64::EPSILON);
        assert!((verdict.score - 0.4).abs() < 1e-9);
    }

    #[test]
    fn judge_should_count_a_keyword_a_file_lists_twice_once() {
        let verdict = judge(
            &input(
                vec![stat("a", 1), stat("b", 1)],
                vec![cofile("f.rs", &["a", "a", "a"], true)],
            ),
            table,
        );
        assert_eq!(verdict.features.shared, 1);
        assert!((verdict.features.covered_weight - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn judge_should_weigh_a_missing_french_word_as_nothing_not_as_off_topic() {
        // "comment" is English-absent French: it neither lifts Q nor stops a
        // file that holds the two English words from covering the prompt.
        let french = KeywordStat {
            french_only: true,
            ..stat("modifie", 0)
        };
        let verdict = judge(
            &input(
                vec![stat("env", 2), stat("cle", 2), french],
                vec![cofile("envfile.rs", &["env", "cle"], true)],
            ),
            table,
        );
        assert!(verdict.on_topic);
        assert!((verdict.score - 1.0).abs() < f64::EPSILON);
        assert_eq!(verdict.features.informative, 2);
        // The same word, English, would have weighed 3 and sunk the share.
        let english = judge(
            &input(
                vec![stat("env", 2), stat("cle", 2), stat("modifie", 0)],
                vec![cofile("envfile.rs", &["env", "cle"], true)],
            ),
            table,
        );
        assert!(english.on_topic, "4 of 7 still covers");
        assert!((english.score - 4.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn judge_should_hand_the_file_count_to_the_weight_function() {
        fn crowded(_df: usize, files: usize, _truncated: bool) -> f64 {
            if files > 50 { 0.0 } else { 1.0 }
        }
        let mut small = input(
            vec![stat("a", 1), stat("b", 1)],
            vec![cofile("f.rs", &["a", "b"], true)],
        );
        small.files_considered = 50;
        assert!(judge(&small, crowded).on_topic);
        small.files_considered = 51;
        assert!(!judge(&small, crowded).on_topic);
    }

    #[test]
    fn confidence_line_should_say_how_much_of_the_prompt_the_file_covers() {
        let on_topic = judge(
            &input(
                vec![stat("a", 1), stat("b", 1), stat("c", 1), stat("d", 0)],
                vec![cofile("f.rs", &["a", "b", "c"], true)],
            ),
            table,
        );
        assert!(on_topic.on_topic);
        // 6 of 9: medium. Three of the four informative terms are held.
        assert_eq!(
            on_topic.confidence_line(),
            "confidence: medium — 3/4 key terms covered; start with the first file"
        );
        let whole = judge(
            &input(
                vec![stat("a", 1), stat("b", 1)],
                vec![cofile("f.rs", &["a", "b"], true)],
            ),
            table,
        );
        assert_eq!(
            whole.confidence_line(),
            "confidence: high — 2/2 key terms covered; start with the first file"
        );
        let off = judge(
            &input(
                vec![stat("a", 1), stat("b", 0), stat("c", 0)],
                vec![cofile("f.rs", &["a"], true)],
            ),
            table,
        );
        assert!(!off.on_topic);
        assert_eq!(
            off.confidence_line(),
            "confidence: low — 1/3 key terms covered; verify with rg before relying on these files"
        );
    }
}
