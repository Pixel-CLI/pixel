// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the snapshot store: a token only ever reads back the
//! record it was minted for, and retention removes what is too old or too
//! many.

use super::*;
use tempfile::tempdir;

fn record(root: &str, head: &str, created_at: String) -> SnapshotRecord {
    let mut fingerprints = BTreeMap::new();
    fingerprints.insert("a.txt".to_string(), head.to_string());
    SnapshotRecord {
        schema_version: 1,
        root: root.to_string(),
        head: Some(head.to_string()),
        branch: None,
        created_at,
        fingerprints,
    }
}

fn now() -> String {
    current_unix_ms().to_string()
}

fn snapshot_files(store: &SnapshotStore, root: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(store.snapshots_dir(root))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The token is the first 12 hex digits of the SHA-256 of root, head and
/// the sorted `path=hash` lines; a missing head hashes as empty.
#[test]
fn snapshot_token_should_hash_root_head_and_sorted_fingerprints() {
    let mut fps = BTreeMap::new();
    fps.insert("b".to_string(), "2".to_string());
    fps.insert("a".to_string(), "1".to_string());
    assert_eq!(
        snapshot_token("/r", Some("h"), &fps),
        sha256_hex("/r\u{0}h\u{0}a=1\nb=2")[..12]
    );
    assert_eq!(
        snapshot_token("/r", None, &fps),
        sha256_hex("/r\u{0}\u{0}a=1\nb=2")[..12]
    );
}

/// A token of the wrong length or with non-hex characters never touches
/// the disk.
#[test]
fn read_should_refuse_malformed_tokens() {
    let dir = tempdir().unwrap();
    let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
    let token = store.record(&record("/repo", "h1", now())).unwrap();
    assert!(store.read("/repo", &token).is_some());
    assert!(store.read("/repo", &token[..11]).is_none(), "11 chars");
    assert!(
        store.read("/repo", &format!("{token}0")).is_none(),
        "13 chars"
    );
    let mut bad = token.clone();
    bad.replace_range(0..1, "g");
    assert!(store.read("/repo", &bad).is_none(), "non-hex");
}

/// A token minted for one worktree never reads a snapshot of another.
#[test]
fn read_should_not_cross_worktrees() {
    let dir = tempdir().unwrap();
    let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
    let token = store.record(&record("/repo-a", "h1", now())).unwrap();
    assert!(store.read("/repo-b", &token).is_none());
}

/// A stored file whose content no longer matches its token, its root or
/// the schema is refused rather than trusted.
#[test]
fn read_should_refuse_records_that_do_not_match_their_file() {
    let dir = tempdir().unwrap();
    let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
    let original = record("/repo", "h1", now());
    let token = store.record(&original).unwrap();
    let path = store.snapshots_dir("/repo").join(format!("{token}.json"));

    let mut other_head = original.clone();
    other_head.head = Some("tampered".into());
    std::fs::write(&path, serde_json::to_vec(&other_head).unwrap()).unwrap();
    assert!(store.read("/repo", &token).is_none(), "token mismatch");

    let mut other_root = original.clone();
    other_root.root = "/elsewhere".into();
    std::fs::write(&path, serde_json::to_vec(&other_root).unwrap()).unwrap();
    assert!(store.read("/repo", &token).is_none(), "root mismatch");

    let mut v2 = original.clone();
    v2.schema_version = 2;
    std::fs::write(&path, serde_json::to_vec(&v2).unwrap()).unwrap();
    assert!(store.read("/repo", &token).is_none(), "unknown schema");

    std::fs::write(&path, b"{ not json").unwrap();
    assert!(store.read("/repo", &token).is_none(), "corrupt file");

    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    assert!(store.read("/repo", &token).is_some(), "restored file reads");
}

/// Recording drops snapshots older than 24 hours and keeps recent ones;
/// files that are not snapshot records are left alone.
#[test]
fn record_should_prune_snapshots_older_than_a_day() {
    let dir = tempdir().unwrap();
    let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
    let day_and_a_minute = RETENTION_MAX_AGE_MS + 60_000;
    let old = store
        .record(&record(
            "/repo",
            "old",
            (current_unix_ms() - day_and_a_minute).to_string(),
        ))
        .unwrap();
    let recent = store
        .record(&record(
            "/repo",
            "recent",
            (current_unix_ms() - 60_000).to_string(),
        ))
        .unwrap();
    let sdir = store.snapshots_dir("/repo");
    std::fs::write(sdir.join("notes.txt"), b"keep").unwrap();
    std::fs::write(sdir.join("garbage.json"), b"{").unwrap();
    let fresh = store.record(&record("/repo", "fresh", now())).unwrap();

    let mut expected = vec![
        format!("{recent}.json"),
        format!("{fresh}.json"),
        "garbage.json".to_string(),
        "notes.txt".to_string(),
    ];
    expected.sort();
    assert_eq!(snapshot_files(&store, "/repo"), expected);
    assert!(store.read("/repo", &old).is_none());
}

/// At most 200 snapshots per worktree survive; the newest are kept.
#[test]
fn record_should_keep_only_the_newest_two_hundred_snapshots() {
    let dir = tempdir().unwrap();
    let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
    let base = current_unix_ms() - 1_000_000;
    let mut tokens = Vec::new();
    for i in 0..=RETENTION_MAX_COUNT {
        let r = record("/repo", &format!("h{i}"), (base + i as u64).to_string());
        tokens.push(store.record(&r).unwrap());
    }
    assert_eq!(snapshot_files(&store, "/repo").len(), RETENTION_MAX_COUNT);
    assert!(store.read("/repo", &tokens[0]).is_none(), "oldest pruned");
    assert!(store.read("/repo", &tokens[1]).is_some());
    assert!(store.read("/repo", tokens.last().unwrap()).is_some());
}

/// Timestamps are unix milliseconds or an ISO date; anything shorter than
/// a date is unknown.
#[test]
fn parse_iso_ms_should_read_unix_ms_and_iso_dates() {
    assert_eq!(parse_iso_ms("1700000000000"), Some(1_700_000_000_000));
    assert_eq!(parse_iso_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
    assert_eq!(parse_iso_ms("2024-01-01"), None);
    assert_eq!(parse_iso_ms("abcd-ef-ghT00:00:00Z"), None);
}
