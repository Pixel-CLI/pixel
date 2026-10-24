// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! One refresh of a repository's index: scan, plan against the current
//! generation, extract what changed, link, publish (`specs/graph.md`,
//! "Incremental speed"). The only place the pipeline's stages meet the
//! store; the daemon calls it from its writer thread, `pixel prepare` from the
//! command line.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::store::{StoreError, Writer};
use crate::tree::{TreeError, discover, scan_with};
use crate::{extract, link, plan};

/// What one refresh did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refreshed {
    /// Files in the published generation.
    pub files: usize,
    /// Files read and extracted by this run.
    pub extracted: usize,
    /// Files whose rows this run removed or replaced.
    pub dropped: usize,
    /// Files the deadline left for the next run.
    pub stale: usize,
    /// Whether the work tree differs from its `HEAD`.
    pub dirty: bool,
}

#[derive(Debug)]
pub enum RefreshError {
    Tree(TreeError),
    Store(StoreError),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefreshError::Tree(err) => err.fmt(f),
            RefreshError::Store(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RefreshError {}

impl From<TreeError> for RefreshError {
    fn from(err: TreeError) -> Self {
        RefreshError::Tree(err)
    }
}

impl From<StoreError> for RefreshError {
    fn from(err: StoreError) -> Self {
        RefreshError::Store(err)
    }
}

/// Brings the index of the work tree containing `path` up to date: waits at
/// most `wait` for the writer lock (`None`: as long as it takes) and leaves
/// a file not started by `deadline` stale for the next run.
pub fn refresh(
    path: &Path,
    wait: Option<Duration>,
    deadline: Option<Instant>,
) -> Result<Refreshed, RefreshError> {
    let repo = discover(path)?;
    let root = repo.root().to_path_buf();
    let mut writer = Writer::open(&root, wait)?;
    writer.retried(|writer| {
        let known = writer.known()?;
        let tree = scan_with(&root, &|path| known.get(path).copied())?;
        let indexed = writer.indexed()?;
        let plan = plan(
            &tree,
            &indexed.files,
            indexed.extractor.as_deref(),
            &indexed.stale,
        );
        let extracted = extract(&root, &plan.extract, deadline);
        let delta = link(&tree, &plan, extracted, |drop| writer.kept(drop))?;
        writer.publish(&delta)?;
        Ok(Refreshed {
            files: delta.files.len(),
            extracted: plan.extract.len(),
            dropped: plan.drop.len(),
            stale: delta.stale.len(),
            dirty: tree.dirty,
        })
    })
}

#[cfg(test)]
mod tests {
    use pixel_model::Graph;

    use super::*;
    use crate::store::Store;
    use crate::testutil::{commit, repo_with};

    fn names(store: &Store, file: &str) -> Vec<String> {
        store
            .symbols(file)
            .expect("symbols")
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn a_first_refresh_indexes_the_tree_and_a_second_changes_nothing() {
        let repo = repo_with(&[
            ("a.rs", "pub fn one() {}\n"),
            ("b.py", "def two():\n    pass\n"),
        ]);
        let first = refresh(repo.path(), None, None).expect("refresh");
        assert_eq!(
            (first.files, first.extracted, first.dropped, first.stale),
            (2, 2, 0, 0)
        );
        assert!(!first.dirty);
        let store = Store::open(repo.path()).expect("store");
        assert_eq!(names(&store, "a.rs"), ["a.rs", "one"]);
        assert_eq!(names(&store, "b.py"), ["b.py", "two"]);
        let second = refresh(repo.path(), None, None).expect("refresh");
        assert_eq!((second.extracted, second.dropped), (0, 0));
    }

    #[test]
    fn an_edit_replaces_only_the_rows_of_the_file_it_touched() {
        let repo = repo_with(&[("a.rs", "pub fn one() {}\n"), ("b.rs", "pub fn two() {}\n")]);
        refresh(repo.path(), None, None).expect("refresh");
        std::fs::write(
            repo.path().join("a.rs"),
            "pub fn one() {}\npub fn three() {}\n",
        )
        .unwrap();
        let again = refresh(repo.path(), None, None).expect("refresh");
        assert_eq!((again.extracted, again.dropped), (1, 1));
        assert!(again.dirty);
        let store = Store::open(repo.path()).expect("store");
        assert_eq!(names(&store, "a.rs"), ["a.rs", "one", "three"]);
        commit(repo.path(), &[], "two");
        let settled = refresh(repo.path(), None, None).expect("refresh");
        assert!(!settled.dirty);
    }

    #[test]
    fn a_directory_that_is_no_work_tree_is_an_error_not_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let err = refresh(dir.path(), None, None).expect_err("not a work tree");
        assert!(matches!(err, RefreshError::Tree(_)), "{err}");
    }
}
