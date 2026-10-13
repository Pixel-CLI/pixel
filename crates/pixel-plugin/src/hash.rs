// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The digest a trust record binds to: every file of a plugin directory.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Executable bits: a `chmod +x` on a plugin file is a change.
const EXEC_BITS: u32 = 0o111;

/// Lowercase hex of a SHA-256 value.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 over the directory's entries, sorted by relative path: kind, path
/// and content of every file, the target text of every symlink (never
/// followed), the executable bits of files, and empty directories. Adding,
/// removing, renaming, editing or re-linking anything changes the digest.
pub fn dir_digest(dir: &Path) -> io::Result<String> {
    let mut entries = Vec::new();
    collect(dir, Path::new(""), &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for relative in &entries {
        let full = dir.join(relative);
        let meta = fs::symlink_metadata(&full)?;
        let path = relative.as_os_str().as_bytes();
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(&full)?;
            let target = target.as_os_str().as_bytes();
            hasher.update(b"l");
            hasher.update((target.len() as u64).to_le_bytes());
            hasher.update(target);
        } else if kind.is_dir() {
            hasher.update(b"d");
        } else if kind.is_file() {
            hasher.update(b"f");
            hasher.update([u8::from(meta.permissions().mode() & EXEC_BITS != 0)]);
            hasher.update(meta.len().to_le_bytes());
            let mut file = File::open(&full)?;
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

fn collect(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let is_dir = entry.file_type()?.is_dir();
        out.push(path.clone());
        if is_dir {
            collect(root, &path, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_missing_directory_is_an_error() {
        assert!(dir_digest(Path::new("/nonexistent/pixel-plugin-test")).is_err());
    }
}
