// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The prebuilt binary of a `github:` plugin: a release asset named
//! `pixel-<name>-<target-triple>`, downloaded with `curl` (as `install.sh`
//! does) and checked against `<asset>.sha256`. A release that publishes no
//! digest is refused unless the caller passes [`Unverified::Allow`].

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::hash::hex;

/// A prebuilt binary is a single executable; a larger download is not one.
const MAX_ASSET_BYTES: u64 = 268_435_456;
/// A slow link may take a while, a stalled one must not hang the install.
const DOWNLOAD_TIMEOUT_SECS: u32 = 300;
/// `curl -f` exits 22 on an HTTP error status (404: no such asset).
const CURL_HTTP_ERROR: i32 = 22;
/// `curl` exits 37 when a `file://` URL cannot be read (the test seam's 404).
const CURL_FILE_UNREADABLE: i32 = 37;
/// Executable bits given to a downloaded program.
const PROGRAM_MODE: u32 = 0o755;

/// What a finished download proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub url: String,
    /// Whether a published `.sha256` matched the bytes.
    pub verified: bool,
    /// The sha256 of the installed bytes, lowercase hex, published or not.
    pub sha256: String,
}

/// Whether a release asset with no published `.sha256` may be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unverified {
    /// Refuse it: nothing pins the bytes that will run (the default).
    #[default]
    Refuse,
    /// Install it anyway, reported as unverified (`--allow-unverified`).
    Allow,
}

/// The target triple of the release assets for the machine running pixel,
/// spelled as the pixel releases name theirs; `None` where none is published.
pub fn release_target() -> Option<String> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn target_for(os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" | "aarch64" => arch,
        _ => return None,
    };
    let tail = match os {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        _ => return None,
    };
    Some(format!("{arch}-{tail}"))
}

/// The release asset of plugin `name` for `target`.
pub fn asset_name(name: &str, target: &str) -> String {
    format!("pixel-{name}-{target}")
}

/// The digest in the text of a `.sha256` file: its first word, when that is
/// 64 hex digits (`sha256sum` output, or the bare digest).
pub fn parse_sha256(text: &str) -> Option<String> {
    let word = text.split_whitespace().next()?.to_ascii_lowercase();
    (word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit())).then_some(word)
}

