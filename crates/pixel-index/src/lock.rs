// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Per-root exclusive build lock via `flock(2)`.
//!
//! When multiple CLI invocations (or CLI + daemon) race to build the same
//! index, the first process acquires an exclusive lock on
//! `root/.pixel/build.lock`, builds the shard, and releases. Concurrent
//! callers block on the lock, then load the already-built shard — no
//! duplicated work, no write races.
//!
//! The lock is advisory (`flock`) and automatically released when the file
//! descriptor is closed (process exit, panic, or explicit drop).

use std::fs::File;
use std::io;
use std::path::Path;

use fs2::FileExt;
use pixel_git::GitRunner;

use crate::index::SHARD_DIR;

/// A guard that holds the build lock. Dropping it releases the lock.
pub struct BuildLock {
    _file: File,
}

/// Comment header placed above our `.pixel/` entry. Earlier releases wrote
/// it into a `.gitignore` created from scratch, which
/// [`is_pixel_only_gitignore`] still recognises; it now heads the entry in
/// `info/exclude`.
const GITIGNORE_HEADER: &str = "# pixel index sidecar\n";

/// True if a repo-root `.gitignore` contains only pixel housekeeping — our
/// [`GITIGNORE_HEADER`] comment (optional) plus the `.pixel/` entry line
/// and nothing else. Such a file is a from-scratch write by an earlier
/// release of [`ensure_pixel_gitignored`] (which now writes `info/exclude`)
/// for a repo that had no ignore file: it must never pollute the
/// index/search file universe any more than `.pixel/` itself does. A real
/// user `.gitignore` carrying any other ignore rule is *not* housekeeping
/// and stays fully indexed.
pub fn is_pixel_only_gitignore(root: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return false;
    };
    is_pixel_only_gitignore_text(&content)
}

/// [`is_pixel_only_gitignore`] over the file's content: for a `.gitignore`
/// read from a commit rather than the working tree.
pub fn is_pixel_only_gitignore_text(content: &str) -> bool {
    // A purely-housekeeping `.gitignore` has at most two kinds of lines: the
    // optional header comment and the `.pixel`/`.pixel/` ignore entry. Anything
    // else (blank flames included) means real user content — index it normally.
    content.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t == ".pixel" || t == ".pixel/" || t == GITIGNORE_HEADER.trim()
    })
}

/// Keep the `.pixel/` sidecar out of `git status` without touching a file
/// git tracks.
///
/// Nothing is written when git already ignores `.pixel/`, whatever the
/// source: a `.gitignore`, `info/exclude`, the user's global excludes.
/// Otherwise the entry goes to the clone's own `info/exclude` (the common
/// one, for a linked worktree), which git never shares and never shows as a
/// change. Appending to the tracked `.gitignore` instead made a read-only
/// command such as `what-changed` dirty the tree it reported on, put that
/// edit in its own diff, and blocked the next `git checkout`.
///
/// The entry is written at most once. A `!.pixel/` in the tracked
/// `.gitignore` outranks `info/exclude`, so the sidecar stays visible and
/// `check-ignore` keeps failing; the entry already present stops a second
/// append rather than one per build. The exclude file is handled as bytes
/// and left untouched on any read error but "absent": a non-UTF-8 byte in a
/// user's rules must not turn into an empty file.
///
/// No-op outside a git work tree root or when git cannot run; idempotent;
/// non-fatal.
pub fn ensure_pixel_gitignored(root: &Path) {
    // Only act at a work tree root (root/.git exists as dir or file): the
    // entry is anchored there.
    let git_marker = root.join(".git");
    if !git_marker.is_dir() && !git_marker.is_file() {
        return;
    }
    let git = GitRunner::new(root);
    let sidecar = format!("{SHARD_DIR}/");
    if git.run_opt(&["check-ignore", "-q", &sidecar]).is_some() {
        return;
    }
    let Some(exclude) = git.run_opt(&["rev-parse", "--git-path", "info/exclude"]) else {
        return;
    };
    // `--git-path` answers relative to the `-C` directory unless absolute.
    let path = root.join(String::from_utf8_lossy(&exclude).trim());
    let mut content = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return,
    };
    let entry = format!("/{sidecar}");
    if content
        .split(|b| *b == b'\n')
        .any(|line| line.trim_ascii() == entry.as_bytes())
    {
        return;
    }
    if !content.is_empty() && !content.ends_with(b"\n") {
        content.push(b'\n');
    }
    content.extend_from_slice(format!("{GITIGNORE_HEADER}{entry}\n").as_bytes());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, content);
}

