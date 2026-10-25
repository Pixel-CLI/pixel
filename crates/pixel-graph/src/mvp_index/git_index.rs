// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Pixel's copy of git's index, its stat cache refreshed (ADR-0039, Q42).
//!
//! Every git Pixel runs passes `--no-optional-locks`, so it never writes
//! `.git/index` (C-18): when that file's stat cache is stale (right after
//! a clone or a checkout, every entry racily clean; or a checkout copied
//! with `cp -R`, every inode new), each `git status` re-hashes the whole
//! tree and keeps nothing. On `antiwork/gumroad` that is ~960 ms a prompt,
//! and the hook printed nothing (pixel-mvp#114).
//!
//! The writer copies `.git/index` into `.pixel-mvp/` and has git refresh
//! the copy's stat cache (`git update-index -q --refresh` with
//! `GIT_INDEX_FILE` on the copy): git rewrites the stat of an entry only
//! when the file's content is that entry's blob, and never its blob, mode
//! or stage. The copy is named after the device, inode, size, mtime and
//! ctime of `.git/index` and after its path: a scan reads it only while
//! `.git/index` still has them, so it is that file's content with a
//! fresher stat cache, and git stays the one judge of what is dirty. Any
//! rewrite of `.git/index` (`git add`, a commit, a checkout, a `git
//! status` that refreshed) is a rename, a new inode: the copy is ignored
//! from then on, and the scan reads `.git/index` itself until the writer
//! copies it again.

use std::path::{Path, PathBuf};

use mvp_model::hash::Hash;

use crate::mvp_index::tree::{STATE, git_in};

/// The copies' names in [`STATE`]: this prefix, then the key.
pub const PREFIX: &str = "git-index.";

/// The copy of one version of git's index file, made or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    /// Git's index file, as `git rev-parse --git-path index` names it.
    source: PathBuf,
    /// The source's path and stat, hashed: the copy's name.
    key: String,
    /// Where the copy is: `.pixel-mvp/git-index.<key>` under the base.
    path: PathBuf,
}

impl Copy {
    /// The copy of the version of `source` there is now, in `base`'s
    /// [`STATE`]: the work tree's root, or the directory a caller keeps
    /// Pixel's state under ([`crate::mvp_index::tree::scan_discovered_in`]); `None`
    /// when `source` is not a regular file (no index yet, before the first
    /// `git add`).
    pub fn of(base: &Path, source: &Path) -> Option<Copy> {
        let key = key(source)?;
        let path = base.join(STATE).join(format!("{PREFIX}{key}"));
        Some(Copy {
            source: source.to_path_buf(),
            key,
            path,
        })
    }

    /// Where the copy is, made or not.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the copy is made: a regular file at its name, never a link
    /// a clone committed there (C-18).
    pub fn made(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.is_file())
    }

    /// Whether git's index file is still the version this copy is of.
    pub fn holds(&self) -> bool {
        key(&self.source).as_deref() == Some(self.key.as_str())
    }

    /// What the next copy starts from: the copy when it is made, its stat
    /// cache already refreshed, else git's index file; `None` unless git's
    /// index is still the version this copy is of once they are read.
    /// Each writer run refreshes the copy again from these bytes: git
    /// leaves an entry whose file changed in the second of the refresh to
    /// be checked again (racily clean), and so does every refresh until one
    /// runs in a later second.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let bytes = if self.made() {
            std::fs::read(&self.path).ok()?
        } else {
            std::fs::read(&self.source).ok()?
        };
        self.holds().then_some(bytes)
    }
}

/// Has git refresh the stat cache of `file`, a copy of git's index of the
/// work tree at `root`: `git update-index -q --refresh` with
/// `GIT_INDEX_FILE` set to it. Git takes `<file>.lock`, never
/// `.git/index.lock`, and writes `file` by a rename, or leaves it as it
/// was when it fails. Split index is turned off so that git writes no
/// shared index into `.git/`. Whether git ran to its end.
pub fn refresh(root: &Path, file: &Path) -> bool {
    let mut command = git_in(root);
    command.env("GIT_INDEX_FILE", file).args([
        "-c",
        "core.splitIndex=false",
        "update-index",
        "-q",
        "--refresh",
    ]);
    crate::mvp_index::bounded::run(command, &[]).is_ok_and(|done| done.status.code().is_some())
}

