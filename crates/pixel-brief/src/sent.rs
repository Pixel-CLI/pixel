// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The evidence a session has already been sent: `.pixel/brief-sessions/`.
//!
//! The region pack puts exact source in front of the agent. Asked again about
//! the same code a prompt later, the pack would send the same lines again, and
//! they would fill the budget a block the agent has never seen could have used.
//! So each hook session keeps the line ranges its packs delivered, and the next
//! pack leaves out what overlaps them: the budget goes to evidence the session
//! has not got.
//!
//! One file per session, named by sixteen hex digits of the SHA-256 of the
//! host's session id (the name the read-feedback log uses). It holds keys, no
//! text: a key is the SHA-256 of a path (sixteen hex digits), the first and the
//! last line of the range, and the time it was sent. A key older than [`TTL`]
//! is forgotten. The session is forgotten whole when the host starts it over
//! (`SessionStart` with source `compact` or `clear`: the agent no longer holds
//! what it was sent), and `PIXEL_BRIEF_DEDUPE=0` turns the whole thing off.
//!
//! The file is written owner-only and atomically, never through a link
//! (`.pixel` or `brief-sessions` as a symbolic link is refused), and `.pixel`
//! is never created for it. Every failure leaves the pack as it would have been.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use pixel_metrics::journal;

/// Directory under `.pixel` holding one file per session.
pub(crate) const DIR: &str = "brief-sessions";
/// How long a session's evidence is remembered.
pub(crate) const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Keys one session keeps; the oldest go first.
const MAX_KEYS: usize = 1_000;
/// A session file is read up to this many bytes.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// One range of one file, as it was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    /// Sixteen hex digits of the SHA-256 of the path.
    path: String,
    first: u64,
    last: u64,
    /// When it was sent, in milliseconds since the Unix epoch.
    at: u64,
}

/// The ranges a session has been sent, as far as they are remembered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    keys: Vec<Key>,
}

/// The sixteen hex digits that stand for `path` in a key.
fn path_key(path: &str) -> String {
    journal::sha256_hex(path)[..16].to_string()
}

impl Seen {
    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether lines `first..=last` of `path` overlap a range already sent.
    pub(crate) fn overlaps(&self, path: &str, first: u64, last: u64) -> bool {
        if self.keys.is_empty() {
            return false;
        }
        let path = path_key(path);
        self.keys
            .iter()
            .any(|key| key.path == path && key.first <= last && first <= key.last)
    }

    fn to_json(&self) -> Value {
        json!({
            "keys": self.keys.iter()
                .map(|key| json!([key.path, key.first, key.last, key.at]))
                .collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value, now_ms: u64) -> Self {
        let ttl = u64::try_from(TTL.as_millis()).unwrap_or(u64::MAX);
        let keys = value
            .get("keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let row = row.as_array()?;
                let key = Key {
                    path: row.first()?.as_str()?.to_string(),
                    first: row.get(1)?.as_u64()?,
                    last: row.get(2)?.as_u64()?,
                    at: row.get(3)?.as_u64()?,
                };
                (now_ms.saturating_sub(key.at) <= ttl
                    && key.path.len() == 16
                    && key.first <= key.last)
                    .then_some(key)
            })
            .collect();
        Self { keys }
    }

    /// The keys plus `ranges` sent at `at_ms`, a range already held kept
    /// once, at most [`MAX_KEYS`] of them, the newest.
    pub(crate) fn with(mut self, ranges: &[(String, u64, u64)], at_ms: u64) -> Self {
        for (path, first, last) in ranges {
            let path = path_key(path);
            match self
                .keys
                .iter_mut()
                .find(|key| key.path == path && key.first == *first && key.last == *last)
            {
                Some(held) => held.at = at_ms,
                None => self.keys.push(Key {
                    path,
                    first: *first,
                    last: *last,
                    at: at_ms,
                }),
            }
        }
        let extra = self.keys.len().saturating_sub(MAX_KEYS);
        self.keys.drain(..extra);
        self
    }
}

fn dir_in(root: &Path) -> PathBuf {
    root.join(pixel_io::sidecar::DIR_NAME).join(DIR)
}

/// The file of `session` under `root`.
fn file_for(root: &Path, session: &str) -> PathBuf {
    dir_in(root).join(format!("{}.json", journal::session_key(session)))
}

/// What `session` has been sent, as far as `now_ms` remembers it. An absent,
/// unreadable or foreign file is nothing sent.
pub fn load(root: &Path, session: &str, now_ms: u64) -> Seen {
    read(&file_for(root, session), now_ms).unwrap_or_default()
}

fn read(path: &Path, now_ms: u64) -> io::Result<Seen> {
    refuse_linked_dirs(path)?;
    let file = pixel_io::nofollow::open_read(path)?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text)?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Seen::from_json(&value, now_ms))
}

