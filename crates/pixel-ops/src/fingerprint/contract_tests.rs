// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for porcelain v2 parsing and the fingerprint bytes: a
//! fingerprint is the SHA-256 of an exact header and a type-tagged content
//! marker, so these tests rebuild that byte string by hand and compare.

use super::*;
use tempfile::tempdir;

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn change(path: &str) -> StatusChange {
    StatusChange {
        path: path.to_string(),
        original_path: None,
        index_status: "?".to_string(),
        worktree_status: "?".to_string(),
        index_oid: None,
        kind: "untracked",
        conflicted: false,
    }
}

const UNTRACKED_HEADER: &str = "{\"path\":\"f\",\"originalPath\":null,\"indexStatus\":\"?\",\"worktreeStatus\":\"?\",\"indexOid\":null,\"kind\":\"untracked\",\"conflicted\":false}";

// --- parse_porcelain_v2 ------------------------------------------------------

/// A rename consumes the following record as its original path, even when
/// that path looks like a record of its own; the record after it is parsed
/// normally.
#[test]
fn parse_porcelain_v2_should_pair_a_rename_with_its_original_path() {
    let out = "2 R. N... 100644 100644 100644 h1 h2 R100 new name.rs\0? old.rs\0? next.txt\0";
    let changes = parse_porcelain_v2(out);
    assert_eq!(
        changes,
        vec![
            StatusChange {
                path: "new name.rs".into(),
                original_path: Some("? old.rs".into()),
                index_status: "R".into(),
                worktree_status: ".".into(),
                index_oid: Some("h2".into()),
                kind: "renamed",
                conflicted: false,
            },
            change("next.txt"),
        ]
    );
}

/// Paths keep their spaces; ignored entries are reported as ignored.
#[test]
fn parse_porcelain_v2_should_keep_spaces_in_paths_and_report_ignored_files() {
    let out = "1 .M N... 100644 100644 100644 h1 h2 dir/a b.txt\0! target/x\0";
    let changes = parse_porcelain_v2(out);
    assert_eq!(changes[0].path, "dir/a b.txt");
    assert_eq!(changes[0].index_status, ".");
    assert_eq!(changes[0].worktree_status, "M");
    assert_eq!(
        changes[1],
        StatusChange {
            path: "target/x".into(),
            original_path: None,
            index_status: "!".into(),
            worktree_status: "!".into(),
            index_oid: None,
            kind: "ignored",
            conflicted: false,
        }
    );
}

/// Records with too few fields or a malformed XY are dropped; unknown
/// record types and empty records are ignored; the rest still parse.
#[test]
fn parse_porcelain_v2_should_drop_malformed_records_and_keep_the_rest() {
    let out = [
        "1 M. N... 100644 short",
        "1 MMM N... 100644 100644 100644 h1 h2 bad-xy.txt",
        "2 R. N... 100644 100644 100644 h1 h2 too-few",
        "orig-of-bad-rename",
        "2 RRR N... 100644 100644 100644 h1 h2 R100 bad-xy-rename",
        "orig",
        "u UU N... 100644 100644 h1 h2 too-few",
        "u UUU N... 100644 100644 100644 100644 h1 h2 h3 bad-xy-unmerged",
        "# branch.oid abc",
        "",
        "? kept.txt",
    ]
    .join("\0");
    assert_eq!(parse_porcelain_v2(&out), vec![change("kept.txt")]);
}

/// An unmerged entry is always conflicted and records the stage-2 oid; an
/// ordinary entry is conflicted when its XY carries a `U`.
#[test]
fn parse_porcelain_v2_should_flag_conflicts_from_unmerged_records_and_u_status() {
    let changes = parse_porcelain_v2(
        &[
            "u AA N... 100644 100644 100644 100644 h1 h2 h3 both added.rs",
            "1 UM N... 100644 100644 100644 h1 h2 odd.rs",
        ]
        .join("\0"),
    );
    assert_eq!(changes[0].path, "both added.rs");
    assert_eq!(changes[0].index_status, "A");
    assert!(
        changes[0].conflicted,
        "unmerged is conflicted whatever its XY"
    );
    assert_eq!(changes[0].index_oid.as_deref(), Some("h2"));
    assert!(changes[1].conflicted);
}

