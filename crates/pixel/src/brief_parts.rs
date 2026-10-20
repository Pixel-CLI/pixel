// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt brief split over several hooks.
//!
//! Claude Code caps what one `UserPromptSubmit` hook injects as additional
//! context at 10,000 characters (10,001 arrives as a preview of about 2,000),
//! and Codex spills a hook's context past 2,500 estimated tokens (four bytes
//! each) to a file behind a head and tail preview. Both cap *per hook* and run
//! the hooks of an event concurrently, so a brief larger than one hook may
//! inject is delivered by several: `pixel run-hook task-event --event
//! prompt-submit --part K/N`.
//!
//! Part 1 computes the brief, as the whole-brief hook always did, and stores
//! it under `<repo>/.pixel/brief-parts/` keyed by the host's session and the
//! prompt. Parts 2 to N read the same payload, wait a bounded time for that
//! file and print their slice. A slice is cut at line boundaries, is at most
//! [`PART_CHARS`] characters (and, for a host that counts bytes, as many
//! bytes), and the slices in order concatenate to the stored brief. A part
//! that has nothing to print, or that times out, prints nothing the host would
//! read as context and never holds the prompt.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

/// Characters one part carries at most: under the 10,000 a Claude Code hook
/// may inject, with room for the host's own framing.
pub(crate) const PART_CHARS: usize = 9_800;
/// Bytes one part carries at most where the host measures bytes (Codex: four
/// bytes per estimated token, 2,500 tokens): the same figure as [`PART_CHARS`],
/// since 9,800 bytes stay under 10,000.
pub(crate) const PART_BYTES: usize = 9_800;
/// What a part after the first waits beyond the bound part 1 is given: part 1
/// stores the brief and marks it done once its own bound has passed.
pub(crate) const WAIT_SLACK: Duration = Duration::from_millis(1_000);
const MISSING_HEAD_STATUS: &[u8] = b"missing-head";
/// How often a waiting part looks for the stored brief.
pub(crate) const POLL: Duration = Duration::from_millis(15);
/// Directory under `.pixel/` holding the stored briefs.
pub(crate) const CACHE_DIR: &str = "brief-parts";
/// A stored brief older than this is deleted by the next part 1.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// A stored brief is read up to this many bytes: [`PART_CHARS`] for each of
/// the most parts, at four bytes a character.
const MAX_STORED_BYTES: u64 = 8 * 9_800 * 4;

/// One slice of a split brief: `index` of `count`, both from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BriefPart {
    pub index: u8,
    pub count: u8,
}

impl FromStr for BriefPart {
    type Err = String;

    /// `K/N` with `1 <= K <= N <= BRIEF_PARTS_MAX`.
    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "expected K/N with 1 <= K <= N <= {}, got `{text}`",
                pixel_setup::config::BRIEF_PARTS_MAX
            )
        };
        let (index, count) = text.split_once('/').ok_or_else(bad)?;
        let index: u8 = index.parse().map_err(|_| bad())?;
        let count: u8 = count.parse().map_err(|_| bad())?;
        ((1..=pixel_setup::config::BRIEF_PARTS_MAX).contains(&count)
            && (1..=count).contains(&index))
        .then_some(Self { index, count })
        .ok_or_else(bad)
    }
}

/// What one part may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) chars: usize,
    pub(crate) bytes: usize,
}

impl Limits {
    /// Characters only: a host that counts characters.
    pub(crate) const CHARS: Self = Self {
        chars: PART_CHARS,
        bytes: usize::MAX,
    };
    /// Characters and bytes: a host that estimates tokens from bytes.
    pub(crate) const BYTES: Self = Self {
        chars: PART_CHARS,
        bytes: PART_BYTES,
    };
}

