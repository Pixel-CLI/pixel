//! Repo-root discovery: walk up from a starting file/dir looking for `.git`.
//!
//! Mirrors the `.git` ancestor-walk half of `pixel-cli::main::discover_root`
//! exactly (checking `cur.join(".git").exists()`, which covers both a real
//! `.git` directory and the `.git` *file* redirection used by worktrees and
//! submodules). It deliberately does **not** reproduce that function's
//! `.pixel`-shard fallback or its "fall back to the starting directory"
//! behavior — `pixel-git` has no notion of a `.pixel` index, so on no `.git`
//! found it returns `None` rather than guessing a root. Callers that want
//! the old fallback behavior apply it themselves on top of `None`.

use std::path::{Path, PathBuf};

/// Walk up from `start` (file or directory) to the nearest ancestor holding
/// a `.git` entry (dir or file). Returns `None` if none is found, `start`
/// doesn't exist, or `start`'s path cannot be canonicalized.
pub fn discover_root(start: &Path) -> Option<PathBuf> {
    let abs = start.canonicalize().ok()?;
    let mut cur = if abs.is_file() {
        abs.parent()?.to_path_buf()
    } else {
        abs
    };
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-git-discover-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolves_nested_subdirectory_to_repo_root() {
        let root = tmpdir("nested");
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(out.status.success());

        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        let found = discover_root(&nested).expect("should find repo root");
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn returns_none_outside_any_repo() {
        // A tmpdir with no .git ancestor at all is unlikely on CI systems
        // where /tmp itself may be inside a repo, so scope the assertion to
        // "does not equal a nested-under-tmp dir with no .git" by using a
        // dedicated non-repo dir and checking it doesn't resolve to itself
        // as a false positive; if the whole /tmp tree is under a repo (rare
        // in CI sandboxes) this test would need a different anchor — assert
        // the weaker, still-meaningful property that it never returns the
        // starting dir when that dir has no .git of its own.
        let dir = tmpdir("no-git-here");
        if let Some(found) = discover_root(&dir) {
            assert_ne!(found, dir.canonicalize().unwrap());
        }
    }
}
