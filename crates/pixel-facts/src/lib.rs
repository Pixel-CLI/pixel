// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-facts — M3 / Engine 2: history-wide fact & diff ingest, search,
//! lifecycle, and rescue-v2 discovery. Owns `.pixel/history.db` plus trigram
//! history segments, with a dedicated low-priority ingest thread that never
//! blocks queries.

pub mod excavate;
pub mod ingest;
pub mod lifecycle;
pub mod poison;
pub mod search;
pub mod store;
pub mod text_index;

pub use store::{FactsError, FactsStore, IndexState};

/// Git fixtures shared by the unit tests of every module: a throwaway
/// repository built with real `git`, so the parsers and the phase
/// functions are exercised against the plumbing they wrap.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::Path;
    use std::process::Command;

    pub(crate) fn git(root: &Path, args: &[&str]) -> String {
        git_env(root, args, &[])
    }

    /// `git` with extra environment variables (a fixed commit date).
    pub(crate) fn git_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .envs(env.iter().copied())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An empty repository on `develop` with identity configured.
    pub(crate) fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "develop"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    /// Write `files`, stage everything and commit; returns the commit oid.
    pub(crate) fn commit(root: &Path, files: &[(&str, &[u8])], message: &str) -> String {
        for (path, bytes) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, bytes).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    /// `commit` with author and committer dates at `unix` seconds (UTC).
    pub(crate) fn commit_at(
        root: &Path,
        files: &[(&str, &[u8])],
        message: &str,
        unix: i64,
    ) -> String {
        for (path, bytes) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, bytes).unwrap();
        }
        let date = format!("{unix} +0000");
        let env = [
            ("GIT_AUTHOR_DATE", date.as_str()),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ];
        git(root, &["add", "-A"]);
        git_env(root, &["commit", "-q", "-m", message], &env);
        git(root, &["rev-parse", "HEAD"])
    }

    /// Unix seconds `days` days before now: fixture dates relative to the
    /// clock, so a window test means the same thing whenever it runs.
    pub(crate) fn days_ago(days: i64) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        i64::try_from(now).unwrap() - days * 86_400
    }

    /// `commit`, but with a raw author and committer date (`"<unix> <tz>"`)
    /// written into the object as is: `git commit` rewrites an offset it
    /// finds invalid, while real histories hold some (`+51800`). Built on
    /// top of HEAD, which must exist; moves the branch to it, returns its oid.
    pub(crate) fn commit_raw_date(
        root: &Path,
        files: &[(&str, &[u8])],
        message: &str,
        raw_date: &str,
    ) -> String {
        for (path, bytes) in files {
            std::fs::write(root.join(path), bytes).unwrap();
        }
        git(root, &["add", "-A"]);
        let tree = git(root, &["write-tree"]);
        let parent = git(root, &["rev-parse", "HEAD"]);
        let object = format!(
            "tree {tree}\nparent {parent}\nauthor t <t@example.com> {raw_date}\ncommitter t <t@example.com> {raw_date}\n\n{message}\n"
        );
        let file = root.join(".git").join("raw-commit");
        std::fs::write(&file, object).unwrap();
        let oid = git(
            root,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                file.to_str().unwrap(),
            ],
        );
        std::fs::remove_file(file).unwrap();
        git(root, &["update-ref", "HEAD", &oid]);
        oid
    }

    /// Two commits: `src/main.rs` added, then extended with `fn helper`.
    pub(crate) fn two_commit_repo() -> (tempfile::TempDir, String, String) {
        let dir = init_repo();
        let first = commit(
            dir.path(),
            &[(
                "src/main.rs",
                b"fn main() {\n    println!(\"hello world\");\n}\n",
            )],
            "Add main with hello world greeting",
        );
        let second = commit(
            dir.path(),
            &[(
                "src/main.rs",
                b"fn main() {\n    println!(\"hello world\");\n}\n\nfn helper() {\n    let secret_token = 42;\n}\n",
            )],
            "Add helper with secret_token variable",
        );
        (dir, first, second)
    }
}
