// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Confinement of repository-relative paths to the repository root.
//!
//! Paths read back from pixel's stores (an index shard's file table, the
//! graph database) are joined to the root and read or written. Those stores
//! can come from a hostile clone, so a stored path is only used when it is a
//! plain relative path (no root, no `.` or `..`) whose directory, once links
//! are resolved, still lies inside the root.

use std::io;
use std::path::{Path, PathBuf};

/// Whether `rel` is a plain repository-relative path: non-empty, not
/// absolute, no `.` or `..` component, no NUL byte. A trailing or doubled
/// `/` is tolerated, as `Path` does.
pub fn is_normal_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\0')
        && !rel.starts_with('/')
        && rel.split('/').all(|part| part != "." && part != "..")
}

fn escapes(rel: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{rel:?} is not a path inside the repository"),
    )
}

/// `root.join(rel)` when `rel` is [`is_normal_relative`] and its parent
/// directory, links resolved, lies inside `root` (links resolved too). The
/// file itself is not resolved: the caller opens it without following a link
/// (`crate::nofollow`) or checks it with `symlink_metadata`.
///
/// # Errors
///
/// `InvalidInput` when `rel` is not a plain relative path or its directory
/// resolves outside `root`; the I/O error of resolving either directory
/// (`NotFound` when the parent does not exist).
pub fn confine(root: &Path, rel: &str) -> io::Result<PathBuf> {
    if !is_normal_relative(rel) {
        return Err(escapes(rel));
    }
    let abs = root.join(rel);
    let parent = abs.parent().ok_or_else(|| escapes(rel))?;
    if !parent.canonicalize()?.starts_with(root.canonicalize()?) {
        return Err(escapes(rel));
    }
    Ok(abs)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::testutil::tmpdir;

    #[test]
    fn is_normal_relative_should_accept_only_plain_relative_paths() {
        for ok in [
            "a.rs",
            "src/a.rs",
            "src//a.rs",
            "src/",
            ".github/x.yml",
            "a..b",
        ] {
            assert!(is_normal_relative(ok), "{ok:?} is plain");
        }
        for bad in [
            "",
            "/etc/passwd",
            "..",
            "../x",
            "a/../../x",
            "a/./b",
            ".",
            "./a",
            "a\0b",
        ] {
            assert!(!is_normal_relative(bad), "{bad:?} is not plain");
        }
    }

    #[test]
    fn confine_should_join_a_plain_path_inside_the_root() {
        let root = tmpdir("confine-ok");
        fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(confine(&root, "src/a.rs").unwrap(), root.join("src/a.rs"));
        // A link that stays inside the root is not an escape.
        symlink(root.join("src"), root.join("inner")).unwrap();
        assert_eq!(
            confine(&root, "inner/a.rs").unwrap(),
            root.join("inner/a.rs")
        );
    }

    #[test]
    fn confine_should_refuse_a_path_that_leaves_the_root() {
        let root = tmpdir("confine-escape");
        let outside = tmpdir("confine-outside");
        symlink(&outside, root.join("link")).unwrap();
        for rel in ["../x", "/etc/passwd", "link/credentials", "link/a/b"] {
            let error = confine(&root, rel).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::Other, "{rel}: {error:?}");
        }
        assert_eq!(
            confine(&root, "link/credentials").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            confine(&root, "missing/a.rs").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