/// `text` cut into pieces of at most `limits`, at line boundaries: a piece
/// takes whole lines while they fit, and a line larger than a piece is cut at
/// a character boundary. The pieces concatenate to `text`, and the same text
/// always gives the same pieces.
pub(crate) fn split(text: &str, limits: Limits) -> Vec<&str> {
    let (max_chars, max_bytes) = (limits.chars.max(1), limits.bytes.max(4));
    let mut pieces = Vec::new();
    // The open piece starts at `start`; its last, unfinished line at `line`.
    let (mut start, mut line) = (0, 0);
    let (mut chars, mut bytes) = (0, 0);
    let (mut line_chars, mut line_bytes) = (0, 0);
    for (offset, c) in text.char_indices() {
        // At most two cuts per character: one back to the last whole line,
        // one inside a line that alone fills a piece.
        for _ in 0..2 {
            if chars < max_chars && bytes + c.len_utf8() <= max_bytes {
                break;
            }
            if line > start {
                // Whole lines precede the unfinished one: they are the piece.
                pieces.push(&text[start..line]);
                start = line;
                chars = line_chars;
                bytes = line_bytes;
            } else {
                // One line fills the piece: cut inside it.
                pieces.push(&text[start..offset]);
                (start, line) = (offset, offset);
                (chars, bytes) = (0, 0);
                (line_chars, line_bytes) = (0, 0);
            }
        }
        chars += 1;
        bytes += c.len_utf8();
        line_chars += 1;
        line_bytes += c.len_utf8();
        if c == '\n' {
            line = offset + 1;
            (line_chars, line_bytes) = (0, 0);
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// The slice `part` serves of `text`: the empty string when the brief has
/// fewer pieces than parts. Pieces past the last part are not served.
pub(crate) fn slice(text: &str, part: BriefPart, limits: Limits) -> &str {
    split(text, limits)
        .get(usize::from(part.index) - 1)
        .copied()
        .unwrap_or("")
}

fn hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// Where the brief of `prompt` in `session` is stored for the parts: the
/// session id and the prompt are hashed, so no host-chosen text reaches a
/// file name.
pub(crate) fn cache_path(root: &Path, session: Option<&str>, prompt: &str) -> PathBuf {
    let session = &hex(session.unwrap_or(""))[..16];
    root.join(pixel_index::index::SHARD_DIR)
        .join(CACHE_DIR)
        .join(format!("{session}-{}.txt", hex(prompt)))
}

/// Refuse a store whose `.pixel` or `brief-parts` directory is a symbolic
/// link: a clone can commit either, and the brief would be written wherever
/// it points. A directory that does not exist yet is fine.
pub(crate) fn refuse_linked_dirs(path: &Path) -> io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    for linked in [dir.parent().unwrap_or(dir), dir] {
        if fs::symlink_metadata(linked).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is a symbolic link; the brief parts do not use it",
                    linked.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Delete the stored briefs under `dir` last written before `now - max_age`,
/// regular files only, and return how many went.
pub(crate) fn prune(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
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

/// The marker beside the stored brief that records part 1's delivery status.
fn done_path(path: &Path) -> PathBuf {
    path.with_extension("done")
}

/// Forget the stored brief at `path` and its marker, so a part that starts
/// before the new one is written waits for it instead of reading the last
/// one's.
pub(crate) fn forget(path: &Path) {
    let _ = refuse_linked_dirs(path).map(|()| {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(done_path(path));
    });
}

/// Part 1 delivered its slice; an empty marker preserves the original format.
pub(crate) fn mark_done(path: &Path) {
    mark_done_with_status(path, b"");
}

/// Part 1 finished without delivering its slice, so a later part may supply it.
pub(crate) fn mark_done_missing_head(path: &Path) {
    mark_done_with_status(path, MISSING_HEAD_STATUS);
}

fn mark_done_with_status(path: &Path, status: &[u8]) {
    let _ = refuse_linked_dirs(path).and_then(|()| {
        let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        fs::create_dir_all(dir)?;
        pixel_git::nofollow::write_replace(
            &done_path(path),
            status,
            pixel_git::nofollow::PRIVATE_MODE,
        )
    });
}

fn done_missing_head(path: &Path) -> bool {
    if refuse_linked_dirs(path).is_err() {
        return false;
    }
    let Ok(file) = pixel_git::nofollow::open_read(&done_path(path)) else {
        return false;
    };
    let mut status = Vec::with_capacity(MISSING_HEAD_STATUS.len() + 1);
    file.take((MISSING_HEAD_STATUS.len() + 1) as u64)
        .read_to_end(&mut status)
        .is_ok_and(|_| status == MISSING_HEAD_STATUS)
}

/// Store `text` (the empty string for a prompt that got no brief, so the
/// other parts do not wait for nothing) at `path`, owner-only and never
/// through a link, and prune what is older than [`MAX_AGE`].
///
/// # Errors
///
/// A linked directory, or any I/O error of the write.
pub(crate) fn store(path: &Path, text: &str, now: SystemTime) -> io::Result<()> {
    refuse_linked_dirs(path)?;
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(dir)?;
    prune(dir, now, MAX_AGE);
    // A new prompt forgets old data before its worker starts. Preserve an
    // explicit missing-head status if that worker finishes after part 1 exits.
    if !done_missing_head(path) {
        let _ = fs::remove_file(done_path(path));
    }
    pixel_git::nofollow::write_replace(path, text.as_bytes(), pixel_git::nofollow::PRIVATE_MODE)
}

/// The brief stored at `path`, waiting at most `bound` for it to appear and
/// looking every `poll`. Once it is there the wait goes on, within the same
/// bound, for part 1's [`mark_done`]: the host orders what several hooks print
/// by when each hook finishes, and a part that prints before part 1 would put
/// the middle of the brief ahead of its start. A part 1 that never marks
/// itself done costs only the rest of the bound. `None` when the brief never
/// comes, or is not readable.
#[cfg(test)]
pub(crate) fn wait_for(path: &Path, bound: Duration, poll: Duration) -> Option<String> {
    wait_for_with_status(path, bound, poll).map(|stored| stored.text)
}

/// The stored brief and whether part 1 explicitly finished without its head.
pub(crate) struct StoredBrief {
    pub(crate) text: String,
    pub(crate) missing_head: bool,
}

/// Wait for a brief and retain the explicit done-marker status for its first part.
/// Only the exact `missing-head` marker shifts later slices; empty legacy markers
/// retain the old part mapping.
pub(crate) fn wait_for_with_status(
    path: &Path,
    bound: Duration,
    poll: Duration,
) -> Option<StoredBrief> {
    let deadline = Instant::now() + bound;
    let mut text = None;
    loop {
        if text.is_none() {
            match refuse_linked_dirs(path).and_then(|()| pixel_git::nofollow::open_read(path)) {
                Ok(file) => {
                    let mut stored = String::new();
                    file.take(MAX_STORED_BYTES)
                        .read_to_string(&mut stored)
                        .ok()?;
                    text = Some(stored);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        if text.is_some() && fs::symlink_metadata(done_path(path)).is_ok() {
            return Some(StoredBrief {
                text: text.take()?,
                missing_head: done_missing_head(path),
            });
        }
        if Instant::now() >= deadline {
            return text.map(|text| StoredBrief {
                text,
                missing_head: false,
            });
        }
        std::thread::sleep(poll);
    }
}

/// The brief stored at `path`, waiting at most `bound` for it to appear and
/// looking every `poll`; unlike [`wait_for_with_status`] it does not wait for
/// part 1's [`mark_done`], since it is part 1 that asks. `None` when none is
/// stored in time or it is not readable.
pub(crate) fn wait_stored(path: &Path, bound: Duration, poll: Duration) -> Option<String> {
    let deadline = Instant::now() + bound;
    loop {
        match refuse_linked_dirs(path).and_then(|()| pixel_git::nofollow::open_read(path)) {
            Ok(file) => {
                let mut stored = String::new();
                file.take(MAX_STORED_BYTES)
                    .read_to_string(&mut stored)
                    .ok()?;
                return Some(stored);
            }
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound => {}
                _ => return None,
            },
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

/// Debug builds only: `PIXEL_BRIEF_TEST_PAD_PARTS=N` makes the brief exactly
/// `N` parts long, each ending in a marker line, whatever the prompt asked.
/// It is how the CLI tests and the live proof reach an oversized brief
/// without a prompt that produces one; a release build has no such variable.
#[cfg(debug_assertions)]
pub(crate) const TEST_PAD_ENV: &str = "PIXEL_BRIEF_TEST_PAD_PARTS";

/// The marker line closing part `index` of `count` in a padded brief.
#[cfg(debug_assertions)]
pub(crate) fn marker(index: u8, count: u8) -> String {
    format!("PIXEL-PART-MARKER-{index}-OF-{count}\n")
}

/// One filler line: 99 characters and a newline.
#[cfg(debug_assertions)]
const FILLER: &str = "PAD pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad
";

/// `text` followed by filler lines and marker lines so that, split with
/// `limits`, it is `count` pieces and piece `k` ends with [`marker`]`(k)`.
/// The brief itself must fit the first piece.
#[cfg(debug_assertions)]
pub(crate) fn pad_to_parts(text: &str, count: u8, limits: Limits) -> String {
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let (mut chars, mut bytes) = split(&out, limits)
        .last()
        .map_or((0, 0), |last| (last.chars().count(), last.len()));
    for index in 1..=count {
        let end = marker(index, count);
        while chars + FILLER.len() + end.len() <= limits.chars
            && bytes + FILLER.len() + end.len() <= limits.bytes
        {
            out.push_str(FILLER);
            chars += FILLER.len();
            bytes += FILLER.len();
        }
        out.push_str(&end);
        (chars, bytes) = (0, 0);
    }
    out
}

#[cfg(test)]
mod tests;
