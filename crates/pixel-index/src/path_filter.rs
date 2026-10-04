// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The path filter behind `pixel search-content -g/--glob` and `-t/--type`:
//! which repo-relative paths a search keeps. It lives in the index so the
//! daemon's search applies it to the candidate files before paging, the
//! same place the path-prefix restriction applies, instead of a client
//! re-paging the daemon and filtering what came back.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// `-t` names and the extensions they select: a subset of ripgrep's table,
/// the languages this index extracts plus the usual config and doc files.
const TYPES: &[(&str, &[&str])] = &[
    ("c", &["c", "h"]),
    ("cpp", &["cpp", "cc", "cxx", "hpp", "hh", "hxx"]),
    ("css", &["css", "scss"]),
    ("go", &["go"]),
    ("html", &["html", "htm"]),
    ("java", &["java"]),
    ("js", &["js", "mjs", "cjs", "jsx"]),
    ("json", &["json"]),
    ("kotlin", &["kt", "kts"]),
    ("markdown", &["md", "markdown"]),
    ("md", &["md", "markdown"]),
    ("py", &["py", "pyi"]),
    ("python", &["py", "pyi"]),
    ("rb", &["rb"]),
    ("ruby", &["rb"]),
    ("rust", &["rs"]),
    ("sh", &["sh", "bash", "zsh"]),
    ("swift", &["swift"]),
    ("toml", &["toml"]),
    ("ts", &["ts", "tsx", "mts", "cts"]),
    ("typescript", &["ts", "tsx", "mts", "cts"]),
    ("yaml", &["yml", "yaml"]),
];

/// Which repo-relative paths a search keeps.
#[derive(Debug)]
pub struct PathFilter {
    /// Every `-g` rule's globs in one set, in flag order (empty without `-g`).
    globs: GlobSet,
    /// For each glob of `globs`, whether its rule was a `!` exclusion. The
    /// glob index grows with the flag order, so the highest matching index
    /// is the last matching rule.
    negated: Vec<bool>,
    /// Whether any rule includes: then a path no rule matches is left out.
    any_include: bool,
    extensions: Vec<&'static str>,
}

impl PathFilter {
    /// `None` when no flag filters anything. A glob starting with `!`
    /// excludes; with any include glob, a path must match one of them. As in
    /// ripgrep, the last glob that matches a path decides, so
    /// `-g '!*.rs' -g '*.rs'` keeps Rust files and `-g '*.rs' -g '!*.rs'`
    /// drops them.
    pub fn new(globs: &[String], types: &[String]) -> Result<Option<Self>, String> {
        if globs.is_empty() && types.is_empty() {
            return Ok(None);
        }
        let mut set = GlobSetBuilder::new();
        let mut negated = Vec::new();
        let mut any_include = false;
        for raw in globs {
            let (pattern, is_negated) = match raw.strip_prefix('!') {
                Some(pattern) => (pattern, true),
                None => (raw.as_str(), false),
            };
            any_include |= !is_negated;
            for glob in gitignore_globs(pattern)? {
                set.add(glob);
                negated.push(is_negated);
            }
        }
        let mut extensions = Vec::new();
        for name in types {
            let exts = type_extensions(name).ok_or_else(|| {
                let known: Vec<&str> = TYPES.iter().map(|(n, _)| *n).collect();
                format!("unknown --type '{name}'; known: {}", known.join(", "))
            })?;
            extensions.extend_from_slice(exts);
        }
        Ok(Some(Self {
            globs: set.build().map_err(|e| format!("bad --glob: {e}"))?,
            negated,
            any_include,
            extensions,
        }))
    }

    pub fn keeps(&self, path: &str) -> bool {
        let path = path.trim_start_matches("./");
        let kept = match self.globs.matches(path).last() {
            Some(&glob) => !self.negated[glob],
            None => !self.any_include,
        };
        if !kept {
            return false;
        }
        self.extensions.is_empty()
            || path
                .rsplit_once('.')
                .is_some_and(|(_, ext)| self.extensions.contains(&ext))
    }
}

/// The extensions a `-t` name selects.
pub fn type_extensions(name: &str) -> Option<&'static [&'static str]> {
    TYPES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, exts)| *exts)
}

