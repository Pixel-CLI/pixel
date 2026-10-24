// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What changed: the diff of the latest commit, of the branch since it left
//! the default branch, or of the working tree, read through one bounded git
//! call and cut into files, line counts and the line ranges each file's new
//! side changed in.
//!
//! The call goes through `pixel_io::GitRunner`, which kills the process at a
//! wall-clock bound and cuts its output at a byte cap while it reads; the
//! bound here is what is left of the brief's window. A repository with no
//! commit, a shallow clone without a base and a binary-only change each fail
//! the read, and the brief goes without the change pack.

use std::path::Path;
use std::time::Instant;

use pixel_io::{GitOptions, GitRunner};

use crate::kinds::{ChangeScope, ChangedFile, Changes, MAX_PATCH_LINES};

/// Bytes of diff read at most; a larger change is cut by git's own error and
/// the pack is left out.
const DIFF_BYTES: usize = 2 * 1024 * 1024;
/// Bytes of the one-line answers (a subject, a merge base).
const LINE_BYTES: usize = 4096;
/// Files a change lists at most.
const MAX_FILES: usize = 400;
/// Characters of one line of a diff kept.
const PATCH_LINE_CHARS: usize = 160;
/// Branches a merge base is looked for against, in order.
const BASES: &[&str] = &["origin/main", "origin/master", "main", "master"];

/// What the diff of `scope` holds, within `deadline`.
pub fn changes(root: &Path, scope: ChangeScope, deadline: Instant) -> Result<Changes, String> {
    let runner = |bytes: usize| {
        let left = deadline.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| {
            GitRunner::with_options(
                root,
                GitOptions {
                    timeout: Some(left),
                    max_output_bytes: Some(bytes),
                },
            )
        })
    };
    let text = |bytes: usize, args: &[&str]| -> Result<String, String> {
        let git = runner(bytes).ok_or("out of time")?;
        let out = git.run(args).map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    };
    let diff_args = ["--no-color", "--no-ext-diff", "--no-renames", "-U0"];
    let (label, subject, diff) = match scope {
        ChangeScope::Latest | ChangeScope::Commit(_) => {
            let rev = match &scope {
                ChangeScope::Commit(sha)
                    if (7..=40).contains(&sha.len())
                        && sha.chars().all(|ch| ch.is_ascii_hexdigit()) =>
                {
                    sha.as_str()
                }
                ChangeScope::Commit(_) => return Err("not a commit id".to_string()),
                _ => "HEAD",
            };
            let subject = text(LINE_BYTES, &["log", "-1", "--format=%h %s", rev]).ok();
            let mut args = vec!["show", "--format="];
            args.extend(diff_args);
            args.push(rev);
            let label = subject
                .as_deref()
                .and_then(|line| line.split_whitespace().next())
                .map_or_else(
                    || "the latest commit".to_string(),
                    |sha| format!("commit {sha}"),
                );
            (
                label,
                subject.map(|line| line.trim().to_string()),
                text(DIFF_BYTES, &args)?,
            )
        }
        ChangeScope::Working => {
            let mut args = vec!["diff"];
            args.extend(diff_args);
            args.push("HEAD");
            (
                "the working tree against HEAD".to_string(),
                None,
                text(DIFF_BYTES, &args)?,
            )
        }
        ChangeScope::Branch => {
            let base = BASES
                .iter()
                .find_map(|base| {
                    text(LINE_BYTES, &["merge-base", "HEAD", base])
                        .ok()
                        .map(|sha| sha.trim().to_string())
                        .filter(|sha| !sha.is_empty())
                })
                .ok_or("no merge base with the default branch")?;
            let mut args = vec!["diff"];
            args.extend(diff_args);
            args.push(&base);
            args.push("HEAD");
            let short: String = base.chars().take(8).collect();
            (
                format!("the branch since {short}"),
                None,
                text(DIFF_BYTES, &args)?,
            )
        }
    };
    let (files, truncated) = parse_diff(&diff);
    if files.is_empty() {
        return Err("no textual change".to_string());
    }
    Ok(Changes {
        label,
        subject,
        files,
        truncated,
    })
}

