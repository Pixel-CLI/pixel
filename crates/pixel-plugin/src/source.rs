// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The `github:` source of `pixel install`: parsed and validated before any
//! URL is built, so an owner, repository or ref can never smuggle an option
//! or a path component into the `git clone` it becomes.

use crate::Error;

/// Prefix of a GitHub source: `github:<owner>/<repo>[@<ref>]`.
pub const GITHUB_PREFIX: &str = "github:";
/// Prefix of an npm source; recognised so it fails with a reason.
pub const NPM_PREFIX: &str = "npm:";
/// The reason an `npm:` source is refused.
pub const NPM_UNSUPPORTED: &str =
    "npm sources are not supported yet; use github:<owner>/<repo> or a local path";
/// Where `github:` sources are cloned from unless the host says otherwise.
pub const DEFAULT_GITHUB_BASE: &str = "https://github.com";
/// GitHub allows 39 characters in an owner and 100 in a repository name.
const MAX_SEGMENT_LEN: usize = 100;

/// A parsed `github:<owner>/<repo>[@<ref>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSource {
    pub owner: String,
    pub repo: String,
    /// A branch or tag; the default branch when absent.
    pub reference: Option<String>,
}

impl GitHubSource {
    /// Parse the text after `github:`.
    pub fn parse(spec: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::Source(format!(
                "`{GITHUB_PREFIX}{spec}` is not {GITHUB_PREFIX}<owner>/<repo>[@<ref>]"
            ))
        };
        let (path, reference) = match spec.split_once('@') {
            Some((path, reference)) => (path, Some(reference)),
            None => (spec, None),
        };
        let (owner, repo) = path.split_once('/').ok_or_else(invalid)?;
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if !segment_ok(owner) || !segment_ok(repo) {
            return Err(invalid());
        }
        if let Some(reference) = reference
            && !reference_ok(reference)
        {
            return Err(invalid());
        }
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            reference: reference.map(str::to_owned),
        })
    }

    /// The URL `git clone` fetches, under `base` (`https://github.com`).
    pub fn clone_url(&self, base: &str) -> String {
        format!(
            "{}/{}/{}.git",
            base.trim_end_matches('/'),
            self.owner,
            self.repo
        )
    }

    /// The directory of release downloads under `base`: the latest release,
    /// or the one tagged with the ref.
    pub fn release_url(&self, base: &str) -> String {
        let tail = match &self.reference {
            Some(reference) => format!("download/{reference}"),
            None => "latest/download".to_owned(),
        };
        format!(
            "{}/{}/{}/releases/{tail}",
            base.trim_end_matches('/'),
            self.owner,
            self.repo
        )
    }

    /// The source as the user typed it, for messages and `plugins.toml`.
    pub fn shown(&self) -> String {
        match &self.reference {
            Some(reference) => format!("{GITHUB_PREFIX}{}/{}@{reference}", self.owner, self.repo),
            None => format!("{GITHUB_PREFIX}{}/{}", self.owner, self.repo),
        }
    }
}

/// An owner or repository name: ASCII letters, digits, `-`, `_` and `.`,
/// never starting with `-` (an option) or `.` (`.` and `..`).
fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_LEN
        && !segment.starts_with(['-', '.'])
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A branch or tag name safe to hand to `git clone --branch` and to put in
/// a URL path: no option form, no `..`, no empty or trailing component.
fn reference_ok(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= MAX_SEGMENT_LEN
        && reference.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !reference.contains("..")
        && !reference.contains("//")
        && !reference.ends_with(['/', '.'])
        && !reference.ends_with(".lock")
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(spec: &str) -> GitHubSource {
        GitHubSource::parse(spec).unwrap()
    }

    #[test]
    fn owner_repo_and_an_optional_ref_are_split() {
        assert_eq!(
            parse("PIXEL-CLI/pixel-git"),
            GitHubSource {
                owner: "PIXEL-CLI".into(),
                repo: "pixel-git".into(),
                reference: None
            }
        );
        let pinned = parse("a/b@release/v1.2");
        assert_eq!(pinned.reference.as_deref(), Some("release/v1.2"));
        assert_eq!(parse("a/b.git").repo, "b", "a .git suffix is the same repo");
        assert_eq!(parse("a/b.git@v1").repo, "b");
    }

    #[test]
    fn urls_and_the_typed_form_follow_the_ref() {
        let plain = parse("o/r");
        assert_eq!(
            plain.clone_url("https://github.com"),
            "https://github.com/o/r.git"
        );
        assert_eq!(plain.clone_url("file:///srv/gh/"), "file:///srv/gh/o/r.git");
        assert_eq!(
            plain.release_url("https://github.com"),
            "https://github.com/o/r/releases/latest/download"
        );
        assert_eq!(plain.shown(), "github:o/r");
        let pinned = parse("o/r@v2");
        assert_eq!(
            pinned.release_url("https://github.com"),
            "https://github.com/o/r/releases/download/v2"
        );
        assert_eq!(pinned.shown(), "github:o/r@v2");
    }

    #[test]
    fn every_malformed_source_is_refused() {
        for bad in [
            "",
            "owner",
            "owner/",
            "/repo",
            "a/b/c",
            "a/-b",
            "-a/b",
            "./b",
            "a/..",
            "a/b c",
            "a/b@",
            "a/b@-x",
            "a/b@../x",
            "a/b@x//y",
            "a/b@x/",
            "a/b@x.lock",
            "a/b@x y",
            "a/b?x=1",
        ] {
            let err = GitHubSource::parse(bad).unwrap_err().to_string();
            assert!(
                err.contains("github:<owner>/<repo>[@<ref>]"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn the_length_limit_is_inclusive() {
        let max = "a".repeat(MAX_SEGMENT_LEN);
        assert!(GitHubSource::parse(&format!("{max}/{max}@{max}")).is_ok());
        let over = "a".repeat(MAX_SEGMENT_LEN + 1);
        assert!(GitHubSource::parse(&format!("{over}/r")).is_err());
        assert!(GitHubSource::parse(&format!("o/{over}")).is_err());
        assert!(GitHubSource::parse(&format!("o/r@{over}")).is_err());
    }
}
