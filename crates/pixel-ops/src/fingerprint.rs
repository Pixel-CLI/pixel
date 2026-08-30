//! File fingerprinting — byte-exact port of usable-git's
//! `src/git/status.ts` (porcelain v2 parsing) + `src/git/fingerprint.ts`
//! (the fingerprint hash itself).
//!
//! A fingerprint is `sha256(JSON-header ++ type-tagged content marker)`.
//! The JSON header must be serialized with the EXACT key order the TS
//! reference uses (`path, originalPath, indexStatus, worktreeStatus,
//! indexOid, kind, conflicted`) and JS's compact `JSON.stringify` spacing
//! (no spaces) — `serde_json`'s `Map` does not preserve insertion order
//! without the `preserve_order` feature (not enabled in this workspace),
//! so the header is built by hand rather than through `serde_json::json!`.
//!
//! Any drift here silently changes every snapshot/fingerprint token, so
//! this is deliberately a literal, unabbreviated port rather than a
//! "close enough" reimplementation.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Port of usable-git's `StatusChange` (`src/git/status.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChange {
    pub path: String,
    pub original_path: Option<String>,
    pub index_status: String,
    pub worktree_status: String,
    pub index_oid: Option<String>,
    /// One of "ordinary" | "renamed" | "unmerged" | "untracked" | "ignored".
    pub kind: &'static str,
    pub conflicted: bool,
}

/// Port of `ordinary()` in status.ts: `1 XY sub mH mI mW hH hI path`.
fn parse_ordinary(record: &str) -> Option<StatusChange> {
    let rest = record.strip_prefix("1 ")?;
    let parts: Vec<&str> = rest.splitn(8, ' ').collect();
    if parts.len() != 8 {
        return None;
    }
    let xy = parts[0];
    if xy.len() != 2 {
        return None;
    }
    let mut chars = xy.chars();
    let index_status = chars.next().unwrap_or('.');
    let worktree_status = chars.next().unwrap_or('.');
    Some(StatusChange {
        path: parts[7].to_string(),
        original_path: None,
        index_status: index_status.to_string(),
        worktree_status: worktree_status.to_string(),
        index_oid: Some(parts[6].to_string()),
        kind: "ordinary",
        conflicted: xy.contains('U'),
    })
}

/// Port of `renamed()` in status.ts: `2 XY sub mH mI mW hH hI X<score> path`
/// with `originalPath` carried in the following NUL-separated record.
fn parse_renamed(record: &str, original_path: Option<&str>) -> Option<StatusChange> {
    let rest = record.strip_prefix("2 ")?;
    let parts: Vec<&str> = rest.splitn(9, ' ').collect();
    if parts.len() != 9 {
        return None;
    }
    let xy = parts[0];
    if xy.len() != 2 {
        return None;
    }
    let mut chars = xy.chars();
    let index_status = chars.next().unwrap_or('.');
    let worktree_status = chars.next().unwrap_or('.');
    Some(StatusChange {
        path: parts[8].to_string(),
        original_path: original_path.map(str::to_string),
        index_status: index_status.to_string(),
        worktree_status: worktree_status.to_string(),
        index_oid: Some(parts[6].to_string()),
        kind: "renamed",
        conflicted: xy.contains('U'),
    })
}

/// Port of `unmerged()` in status.ts: `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
fn parse_unmerged(record: &str) -> Option<StatusChange> {
    let rest = record.strip_prefix("u ")?;
    let parts: Vec<&str> = rest.splitn(10, ' ').collect();
    if parts.len() != 10 {
        return None;
    }
    let xy = parts[0];
    if xy.len() != 2 {
        return None;
    }
    let mut chars = xy.chars();
    let index_status = chars.next().unwrap_or('U');
    let worktree_status = chars.next().unwrap_or('U');
    Some(StatusChange {
        path: parts[9].to_string(),
        original_path: None,
        index_status: index_status.to_string(),
        worktree_status: worktree_status.to_string(),
        index_oid: Some(parts[7].to_string()),
        kind: "unmerged",
        conflicted: true,
    })
}