/// A trailing rename with no original-path record keeps a null original.
#[test]
fn parse_porcelain_v2_should_accept_a_rename_without_its_original_record() {
    let changes = parse_porcelain_v2("2 R. N... 100644 100644 100644 h1 h2 R90 moved.rs");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].original_path, None);
    assert_eq!(changes[0].kind, "renamed");
}

// --- fingerprint_change --------------------------------------------------------

/// A regular file hashes the header, the file tag and its full bytes.
#[test]
fn fingerprint_change_should_hash_header_file_tag_and_bytes() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"content").unwrap();
    let expected = sha(format!("{UNTRACKED_HEADER}\0file\0content").as_bytes());
    assert_eq!(fingerprint_change(dir.path(), &change("f")), expected);
}

/// A missing path hashes the header and the missing tag.
#[test]
fn fingerprint_change_should_hash_the_missing_tag_for_an_absent_path() {
    let dir = tempdir().unwrap();
    let expected = sha(format!("{UNTRACKED_HEADER}\0missing\0").as_bytes());
    assert_eq!(fingerprint_change(dir.path(), &change("f")), expected);
}

/// A symlink hashes its target, never the bytes it points to.
#[test]
fn fingerprint_change_should_hash_the_link_target_of_a_symlink() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("real"), b"secret bytes").unwrap();
    std::os::unix::fs::symlink("real", dir.path().join("f")).unwrap();
    let expected = sha(format!("{UNTRACKED_HEADER}\0symlink\0real").as_bytes());
    assert_eq!(fingerprint_change(dir.path(), &change("f")), expected);
}

/// Anything else (here a directory) hashes its mode.
#[test]
fn fingerprint_change_should_hash_the_mode_of_a_directory() {
    let dir = tempdir().unwrap();
    std::fs::create_dir(dir.path().join("f")).unwrap();
    let mode = std::fs::symlink_metadata(dir.path().join("f"))
        .unwrap()
        .mode();
    let expected = sha(format!("{UNTRACKED_HEADER}\0mode:{mode}\0").as_bytes());
    assert_eq!(fingerprint_change(dir.path(), &change("f")), expected);
}

/// The header escapes values as JSON strings and writes the original path
/// and conflict flag of a rename.
#[test]
fn fingerprint_change_should_escape_header_values_and_carry_rename_fields() {
    let dir = tempdir().unwrap();
    let c = StatusChange {
        path: "q\"uote".into(),
        original_path: Some("old\\path".into()),
        index_status: "R".into(),
        worktree_status: ".".into(),
        index_oid: Some("abc".into()),
        kind: "renamed",
        conflicted: true,
    };
    let header = "{\"path\":\"q\\\"uote\",\"originalPath\":\"old\\\\path\",\"indexStatus\":\"R\",\"worktreeStatus\":\".\",\"indexOid\":\"abc\",\"kind\":\"renamed\",\"conflicted\":true}";
    let expected = sha(format!("{header}\0missing\0").as_bytes());
    assert_eq!(fingerprint_change(dir.path(), &c), expected);
}

// --- status_change_for_path ------------------------------------------------------

fn repo() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    pixel_git::GitRunner::new(dir.path())
        .run_isolated(&["init", "-q"])
        .unwrap();
    dir
}

/// In a repository, an untracked file is reported as git reports it.
#[test]
fn status_change_for_path_should_read_the_live_status_of_a_changed_path() {
    let dir = repo();
    std::fs::write(dir.path().join("new.txt"), b"x").unwrap();
    assert_eq!(
        status_change_for_path(dir.path(), "new.txt"),
        change("new.txt")
    );
}

/// A path with no pending change, or a directory git cannot read, gets the
/// synthetic clean entry, and `fingerprint_path` hashes that entry.
#[test]
fn status_change_for_path_should_fall_back_to_a_clean_entry() {
    let clean = StatusChange {
        path: "absent.txt".into(),
        original_path: None,
        index_status: ".".into(),
        worktree_status: ".".into(),
        index_oid: None,
        kind: "ordinary",
        conflicted: false,
    };
    let dir = repo();
    assert_eq!(status_change_for_path(dir.path(), "absent.txt"), clean);
    let plain = tempdir().unwrap();
    assert_eq!(status_change_for_path(plain.path(), "absent.txt"), clean);
    assert_eq!(
        fingerprint_path(plain.path(), "absent.txt"),
        fingerprint_change(plain.path(), &clean)
    );
}