/// A `.gitignore`-style glob as ripgrep reads it: without a `/` it matches a
/// file or directory name at any depth, with one it is anchored at the root;
/// either way a matched directory keeps everything under it.
fn gitignore_globs(raw: &str) -> Result<Vec<Glob>, String> {
    let anchored = raw.trim_start_matches('/').trim_end_matches('/');
    let base = if raw.trim_end_matches('/').contains('/') {
        anchored.to_string()
    } else {
        format!("**/{anchored}")
    };
    [base.clone(), format!("{base}/**")]
        .iter()
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("bad --glob '{raw}': {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(globs: &[&str], types: &[&str]) -> PathFilter {
        let globs: Vec<String> = globs.iter().map(ToString::to_string).collect();
        let types: Vec<String> = types.iter().map(ToString::to_string).collect();
        PathFilter::new(&globs, &types).unwrap().unwrap()
    }

    #[test]
    fn no_flag_means_no_filter() {
        assert!(PathFilter::new(&[], &[]).unwrap().is_none());
    }

    #[test]
    fn a_name_glob_matches_at_any_depth() {
        let f = filter(&["*.rs"], &[]);
        assert!(f.keeps("crates/pixel/src/main.rs"));
        assert!(f.keeps("build.rs"));
        assert!(f.keeps("./src/lib.rs"));
        assert!(!f.keeps("docs/README.md"));
    }

    #[test]
    fn a_negated_glob_excludes_even_what_an_include_keeps() {
        // The exact flag Opus passed in the demo series.
        let f = filter(&["!**/tests/**"], &[]);
        assert!(!f.keeps("crates/pixel-ops/tests/all/crash_matrix.rs"));
        assert!(f.keeps("crates/pixel-ops/src/push.rs"));
        let f = filter(&["*.rs", "!**/tests/**"], &[]);
        assert!(!f.keeps("crates/pixel-ops/tests/all/crash_matrix.rs"));
        assert!(f.keeps("crates/pixel-ops/src/push.rs"));
        assert!(!f.keeps("ARCHITECTURE.md"));
    }

    #[test]
    fn the_last_matching_glob_decides_as_in_ripgrep() {
        // An agent narrowing a search appends a flag; the later one must win,
        // or `-g '!*.rs' -g '*.rs'` silently returns nothing.
        let f = filter(&["!*.rs", "*.rs"], &[]);
        assert!(f.keeps("src/lib.rs"));
        assert!(!f.keeps("README.md"), "an include rule leaves the rest out");
        let f = filter(&["*.rs", "!*.rs"], &[]);
        assert!(!f.keeps("src/lib.rs"));
        assert!(!f.keeps("README.md"));
        // A path neither rule matches: kept when every rule excludes.
        let f = filter(&["!*.rs", "!*.md"], &[]);
        assert!(f.keeps("scripts/gen.sh"));
        assert!(!f.keeps("README.md"));
        assert!(!f.keeps("src/lib.rs"));
        // The later exclusion overrides an earlier, broader include.
        let f = filter(&["crates", "!**/tests/**", "crates/pixel/tests/cli"], &[]);
        assert!(f.keeps("crates/pixel/src/main.rs"));
        assert!(!f.keeps("crates/pixel-ops/tests/all/a.rs"));
        assert!(f.keeps("crates/pixel/tests/cli/main.rs"));
        assert!(
            !f.keeps("README.md"),
            "two includes still leave the rest out"
        );
    }

    #[test]
    fn a_directory_name_keeps_what_is_under_it_and_a_slash_anchors() {
        let f = filter(&["tests"], &[]);
        assert!(f.keeps("crates/pixel/tests/cli/main.rs"));
        assert!(!f.keeps("crates/pixel/src/tests_helper.rs"));
        let f = filter(&["crates/pixel-ops"], &[]);
        assert!(f.keeps("crates/pixel-ops/src/push.rs"));
        assert!(!f.keeps("vendor/crates/pixel-ops/src/push.rs"));
        let f = filter(&["/src/"], &[]);
        assert!(f.keeps("src/lib.rs"));
        assert!(f.keeps("./src/lib.rs"), "a leading ./ is the root");
        assert!(!f.keeps("crates/pixel/src/lib.rs"));
        // A trailing slash alone does not anchor, as in .gitignore.
        let f = filter(&["tests/"], &[]);
        assert!(f.keeps("crates/pixel/tests/cli/main.rs"));
    }

    #[test]
    fn a_star_does_not_cross_a_directory() {
        let f = filter(&["crates/*.rs"], &[]);
        assert!(f.keeps("crates/lib.rs"));
        assert!(!f.keeps("crates/pixel/src/lib.rs"));
    }

    #[test]
    fn types_select_extensions_and_combine_with_globs() {
        let f = filter(&[], &["rust", "md"]);
        assert!(f.keeps("crates/pixel/src/main.rs"));
        assert!(f.keeps("README.md"));
        assert!(!f.keeps("scripts/gen.sh"));
        assert!(!f.keeps("Makefile"));
        let f = filter(&["!**/tests/**"], &["rust"]);
        assert!(!f.keeps("crates/pixel/tests/cli/main.rs"));
        assert!(!f.keeps("docs/a.md"));
        assert!(f.keeps("crates/pixel/src/main.rs"));
    }

    #[test]
    fn an_unknown_type_is_an_error_that_lists_the_known_ones() {
        let err = PathFilter::new(&[], &["cobol".into()]).unwrap_err();
        assert!(
            err.starts_with("unknown --type 'cobol'; known: c, cpp,"),
            "{err}"
        );
        assert!(err.contains("rust"), "{err}");
    }

    #[test]
    fn a_malformed_glob_is_an_error() {
        let err = PathFilter::new(&["a[".into()], &[]).unwrap_err();
        assert!(err.starts_with("bad --glob 'a['"), "{err}");
    }

    #[test]
    fn type_extensions_come_from_the_table() {
        assert_eq!(type_extensions("rust"), Some(&["rs"][..]));
        assert_eq!(
            type_extensions("ts"),
            Some(&["ts", "tsx", "mts", "cts"][..])
        );
        assert_eq!(type_extensions("nope"), None);
    }
}