/// Download `url` to `dest`, mark it executable and check it against
/// `<url>.sha256`; without that file the download is refused unless
/// `unverified` allows it. The bytes land beside `dest` first and replace it
/// only once checked, so a mismatch, a missing digest or a failed download
/// leaves `dest` as it was: absent, or the program the repository shipped.
pub fn download_program(
    url: &str,
    dest: &Path,
    allow_file: bool,
    unverified: Unverified,
) -> Result<Downloaded, String> {
    let Some(name) = dest.file_name() else {
        return Err(format!("{} names no file", dest.display()));
    };
    let partial = dest.with_file_name(format!(".{}.download", name.to_string_lossy()));
    let result = download_checked(url, &partial, allow_file, unverified).and_then(|downloaded| {
        std::fs::rename(&partial, dest)
            .map(|()| downloaded)
            .map_err(|e| format!("cannot install {}: {e}", dest.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn download_checked(
    url: &str,
    dest: &Path,
    allow_file: bool,
    unverified: Unverified,
) -> Result<Downloaded, String> {
    curl(url, dest, allow_file).map_err(|e| format!("cannot download {url}: {}", e.message))?;
    let checksum = checksum_of(&format!("{url}.sha256"), allow_file)?;
    let bytes = std::fs::read(dest).map_err(|e| format!("cannot read {}: {e}", dest.display()))?;
    let actual = hex(&Sha256::digest(&bytes));
    let verified = match checksum {
        None if unverified == Unverified::Refuse => {
            return Err(format!(
                "{url}.sha256 is not published, so the download cannot be verified (sha256 {actual}); pass --allow-unverified to install it anyway"
            ));
        }
        None => false,
        Some(expected) if actual != expected => {
            return Err(format!(
                "{url} does not match its published sha256 (expected {expected}, got {actual})"
            ));
        }
        Some(_) => true,
    };
    set_executable(dest)?;
    Ok(Downloaded {
        url: url.to_owned(),
        verified,
        sha256: actual,
    })
}

/// The published digest of an asset: `None` when the release has none, an
/// error when it has one that cannot be read or is not a digest.
fn checksum_of(url: &str, allow_file: bool) -> Result<Option<String>, String> {
    let scratch =
        tempfile::tempdir().map_err(|e| format!("cannot create a scratch directory: {e}"))?;
    let file = scratch.path().join("asset.sha256");
    match curl(url, &file, allow_file) {
        Ok(()) => {}
        Err(CurlError {
            code: Some(CURL_HTTP_ERROR | CURL_FILE_UNREADABLE),
            ..
        }) => return Ok(None),
        Err(e) => return Err(format!("cannot download {url}: {}", e.message)),
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read the downloaded {url}: {e}"))?;
    parse_sha256(&text)
        .map(Some)
        .ok_or_else(|| format!("{url} does not hold a sha256 digest"))
}

fn set_executable(path: &Path) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PROGRAM_MODE))
        .map_err(|e| format!("cannot make {} executable: {e}", path.display()))
}

struct CurlError {
    code: Option<i32>,
    message: String,
}

/// `curl` restricted to HTTPS (HTTP redirects included), or to `file://`
/// when the host redirected `github:` sources to a local directory.
#[cfg_attr(test, mutants::skip)] // thin adapter over the curl process; the argv is tested
fn curl(url: &str, dest: &Path, allow_file: bool) -> Result<(), CurlError> {
    let output = Command::new("curl")
        .args(curl_args(url, dest, allow_file))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| CurlError {
            code: None,
            message: format!("cannot run curl: {e}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(CurlError {
        code: output.status.code(),
        message: if reason.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            reason
        },
    })
}

fn curl_args(url: &str, dest: &Path, allow_file: bool) -> Vec<String> {
    let proto = if allow_file { "=file" } else { "=https" };
    [
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        proto,
        "--proto-redir",
        "=https",
        "--max-time",
        &DOWNLOAD_TIMEOUT_SECS.to_string(),
        "--max-filesize",
        &MAX_ASSET_BYTES.to_string(),
        "--output",
        &dest.to_string_lossy(),
        "--",
        url,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The directory that will hold `<staged>/<run>`, created component by
/// component. A symlink or a file where a directory belongs is refused: the
/// repository chooses what it ships, and a link would carry the download
/// out of the plugin directory.
pub fn program_parent(staged: &Path, run: &str) -> Result<PathBuf, String> {
    let mut current = staged.to_path_buf();
    let parts: Vec<_> = Path::new(run).components().collect();
    let Some((_, parents)) = parts.split_last() else {
        return Err(format!("`run` = \"{run}\" names no file"));
    };
    for part in parents {
        let Component::Normal(part) = part else {
            return Err(format!("`run` = \"{run}\" is not a plain relative path"));
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is not a directory in the plugin; refusing to download into it",
                    current.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .map_err(|e| format!("cannot create {}: {e}", current.display()))?;
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", current.display())),
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_target_is_the_triple_the_releases_use() {
        assert_eq!(
            target_for("macos", "aarch64").as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            target_for("macos", "x86_64").as_deref(),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(
            target_for("linux", "x86_64").as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_for("linux", "aarch64").as_deref(),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(target_for("windows", "x86_64"), None);
        assert_eq!(target_for("linux", "riscv64"), None);
        assert_eq!(
            asset_name("git", "aarch64-apple-darwin"),
            "pixel-git-aarch64-apple-darwin"
        );
    }

    #[test]
    fn a_digest_file_is_the_first_word_in_either_form() {
        let digest = "ab".repeat(32);
        assert_eq!(parse_sha256(&digest).as_deref(), Some(digest.as_str()));
        assert_eq!(
            parse_sha256(&format!("{}  pixel-git-x\n", digest.to_uppercase())).as_deref(),
            Some(digest.as_str())
        );
        for bad in ["", "   \n", "abc", &"zz".repeat(32), &"ab".repeat(33)] {
            assert_eq!(parse_sha256(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn curl_is_limited_to_one_protocol_and_cannot_read_an_option_as_the_url() {
        let https = curl_args("https://h/x", Path::new("/d/x"), false);
        let at = |args: &[String], flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(at(&https, "--proto").as_deref(), Some("=https"));
        assert_eq!(at(&https, "--proto-redir").as_deref(), Some("=https"));
        assert_eq!(at(&https, "--max-filesize").as_deref(), Some("268435456"));
        assert_eq!(at(&https, "--max-time").as_deref(), Some("300"));
        assert_eq!(at(&https, "--output").as_deref(), Some("/d/x"));
        assert_eq!(&https[https.len() - 2..], ["--", "https://h/x"]);
        let local = curl_args("file:///x", Path::new("/d/x"), true);
        assert_eq!(at(&local, "--proto").as_deref(), Some("=file"));
        assert_eq!(at(&local, "--proto-redir").as_deref(), Some("=https"));
    }

    #[test]
    fn download_should_refuse_when_the_release_publishes_no_digest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("asset");
        fs::write(&src, "#!/bin/sh\n").unwrap();
        let dest = dir.path().join("out");
        let url = format!("file://{}", src.display());
        let err = download_program(&url, &dest, true, Unverified::Refuse).unwrap_err();
        let digest = hex(&Sha256::digest(b"#!/bin/sh\n"));
        assert_eq!(
            err,
            format!(
                "{url}.sha256 is not published, so the download cannot be verified (sha256 {digest}); pass --allow-unverified to install it anyway"
            )
        );
        assert!(!dest.exists(), "an unverifiable download is not installed");
        assert!(
            !dir.path().join(".out.download").exists(),
            "nor left beside it"
        );
    }

    #[test]
    fn download_should_install_unverified_when_allowed_and_no_digest_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("asset");
        fs::write(&src, "#!/bin/sh\n").unwrap();
        let dest = dir.path().join("out");
        let url = format!("file://{}", src.display());
        let got = download_program(&url, &dest, true, Unverified::Allow).unwrap();
        assert_eq!(
            got,
            Downloaded {
                url,
                verified: false,
                sha256: hex(&Sha256::digest(b"#!/bin/sh\n")),
            }
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "#!/bin/sh\n");
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn a_matching_digest_verifies_and_a_wrong_one_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("asset");
        fs::write(&src, "payload").unwrap();
        let good = hex(&Sha256::digest(b"payload"));
        fs::write(dir.path().join("asset.sha256"), format!("{good}  asset\n")).unwrap();
        let url = format!("file://{}", src.display());
        let dest = dir.path().join("out");
        assert_eq!(
            download_program(&url, &dest, true, Unverified::Refuse).unwrap(),
            Downloaded {
                url: url.clone(),
                verified: true,
                sha256: good,
            }
        );

        fs::remove_file(&dest).unwrap();
        fs::write(dir.path().join("asset.sha256"), "00".repeat(32)).unwrap();
        let err = download_program(&url, &dest, true, Unverified::Allow).unwrap_err();
        assert!(err.contains("does not match its published sha256"), "{err}");
        assert!(!dest.exists(), "a rejected download is removed");

        fs::write(dir.path().join("asset.sha256"), "not a digest").unwrap();
        let err = download_program(&url, &dest, true, Unverified::Allow).unwrap_err();
        assert!(err.contains("does not hold a sha256 digest"), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn a_missing_asset_is_an_error_naming_the_url() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}/nope", dir.path().display());
        let dest = dir.path().join("out");
        let err = download_program(&url, &dest, true, Unverified::Allow).unwrap_err();
        assert!(err.starts_with(&format!("cannot download {url}")), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn a_file_url_is_refused_unless_the_host_allows_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("asset");
        fs::write(&src, "x").unwrap();
        let dest = dir.path().join("out");
        let url = format!("file://{}", src.display());
        assert!(download_program(&url, &dest, false, Unverified::Allow).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn the_program_directory_is_built_inside_the_plugin_only() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        fs::create_dir_all(&staged).unwrap();
        assert_eq!(program_parent(&staged, "run").unwrap(), staged);
        let nested = program_parent(&staged, "bin/arch/run").unwrap();
        assert_eq!(nested, staged.join("bin/arch"));
        assert!(nested.is_dir());

        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, staged.join("linked")).unwrap();
        let err = program_parent(&staged, "linked/run").unwrap_err();
        assert!(err.contains("not a directory in the plugin"), "{err}");
        fs::write(staged.join("file"), "x").unwrap();
        assert!(program_parent(&staged, "file/run").is_err());
        assert!(program_parent(&staged, "../run").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }
}
