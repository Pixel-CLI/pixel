// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What the edit hooks remember about one agent session
//! (`.pixel/sessions/<key>.json`, `specs/hooks.md`).
//!
//! * `baselines`: per edited file, the shapes of its symbols before the
//!   session's first edit to it, so the stale-caller check does not depend on
//!   the graph still being the generation before the edit;
//! * `carded`: the symbol/new-shape pairs a card was already sent for;
//! * `candidates`: the callers a card named, kept for a symbol the graph has
//!   already republished away (a removal or a rename);
//! * `pending`: symbols whose shape changed, with the caller sites that have
//!   not changed since, each at the content hash its file had when recorded;
//! * `blocked`: the hashes of pending sets Stop already blocked on.
//!
//! The file holds paths, names and declaration headers, is written owner-only
//! and atomically, never through a link, and is dropped after a day. Every
//! failure to read it is an empty session; every failure to write it loses the
//! memory, never the hook.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pixel_metrics::journal;
use pixel_model::hash::Hash;
use serde_json::{Value, json};

/// Directory under `.pixel` holding one file per session.
pub const DIR: &str = "sessions";
/// How long a session file is kept after its last write.
pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Files whose baseline a session keeps.
pub const MAX_BASELINES: usize = 64;
/// Symbols a session keeps pending.
pub const MAX_PENDING: usize = 24;
/// Caller sites kept per pending symbol.
pub const MAX_SITES: usize = 16;
/// Callers kept per card, for a symbol the graph may drop.
const MAX_CANDIDATES: usize = 64;
/// A session file is read up to this many bytes.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// One symbol of a file as the session first saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub shape: String,
}

/// A caller site and the content hash its file had when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSite {
    pub file: String,
    pub line: u32,
    pub holder: String,
    pub hash: String,
}

/// A symbol whose shape changed and the sites that still use the old one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub uid: String,
    pub name: String,
    pub file: String,
    /// The shape before the session's first edit.
    pub old: String,
    /// The shape now; `None` when the symbol is gone.
    pub new: Option<String>,
    pub sites: Vec<PendingSite>,
}

/// A caller named by a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub file: String,
    pub line: u32,
    pub holder: String,
}

/// The memory of one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub baselines: BTreeMap<String, Vec<Base>>,
    pub carded: BTreeSet<String>,
    pub candidates: BTreeMap<String, Vec<Candidate>>,
    pub pending: Vec<Pending>,
    pub blocked: BTreeSet<String>,
    /// Sites (`symbol|file`) that left the pending set because their file
    /// changed: a later edit of the symbol does not put them back.
    pub fixed: BTreeSet<String>,
}

/// The key under which a symbol's card is remembered: the symbol and the
/// shape the proposed text gives it, so a second, different change cards again.
#[must_use]
pub fn card_key(uid: &str, change: &str) -> String {
    format!("{uid}\u{1}{}", &Hash::of(change.as_bytes()).to_hex()[..16])
}

/// The content hash of `bytes`, as a pending site records it.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    Hash::of(bytes).to_hex()
}

impl Session {
    /// The hash of the pending set: the symbols and the sites in it, not the
    /// file hashes, so the same set blocks once.
    #[must_use]
    pub fn pending_hash(&self) -> String {
        let mut lines: Vec<String> = self
            .pending
            .iter()
            .flat_map(|p| {
                p.sites
                    .iter()
                    .map(move |s| format!("{}|{}:{}", p.uid, s.file, s.line))
            })
            .collect();
        lines.sort_unstable();
        Hash::of(lines.join("\n").as_bytes()).to_hex()[..16].to_owned()
    }

    /// Whether `file` is something the session may record a baseline for.
    #[must_use]
    pub fn has_room_for(&self, file: &str) -> bool {
        self.baselines.contains_key(file) || self.baselines.len() < MAX_BASELINES
    }

    /// The key under which a site of `uid` in `file` is remembered as fixed.
    #[must_use]
    pub fn fixed_key(uid: &str, file: &str) -> String {
        format!("{uid}|{file}")
    }

    /// Keeps the newest candidates of `uid`, bounded.
    pub fn remember_candidates(&mut self, uid: &str, sites: Vec<Candidate>) {
        if self.candidates.len() >= MAX_CANDIDATES && !self.candidates.contains_key(uid) {
            return;
        }
        self.candidates.insert(uid.to_owned(), sites);
    }

