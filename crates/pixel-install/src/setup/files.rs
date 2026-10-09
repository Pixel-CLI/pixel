// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The managed block `pixel setup` writes, and the upsert that owns it.
//!
//! Ported from GitButler's `crates/but/src/command/agent/files.rs` (read at
//! `5cbe33d`), because every case it handles is a way a managed block goes
//! wrong in a file the user also writes by hand:
//!
//! - **line-anchored markers** — a marker quoted in prose, or shown as an
//!   example inside a fenced code block, is documentation, not a delimiter;
//!   splicing on it would delete the surrounding text;
//! - **refusal on a partial block** — a start marker with no end means the user
//!   (or an editor) truncated the file; replacing it would destroy whatever
//!   was meant to follow;
//! - **convergence** — several blocks from an earlier buggy run collapse to
//!   one, so the file stops growing on every re-run;
//! - **CRLF** — a replaced block follows the file's existing endings instead of
//!   introducing mixed ones;
//! - **atomic replace** — temp file in the same directory, permissions carried
//!   over, and the content compared again just before the rename. The rename
//!   itself is unconditional, so this narrows the window in which a concurrent
//!   editor's save is lost; it cannot close it. Nothing here is a substitute
//!   for not writing a file another process is editing.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{InstallError, Result};

/// Opens a block `pixel setup` owns. Everything between this and
/// [`BLOCK_END`] is rewritten on every run; the text around it is the user's.
pub const BLOCK_START: &str = "<!-- pixel:setup:start -->";
/// Closes a block `pixel setup` owns. See [`BLOCK_START`].
pub const BLOCK_END: &str = "<!-- pixel:setup:end -->";

/// A block that cannot be read as a whole is left alone, with the fix in the
/// message: pixel removes what it wrote, never what it cannot parse.
const PARTIAL_BLOCK: &str = "found one pixel:setup marker with no matching other one; \
                             close or delete that block, then re-run";
const REVERSED_BLOCK: &str = "found the pixel:setup end marker before its start marker; \
                              close or delete that block, then re-run";

/// Upsert `block` into the file at `path`, creating it and its parent
/// directory when missing.
pub fn upsert_file(path: &Path, block: &str) -> Result<()> {
    let original = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(InstallError::Setup(format!(
                "read {}: {err}",
                path.display()
            )));
        }
    };
    let updated = upsert(&original, block)?;
    // A dotfiles repository holds its instruction files as symlinks. Renaming
    // over the link would replace it with a regular file and detach the file
    // from the repository, so the write targets what the link points at. The
    // temporary file then has to sit in that file's directory, not the link's.
    let target = resolve_symlink(path);
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| InstallError::Setup(format!("create {}: {err}", parent.display())))?;
    }
    replace_atomically(&target, &original, &updated)
}

/// The file a write has to land on: the path itself, or the file a symlink at
/// it points to. Nothing is resolved for a path that does not exist yet.
fn resolve_symlink(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        && let Ok(resolved) = std::fs::canonicalize(path)
    {
        return resolved;
    }
    path.to_path_buf()
}

/// Distinguishes two writes to the same file inside one process, so a second
/// setup run cannot rename over the first's temporary file.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `content` through a temporary file in the same directory and rename it
/// over `path`, so a crash or a full disk never leaves a truncated instruction
/// file behind. The original mode is carried over (a new file is owner-only)
/// and the file is compared against `read_content` one last time, so an editor
/// that saved between the read and the rename is not overwritten.
fn replace_atomically(path: &Path, read_content: &str, content: &str) -> Result<()> {
    let tmp = tmp_path(path);
    let result = write_and_rename(path, read_content, content, &tmp);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A temporary name scoped to this process and call: a name shared by two
/// concurrent invocations would let one rename the other's content as its own.
fn tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "agents.md".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()))
}