/// Port of `parsePorcelainV2()` in status.ts (the change-collection half —
/// branch header lines are irrelevant to fingerprinting and are skipped,
/// callers should not pass `--branch`).
pub fn parse_porcelain_v2(output: &str) -> Vec<StatusChange> {
    let records: Vec<&str> = output.split('\0').collect();
    let mut changes = Vec::new();
    let mut i = 0usize;
    while i < records.len() {
        let record = records[i];
        if record.is_empty() {
            i += 1;
            continue;
        }
        if record.starts_with("1 ") {
            if let Some(change) = parse_ordinary(record) {
                changes.push(change);
            }
        } else if record.starts_with("2 ") {
            let original = records.get(i + 1).copied();
            if let Some(change) = parse_renamed(record, original) {
                changes.push(change);
            }
            i += 1; // consume the originalPath record, same as the TS loop.
        } else if record.starts_with("u ") {
            if let Some(change) = parse_unmerged(record) {
                changes.push(change);
            }
        } else if let Some(path) = record.strip_prefix("? ") {
            changes.push(StatusChange {
                path: path.to_string(),
                original_path: None,
                index_status: "?".to_string(),
                worktree_status: "?".to_string(),
                index_oid: None,
                kind: "untracked",
                conflicted: false,
            });
        } else if let Some(path) = record.strip_prefix("! ") {
            changes.push(StatusChange {
                path: path.to_string(),
                original_path: None,
                index_status: "!".to_string(),
                worktree_status: "!".to_string(),
                index_oid: None,
                kind: "ignored",
                conflicted: false,
            });
        }
        i += 1;
    }
    changes
}

/// Run `git status --porcelain=v2 -z --untracked-files=all -- <path>` and
/// return the `StatusChange` for `path`, or a synthetic "clean" change if
/// `path` has no pending changes (not produced by the reference, which
/// only ever fingerprints paths already known to be in the change list;
/// kept as a defensive fallback).
pub fn status_change_for_path(root: &Path, path: &str) -> StatusChange {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--",
            path,
        ])
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        let changes = parse_porcelain_v2(&text);
        if let Some(found) = changes.into_iter().find(|c| c.path == path) {
            return found;
        }
    }
    StatusChange {
        path: path.to_string(),
        original_path: None,
        index_status: ".".to_string(),
        worktree_status: ".".to_string(),
        index_oid: None,
        kind: "ordinary",
        conflicted: false,
    }
}

/// JSON-escape a string exactly the way JS's `JSON.stringify` would.
/// `serde_json` cannot be used for the header itself (see module docs),
/// but its string-escaping is byte-compatible with JS for our purposes,
/// so we reuse `serde_json::to_string` on a bare `&str` for each value —
/// that is JS-string-escape-set compatible and needs no bespoke escaper.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn json_string_or_null(value: Option<&str>) -> String {
    match value {
        Some(v) => json_string(v),
        None => "null".to_string(),
    }
}

/// Build the exact JSON header bytes the TS reference hashes:
/// `JSON.stringify({path, originalPath, indexStatus, worktreeStatus,
/// indexOid, kind, conflicted})`, with that literal key order and no
/// extra whitespace.
fn json_header(change: &StatusChange) -> String {
    format!(
        "{{\"path\":{},\"originalPath\":{},\"indexStatus\":{},\"worktreeStatus\":{},\"indexOid\":{},\"kind\":{},\"conflicted\":{}}}",
        json_string(&change.path),
        json_string_or_null(change.original_path.as_deref()),
        json_string(&change.index_status),
        json_string(&change.worktree_status),
        json_string_or_null(change.index_oid.as_deref()),
        json_string(change.kind),
        change.conflicted,
    )
}

/// Byte-exact port of `fingerprintChange()` in fingerprint.ts.
///
/// Hashes the JSON header, then a type-tagged marker over the on-disk
/// content at `root/change.path`:
///   - symlink → `\0symlink\0` + link target bytes
///   - regular file → `\0file\0` + full file bytes
///   - other (device, fifo, socket, dir, ...) → `\0mode:<mode>\0`
///   - missing (ENOENT) → `\0missing\0`
pub fn fingerprint_change(root: &Path, change: &StatusChange) -> String {
    let mut hasher = Sha256::new();
    hasher.update(json_header(change).as_bytes());

    let path = root.join(&change.path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                hasher.update(b"\0symlink\0");
                if let Ok(target) = std::fs::read_link(&path) {
                    hasher.update(target.to_string_lossy().as_bytes());
                }
            } else if meta.is_file() {
                hasher.update(b"\0file\0");
                match std::fs::read(&path) {
                    Ok(bytes) => hasher.update(&bytes),
                    Err(_) => hasher.update(b"\0missing\0"),
                }
            } else {
                hasher.update(format!("\0mode:{}\0", meta.mode()).as_bytes());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            hasher.update(b"\0missing\0");
        }
        Err(_) => {
            hasher.update(b"\0missing\0");
        }
    }

    hex::encode(hasher.finalize())
}

