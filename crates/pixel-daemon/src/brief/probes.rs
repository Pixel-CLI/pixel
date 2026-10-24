// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Bounded content probes: how widely a keyword occurs as a whole word in the
//! text index, with the first lines that matched. `relevance` counts
//! documents with them.

use std::collections::{BTreeMap, BTreeSet};

use pixel_index::indexset::IndexSet;

/// Matches one keyword's probe keeps at most.
const CONTENT_PROBE_LIMIT: usize = 1000;
/// Match lines kept per file, so a reader can check a match without
/// searching again.
const EVIDENCE_MAX_LINES_PER_TARGET: usize = 2;

/// One line that matched a content probe, kept so a reader can check the match
/// without searching again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeLine {
    pub line: u64,
    pub text: String,
    pub keyword: String,
}

/// What the bounded content probes of a keyword list found. `targets` ranks
/// and justifies files with it; `relevance` counts documents with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ContentProbes {
    /// Every keyword probed, with or without a match.
    pub probed: BTreeSet<String>,
    /// keyword → matches per file, path ascending. Only keywords with a match.
    pub hits: BTreeMap<String, Vec<(String, u32)>>,
    /// Keywords whose probe stopped at [`CONTENT_PROBE_LIMIT`]: their `hits`
    /// are the files of a path-ordered prefix, not all of them.
    pub truncated: BTreeSet<String>,
    /// path → the first lines that matched, at most
    /// [`EVIDENCE_MAX_LINES_PER_TARGET`], across keywords in probe order.
    pub lines: BTreeMap<String, Vec<ProbeLine>>,
}

impl ContentProbes {
    /// Fold the probes of other keywords in; each path keeps at most
    /// [`EVIDENCE_MAX_LINES_PER_TARGET`] lines, the earlier ones first.
    pub(crate) fn absorb(&mut self, other: ContentProbes) {
        self.probed.extend(other.probed);
        self.hits.extend(other.hits);
        self.truncated.extend(other.truncated);
        for (path, lines) in other.lines {
            let kept = self.lines.entry(path).or_default();
            let room = EVIDENCE_MAX_LINES_PER_TARGET.saturating_sub(kept.len());
            kept.extend(lines.into_iter().take(room));
        }
    }
}

/// The cap sentence for a probe that stopped at [`CONTENT_PROBE_LIMIT`].
pub(crate) fn probe_cap(keyword: &str) -> String {
    format!(
        "content probe truncated at {CONTENT_PROBE_LIMIT} matches for keyword \
         '{keyword}'; files beyond the cap carry no content signal"
    )
}

/// Run a bounded, word-bounded content probe per keyword over `index`.
///
/// Word-bounded so "auth" cannot count every "author" as signal. A probe the
/// index cannot run is skipped: it is not recorded in `probed`.
pub(crate) fn probe_content(index: &IndexSet, keywords: &[String]) -> ContentProbes {
    let mut probes = ContentProbes::default();
    for keyword in keywords {
        probes.probed.insert(keyword.clone());
    }
    if keywords.is_empty() {
        return probes;
    }
    // One alternation for every keyword. The old loop gave each keyword its
    // own index scan and regex compile: a six-word task paid six scans of the
    // same candidate set, and the four parallel probes of one brief paid that
    // again. One scan returns the union and each line is attributed to the
    // keyword it carries below. The retention cap grows with the keyword
    // count so each keyword can still keep `CONTENT_PROBE_LIMIT` matches.
    let alternation = keywords
        .iter()
        .map(|k| regex_escape_keyword(k))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!(r"(?i)\b({alternation})\b");
    let cap = CONTENT_PROBE_LIMIT.saturating_mul(keywords.len());
    let Ok((matches, stats)) = index.search_page_in(&pattern, 0, Some(cap), None) else {
        return probes;
    };
    let mut counts: BTreeMap<&str, BTreeMap<String, u32>> = BTreeMap::new();
    let mut retained: BTreeMap<&str, usize> = BTreeMap::new();
    // Keyword-major, so each keyword's evidence fills its first two lines per
    // path before the next keyword's does (the order the per-keyword scans
    // produced) even though one page now carries every keyword's matches.
    for keyword in keywords {
        let mut per_file: BTreeMap<String, u32> = BTreeMap::new();
        for m in &matches {
            let kept = retained.get(keyword.as_str()).copied().unwrap_or(0);
            if kept >= CONTENT_PROBE_LIMIT {
                break;
            }
            if !line_has_keyword(&m.line, keyword) {
                continue;
            }
            *per_file.entry(m.path.clone()).or_default() += 1;
            *retained.entry(keyword.as_str()).or_default() += 1;
            // keep the first ~2 match
            // lines per file so the caller can verify a content match without
            // re-searching. Near-zero cost: the lines are already fetched.
            let lines = probes.lines.entry(m.path.clone()).or_default();
            if lines.len() < EVIDENCE_MAX_LINES_PER_TARGET {
                lines.push(ProbeLine {
                    line: m.line_number,
                    text: m.line.clone(),
                    keyword: keyword.clone(),
                });
            }
        }
        if per_file.is_empty() {
            continue;
        }
        // A retained cap, or a page that stopped before the candidates ran
        // out, means this keyword may have matches the caller cannot see:
        // name it so the op reports a lower bound instead of completeness.
        // A page-wide stop flags every keyword that matched, which is
        // conservative where only one of them filled the page.
        if stats.truncated
            || retained.get(keyword.as_str()).copied().unwrap_or(0) >= CONTENT_PROBE_LIMIT
        {
            probes.truncated.insert(keyword.clone());
        }
        counts.insert(keyword, per_file);
    }
    for (keyword, per_file) in counts {
        probes
            .hits
            .insert(keyword.to_string(), per_file.into_iter().collect());
    }
    probes
}