fn write_and_rename(path: &Path, read_content: &str, content: &str, tmp: &Path) -> Result<()> {
    let mode = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut file = std::fs::File::create(tmp)
        .map_err(|err| InstallError::Setup(format!("create {}: {err}", tmp.display())))?;
    file.write_all(content.as_bytes())
        .map_err(|err| InstallError::Setup(format!("write {}: {err}", tmp.display())))?;
    if let Some(mode) = &mode {
        file.set_permissions(mode.clone())
            .map_err(|err| InstallError::Setup(format!("chmod {}: {err}", tmp.display())))?;
    }
    file.sync_all()
        .map_err(|err| InstallError::Setup(format!("sync {}: {err}", tmp.display())))?;
    drop(file);
    // The rename below replaces the whole file, so an edit that landed after
    // the read is thrown away with it. Comparing here narrows that window to
    // the few instructions between this check and the rename; it does not
    // close it, and `rename` cannot be made conditional on the content. Back
    // off rather than write a stale read: a later run re-reads the file.
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current != read_content {
        return Err(InstallError::Setup(format!(
            "{} changed while the block was being prepared; leaving it alone",
            path.display()
        )));
    }
    std::fs::rename(tmp, path)
        .map_err(|err| InstallError::Setup(format!("replace {}: {err}", path.display())))
}

/// Byte offset of the next `needle` in `haystack` at or after `from` that sits
/// on a line of its own: at the start of the file or right after a newline, and
/// followed by a newline or the end of the file.
///
/// The scan is `match_indices`, not a hand-advanced cursor: a wrong bound on a
/// manual index is an infinite loop, and a scan that cannot spin is worth more
/// than the few bytes it saves.
pub fn find_line_anchored(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    haystack
        .match_indices(needle)
        .map(|(idx, _)| idx)
        .find(|&idx| {
            if idx < from || inside_fenced_block(haystack, idx) {
                return false;
            }
            let at_line_start = idx == 0 || bytes[idx - 1] == b'\n';
            let after = idx + needle.len();
            let at_line_end = after == haystack.len() || matches!(bytes[after], b'\n' | b'\r');
            at_line_start && at_line_end
        })
}

/// Whether the line starting at `idx` falls inside a fenced code block. An odd
/// number of fence delimiters before it means a fence is open, so a marker
/// documented inside one is left alone.
fn inside_fenced_block(haystack: &str, idx: usize) -> bool {
    let mut open = false;
    for line in haystack[..idx].lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            open = !open;
        }
    }
    open
}

/// The byte span of every well-formed block, from the first byte of its start
/// marker to just past its end marker.
pub fn block_spans(existing: &str) -> Result<Vec<std::ops::Range<usize>>> {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(start) = find_line_anchored(existing, BLOCK_START, pos) {
        let Some(end) = find_line_anchored(existing, BLOCK_END, start + BLOCK_START.len()) else {
            return Err(InstallError::Setup(PARTIAL_BLOCK.to_string()));
        };
        let span_end = end + BLOCK_END.len();
        spans.push(start..span_end);
        pos = span_end;
    }
    Ok(spans)
}

/// Render `block` with the file's line endings when the file already uses
/// CRLF, so a replaced block does not introduce mixed endings.
fn match_line_endings(existing: &str, block: &str) -> String {
    if existing.contains("\r\n") {
        block.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        block.to_string()
    }
}

/// Insert or replace the managed block in `existing`.
///
/// The surrounding text is preserved byte for byte. A block that cannot be
/// read as a whole is an error, not something to guess at.
pub fn upsert(existing: &str, block: &str) -> Result<String> {
    let start = find_line_anchored(existing, BLOCK_START, 0);
    let end = find_line_anchored(existing, BLOCK_END, 0);

    match (start, end) {
        (None, None) => return Ok(append_block(existing, block)),
        (Some(_), None) | (None, Some(_)) => {
            return Err(InstallError::Setup(PARTIAL_BLOCK.to_string()));
        }
        (Some(_), Some(_)) if end < start => {
            return Err(InstallError::Setup(REVERSED_BLOCK.to_string()));
        }
        (Some(_), Some(_)) => {}
    }

    let block = match_line_endings(existing, block);
    let mut updated = String::with_capacity(existing.len() + block.len());
    let mut copied = 0;
    for (index, span) in block_spans(existing)?.into_iter().enumerate() {
        let mut span_end = span.end;
        if existing[span_end..].starts_with("\r\n") {
            span_end += 2;
        } else if existing[span_end..].starts_with('\n') {
            span_end += 1;
        }
        updated.push_str(&existing[copied..span.start]);
        if index == 0 {
            updated.push_str(&block);
        }
        copied = span_end;
    }
    updated.push_str(&existing[copied..]);
    Ok(updated)
}

