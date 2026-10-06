// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Candidate verification: run the real regex over candidate files.
//!
//! The index is a filter, not an oracle — every candidate is re-checked with
//! ripgrep's matcher/searcher crates, which makes the whole pipeline immune
//! to gram-hash collisions and planner under-approximation.

use std::io::Read;
use std::path::Path;

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

use crate::index::{MAX_FILE_BYTES, open_regular_bounded};

#[derive(Debug, Clone)]
pub struct MatchLine {
    pub path: String,
    pub line_number: u64,
    pub line: String,
}

#[derive(Debug)]
pub enum VerifyError {
    BadPattern(String),
    Io(std::io::Error),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadPattern(e) => write!(f, "bad pattern: {e}"),
            VerifyError::Io(e) => write!(f, "verify io error: {e}"),
        }
    }
}

impl std::error::Error for VerifyError {}

pub struct Verifier {
    matcher: RegexMatcher,
}

impl Verifier {
    pub fn new(pattern: &str) -> Result<Self, VerifyError> {
        let matcher =
            RegexMatcher::new(pattern).map_err(|e| VerifyError::BadPattern(e.to_string()))?;
        Ok(Self { matcher })
    }

    /// Search one file, appending matches to `out`. `display_path` is what
    /// gets reported (typically repo-relative).
    pub fn search_file(
        &self,
        abs_path: &Path,
        display_path: &str,
        out: &mut Vec<MatchLine>,
        max_matches: Option<usize>,
    ) -> Result<(), VerifyError> {
        let mut skip = 0;
        self.search_file_page(abs_path, display_path, out, &mut skip, max_matches)
    }

    /// Search one file while skipping matches from preceding pages. `skip`
    /// is decremented in place across files, so callers retain only the
    /// requested page rather than every match before it.
    pub fn search_file_page(
        &self,
        abs_path: &Path,
        display_path: &str,
        out: &mut Vec<MatchLine>,
        skip: &mut usize,
        max_matches: Option<usize>,
    ) -> Result<(), VerifyError> {
        if max_matches == Some(0) {
            return Ok(());
        }
        let start_len = out.len();
        let file = open_regular_bounded(abs_path, MAX_FILE_BYTES).map_err(VerifyError::Io)?;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .build();
        searcher
            .search_reader(
                &self.matcher,
                file.take(MAX_FILE_BYTES.saturating_add(1)),
                UTF8(|line_number, line| {
                    if *skip > 0 {
                        *skip -= 1;
                        return Ok(true);
                    }
                    out.push(MatchLine {
                        path: display_path.to_string(),
                        line_number,
                        line: line.trim_end_matches(['\n', '\r']).to_string(),
                    });
                    Ok(max_matches.is_none_or(|limit| out.len() - start_len < limit))
                }),
            )
            .map_err(VerifyError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lines_and_skips_binary() {
        let dir = std::env::temp_dir().join(format!("gpx-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        std::fs::write(&text, "one handleClick\nplain\nhandleClick again\n").unwrap();
        let bin = dir.join("b.bin");
        std::fs::write(&bin, b"handleClick\x00binary").unwrap();

        let v = Verifier::new("handleClick").unwrap();
        let mut out = Vec::new();
        v.search_file(&text, "a.txt", &mut out, None).unwrap();
        v.search_file(&bin, "b.bin", &mut out, None).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line_number, 1);
        assert_eq!(out[1].line_number, 3);
        assert!(out.iter().all(|m| m.path == "a.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_file_page_skips_then_stops_at_the_page_size() {
        let dir = std::env::temp_dir().join(format!("gpx-verify-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("p.txt");
        std::fs::write(&text, "hit 1\nhit 2\nhit 3\nhit 4\nhit 5\n").unwrap();
        let v = Verifier::new("hit").unwrap();
        let mut out = vec![MatchLine {
            path: "earlier".into(),
            line_number: 9,
            line: "earlier".into(),
        }];
        let mut skip = 1;
        v.search_file_page(&text, "p.txt", &mut out, &mut skip, Some(2))
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let lines: Vec<u64> = out.iter().map(|m| m.line_number).collect();
        assert_eq!(lines, vec![9, 2, 3]);
        assert_eq!(skip, 0);
    }

    #[test]
    fn verify_error_display_names_the_failure() {
        assert_eq!(
            VerifyError::BadPattern("x".into()).to_string(),
            "bad pattern: x"
        );
        let io = VerifyError::Io(std::io::Error::other("boom")).to_string();
        assert_eq!(io, "verify io error: boom");
    }
}