/// Files, line counts and changed ranges of a `-U0` unified diff, largest
/// change first; the flag is set when more than [`MAX_FILES`] files changed.
pub(crate) fn parse_diff(diff: &str) -> (Vec<ChangedFile>, bool) {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            // `a/x b/x`: with renames off both sides name the same path.
            let path = rest
                .rsplit_once(" b/")
                .map_or(rest, |(_, path)| path)
                .trim_matches('"');
            files.push(ChangedFile {
                path: path.to_string(),
                status: 'M',
                added: 0,
                removed: 0,
                hunks: Vec::new(),
                patch: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if !in_hunk {
            if line.starts_with("new file mode") {
                file.status = 'A';
            } else if line.starts_with("deleted file mode") {
                file.status = 'D';
            } else if let Some(hunk) = line.strip_prefix("@@ ") {
                in_hunk = true;
                if let Some(range) = new_side(hunk) {
                    file.hunks.push(range);
                }
                keep(&mut file.patch, line);
            }
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            if let Some(range) = new_side(hunk) {
                file.hunks.push(range);
            }
            keep(&mut file.patch, line);
        } else if line.starts_with('+') {
            file.added += 1;
            keep(&mut file.patch, line);
        } else if line.starts_with('-') {
            file.removed += 1;
            keep(&mut file.patch, line);
        }
    }
    files.sort_by(|a, b| {
        (b.added + b.removed)
            .cmp(&(a.added + a.removed))
            .then_with(|| a.path.cmp(&b.path))
    });
    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    (files, truncated)
}

/// `line` appended to `patch` while it holds fewer than
/// [`MAX_PATCH_LINES`] lines, each cut to a readable width.
fn keep(patch: &mut Vec<String>, line: &str) {
    if patch.len() < MAX_PATCH_LINES {
        patch.push(line.chars().take(PATCH_LINE_CHARS).collect());
    }
}

/// The lines of the new side a hunk header (`-3,2 +5,4 @@ ...`) changed:
/// `5..=8`; a pure deletion (`+5,0`) is the line it sits after.
fn new_side(header: &str) -> Option<(u64, u64)> {
    let plus = header
        .split_whitespace()
        .find(|word| word.starts_with('+'))?;
    let (start, count) = plus[1..].split_once(',').unwrap_or((&plus[1..], "1"));
    let start: u64 = start.parse().ok()?;
    let count: u64 = count.parse().ok()?;
    let first = start.max(1);
    Some((first, if count == 0 { first } else { start + count - 1 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -3,2 +3,3 @@ fn a() {
-old1
-old2
+new1
+new2
+new3
@@ -20 +21,0 @@ fn b() {
-gone
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+one
+++two
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1,3 +0,0 @@
-x
-y
-z
";

    #[test]
    fn parse_diff_should_read_status_counts_and_the_new_side_of_each_hunk() {
        let (files, truncated) = parse_diff(DIFF);
        assert!(!truncated);
        let summary: Vec<(&str, char, usize, usize)> = files
            .iter()
            .map(|file| (file.path.as_str(), file.status, file.added, file.removed))
            .collect();
        // Largest change first; ties by path.
        assert_eq!(
            summary,
            [
                ("src/a.rs", 'M', 3, 3),
                ("old.txt", 'D', 0, 3),
                ("src/new.rs", 'A', 2, 0)
            ]
        );
        // `-3,2 +3,3` changed 3..=5; the deletion after 20 sits at 21; a new
        // file is all of it; a deleted one has no new side to point at.
        assert_eq!(files[0].hunks, [(3, 5), (21, 21)]);
        assert_eq!(files[2].hunks, [(1, 2)]);
        // The diff is kept as written, headers and +/- lines, not the file markers.
        assert_eq!(
            files[0].patch,
            [
                "@@ -3,2 +3,3 @@ fn a() {",
                "-old1",
                "-old2",
                "+new1",
                "+new2",
                "+new3",
                "@@ -20 +21,0 @@ fn b() {",
                "-gone"
            ]
        );
        assert_eq!(files[1].hunks, [(1, 1)]);
    }

    #[test]
    fn parse_diff_should_cut_the_kept_diff_of_a_long_change() {
        let body: String = (0..MAX_PATCH_LINES + 50)
            .map(|n| format!("+line {n}\n"))
            .collect();
        let (files, _) = parse_diff(&format!("diff --git a/x b/x\n@@ -0,0 +1,300 @@\n{body}"));
        assert_eq!(files[0].patch.len(), MAX_PATCH_LINES);
        assert_eq!(
            files[0].added,
            MAX_PATCH_LINES + 50,
            "the counts stay exact"
        );
    }

    #[test]
    fn parse_diff_should_count_a_plus_plus_plus_line_inside_a_hunk_as_added() {
        let (files, _) = parse_diff(DIFF);
        let new = files.iter().find(|file| file.path == "src/new.rs").unwrap();
        assert_eq!(new.added, 2, "`+++two` is the added line `++two`");
    }

    #[test]
    fn parse_diff_should_cut_the_file_list_and_say_so() {
        let many: String = (0..MAX_FILES + 3)
            .map(|n| format!("diff --git a/f{n}.rs b/f{n}.rs\n@@ -1 +1 @@\n-a\n+b\n"))
            .collect();
        let (files, truncated) = parse_diff(&many);
        assert_eq!(files.len(), MAX_FILES);
        assert!(truncated);
        assert!(parse_diff("").0.is_empty());
        assert!(parse_diff("not a diff\n").0.is_empty());
    }

    #[test]
    fn new_side_should_read_headers_with_and_without_counts() {
        assert_eq!(new_side("-1 +1 @@"), Some((1, 1)));
        assert_eq!(new_side("-1,3 +7,4 @@ ctx"), Some((7, 10)));
        assert_eq!(new_side("-9,2 +8,0 @@"), Some((8, 8)));
        assert_eq!(new_side("-0,0 +1,5 @@"), Some((1, 5)));
        assert_eq!(new_side("garbage"), None);
        assert_eq!(new_side("-1 +x @@"), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-git-changes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        dir
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(20)
    }

    #[test]
    fn changes_should_read_the_latest_commit_the_working_tree_and_the_branch() {
        let dir = fixture("scopes");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        git(&dir, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "second change"]);

        let latest = changes(&dir, ChangeScope::Latest, soon()).unwrap();
        assert!(latest.label.starts_with("commit "), "{}", latest.label);
        assert!(
            latest
                .subject
                .as_deref()
                .unwrap()
                .ends_with("second change")
        );
        let a = latest
            .files
            .iter()
            .find(|file| file.path == "a.txt")
            .unwrap();
        assert_eq!((a.added, a.removed, a.status), (2, 1, 'M'));
        assert_eq!(a.hunks, [(2, 2), (4, 4)]);
        assert!(
            latest
                .files
                .iter()
                .any(|file| file.path == "b.txt" && file.status == 'A')
        );

        // A commit by id; a made-up or malformed id is refused before git runs.
        let sha = latest
            .subject
            .as_deref()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        std::fs::write(dir.join("later.txt"), "x\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "third"]);
        let older = changes(&dir, ChangeScope::Commit(sha), soon()).unwrap();
        assert!(older.subject.as_deref().unwrap().ends_with("second change"));
        assert!(older.files.iter().all(|file| file.path != "later.txt"));
        assert_eq!(
            changes(&dir, ChangeScope::Commit("--output=x".into()), soon()).unwrap_err(),
            "not a commit id"
        );
        assert!(changes(&dir, ChangeScope::Commit("abcdef0".into()), soon()).is_err());

        // The branch left `main` one commit ago: the same files.
        let branch = changes(&dir, ChangeScope::Branch, soon()).unwrap();
        assert!(
            branch.label.starts_with("the branch since "),
            "{}",
            branch.label
        );
        assert_eq!(branch.files.len(), 3);

        // A clean working tree has nothing to review; an edit is the change.
        assert!(changes(&dir, ChangeScope::Working, soon()).is_err());
        std::fs::write(dir.join("b.txt"), "b\nmore\n").unwrap();
        let working = changes(&dir, ChangeScope::Working, soon()).unwrap();
        assert_eq!(working.files.len(), 1);
        assert_eq!(working.files[0].path, "b.txt");
        assert_eq!(working.subject, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changes_should_read_the_only_commit_of_a_repository_and_fail_open_elsewhere() {
        let dir = fixture("root");
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "root"]);
        let root = changes(&dir, ChangeScope::Latest, soon()).unwrap();
        assert_eq!(root.files[0].status, 'A');
        // No remote and a branch that is the default one: nothing to compare to
        // but `main` itself, which has no commits ahead.
        assert!(changes(&dir, ChangeScope::Branch, soon()).is_err());
        // An expired window runs no git call.
        assert_eq!(
            changes(&dir, ChangeScope::Latest, Instant::now()).unwrap_err(),
            "out of time"
        );
        // Not a repository.
        let plain =
            std::env::temp_dir().join(format!("pixel-git-changes-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        assert!(changes(&plain, ChangeScope::Latest, soon()).is_err());
        std::fs::remove_dir_all(&plain).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
