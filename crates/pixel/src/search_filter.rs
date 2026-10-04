// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The ripgrep flags agents reach for on `pixel search-content`: `-g/--glob`,
//! `-t/--type` and `-l/--files-with-matches`. Recorded Opus and Sonnet runs
//! passed `--glob`, `--path` and `--file` and got a usage error, each costing
//! a turn; every model has learned ripgrep's interface, so the command takes
//! its common flags instead of teaching a new one.
//!
//! `-g` and `-t` travel in the `Search` request and the daemon filters the
//! candidate files (`pixel_index::path_filter`); `-l` is rendering, here.

use std::collections::HashSet;

use serde_json::Value;

/// The distinct `path` of each match, in the order the matches came, that
/// `seen` does not hold yet; each one returned is added to it. One `seen`
/// shared across the roots of a search prints a path once even when two
/// roots index it (a nested repository under a gitless parent).
pub fn files_with_matches(matches: &[Value], seen: &mut HashSet<String>) -> Vec<String> {
    matches
        .iter()
        .filter_map(|m| m.get("path").and_then(Value::as_str))
        .filter(|path| seen.insert((*path).to_string()))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_with_matches_are_distinct_and_in_order() {
        let matches = vec![
            serde_json::json!({"path": "b.rs", "line": 1}),
            serde_json::json!({"path": "a.rs", "line": 2}),
            serde_json::json!({"path": "b.rs", "line": 9}),
            serde_json::json!({"line": 3}),
        ];
        let mut seen = HashSet::new();
        assert_eq!(files_with_matches(&matches, &mut seen), ["b.rs", "a.rs"]);
        // A second root's page prints only the paths the first did not.
        let next = vec![
            serde_json::json!({"path": "a.rs", "line": 4}),
            serde_json::json!({"path": "c.rs", "line": 5}),
        ];
        assert_eq!(files_with_matches(&next, &mut seen), ["c.rs"]);
    }
}