/// Convenience: fingerprint `path` in `root` by reading its live status
/// first. Used by mutations that need to compare an `expected_fingerprints`
/// map against the current on-disk/index state.
pub fn fingerprint_path(root: &Path, path: &str) -> String {
    let change = status_change_for_path(root, path);
    fingerprint_change(root, &change)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_ordinary_staged_modification() {
        // "1 M. N... 100644 100644 100644 <hH> <hI> path"
        let record = "1 M. N... 100644 100644 100644 aaaa0000 bbbb1111 staged.txt";
        let change = parse_ordinary(record).unwrap();
        assert_eq!(change.path, "staged.txt");
        assert_eq!(change.index_status, "M");
        assert_eq!(change.worktree_status, ".");
        assert_eq!(change.index_oid.as_deref(), Some("bbbb1111"));
        assert_eq!(change.kind, "ordinary");
        assert!(!change.conflicted);
    }

    #[test]
    fn parses_untracked() {
        let changes = parse_porcelain_v2("? loose.txt\0");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "loose.txt");
        assert_eq!(changes[0].kind, "untracked");
        assert_eq!(changes[0].index_status, "?");
        assert!(changes[0].index_oid.is_none());
    }

    #[test]
    fn parses_unmerged_as_conflicted() {
        // "u XY sub m1 m2 m3 mW h1 h2 h3 path"
        let record = "u UU N... 100644 100644 100644 100644 h1 h2 h3 conflict.txt";
        let changes = parse_porcelain_v2(&format!("{record}\0"));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "conflict.txt");
        assert!(changes[0].conflicted);
        assert_eq!(changes[0].index_oid.as_deref(), Some("h2"));
    }

    #[test]
    fn json_header_key_order_and_nulls() {
        let change = StatusChange {
            path: "a.txt".to_string(),
            original_path: None,
            index_status: "M".to_string(),
            worktree_status: ".".to_string(),
            index_oid: Some("deadbeef".to_string()),
            kind: "ordinary",
            conflicted: false,
        };
        let header = json_header(&change);
        assert_eq!(
            header,
            "{\"path\":\"a.txt\",\"originalPath\":null,\"indexStatus\":\"M\",\"worktreeStatus\":\".\",\"indexOid\":\"deadbeef\",\"kind\":\"ordinary\",\"conflicted\":false}"
        );
    }

    #[test]
    fn fingerprint_changes_with_content() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let change = StatusChange {
            path: "a.txt".to_string(),
            original_path: None,
            index_status: "?".to_string(),
            worktree_status: "?".to_string(),
            index_oid: None,
            kind: "untracked",
            conflicted: false,
        };
        let fp1 = fingerprint_change(dir.path(), &change);
        std::fs::write(dir.path().join("a.txt"), b"world").unwrap();
        let fp2 = fingerprint_change(dir.path(), &change);
        assert_ne!(fp1, fp2, "fingerprint must change when file content changes");
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn fingerprint_missing_file_is_stable() {
        let dir = tempdir().unwrap();
        let change = StatusChange {
            path: "gone.txt".to_string(),
            original_path: None,
            index_status: "D".to_string(),
            worktree_status: ".".to_string(),
            index_oid: None,
            kind: "ordinary",
            conflicted: false,
        };
        let fp1 = fingerprint_change(dir.path(), &change);
        let fp2 = fingerprint_change(dir.path(), &change);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_differs_by_status_metadata_not_just_content() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"same content").unwrap();
        let as_untracked = StatusChange {
            path: "a.txt".to_string(),
            original_path: None,
            index_status: "?".to_string(),
            worktree_status: "?".to_string(),
            index_oid: None,
            kind: "untracked",
            conflicted: false,
        };
        let as_modified = StatusChange {
            path: "a.txt".to_string(),
            original_path: None,
            index_status: "M".to_string(),
            worktree_status: ".".to_string(),
            index_oid: Some("deadbeef".to_string()),
            kind: "ordinary",
            conflicted: false,
        };
        assert_ne!(
            fingerprint_change(dir.path(), &as_untracked),
            fingerprint_change(dir.path(), &as_modified),
            "two files with identical bytes but different status metadata must not collide",
        );
    }
}