/// Delete the session files last written before `now - TTL`, regular files
/// only, and return how many went.
fn prune(dir: &Path, now: SystemTime) -> usize {
    prune_older(dir, now, TTL)
}

/// Refuse a store whose `.pixel` or `brief-sessions` directory is a symbolic link: a
/// clone can commit either, and the ranges would be written wherever it
/// points. A directory that does not exist yet is fine.
fn refuse_linked_dirs(path: &Path) -> io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    for linked in [dir.parent().unwrap_or(dir), dir] {
        if fs::symlink_metadata(linked).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is a symbolic link; the sent ranges do not use it",
                    linked.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Delete the files under `dir` last written before `now - max_age`, regular
/// files only, and return how many went.
fn prune_older(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.metadata().is_ok_and(|meta| {
                meta.file_type().is_file()
                    && meta.modified().is_ok_and(|written| {
                        now.duration_since(written).is_ok_and(|age| age > max_age)
                    })
            })
        })
        .filter(|entry| fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Remember that `ranges` (path, first line, last line) were sent to
/// `session` at `now_ms`; forget what is older than [`TTL`] and the session
/// files nobody has written for as long.
///
/// # Errors
///
/// A linked directory, no `.pixel` directory, or any I/O error of the write.
pub fn record(
    root: &Path,
    session: &str,
    ranges: &[(String, u64, u64)],
    now_ms: u64,
) -> io::Result<()> {
    if ranges.is_empty() {
        return Ok(());
    }
    let path = file_for(root, session);
    refuse_linked_dirs(&path)?;
    let dir = dir_in(root);
    if !fs::symlink_metadata(root.join(pixel_io::sidecar::DIR_NAME))?.is_dir() {
        return Err(io::ErrorKind::NotFound.into());
    }
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    prune(&dir, SystemTime::now());
    let next = load(root, session, now_ms).with(ranges, now_ms);
    pixel_io::nofollow::write_replace(
        &path,
        next.to_json().to_string().as_bytes(),
        pixel_io::nofollow::PRIVATE_MODE,
    )
}

/// Forget everything `session` was sent: the host started it over, so the
/// agent no longer holds it. Best effort.
pub fn reset(root: &Path, session: &str) {
    let path = file_for(root, session);
    if refuse_linked_dirs(&path).is_ok() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 60 * 60 * 1_000;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-brief-sent-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".pixel")).unwrap();
        dir
    }

    fn range(path: &str, first: u64, last: u64) -> (String, u64, u64) {
        (path.to_string(), first, last)
    }

    #[test]
    fn a_range_should_be_seen_when_it_overlaps_one_sent_and_only_in_the_same_file() {
        let seen = Seen::default().with(&[range("src/a.rs", 10, 20)], NOW);
        assert!(seen.overlaps("src/a.rs", 10, 20));
        assert!(
            seen.overlaps("src/a.rs", 20, 30),
            "sharing the last line is an overlap"
        );
        assert!(
            seen.overlaps("src/a.rs", 1, 10),
            "sharing the first line is an overlap"
        );
        assert!(seen.overlaps("src/a.rs", 12, 14));
        assert!(!seen.overlaps("src/a.rs", 21, 30));
        assert!(!seen.overlaps("src/a.rs", 1, 9));
        assert!(!seen.overlaps("src/b.rs", 10, 20));
        assert!(!Seen::default().overlaps("src/a.rs", 1, 100));
    }

    #[test]
    fn the_file_should_hold_hashes_and_numbers_and_never_a_path() {
        let root = scratch("hashes");
        record(
            &root,
            "session-one",
            &[range("src/secret-plan.rs", 5, 9)],
            NOW,
        )
        .unwrap();
        let path = file_for(&root, "session-one");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("secret-plan") && !text.contains("session-one"),
            "{text}"
        );
        assert!(text.contains(&path_key("src/secret-plan.rs")));
        assert_eq!(path.parent().unwrap(), root.join(".pixel").join(DIR));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.join(".pixel").join(DIR))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn what_was_sent_should_be_remembered_for_a_day_and_not_longer() {
        let root = scratch("ttl");
        record(&root, "s", &[range("a.rs", 1, 9)], NOW).unwrap();
        assert!(load(&root, "s", NOW + 23 * HOUR).overlaps("a.rs", 1, 9));
        assert!(!load(&root, "s", NOW + 25 * HOUR).overlaps("a.rs", 1, 9));
        // A later send drops what has expired and keeps what has not.
        record(&root, "s", &[range("b.rs", 1, 9)], NOW + 25 * HOUR).unwrap();
        let seen = load(&root, "s", NOW + 25 * HOUR);
        assert!(!seen.overlaps("a.rs", 1, 9));
        assert!(seen.overlaps("b.rs", 1, 9));
    }

    #[test]
    fn a_session_should_never_see_what_another_was_sent() {
        let root = scratch("sessions");
        record(&root, "one", &[range("a.rs", 1, 9)], NOW).unwrap();
        assert!(load(&root, "one", NOW).overlaps("a.rs", 1, 9));
        assert!(load(&root, "two", NOW).is_empty());
    }

    #[test]
    fn reset_should_forget_the_session_and_only_that_one() {
        let root = scratch("reset");
        record(&root, "one", &[range("a.rs", 1, 9)], NOW).unwrap();
        record(&root, "two", &[range("a.rs", 1, 9)], NOW).unwrap();
        reset(&root, "one");
        assert!(load(&root, "one", NOW).is_empty());
        assert!(load(&root, "two", NOW).overlaps("a.rs", 1, 9));
        reset(&root, "never-seen");
    }

    #[test]
    fn a_repeated_range_should_be_kept_once_and_the_oldest_keys_dropped_past_the_cap() {
        let mut seen = Seen::default()
            .with(&[range("a.rs", 1, 9)], NOW)
            .with(&[range("a.rs", 1, 9)], NOW + 5);
        assert_eq!(seen.keys.len(), 1);
        assert_eq!(seen.keys[0].at, NOW + 5);
        let many: Vec<_> = (0..MAX_KEYS as u64 + 10)
            .map(|n| range("b.rs", n * 10, n * 10 + 5))
            .collect();
        seen = seen.with(&many, NOW + 10);
        assert_eq!(seen.keys.len(), MAX_KEYS);
        assert!(!seen.overlaps("a.rs", 1, 9), "the oldest key went first");
        assert!(seen.overlaps(
            "b.rs",
            (MAX_KEYS as u64 + 9) * 10,
            (MAX_KEYS as u64 + 9) * 10
        ));
    }

    #[test]
    fn a_damaged_or_foreign_file_should_be_nothing_sent() {
        let root = scratch("damaged");
        let path = file_for(&root, "s");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for text in [
            "",
            "not json",
            "{}",
            "{\"keys\": [[1,2,3]]}",
            "{\"keys\": [[\"short\",1,2,3]]}",
        ] {
            fs::write(&path, text).unwrap();
            assert!(load(&root, "s", NOW).is_empty(), "{text}");
        }
        // And a good record replaces it.
        record(&root, "s", &[range("a.rs", 1, 2)], NOW).unwrap();
        assert!(load(&root, "s", NOW).overlaps("a.rs", 1, 2));
    }

    #[test]
    fn a_linked_directory_should_refuse_the_record_and_leave_the_target_alone() {
        let root = scratch("linked");
        let elsewhere = scratch("linked-target");
        std::os::unix::fs::symlink(&elsewhere, root.join(".pixel").join(DIR)).unwrap();
        assert!(record(&root, "s", &[range("a.rs", 1, 2)], NOW).is_err());
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            1,
            "only its own .pixel"
        );
        assert!(load(&root, "s", NOW).is_empty());
    }

    #[test]
    fn a_repository_without_dot_pixel_should_not_get_one() {
        let root =
            std::env::temp_dir().join(format!("pixel-brief-sent-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert!(record(&root, "s", &[range("a.rs", 1, 2)], NOW).is_err());
        assert!(!root.join(".pixel").exists());
    }

    #[test]
    fn an_old_session_file_of_another_session_should_go_when_a_record_is_written() {
        let root = scratch("prune");
        record(&root, "old", &[range("a.rs", 1, 2)], NOW).unwrap();
        let old = file_for(&root, "old");
        let long_ago = SystemTime::now() - TTL - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        record(&root, "new", &[range("b.rs", 1, 2)], NOW).unwrap();
        assert!(!old.exists());
        assert!(file_for(&root, "new").exists());
    }
}