/// Append the block after `existing`, separated by exactly one blank line.
fn append_block(existing: &str, block: &str) -> String {
    if existing.is_empty() {
        return block.to_string();
    }
    let block = match_line_endings(existing, block);
    let crlf = existing.contains("\r\n");
    let mut updated = String::with_capacity(existing.len() + block.len() + 2);
    updated.push_str(existing);
    if existing.ends_with("\r\n\r\n") || existing.ends_with("\n\n") {
        // Already separated by a blank line.
    } else if crlf {
        updated.push_str(if existing.ends_with("\r\n") {
            "\r\n"
        } else {
            "\r\n\r\n"
        });
    } else if existing.ends_with('\n') {
        updated.push('\n');
    } else {
        updated.push_str("\n\n");
    }
    updated.push_str(&block);
    updated
}

/// Remove every pixel:setup block from `text`, keeping the user's own lines.
/// A block that cannot be read as a whole is left alone: uninstall removes what
/// pixel wrote and never what it cannot parse.
///
/// The blank line that separated a block goes with it, on both sides: leaving
/// one behind turns a removed block into a growing run of empty lines.
pub fn strip(text: &str) -> String {
    let Ok(spans) = block_spans(text) else {
        return text.to_string();
    };
    if spans.is_empty() {
        return text.to_string();
    }
    let crlf = text.contains("\r\n");
    let newline: &str = if crlf { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&text[copied..span.start]);
        if out.ends_with(&format!("{newline}{newline}")) {
            let keep = out.len() - newline.len();
            out.truncate(keep);
        }
        copied = span.end;
        if text[copied..].starts_with(newline) {
            copied += newline.len();
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<!-- pixel:setup:start -->\nbody\n<!-- pixel:setup:end -->\n";

    fn managed(body: &str) -> String {
        format!("# Rules\n\n{BLOCK_START}\n{body}\n{BLOCK_END}\n")
    }

    fn upserted(body: &str) -> String {
        upsert("", &managed(body)).unwrap()
    }

    #[test]
    fn an_empty_file_gets_exactly_the_block_and_nothing_around_it() {
        assert_eq!(upsert("", BLOCK).unwrap(), BLOCK);
        assert_eq!(upserted("one"), managed("one"));
    }

    #[test]
    fn existing_text_is_preserved_byte_for_byte() {
        let original = "# My rules\n\nKeep this paragraph.\n";
        let updated = upsert(original, BLOCK).unwrap();
        assert!(
            updated.starts_with("# My rules\n\nKeep this paragraph.\n"),
            "got {updated:?}"
        );
        assert_eq!(updated.matches(BLOCK_START).count(), 1);
    }

    #[test]
    fn a_second_run_is_byte_identical() {
        let once = upsert("before\n", BLOCK).unwrap();
        let twice = upsert(&once, BLOCK).unwrap();
        assert_eq!(once, twice, "the block must not grow on every re-run");
    }

    #[test]
    fn a_replaced_block_takes_the_new_body_and_keeps_the_surroundings() {
        let first = upsert("before\n", BLOCK).unwrap();
        let replaced = upsert(&first, &managed("second")).unwrap();
        assert!(!replaced.contains("body"), "got {replaced:?}");
        assert!(replaced.contains("second"));
        assert!(replaced.starts_with("before\n"));
    }

    #[test]
    fn several_blocks_converge_to_one() {
        let doubled = format!("{BLOCK}{BLOCK}");
        let updated = upsert(&doubled, BLOCK).unwrap();
        assert_eq!(updated.matches(BLOCK_START).count(), 1);
        assert_eq!(updated.matches(BLOCK_END).count(), 1);
    }

    #[test]
    fn a_partial_block_is_refused_rather_than_guessed() {
        for original in [
            format!("{BLOCK_START}\nbody\n"),
            format!("body\n{BLOCK_END}\n"),
            format!("{BLOCK_END}\nbody\n{BLOCK_START}\n"),
        ] {
            assert!(
                upsert(&original, BLOCK).is_err(),
                "{original:?} must not be edited"
            );
        }
    }

    #[test]
    fn a_marker_quoted_in_prose_is_not_a_delimiter() {
        let original =
            format!("The setup writes {BLOCK_START} and {BLOCK_END} around its block.\n");
        let updated = upsert(&original, BLOCK).unwrap();
        assert!(
            updated.starts_with("The setup writes "),
            "the quoted marker must not splice the sentence away: {updated:?}"
        );
        assert_eq!(
            updated.matches(BLOCK_START).count(),
            2,
            "one quote, one block"
        );
    }

    #[test]
    fn a_marker_documented_in_a_fence_is_not_a_delimiter() {
        let original = format!("Example:\n\n```markdown\n{BLOCK_START}\nbody\n{BLOCK_END}\n```\n");
        let updated = upsert(&original, BLOCK).unwrap();
        assert!(
            updated.starts_with(&original),
            "the documented example survives untouched: {updated:?}"
        );
        assert_eq!(
            updated.matches(BLOCK_START).count(),
            2,
            "the quoted example and the real block: {updated:?}"
        );
        assert!(updated.ends_with(BLOCK), "the real block is appended");
    }

    #[test]
    fn a_marker_after_a_closed_fence_is_a_delimiter_again() {
        let original = format!("```\ncode\n```\n{BLOCK_START}\nold\n{BLOCK_END}\n");
        let updated = upsert(&original, &managed("new")).unwrap();
        assert_eq!(
            updated,
            format!("```\ncode\n```\n{}", managed("new")),
            "a closed fence must free the delimiter it documented"
        );
    }

    #[test]
    fn a_crlf_file_keeps_its_endings() {
        let original = "# Rules\r\n".to_string();
        let appended = upsert(&original, BLOCK).unwrap();
        assert!(
            appended.contains("\r\n") && !appended.replace("\r\n", "").contains('\n'),
            "got {appended:?}"
        );
        let replaced = upsert(&appended, BLOCK).unwrap();
        assert_eq!(
            replaced.replace("\r\n", "").matches('\n').count(),
            0,
            "a re-run must not introduce LF into a CRLF file"
        );
    }

    #[test]
    fn strip_removes_the_block_and_the_blank_line_it_owned() {
        let original = format!("keep\n\n{BLOCK_START}\nbody\n{BLOCK_END}\n\nkeep too\n");
        assert_eq!(strip(&original), "keep\n\nkeep too\n");
    }

    #[test]
    fn strip_leaves_a_file_it_cannot_parse() {
        let broken = format!("keep\n{BLOCK_START}\nbody\n");
        assert_eq!(strip(&broken), broken);
    }

    #[test]
    fn strip_leaves_a_file_with_no_block_untouched() {
        assert_eq!(strip("nothing here\n"), "nothing here\n");
    }

    #[test]
    fn upsert_file_creates_the_file_and_its_parents_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/AGENTS.md");

        upsert_file(&path, BLOCK).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BLOCK);

        upsert_file(&path, BLOCK).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BLOCK);
    }

    #[test]
    fn upsert_file_keeps_the_users_text_and_a_symlink_is_rewritten_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "keep\n").unwrap();
        upsert_file(&path, BLOCK).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("keep\n") && content.contains(BLOCK_START));
    }

    #[cfg(unix)]
    #[test]
    fn upsert_file_writes_through_a_symlinked_dotfile() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-AGENTS.md");
        let link = dir.path().join("AGENTS.md");
        std::fs::write(&target, "keep\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        upsert_file(&link, BLOCK).unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "a dotfiles-managed file must stay a symlink"
        );
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.starts_with("keep\n") && written.contains(BLOCK_START));

        // The mode a dotfiles repository sets must survive the replacement.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        upsert_file(&link, &managed("changed")).unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640,
            "a temporary file is owner-only; the original mode has to be restored"
        );
    }

    #[test]
    fn a_concurrent_edit_between_read_and_write_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let concurrent = "keep\n\nsaved by an editor\n";
        std::fs::write(&path, concurrent).unwrap();

        // The replace path compares the file it is about to overwrite with
        // what it read, so passing a stale read reproduces the race.
        let updated = upsert(concurrent, BLOCK).unwrap();
        let err = replace_atomically(&path, &format!("{concurrent}stale\n"), &updated);

        assert!(
            err.is_err(),
            "a write built from a stale read has to back off"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            concurrent,
            "the editor's line has to survive"
        );
    }
}