/// The key of the version of git's index file at `source`: its path,
/// device, inode, size, mtime and ctime, hashed. Git writes the file by a
/// rename, so every version has an inode of its own; a write in place
/// moves its ctime, which no user tool sets back.
#[cfg(unix)]
fn key(source: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(source).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut stamp = source.as_os_str().as_encoded_bytes().to_vec();
    stamp.push(0);
    for value in [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
    ] {
        stamp.extend(value.to_le_bytes());
    }
    Some(Hash::of(&stamp).to_hex()[..16].to_owned())
}

/// No inode or ctime to tell versions of git's index apart: no copy is
/// made or read.
#[cfg(not(unix))]
// No supported release/test target compiles this fallback, which only disables index copying.
#[cfg_attr(test, mutants::skip)]
fn key(_: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvp_index::tree::{discover, scan_discovered, scan_discovered_in};
    use std::process::Stdio;

    /// Runs git in `dir`, as a test sets a repository up.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = git_in(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository holding `a.rb`, committed, with Pixel's state
    /// directory.
    fn committed() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        std::fs::write(root.join("a.rb"), "def a; end\n").unwrap();
        run_git(root, &["add", "a.rb"]);
        run_git(root, &["commit", "-qm", "one"]);
        std::fs::create_dir(root.join(STATE)).unwrap();
        dir
    }

    /// The copy of the git index there is now, made and refreshed as the
    /// writer makes it.
    fn made(root: &Path) -> Copy {
        made_in(root, discover(root).unwrap().root())
    }

    /// [`made`], in `base`'s state directory.
    fn made_in(root: &Path, base: &Path) -> Copy {
        let repo = discover(root).unwrap();
        let copy = Copy::of(base, repo.index().unwrap()).unwrap();
        std::fs::write(copy.path(), copy.bytes().unwrap()).unwrap();
        assert!(refresh(repo.root(), copy.path()));
        assert!(copy.made() && copy.holds());
        copy
    }

    // ADR-0039: the copy has the entries of the version of git's index it
    // was made of. Once git's index changes, the copy is not that index any
    // more: a change staged, then undone in the file only, leaves the copy
    // equal to HEAD and the file too, so a scan that read the copy would
    // call the tree clean.
    #[cfg(unix)]
    #[test]
    fn c11_a_change_staged_after_the_copy_is_read_from_gits_own_index() {
        let dir = committed();
        let root = dir.path();
        let copy = made(root);
        let tree = scan_discovered(discover(root).unwrap(), &|_| None).unwrap();
        assert_eq!(tree.index.as_deref(), Some(copy.path()), "the copy read");
        assert!(!tree.dirty && !tree.unsettled);
        std::fs::write(root.join("a.rb"), "def b; end\n").unwrap();
        run_git(root, &["add", "a.rb"]);
        std::fs::write(root.join("a.rb"), "def a; end\n").unwrap();
        assert!(!copy.holds(), "git's index is another version");
        let tree = scan_discovered(discover(root).unwrap(), &|_| None).unwrap();
        assert!(tree.dirty, "the staged change is seen");
        assert_eq!(tree.index, None, "git's own index read");
        assert!(tree.unsettled, "the writer is to make the next copy");
        // What reading the copy of the old version would have said.
        let stale = crate::mvp_index::tree::scan_using(
            root,
            tree.commit.clone(),
            Some(copy.path()),
            &|_| None,
        )
        .unwrap();
        assert!(!stale.dirty);
    }

    // v1 keeps pixel-mvp's state in `.pixel/.pixel-mvp/` (ADR-0036): the
    // copy its writer places there must be the one its scan reads, or every
    // scan pays git's stale stat cache again (v1#993). Its key is still
    // git's index file, so a change staged after it is read from git's own,
    // and neither the copy nor the scans write `.git/index` (C-18).
    #[cfg(unix)]
    #[test]
    fn c11_c18_a_copy_under_a_callers_state_directory_is_the_one_its_scan_reads() {
        let dir = committed();
        let root = dir.path();
        let base = root.join(".pixel");
        std::fs::create_dir_all(base.join(STATE)).unwrap();
        // As the store makes it: the state is never listed by git.
        std::fs::write(base.join(STATE).join(".gitignore"), "*\n").unwrap();
        let git_index = root.join(".git/index");
        let stat = |path: &Path| {
            let meta = std::fs::metadata(path).unwrap();
            (std::fs::read(path).unwrap(), meta.modified().unwrap())
        };
        let before = stat(&git_index);
        let copy = made_in(root, &base);
        assert!(copy.path().starts_with(base.join(STATE)), "{copy:?}");
        let tree = scan_discovered_in(discover(root).unwrap(), &|_| None, &base).unwrap();
        assert_eq!(tree.index.as_deref(), Some(copy.path()), "the copy read");
        assert!(!tree.dirty && !tree.unsettled);
        // The root's own state holds no copy: read from there, git's own
        // index is, and the writer is asked for a copy.
        let tree = scan_discovered(discover(root).unwrap(), &|_| None).unwrap();
        assert_eq!(tree.index, None);
        assert!(!tree.dirty && tree.unsettled);
        assert_eq!(stat(&git_index), before, "git's index untouched");
        std::fs::write(root.join("a.rb"), "def b; end\n").unwrap();
        run_git(root, &["add", "a.rb"]);
        std::fs::write(root.join("a.rb"), "def a; end\n").unwrap();
        let tree = scan_discovered_in(discover(root).unwrap(), &|_| None, &base).unwrap();
        assert!(tree.dirty, "the staged change is seen");
        assert_eq!(tree.index, None, "git's own index read");
    }

    // The copy is found by the path git names: the same from any directory
    // of the work tree, and a linked work tree's own index.
    #[test]
    fn c11_gits_index_is_found_from_any_directory_and_in_a_linked_work_tree() {
        let dir = committed();
        let root = dir.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        let top = discover(root).unwrap();
        let sub = discover(&root.join("sub")).unwrap();
        assert_eq!(top.index(), sub.index());
        assert_eq!(
            top.index().unwrap(),
            root.canonicalize().unwrap().join(".git/index")
        );
        let linked = dir.path().join("linked");
        run_git(root, &["worktree", "add", "-q", "linked"]);
        let other = discover(&linked).unwrap();
        assert_ne!(other.index(), top.index());
        assert!(
            other
                .index()
                .unwrap()
                .starts_with(root.canonicalize().unwrap().join(".git/worktrees"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn c11_a_copy_is_of_one_version_of_gits_index_only() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("index");
        std::fs::write(&source, b"one").unwrap();
        let copy = Copy::of(dir.path(), &source).unwrap();
        assert!(copy.holds());
        assert_eq!(Copy::of(dir.path(), &source), Some(copy.clone()));
        // Git writes its index by a rename: a new inode, another version.
        let next = dir.path().join("index.lock");
        std::fs::write(&next, b"one").unwrap();
        std::fs::rename(&next, &source).unwrap();
        assert!(!copy.holds());
        assert_ne!(Copy::of(dir.path(), &source), Some(copy));
        // No index file, no copy.
        assert_eq!(Copy::of(dir.path(), &dir.path().join("absent")), None);
    }

    #[test]
    fn audited_refresh_should_report_failure_when_git_cannot_be_started() {
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_index::tests::audited_refresh_child",
                "--nocapture",
            ])
            .env("PIXEL_TEST_REFRESH_ROOT", root.path())
            .env("PATH", root.path())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn audited_refresh_child() {
        let Some(root) = std::env::var_os("PIXEL_TEST_REFRESH_ROOT") else {
            return;
        };
        let root = std::path::Path::new(&root);
        assert!(!refresh(root, &root.join("index")));
    }
}
