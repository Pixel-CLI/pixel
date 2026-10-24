// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `.pixel/journal.jsonl`: one typed line per hook decision (`specs/metrics.md`).
//!
//! The action log says what a command cost; the journal says what a hook
//! decided and, joined by the hashed session, whether the agent still had to
//! look. It holds paths, counts and the masked typed text, never a prompt, a
//! command or an output. The file keeps the last [`MAX_LINES`] lines, is
//! written with the owner-only mode under an exclusive lock, never creates
//! `.pixel/` and never follows a link at the file or its directory; `PIXEL_LOG=0`
//! switches it off. Writing is best effort: a failure never reaches the host.

use std::fs::Permissions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;

pub mod feedback;

/// File name under the repository's `.pixel` directory.
pub const FILE_NAME: &str = "journal.jsonl";
/// Environment switch of the journal: `0`, `false` or `off`.
pub const LOG_ENV: &str = "PIXEL_LOG";
/// Lines the journal keeps; the oldest go first.
pub const MAX_LINES: usize = 1_500;
/// Characters of the typed text a line carries.
pub const LOGGED_TYPED_CHARS: usize = 600;

/// Lowercase hex SHA-256 of `text`.
pub fn sha256_hex(text: &str) -> String {
    pixel_model::hash::Hash::of(text.as_bytes()).to_hex()
}

/// Prefixes the keys of the common providers carry.
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "eyJ",
];
/// A token with a known prefix is a key from this length on.
const PREFIXED_KEY_CHARS: usize = 12;
/// A bare run of key characters is a key from this length on.
const KEY_RUN_CHARS: usize = 32;

/// Whether `token` is shaped like a provider key: a known prefix and some
/// length, or a long unbroken run of key characters. Generous on purpose: a
/// masked identifier costs a less readable line, a leaked key costs more.
fn looks_like_a_key(token: &str) -> bool {
    let token = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    let key_chars = token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    key_chars
        && ((token.len() >= PREFIXED_KEY_CHARS
            && KEY_PREFIXES.iter().any(|prefix| token.starts_with(prefix)))
            || token.len() >= KEY_RUN_CHARS)
}

/// `text` with every key-shaped word replaced by `<redacted>`.
fn mask_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        if looks_like_a_key(word) {
            out.push_str("<redacted>");
            if let Some(space) = word.chars().last().filter(|ch| ch.is_whitespace()) {
                out.push(space);
            }
        } else {
            mask_pieces(word, &mut out);
        }
    }
    out
}

/// Characters that glue a name to its value (`X=ghp_…`, `{"key":"sk-…"}`).
const VALUE_SEPARATORS: [char; 8] = ['=', ':', '"', '\'', ',', ';', '(', ')'];

/// Pushes `word` with each key-shaped piece between separators masked and the
/// separators kept, so `X=ghp_…` becomes `X=<redacted>`.
fn mask_pieces(word: &str, out: &mut String) {
    let is_separator = |ch: char| ch.is_whitespace() || VALUE_SEPARATORS.contains(&ch);
    for piece in word.split_inclusive(is_separator) {
        let (body, separator) = piece
            .strip_suffix(is_separator)
            .map_or((piece, ""), |body| (body, &piece[body.len()..]));
        out.push_str(if looks_like_a_key(body) {
            "<redacted>"
        } else {
            body
        });
        out.push_str(separator);
    }
}

/// The typed text as a log line carries it: the paste tail only, credential
/// shapes masked, then bounded. Masking first, so a secret cut by the bound
/// is never left half visible.
pub fn logged_typed(task: &str) -> String {
    let masked = mask_keys(&pixel_io::redact(task));
    masked.chars().take(LOGGED_TYPED_CHARS).collect()
}

/// Whether an environment value leaves a feature on (this log, the brief's
/// receipt and excerpts): only an explicit off word turns it off.
pub fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// The journal of `root`, or `None` when `PIXEL_LOG` switches it off.
pub fn path_for(root: &Path) -> Option<PathBuf> {
    enabled(std::env::var(LOG_ENV).ok().as_deref()).then(|| path_in(root))
}

/// The journal of `root`, whether or not it is switched on.
pub fn path_in(root: &Path) -> PathBuf {
    pixel_io::sidecar::dir(root).join(FILE_NAME)
}

/// The session as the journal spells it: sixteen hex digits of the SHA-256 of
/// the host's session id, so no host-chosen text reaches the file.
pub fn session_key(id: &str) -> String {
    sha256_hex(id)[..16].to_string()
}

