// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The digest a trust record binds to: every file of a plugin directory.

use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustix::fs::FileType;
use sha2::{Digest, Sha256};

use crate::directory::{Directory, Entry};

/// Executable bits: a `chmod +x` on a plugin file is a change.
const EXEC_BITS: rustix::fs::RawMode = 0o111;

/// Lowercase hex of a SHA-256 value.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 over the directory's entries, sorted by relative path: kind, path
/// and content of every file, the target text of every symlink (never
/// followed), the executable bits of files, and empty directories. Adding,
/// removing, renaming, editing or re-linking anything changes the digest.
pub fn dir_digest(dir: &Path) -> io::Result<String> {
    dir_digest_with(dir, &mut |_| {})
}

fn dir_digest_with(dir: &Path, before: &mut impl FnMut(&Path)) -> io::Result<String> {
    let mut entries = Vec::new();
    let root = Rc::new(Directory::open(dir)?);
    collect(&root, dir, Path::new(""), &mut entries, before)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (relative, parent, entry) in &entries {
        let path = relative.as_os_str().as_bytes();
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        let kind = entry.kind();
        if kind == FileType::Symlink {
            let target = parent.read_link(entry)?;
            let target = target.as_os_str().as_bytes();
            hasher.update(b"l");
            hasher.update((target.len() as u64).to_le_bytes());
            hasher.update(target);
        } else if kind == FileType::Directory {
            hasher.update(b"d");
        } else if kind == FileType::RegularFile {
            hasher.update(b"f");
            hasher.update([u8::from(entry.stat.st_mode & EXEC_BITS != 0)]);
            hasher.update(entry.len().to_le_bytes());
            // Listed as a regular file: opened as that very file, never
            // through a link or a FIFO put at its name since.
            let mut file = parent.open_file(entry)?;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
        } else {
            hasher.update(b"o");
        }
    }
    Ok(hex(&hasher.finalize()))
}

// One descriptor per source directory stays alive through sorted hashing.
// Only display paths are reconstructed; all reads use the pinned parent.
type Listed = (PathBuf, Rc<Directory>, Entry);

fn collect(
    parent: &Rc<Directory>,
    root: &Path,
    relative: &Path,
    out: &mut Vec<Listed>,
    before: &mut impl FnMut(&Path),
) -> io::Result<()> {
    for entry in parent.entries()? {
        let path = relative.join(&entry.name);
        before(&root.join(&path));
        if entry.kind() == FileType::Directory {
            let child = Rc::new(parent.descend(&entry)?);
            collect(&child, root, &path, out, before)?;
        }
        out.push((path, Rc::clone(parent), entry));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn seed() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::write(dir.path().join("bin/run"), "#!/bin/sh\n").unwrap();
        fs::write(dir.path().join("pixel-plugin.toml"), "x").unwrap();
        dir
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn the_digest_is_stable_and_64_hex_digits() {
        let dir = seed();
        let first = dir_digest(dir.path()).unwrap();
        assert_eq!(
            first,
            "2352c280cf9f346e771570051cfe715a9dce4363f1af9376af12068816c4e09d"
        );
        assert_eq!(first.len(), 64);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(dir_digest(dir.path()).unwrap(), first);
    }

    #[test]
    fn an_identical_copy_has_the_same_digest() {
        let a = seed();
        let b = seed();
        assert_eq!(dir_digest(a.path()).unwrap(), dir_digest(b.path()).unwrap());
    }

    #[test]
    fn every_kind_of_change_moves_the_digest() {
        let dir = seed();
        let base = dir_digest(dir.path()).unwrap();
        let changed = |label: &str, f: &dyn Fn(&Path)| {
            let d = seed();
            f(d.path());
            assert_ne!(dir_digest(d.path()).unwrap(), base, "{label}");
        };
        changed("edit", &|p| {
            fs::write(p.join("bin/run"), "#!/bin/sh\nx\n").unwrap()
        });
        changed("same length edit", &|p| {
            fs::write(p.join("bin/run"), "#!/bin/zh\n").unwrap()
        });
        changed("new file", &|p| fs::write(p.join("extra"), "").unwrap());
        changed("empty dir", &|p| fs::create_dir(p.join("empty")).unwrap());
        changed("removal", &|p| {
            fs::remove_file(p.join("pixel-plugin.toml")).unwrap()
        });
        changed("rename", &|p| {
            fs::rename(p.join("bin/run"), p.join("bin/run2")).unwrap();
        });
        changed("chmod +x", &|p| {
            fs::set_permissions(p.join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
        });
        changed("symlink", &|p| {
            std::os::unix::fs::symlink("/etc/hosts", p.join("l")).unwrap()
        });
    }

    #[test]
    fn a_symlink_is_hashed_by_target_text_and_never_followed() {
        let a = seed();
        let b = seed();
        std::os::unix::fs::symlink("one", a.path().join("l")).unwrap();
        std::os::unix::fs::symlink("two", b.path().join("l")).unwrap();
        assert_ne!(dir_digest(a.path()).unwrap(), dir_digest(b.path()).unwrap());
        // A dangling link does not fail the walk.
        assert!(dir_digest(a.path()).is_ok());
    }

    #[test]
    fn file_content_moving_between_names_is_not_confused() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::write(a.path().join("ab"), "c").unwrap();
        fs::write(b.path().join("a"), "bc").unwrap();
        assert_ne!(dir_digest(a.path()).unwrap(), dir_digest(b.path()).unwrap());
    }

    #[test]
    fn digest_should_refuse_a_directory_replaced_after_listing() {
        for linked in [true, false] {
            let dir = seed();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), "outside secret").unwrap();
            let mut swapped = false;
            let result = dir_digest_with(dir.path(), &mut |path| {
                if path == dir.path().join("bin") && !swapped {
                    swapped = true;
                    fs::rename(path, dir.path().join("held")).unwrap();
                    if linked {
                        std::os::unix::fs::symlink(outside.path(), path).unwrap();
                    } else {
                        fs::create_dir(path).unwrap();
                        fs::write(path.join("secret"), "replacement").unwrap();
                    }
                }
            });
            assert!(swapped);
            assert!(
                result.is_err(),
                "accepted a replaced directory (link={linked})"
            );
        }
    }

    #[test]
    fn digest_should_keep_reading_a_directory_pinned_before_its_ancestor_is_relinked() {
        let dir = seed();
        let expected = dir_digest(dir.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("run"), "outside secret").unwrap();
        let held = tempfile::tempdir().unwrap();
        let mut swapped = false;
        let actual = dir_digest_with(dir.path(), &mut |path| {
            if path == dir.path().join("bin/run") && !swapped {
                swapped = true;
                fs::rename(dir.path().join("bin"), held.path().join("bin")).unwrap();
                std::os::unix::fs::symlink(outside.path(), dir.path().join("bin")).unwrap();
            }
        })
        .unwrap();
        assert!(swapped);
        assert_eq!(actual, expected);
    }

    #[test]
    fn digest_should_preserve_the_legacy_global_path_order_and_encoding() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::create_dir(dir.path().join("empty")).unwrap();
        fs::write(dir.path().join("a."), "x").unwrap();
        fs::write(dir.path().join("a/run"), "y").unwrap();
        fs::set_permissions(dir.path().join("a/run"), fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("target", dir.path().join("nλ")).unwrap();
        assert_eq!(
            dir_digest(dir.path()).unwrap(),
            "e4093c7d70e28f989b5116f8b8235732c3e6892c042bd1acdf1cd65452b6e459"
        );
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        assert!(dir_digest(Path::new("/nonexistent/pixel-plugin-test")).is_err());
    }
}