impl BuildLock {
    /// Acquire an exclusive lock on `root/.pixel/build.lock`, blocking
    /// until it is available. The `.pixel` directory is created if missing.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let dir = root.join(SHARD_DIR);
        pixel_git::sidecar::private_dir(&dir)?;
        ensure_pixel_gitignored(root);
        let file = pixel_git::nofollow::open_lock(&dir.join("build.lock"))?;
        file.lock_exclusive()?;
        Ok(BuildLock { _file: file })
    }

    /// Try to acquire an exclusive lock without blocking. Returns `None`
    /// if the lock is held by another process.
    pub fn try_acquire(root: &Path) -> io::Result<Option<Self>> {
        let dir = root.join(SHARD_DIR);
        pixel_git::sidecar::private_dir(&dir)?;
        ensure_pixel_gitignored(root);
        let file = pixel_git::nofollow::open_lock(&dir.join("build.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(BuildLock { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // `fs2` unlocks automatically on drop, but be explicit.
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only pixel's own housekeeping `.gitignore` is not project content:
    /// the header, `.pixel` or `.pixel/` and blank lines, read from the
    /// worktree file or from committed text alike.
    #[test]
    fn pixel_only_gitignore_is_recognised_from_the_file_and_from_text() {
        let dir = temp_dir();
        assert!(!is_pixel_only_gitignore(&dir), "no file is not pixel-only");
        std::fs::write(
            dir.join(".gitignore"),
            format!("{GITIGNORE_HEADER}.pixel/\n"),
        )
        .unwrap();
        assert!(is_pixel_only_gitignore(&dir));
        std::fs::write(dir.join(".gitignore"), ".pixel/\ntarget/\n").unwrap();
        assert!(!is_pixel_only_gitignore(&dir));
        assert!(is_pixel_only_gitignore_text(".pixel\n\n"));
        assert!(!is_pixel_only_gitignore_text("node_modules\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A real repository in a fresh temp dir. `core.excludesFile` is set in
    /// the repo's own config so the developer's global excludes (which may
    /// well list `.pixel/`) cannot decide what these tests see.
    fn git_repo() -> (std::path::PathBuf, GitRunner) {
        let dir = temp_dir();
        let git = GitRunner::new(&dir);
        for args in [
            &["init", "-q"][..],
            &["config", "core.excludesFile", "/dev/null"],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
        ] {
            git.run(args).unwrap();
        }
        (dir, git)
    }

    fn commit_all(git: &GitRunner) {
        git.run(&["add", "-A"]).unwrap();
        git.run(&["commit", "-qm", "baseline"]).unwrap();
    }

    fn status(git: &GitRunner) -> String {
        String::from_utf8(
            git.run(&["status", "--porcelain", "--untracked-files=all"])
                .unwrap(),
        )
        .unwrap()
    }

    fn exclude_file(git: &GitRunner) -> std::path::PathBuf {
        let rel = git
            .run(&["rev-parse", "--git-path", "info/exclude"])
            .unwrap();
        git.root().join(String::from_utf8_lossy(&rel).trim())
    }

    /// The case that made `what-changed` dirty its own tree: a tracked
    /// `.gitignore` without the entry stays byte for byte what the commit
    /// holds, the status stays empty, and `.pixel/` is ignored all the same.
    #[test]
    fn a_tracked_gitignore_is_left_alone_and_the_sidecar_ignored_locally() {
        let (dir, git) = git_repo();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        commit_all(&git);
        std::fs::create_dir_all(dir.join(SHARD_DIR)).unwrap();
        std::fs::write(dir.join(SHARD_DIR).join("shard"), "x").unwrap();

        ensure_pixel_gitignored(&dir);

        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "target/\n"
        );
        assert_eq!(status(&git), "", "no tracked edit, no untracked sidecar");
        assert!(git.run_opt(&["check-ignore", "-q", ".pixel/"]).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A repository with no `.gitignore` gets none: creating one left an
    /// untracked file in the status, the very noise the entry is for.
    #[test]
    fn no_gitignore_is_created_and_the_exclude_holds_exactly_the_entry() {
        let (dir, git) = git_repo();
        let exclude = exclude_file(&git);
        std::fs::remove_file(&exclude).ok();

        ensure_pixel_gitignored(&dir);

        assert!(!dir.join(".gitignore").exists(), "no .gitignore created");
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            format!("{GITIGNORE_HEADER}/.pixel/\n")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An exclude file the user wrote without a final newline keeps its last
    /// pattern: the entry starts on a line of its own.
    #[test]
    fn an_exclude_without_a_final_newline_keeps_its_last_pattern() {
        let (dir, git) = git_repo();
        let exclude = exclude_file(&git);
        std::fs::write(&exclude, "scratch/").unwrap();

        ensure_pixel_gitignored(&dir);

        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            format!("scratch/\n{GITIGNORE_HEADER}/.pixel/\n")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Whatever already ignores `.pixel/` (the tracked `.gitignore`, in
    /// either spelling, or a previous run's exclude entry), nothing is
    /// written again.
    #[test]
    fn an_already_ignored_sidecar_writes_nothing() {
        for entry in [".pixel/", ".pixel"] {
            let (dir, git) = git_repo();
            std::fs::write(dir.join(".gitignore"), format!("target/\n{entry}\n")).unwrap();
            let exclude = exclude_file(&git);
            let before = std::fs::read_to_string(&exclude).unwrap_or_default();

            ensure_pixel_gitignored(&dir);

            assert_eq!(
                std::fs::read_to_string(&exclude).unwrap_or_default(),
                before,
                "{entry}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
        let (dir, git) = git_repo();
        ensure_pixel_gitignored(&dir);
        let once = std::fs::read_to_string(exclude_file(&git)).unwrap();
        ensure_pixel_gitignored(&dir);
        assert_eq!(std::fs::read_to_string(exclude_file(&git)).unwrap(), once);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tracked `!.pixel/` outranks `info/exclude`: `check-ignore` keeps
    /// failing, and every build used to append one more entry. It is
    /// written once, and the sidecar is left as visible as the user asked.
    #[test]
    fn a_negated_sidecar_gets_one_exclude_entry_not_one_per_build() {
        let (dir, git) = git_repo();
        std::fs::write(dir.join(".gitignore"), "!.pixel/\n").unwrap();
        let exclude = exclude_file(&git);
        std::fs::remove_file(&exclude).ok();

        ensure_pixel_gitignored(&dir);
        ensure_pixel_gitignored(&dir);
        ensure_pixel_gitignored(&dir);

        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            format!("{GITIGNORE_HEADER}/.pixel/\n")
        );
        assert!(git.run_opt(&["check-ignore", "-q", ".pixel/"]).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Rules the user wrote in another encoding survive byte for byte: a
    /// UTF-8 read of them failed and the file was rewritten from empty.
    #[test]
    fn a_non_utf8_exclude_keeps_its_bytes() {
        let (dir, git) = git_repo();
        let exclude = exclude_file(&git);
        std::fs::write(&exclude, b"caf\xe9/\n").unwrap();

        ensure_pixel_gitignored(&dir);

        let mut expected = b"caf\xe9/\n".to_vec();
        expected.extend_from_slice(format!("{GITIGNORE_HEADER}/.pixel/\n").as_bytes());
        assert_eq!(std::fs::read(&exclude).unwrap(), expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An exclude file that cannot be read for another reason than being
    /// absent is left as it is. Write-only is the case that loses data: the
    /// read fails but a write would succeed and replace the user's rules
    /// with pixel's entry alone.
    #[test]
    fn an_unreadable_exclude_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, git) = git_repo();
        let exclude = exclude_file(&git);
        std::fs::write(&exclude, "keep/\n").unwrap();
        std::fs::set_permissions(&exclude, std::fs::Permissions::from_mode(0o200)).unwrap();
        if std::fs::read(&exclude).is_ok() {
            // Root reads through the mode bits, so there is no read error to
            // provoke; the CI runner is not root and still checks this.
            eprintln!(
                "skipped: {} is readable despite mode 0200",
                exclude.display()
            );
            std::fs::remove_dir_all(&dir).ok();
            return;
        }

        ensure_pixel_gitignored(&dir);

        std::fs::set_permissions(&exclude, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), "keep/\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A linked worktree has a `.git` file, not a directory, and git reads
    /// its excludes from the common directory: the entry lands there and
    /// the worktree's status stays clean.
    #[test]
    fn a_linked_worktree_is_ignored_through_the_common_exclude() {
        let (dir, git) = git_repo();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        commit_all(&git);
        let linked = temp_dir();
        std::fs::remove_dir_all(&linked).unwrap();
        git.run(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            &linked.to_string_lossy(),
        ])
        .unwrap();
        let linked_git = GitRunner::new(&linked);
        std::fs::create_dir_all(linked.join(SHARD_DIR)).unwrap();
        std::fs::write(linked.join(SHARD_DIR).join("shard"), "x").unwrap();

        ensure_pixel_gitignored(&linked);

        assert_eq!(status(&linked_git), "");
        assert!(!linked.join(".gitignore").exists());
        assert_eq!(
            exclude_file(&linked_git).canonicalize().unwrap(),
            exclude_file(&git).canonicalize().unwrap()
        );
        std::fs::remove_dir_all(&linked).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gitignore_untouched_outside_git_repo() {
        let dir = temp_dir();
        ensure_pixel_gitignored(&dir);
        assert!(!dir.join(".gitignore").exists(), "no .gitignore created");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pixel-gitignore-{}-{}-{}",
            std::process::id(),
            seq,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_blocks_second_attempt() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // First acquire succeeds.
        let _guard = BuildLock::acquire(&dir).unwrap();
        // Try-acquire (non-blocking) should return None while held.
        let second = BuildLock::try_acquire(&dir).unwrap();
        assert!(
            second.is_none(),
            "second try_acquire should fail while lock is held"
        );

        // After dropping, try-acquire succeeds.
        drop(_guard);
        let third = BuildLock::try_acquire(&dir).unwrap();
        assert!(
            third.is_some(),
            "try_acquire should succeed after lock is released"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
