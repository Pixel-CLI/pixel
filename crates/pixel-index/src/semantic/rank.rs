// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Ranking: a lexical BM25 over chunk terms, reciprocal-rank fusion of two
//! rankings, and the path classes that keep tests, config and docs below code
//! unless the question names them.

use std::collections::HashMap;

/// Reciprocal-rank fusion constant: a document at rank `r` of a list adds
/// `1 / (RRF_K + r)`.
pub const RRF_K: f64 = 60.0;

/// BM25 term-frequency saturation.
const K1: f64 = 1.2;
/// BM25 length normalisation.
const B: f64 = 0.75;

/// An inverted index over a fixed list of documents.
#[derive(Debug, Default)]
pub struct Bm25 {
    postings: HashMap<String, Vec<(u32, u32)>>,
    lengths: Vec<u32>,
    average: f64,
}

impl Bm25 {
    /// Indexes `texts`; document `i` is the `i`-th text.
    pub fn build<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut lengths = Vec::new();
        for (doc, text) in texts.into_iter().enumerate() {
            let terms = pixel_model::terms(text);
            lengths.push(terms.len() as u32);
            let mut counts: HashMap<&str, u32> = HashMap::new();
            for term in &terms {
                *counts.entry(term.as_str()).or_default() += 1;
            }
            for (term, count) in counts {
                postings
                    .entry(term.to_owned())
                    .or_default()
                    .push((doc as u32, count));
            }
        }
        let total: u64 = lengths.iter().map(|l| u64::from(*l)).sum();
        let average = if lengths.is_empty() {
            0.0
        } else {
            total as f64 / lengths.len() as f64
        };
        Self {
            postings,
            lengths,
            average,
        }
    }

    /// How many documents are indexed.
    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// The documents that match any term of `query`, best first, at most
    /// `limit`, ties broken by document number.
    pub fn top(&self, query: &str, limit: usize) -> Vec<(usize, f64)> {
        let n = self.lengths.len() as f64;
        let mut scores: HashMap<u32, f64> = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for term in pixel_model::terms(query) {
            if !seen.insert(term.clone()) {
                continue;
            }
            let Some(list) = self.postings.get(&term) else {
                continue;
            };
            let df = list.len() as f64;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (doc, tf) in list {
                let tf = f64::from(*tf);
                let len = f64::from(self.lengths[*doc as usize]);
                let norm = 1.0 - B + B * len / self.average.max(1.0);
                *scores.entry(*doc).or_default() += idf * tf * (K1 + 1.0) / (tf + K1 * norm);
            }
        }
        let mut ranked: Vec<(usize, f64)> =
            scores.into_iter().map(|(d, s)| (d as usize, s)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);
        ranked
    }
}

/// Fuses ranked lists of document numbers (best first) into one score per
/// document, sorted best first, ties broken by document number.
pub fn rrf(lists: &[Vec<usize>]) -> Vec<(usize, f64)> {
    let mut score: HashMap<usize, f64> = HashMap::new();
    for list in lists {
        for (rank, doc) in list.iter().enumerate() {
            *score.entry(*doc).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(usize, f64)> = score.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    fused
}

/// What kind of file a path is, for ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Code,
    Test,
    Config,
    Doc,
}

impl PathClass {
    /// Classifies by path alone.
    pub fn of(path: &str) -> Self {
        if pixel_model::is_test_path(path) {
            return Self::Test;
        }
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
        match ext {
            "md" | "mdx" | "rst" | "txt" | "adoc" => Self::Doc,
            "json" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "lock" | "xml" => {
                Self::Config
            }
            _ => Self::Code,
        }
    }

    /// The words of a question that ask for this class of file.
    fn asked_by(self) -> &'static [&'static str] {
        match self {
            Self::Code => &[],
            Self::Test => &["test", "tests", "spec", "specs", "fixture"],
            Self::Config => &[
                "config",
                "configuration",
                "setting",
                "settings",
                "yaml",
                "toml",
                "json",
                "env",
            ],
            Self::Doc => &[
                "doc",
                "docs",
                "documentation",
                "readme",
                "guide",
                "changelog",
            ],
        }
    }

    /// True when `question` asks for this class by name.
    pub fn named_in(self, question: &str) -> bool {
        let words = self.asked_by();
        question
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| words.iter().any(|a| w.eq_ignore_ascii_case(a)))
    }
}

/// The factor a class's score takes unless the question names it.
pub const OFF_CLASS_FACTOR: f64 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_should_rank_the_document_with_the_rarer_matching_term_first() {
        let docs = [
            "retry limit config",
            "retry retry retry loop",
            "unrelated sidebar animation",
            "limit",
        ];
        let index = Bm25::build(docs);
        assert_eq!(index.len(), 4);
        let top = index.top("retry limit", 10);
        assert_eq!(top[0].0, 0, "both terms win: {top:?}");
        assert!(top.iter().all(|(d, _)| *d != 2), "no term, no score");
        assert_eq!(index.top("sidebar", 10).len(), 1);
        assert!(index.top("zzz", 10).is_empty());
        assert_eq!(index.top("retry limit", 1).len(), 1);
    }

    #[test]
    fn bm25_should_score_a_repeated_query_term_once() {
        let index = Bm25::build(["alpha beta", "alpha"]);
        let once = index.top("alpha", 5);
        let twice = index.top("alpha alpha", 5);
        assert_eq!(once, twice);
        assert!(Bm25::default().top("alpha", 5).is_empty());
    }

    #[test]
    fn rrf_should_favour_a_document_both_lists_rank_high() {
        let fused = rrf(&[vec![1, 2, 3], vec![3, 1, 4]]);
        let order: Vec<usize> = fused.iter().map(|(d, _)| *d).collect();
        assert_eq!(order, [1, 3, 2, 4]);
        let first = fused[0].1;
        assert!((first - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        assert!(rrf(&[]).is_empty());
    }

    #[test]
    fn rrf_should_break_a_tie_by_document_number() {
        let fused = rrf(&[vec![7], vec![5]]);
        assert_eq!(fused.iter().map(|(d, _)| *d).collect::<Vec<_>>(), [5, 7]);
    }

    #[test]
    fn path_class_should_read_the_path_and_the_question() {
        assert_eq!(PathClass::of("src/client.rs"), PathClass::Code);
        assert_eq!(PathClass::of("tests/client.rs"), PathClass::Test);
        assert_eq!(PathClass::of("docs/guide.md"), PathClass::Doc);
        assert_eq!(PathClass::of("Cargo.toml"), PathClass::Config);
        assert!(PathClass::Test.named_in("where is the retry test?"));
        assert!(PathClass::Config.named_in("which setting sets the limit"));
        assert!(!PathClass::Doc.named_in("where is the limit read"));
        assert!(!PathClass::Code.named_in("code"));
    }
}