/// Whether `line` contains `keyword` as a whole word, ASCII
/// case-insensitively — the attribution the combined probe cannot get from
/// the shared `MatchLine`. `[a-z0-9_]` keywords make ASCII folding exact for
/// the characters that can differ.
fn line_has_keyword(line: &str, keyword: &str) -> bool {
    let needle = keyword.as_bytes();
    let hay = line.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    (0..=hay.len() - needle.len()).any(|start| {
        hay[start..start + needle.len()].eq_ignore_ascii_case(needle)
            && line[..start].chars().next_back().is_none_or(|c| !word(c))
            && line[start + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !word(c))
    })
}

/// `kw` escaped for a regular expression, keeping non-ASCII characters.
fn regex_escape_keyword(kw: &str) -> String {
    let mut out = String::with_capacity(kw.len());
    for c in kw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if c.is_ascii() {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{open, repo};

    fn line(keyword: &str, number: u64, text: &str) -> ProbeLine {
        ProbeLine {
            line: number,
            text: text.to_owned(),
            keyword: keyword.to_owned(),
        }
    }

    #[test]
    fn probe_content_should_match_whole_words_and_keep_the_first_two_lines_per_path() {
        let (_dir, root) = repo(&[
            (
                "a.rs",
                "// alpha one\n// beta\n// alpha two\n// alpha three\n",
            ),
            ("b.rs", "// alphabet and alpha_beta, not the word\n"),
        ]);
        let (index, _) = open(&root);
        let probes = probe_content(
            &index,
            &["alpha".to_owned(), "beta".to_owned(), "zeta".to_owned()],
        );
        assert_eq!(
            probes.hits["alpha"],
            [("a.rs".to_owned(), 3)],
            "alphabet and alpha_beta are other words"
        );
        assert_eq!(probes.hits["beta"], [("a.rs".to_owned(), 1)]);
        assert!(!probes.hits.contains_key("zeta"));
        assert_eq!(
            probes.probed,
            BTreeSet::from(["alpha".to_owned(), "beta".to_owned(), "zeta".to_owned()]),
            "a probe without a match still ran"
        );
        assert!(probes.truncated.is_empty());
        assert_eq!(
            probes.lines["a.rs"],
            [
                line("alpha", 1, "// alpha one"),
                line("alpha", 3, "// alpha two")
            ],
            "two lines per path, in probe order"
        );
    }

    #[test]
    fn probe_content_should_stop_at_the_match_cap_and_say_so() {
        // 12 files x 100 lines = 1,200 matches, past CONTENT_PROBE_LIMIT.
        let files: Vec<(String, String)> = (0..12)
            .map(|file| {
                (
                    format!("f{file:02}.rs"),
                    (0..100).map(|n| format!("// needle {n}\n")).collect(),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect();
        let (_dir, root) = repo(&borrowed);
        let (index, _) = open(&root);
        let probes = probe_content(&index, &["needle".to_owned()]);
        assert_eq!(probes.truncated, BTreeSet::from(["needle".to_owned()]));
        let matched: u32 = probes.hits["needle"].iter().map(|(_, count)| count).sum();
        assert_eq!(matched, 1000, "exactly the cap, not one more");
        assert_eq!(
            probes.hits["needle"].len(),
            10,
            "a path-ordered prefix of the files"
        );
        assert_eq!(
            probe_cap("needle"),
            "content probe truncated at 1000 matches for keyword 'needle'; files beyond the cap carry no content signal"
        );
    }

    #[test]
    fn a_keyword_is_matched_as_a_word_whatever_its_case_and_punctuation() {
        assert!(line_has_keyword("Call the Daemon.", "daemon"));
        assert!(line_has_keyword("daemon", "daemon"));
        assert!(!line_has_keyword("daemons", "daemon"));
        assert!(!line_has_keyword("a_daemon", "daemon"));
        assert!(!line_has_keyword("", "daemon"));
        assert_eq!(regex_escape_keyword("c++"), "c\\+\\+");
        assert_eq!(regex_escape_keyword("naïve_x"), "naïve_x");
    }

    #[test]
    fn absorb_should_keep_two_lines_per_path_and_the_earlier_ones() {
        let mut held = ContentProbes::default();
        held.lines.insert(
            "a.rs".into(),
            vec![line("one", 1, "one"), line("one", 2, "one")],
        );
        let mut other = ContentProbes::default();
        other.probed.insert("two".into());
        other
            .lines
            .insert("a.rs".into(), vec![line("two", 9, "two")]);
        other.lines.insert(
            "b.rs".into(),
            vec![line("two", 1, "two"), line("two", 2, "x")],
        );
        held.absorb(other);
        assert_eq!(held.lines["a.rs"].len(), 2, "no room for a third");
        assert_eq!(held.lines["a.rs"][1].keyword, "one");
        assert_eq!(held.lines["b.rs"].len(), 2);
        assert!(held.probed.contains("two"));
    }
}
