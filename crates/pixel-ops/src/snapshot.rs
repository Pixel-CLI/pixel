// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Snapshot store — durable worktree snapshots keyed by worktree root.
//!
//! Port of usable-git's `snapshot-store.ts`. A snapshot captures the repo's
//! HEAD, branch, and file fingerprints (sha256 of content for tracked files).
//! The 12-char hex token is deterministic: same root+head+fingerprints →
//! same token. Snapshots are retained 24h / 200 records per worktree.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::durable::{ensure_dir, sha256_hex, state_root, write_durably};

/// 12-char hex snapshot token (first 12 of sha256).
pub fn snapshot_token(
    root: &str,
    head: Option<&str>,
    fingerprints: &BTreeMap<String, String>,
) -> String {
    // Sort fingerprints by path for determinism (BTreeMap is already sorted).
    let mut sorted: Vec<(String, String)> = fingerprints
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    sorted.sort();
    let fp_str: String = sorted
        .iter()
        .map(|(p, h)| format!("{p}={h}"))
        .collect::<Vec<_>>()
        .join("\n");
    let input = format!("{}\u{0}{}\u{0}{}", root, head.unwrap_or(""), fp_str);
    sha256_hex(&input)[..12].to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub schema_version: u32,
    pub root: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub created_at: String,
    pub fingerprints: BTreeMap<String, String>,
}

impl SnapshotRecord {
    pub fn token(&self) -> String {
        snapshot_token(&self.root, self.head.as_deref(), &self.fingerprints)
    }
}

pub struct SnapshotStore {
    state_root: PathBuf,
}

