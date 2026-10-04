// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Generic fallback for wrapped commands: last-N-lines tail for the record,
//! full output preserved via raw_fallbacks by the caller.

/// Last `n` lines of `output` (lossy — callers pass decoded text).
pub fn tail_lines(output: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| (*s).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_shorter_than_n() {
        assert_eq!(tail_lines("a\nb", 100), vec!["a", "b"]);
    }

    #[test]
    fn tail_truncates_from_front() {
        let output: String = (1..=150).map(|i| format!("line{i}\n")).collect();
        let tail = tail_lines(&output, 100);
        assert_eq!(tail.len(), 100);
        assert_eq!(tail[0], "line51");
        assert_eq!(tail[99], "line150");
    }

    #[test]
    fn tail_empty() {
        assert!(tail_lines("", 100).is_empty());
    }
}