    fn to_json(&self) -> Value {
        json!({
            "v": 1,
            "baselines": self.baselines.iter().map(|(file, rows)| {
                (file.clone(), rows.iter().map(|b| json!([b.uid, b.name, b.kind, b.shape])).collect::<Vec<_>>())
            }).collect::<BTreeMap<_, _>>(),
            "carded": self.carded,
            "candidates": self.candidates.iter().map(|(uid, sites)| {
                (uid.clone(), sites.iter().map(|c| json!([c.file, c.line, c.holder])).collect::<Vec<_>>())
            }).collect::<BTreeMap<_, _>>(),
            "pending": self.pending.iter().map(|p| json!({
                "uid": p.uid, "name": p.name, "file": p.file, "old": p.old, "new": p.new,
                "sites": p.sites.iter().map(|s| json!([s.file, s.line, s.holder, s.hash])).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "blocked": self.blocked,
            "fixed": self.fixed,
        })
    }

    fn from_json(value: &Value) -> Self {
        let strings = |key: &str| -> BTreeSet<String> {
            value
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let baselines = value
            .get("baselines")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(file, rows)| {
                let rows = rows
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        let row = row.as_array()?;
                        Some(Base {
                            uid: row.first()?.as_str()?.to_owned(),
                            name: row.get(1)?.as_str()?.to_owned(),
                            kind: row.get(2)?.as_str()?.to_owned(),
                            shape: row.get(3)?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
                (file.clone(), rows)
            })
            .collect();
        let candidates = value
            .get("candidates")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(uid, sites)| {
                let sites = sites
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        let row = row.as_array()?;
                        Some(Candidate {
                            file: row.first()?.as_str()?.to_owned(),
                            line: u32::try_from(row.get(1)?.as_u64()?).ok()?,
                            holder: row.get(2)?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
                (uid.clone(), sites)
            })
            .collect();
        let pending = value
            .get("pending")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let sites = p
                    .get("sites")?
                    .as_array()?
                    .iter()
                    .filter_map(|row| {
                        let row = row.as_array()?;
                        Some(PendingSite {
                            file: row.first()?.as_str()?.to_owned(),
                            line: u32::try_from(row.get(1)?.as_u64()?).ok()?,
                            holder: row.get(2)?.as_str()?.to_owned(),
                            hash: row.get(3)?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
                Some(Pending {
                    uid: p.get("uid")?.as_str()?.to_owned(),
                    name: p.get("name")?.as_str()?.to_owned(),
                    file: p.get("file")?.as_str()?.to_owned(),
                    old: p.get("old")?.as_str()?.to_owned(),
                    new: p.get("new").and_then(Value::as_str).map(str::to_owned),
                    sites,
                })
            })
            .collect();
        Self {
            baselines,
            carded: strings("carded"),
            candidates,
            pending,
            blocked: strings("blocked"),
            fixed: strings("fixed"),
        }
    }
}

fn dir_in(root: &Path) -> PathBuf {
    root.join(pixel_io::sidecar::DIR_NAME).join(DIR)
}

/// The file of `session` under `root`.
#[must_use]
pub fn file_for(root: &Path, session: &str) -> PathBuf {
    dir_in(root).join(format!("{}.json", journal::session_key(session)))
}

/// Refuses a store whose `.pixel` or `sessions` directory is a link: a clone can
/// commit either and the state would be written wherever it points.
fn refuse_linked_dirs(path: &Path) -> io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    for linked in [dir.parent().unwrap_or(dir), dir] {
        if fs::symlink_metadata(linked).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symbolic link", linked.display()),
            ));
        }
    }
    Ok(())
}

/// The session as the file holds it: empty when there is no file, it cannot be
/// read, or it is not ours.
#[must_use]
pub fn load(root: &Path, session: &str) -> Session {
    let path = file_for(root, session);
    let read = || -> io::Result<Session> {
        refuse_linked_dirs(&path)?;
        let file = pixel_io::nofollow::open_read(&path)?;
        let mut text = String::new();
        file.take(MAX_FILE_BYTES).read_to_string(&mut text)?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Session::from_json(&value))
    };
    read().unwrap_or_default()
}

/// Deletes the session files nobody wrote for longer than `max_age`.
fn prune(dir: &Path, now: SystemTime, max_age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry.metadata().is_ok_and(|meta| {
            meta.file_type().is_file()
                && meta
                    .modified()
                    .is_ok_and(|written| now.duration_since(written).is_ok_and(|age| age > max_age))
        });
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Writes the session.
///
/// # Errors
///
/// A linked directory, no `.pixel` directory, or any I/O error of the write.
pub fn save(root: &Path, session: &str, state: &Session) -> io::Result<()> {
    let path = file_for(root, session);
    refuse_linked_dirs(&path)?;
    if !fs::symlink_metadata(root.join(pixel_io::sidecar::DIR_NAME))?.is_dir() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let dir = dir_in(root);
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    prune(&dir, SystemTime::now(), TTL);
    pixel_io::nofollow::write_replace(
        &path,
        state.to_json().to_string().as_bytes(),
        pixel_io::nofollow::PRIVATE_MODE,
    )
}

/// Forgets the session: the host started it over. Best effort.
pub fn reset(root: &Path, session: &str) {
    let path = file_for(root, session);
    if refuse_linked_dirs(&path).is_ok() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests;