const RETENTION_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const RETENTION_MAX_COUNT: usize = 200;

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            state_root: state_root(),
        }
    }

    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn snapshots_dir(&self, root: &str) -> PathBuf {
        self.state_root.join("snapshots").join(sha256_hex(root))
    }

    /// Record a snapshot and return its token.
    pub fn record(&self, record: &SnapshotRecord) -> std::io::Result<String> {
        let dir = self.snapshots_dir(&record.root);
        ensure_dir(&dir)?;
        let token = record.token();
        let path = dir.join(format!("{token}.json"));
        let json = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
        write_durably(&path, &json)?;
        self.prune(&record.root)?;
        Ok(token)
    }

    /// Read a snapshot by token. Returns None if missing/invalid/mismatched.
    pub fn read(&self, root: &str, token: &str) -> Option<SnapshotRecord> {
        if !token.chars().all(|c| c.is_ascii_hexdigit()) || token.len() != 12 {
            return None;
        }
        let path = self.snapshots_dir(root).join(format!("{token}.json"));
        let data = std::fs::read(&path).ok()?;
        let record: SnapshotRecord = serde_json::from_slice(&data).ok()?;
        if record.schema_version != 1 {
            return None;
        }
        if record.root != root {
            return None;
        }
        if record.token() != token {
            return None;
        }
        Some(record)
    }

    /// Prune old/excess snapshots for a given root.
    fn prune(&self, root: &str) -> std::io::Result<()> {
        let dir = self.snapshots_dir(root);
        if !dir.exists() {
            return Ok(());
        }
        let mut entries: Vec<(PathBuf, String, u64)> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let record: SnapshotRecord = match serde_json::from_slice(&data) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let ts = parse_iso_ms(&record.created_at).unwrap_or(0);
            entries.push((path, record.token(), ts));
        }
        // Sort by timestamp descending (newest first).
        entries.sort_by_key(|(_, _, ts)| std::cmp::Reverse(*ts));
        let now = current_unix_ms();
        let mut kept = 0;
        for (path, _token, ts) in &entries {
            let age = now.saturating_sub(*ts);
            if age > RETENTION_MAX_AGE_MS || kept >= RETENTION_MAX_COUNT {
                let _ = std::fs::remove_file(path);
            } else {
                kept += 1;
            }
        }
        Ok(())
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    // Try plain unix-ms first (our own format).
    if let Ok(ms) = s.parse::<u64>() {
        return Some(ms);
    }
    // Fall back to ISO-8601 like "2024-01-01T00:00:00.000Z".
    if s.len() < 19 {
        return None;
    }
    // A stored `created_at` is untrusted text (#784): `get` refuses a range
    // that splits a character, and a year before 1970 or a month or day of
    // 00 is unreadable rather than an underflow. `prune` treats `None` as
    // the oldest snapshot.
    let year: u64 = s.get(0..4)?.parse().ok()?;
    let month: u64 = s.get(5..7)?.parse().ok()?;
    let day: u64 = s.get(8..10)?.parse().ok()?;
    let days = year.checked_sub(1970)? * 365 + month.checked_sub(1)? * 30 + day.checked_sub(1)?;
    days.checked_mul(86_400_000)
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_record(root: &str, head: &str, fps: &[(&str, &str)]) -> SnapshotRecord {
        let mut fingerprints = BTreeMap::new();
        for (p, h) in fps {
            fingerprints.insert(p.to_string(), h.to_string());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or_else(|_| "0".to_string(), |d| d.as_millis().to_string());
        SnapshotRecord {
            schema_version: 1,
            root: root.to_string(),
            head: Some(head.to_string()),
            branch: Some("main".to_string()),
            created_at: now,
            fingerprints,
        }
    }

    #[test]
    fn token_is_deterministic() {
        let r = make_record("/repo", "abc123", &[("a.txt", "hash1")]);
        let t1 = snapshot_token(&r.root, r.head.as_deref(), &r.fingerprints);
        let t2 = snapshot_token(&r.root, r.head.as_deref(), &r.fingerprints);
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 12);
    }

    #[test]
    fn token_changes_with_content() {
        let r1 = make_record("/repo", "abc123", &[("a.txt", "hash1")]);
        let r2 = make_record("/repo", "abc123", &[("a.txt", "hash2")]);
        assert_ne!(
            snapshot_token(&r1.root, r1.head.as_deref(), &r1.fingerprints),
            snapshot_token(&r2.root, r2.head.as_deref(), &r2.fingerprints)
        );
    }

    #[test]
    fn store_record_and_read_roundtrip() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
        let root = "/test/repo";
        let record = make_record(
            root,
            "abc123def456",
            &[("a.txt", "hash1"), ("b.txt", "hash2")],
        );
        let token = store.record(&record).unwrap();
        let read = store.read(root, &token).unwrap();
        assert_eq!(read.root, root);
        assert_eq!(read.head, Some("abc123def456".to_string()));
        assert_eq!(read.fingerprints.len(), 2);
    }

    #[test]
    fn store_read_returns_none_for_bad_token() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
        assert!(store.read("/repo", "badtoken").is_none());
        assert!(store.read("/repo", "ZZZZZZZZZZZZ").is_none());
    }

    /// Timestamps written here are compared with wall-clock time by their
    /// readers; a placeholder (0, 1) would date every entry to 1970.
    #[test]
    fn current_unix_ms_is_the_current_unix_epoch_in_milliseconds() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let now = current_unix_ms();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            now >= before && now <= after,
            "{before} <= {now} <= {after}"
        );
        assert!(now > 1_577_836_800_000, "{now}"); // 2020-01-01T00:00:00Z
    }

    #[test]
    fn parse_iso_ms_should_refuse_a_malformed_date_instead_of_panicking() {
        // Each of these panicked (#784): a multibyte char across the year's
        // slice bound, a year before 1970, a month or a day of 00.
        assert_eq!(parse_iso_ms("aaaé-01-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("1969-12-31T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2024-00-10T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2024-05-00T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2024-é1-10T00:00:00Z"), None);
    }

    #[test]
    fn parse_iso_ms_should_read_unix_ms_and_the_iso_day_it_approximates() {
        assert_eq!(parse_iso_ms("1700000000000"), Some(1_700_000_000_000));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        // One 30-day month and no day offset past the 1st.
        assert_eq!(parse_iso_ms("1970-02-01T00:00:00Z"), Some(2_592_000_000));
        // 54 years of 365 days plus one day.
        assert_eq!(
            parse_iso_ms("2024-01-02T00:00:00.000Z"),
            Some(1_703_030_400_000)
        );
        assert_eq!(parse_iso_ms("2024-01-02"), None);
    }

    #[test]
    fn record_should_succeed_and_prune_a_snapshot_whose_date_cannot_be_read() {
        // One unreadable `created_at` used to panic `prune`, hence every
        // later `record` in the worktree (#784). It is now the oldest entry.
        let dir = tempdir().unwrap();
        let store = SnapshotStore::with_state_root(dir.path().to_path_buf());
        let root = "/test/repo";
        let snapshots = store.snapshots_dir(root);
        std::fs::create_dir_all(&snapshots).unwrap();
        let mut bad = make_record(root, "old", &[("a.txt", "hash0")]);
        bad.created_at = "1969-12-31T00:00:00Z".to_string();
        let bad_path = snapshots.join("bad.json");
        std::fs::write(&bad_path, serde_json::to_vec(&bad).unwrap()).unwrap();
        let good = make_record(root, "new", &[("a.txt", "hash1")]);
        let token = store.record(&good).unwrap();
        assert!(store.read(root, &token).is_some());
        assert!(!bad_path.exists(), "the unreadable snapshot is pruned");
    }
}