/// `existing` with `line` added, keeping the last `cap` lines.
#[cfg(test)]
fn with_line(existing: &str, line: &str, cap: usize) -> String {
    with_lines(existing, &[line.to_string()], cap)
}

/// `existing` with `added` appended in order, keeping the last `cap` lines.
fn with_lines(existing: &str, added: &[String], cap: usize) -> String {
    let kept: Vec<&str> = existing.lines().collect();
    let skip = (kept.len() + added.len()).saturating_sub(cap);
    let mut out = String::with_capacity(
        existing.len() + added.iter().map(|line| line.len() + 1).sum::<usize>(),
    );
    for old in kept.iter().skip(skip) {
        out.push_str(old);
        out.push('\n');
    }
    // A batch larger than the cap keeps its own tail.
    let from = skip.saturating_sub(kept.len());
    for line in added.iter().skip(from) {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Refuse a log whose directory is a symbolic link: a clone can commit
/// `.pixel` as one, and the log would then be written wherever it points,
/// before any store has checked the directory. A missing directory is the
/// `NotFound` of its metadata.
fn refuse_linked_dir(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link; the journal is not written through it",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Add `line` to the log at `path`, keeping its last `cap` lines. See
/// [`append_with`] for how the file is opened and locked.
pub fn append(path: &Path, line: &str, cap: usize) -> io::Result<()> {
    append_with(path, cap, |_| vec![line.to_string()])
}

/// Add the lines `derive` returns to the log at `path`, keeping its last `cap`
/// lines. `derive` reads the log as it stands under the lock, so a line that
/// summarises the lines before it (a turn's reads, joined to its brief) is
/// computed and written without another hook in between. Takes an exclusive
/// lock so two hooks never interleave a trim, never creates the directory (a
/// repository without `.pixel` is not one the brief ran in), and never follows
/// a link at the file or its directory: the file holds what the user typed, and
/// a planted link would send it, and the trim, elsewhere. An existing file is
/// brought to the owner-only mode through its descriptor; a file that cannot be
/// (another user's, writable by this one) takes no line.
pub fn append_with(
    path: &Path,
    cap: usize,
    derive: impl FnOnce(&str) -> Vec<String>,
) -> io::Result<()> {
    refuse_linked_dir(path)?;
    let mut file = pixel_io::nofollow::open_lock(path)?;
    file.lock_exclusive()?;
    file.set_permissions(Permissions::from_mode(pixel_io::nofollow::PRIVATE_MODE))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let existing = String::from_utf8_lossy(&bytes);
    let next = with_lines(&existing, &derive(&existing), cap);
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(next.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pixel-journal-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_hex_should_be_the_lowercase_digest_of_the_text() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    /// `len` characters of short words, so no word of it is key-shaped.
    fn filler(len: usize) -> String {
        "ab ".repeat(len / 3 + 1)[..len].to_string()
    }

    #[test]
    fn logged_typed_should_mask_a_credential_and_cut_at_the_bound() {
        let masked = logged_typed("deploy with token=abcDEF123456 now");
        assert_eq!(masked, "deploy with token=<redacted> now");
        let long = filler(LOGGED_TYPED_CHARS + 50);
        assert_eq!(logged_typed(&long).chars().count(), LOGGED_TYPED_CHARS);
        // Exactly at the bound, and ending on a letter so the trim keeps it.
        let exact = format!("{}c", filler(LOGGED_TYPED_CHARS - 1));
        assert_eq!(logged_typed(&exact), exact);
        // A password that straddles the bound is masked before the cut: cut
        // first, `https://alice:SUPER` has no `@` left for the scrub to find.
        let tail = " https://alice:SUPERSECRETPASSWORD@example.com/x";
        let inside = tail.find("SUPERSECRET").unwrap() + "SUPER".len();
        let straddle = format!("{}{tail}", filler(LOGGED_TYPED_CHARS - inside));
        let logged = logged_typed(&straddle);
        assert!(!logged.contains("SUPER"), "{logged}");
        assert!(logged.contains("https://<redacted>@"), "{logged}");
    }

    #[test]
    fn mask_keys_should_hide_a_prefixed_key_and_a_long_run_and_keep_prose() {
        // A known prefix from PREFIXED_KEY_CHARS on, the whitespace kept.
        assert_eq!(
            mask_keys("key\tsk-abcdef123456\nnext"),
            "key\t<redacted>\nnext"
        );
        assert_eq!(mask_keys("x ghp_12345678"), "x <redacted>");
        assert_eq!(mask_keys("x ghp_1234567"), "x ghp_1234567");
        // Surrounding punctuation does not hide a key.
        assert_eq!(mask_keys("(AKIAABCDEFGHIJ), ok"), "<redacted> ok");
        // A bare run from KEY_RUN_CHARS on, whatever it starts with.
        let run = "a".repeat(KEY_RUN_CHARS);
        assert_eq!(mask_keys(&format!("{run} end")), "<redacted> end");
        let short = "a".repeat(KEY_RUN_CHARS - 1);
        assert_eq!(mask_keys(&format!("{short} end")), format!("{short} end"));
        // A long path is not a key: `/` is not a key character.
        let path = "crates/pixel-metrics/src/journal.rs";
        assert!(path.len() >= KEY_RUN_CHARS);
        assert_eq!(mask_keys(path), path);
        assert_eq!(
            mask_keys("how does the daemon start"),
            "how does the daemon start"
        );
    }

    #[test]
    fn mask_keys_should_mask_the_value_glued_to_a_name() {
        let anthropic = format!("sk-ant-api03-{}", "a".repeat(40));
        let ghp = format!("ghp_{}", "b".repeat(36));
        assert_eq!(
            mask_keys(&format!("ANTHROPIC_API_KEY={anthropic} next")),
            "ANTHROPIC_API_KEY=<redacted> next"
        );
        assert_eq!(mask_keys(&format!("export X={ghp}")), "export X=<redacted>");
        assert_eq!(
            mask_keys(&format!("{{\"key\":\"{anthropic}\"}}")),
            "{\"key\":\"<redacted>\"}"
        );
        // Ordinary assignments and prose keep.
        assert_eq!(mask_keys("a=b x = 1 key: value"), "a=b x = 1 key: value");
    }

    #[test]
    fn logged_typed_should_mask_secret_named_assignments() {
        assert_eq!(
            logged_typed("GITHUB_TOKEN=abc123 DB_PASSWORD=hunter2 a=b"),
            "GITHUB_TOKEN=<redacted> DB_PASSWORD=<redacted> a=b"
        );
    }

    #[test]
    fn enabled_should_turn_off_for_zero_false_and_off_only() {
        assert!(enabled(None));
        assert!(enabled(Some("1")));
        assert!(enabled(Some("on")));
        assert!(enabled(Some("")));
        for off in ["0", "false", "off"] {
            assert!(!enabled(Some(off)), "{off}");
        }
    }

    #[test]
    fn with_line_should_keep_exactly_the_last_cap_lines() {
        let existing = "a\nb\nc\n";
        // Room for a fourth: nothing is dropped.
        assert_eq!(with_line(existing, "d", 4), "a\nb\nc\nd\n");
        // Exactly full after the new line: still nothing dropped.
        assert_eq!(with_line("a\nb\n", "c", 3), "a\nb\nc\n");
        // One over: the oldest goes.
        assert_eq!(with_line(existing, "d", 3), "b\nc\nd\n");
        assert_eq!(with_line(existing, "d", 1), "d\n");
        assert_eq!(with_line("", "a", 5), "a\n");
    }

    #[test]
    fn append_should_write_a_line_and_keep_only_the_last_cap_of_them() {
        let dir = scratch("cap");
        let path = dir.join(FILE_NAME);
        for n in 0..5 {
            append(&path, &format!("{{\"n\":{n}}}"), 3).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_keep_the_last_cap_lines_by_default() {
        let dir = scratch("default-cap");
        let path = dir.join(FILE_NAME);
        for n in 0..=MAX_LINES {
            append(&path, &format!("{n}"), MAX_LINES).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines.first(), Some(&"1"));
        assert_eq!(lines.last(), Some(&"1500"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_be_owner_only_and_not_create_a_missing_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let path = dir.join(FILE_NAME);
        append(&path, "x", 3).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let missing = dir.join("absent").join(FILE_NAME);
        assert!(append(&missing, "x", 3).is_err());
        assert!(!dir.join("absent").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_tighten_an_existing_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("tighten");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(&path, "new", 3).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_refuse_a_link_at_the_log_and_leave_its_target_alone() {
        let dir = scratch("linked-file");
        let target = dir.join("elsewhere.txt");
        std::fs::write(&target, "keep me\n").unwrap();
        let path = dir.join(FILE_NAME);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(append(&path, "leak", 3).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me\n");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_refuse_a_linked_directory_and_write_nothing_through_it() {
        let dir = scratch("linked-dir");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let sidecar = dir.join(".pixel");
        std::os::unix::fs::symlink(&real, &sidecar).unwrap();
        assert!(append(&sidecar.join(FILE_NAME), "leak", 3).is_err());
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
