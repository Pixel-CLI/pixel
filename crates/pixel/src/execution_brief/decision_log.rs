// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The brief's decision log: one JSON line per prompt the brief judged, kept
//! in `.pixel/brief-decisions.jsonl` so the gate can be tuned on real prompts
//! instead of guesses. Without it a silent brief is invisible: nobody can tell
//! a prompt the gate rightly refused from one it should have answered.
//!
//! The log holds the typed text only (never a pasted block), masked with the
//! same credential scrub every captured git stderr passes, and a hash of the
//! whole typed text to count repeats. It keeps the last [`MAX_LINES`] lines,
//! is written with the owner-only mode, never through a symbolic link, and
//! `PIXEL_BRIEF_LOG=0` switches it off. Writing is best effort: a failure
//! never reaches the prompt.

use std::fs::Permissions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::relevance;

/// File name under the repository's `.pixel` directory.
pub(crate) const LOG_FILE: &str = "brief-decisions.jsonl";
/// Environment opt-out: `0`, `false` or `off` writes nothing.
pub(crate) const LOG_ENV: &str = "PIXEL_BRIEF_LOG";
/// Lines the log keeps; the oldest go first.
pub(crate) const MAX_LINES: usize = 500;
/// Characters of the typed text a line carries.
pub(crate) const LOGGED_TYPED_CHARS: usize = 600;

/// What the brief decided about one prompt, as the log and `pixel brief
/// --json` show it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    /// Milliseconds since the Unix epoch when the decision was recorded.
    pub(crate) ts_ms: u64,
    /// `strong`, `weak`, `prose`, or `None` when the prompt asked nothing
    /// about the repository.
    pub(crate) signal: Option<&'static str>,
    /// `open`, `closed`, `denied`, `unjudged` or `declined`.
    pub(crate) gate: &'static str,
    /// The gate decision silenced the brief (it is always `true` for prose;
    /// a weak prompt records the verdict it would have been refused by).
    pub(crate) enforced: bool,
    /// Why the gate decided as it did.
    pub(crate) reason: Option<String>,
    /// The best file's share of the prompt's keyword weight.
    pub(crate) score: Option<f64>,
    /// The tier the score fell in (`high`, `low`, `off`), when the relevance
    /// decision was made.
    pub(crate) tier: Option<&'static str>,
    /// The gate model's source (the json its constants were copied from).
    pub(crate) model: &'static str,
    pub(crate) best_file: Option<String>,
    /// What the relevance decision weighed, when the probe answered.
    pub(crate) features: Option<relevance::Features>,
    /// The intent judge's label and probability, when it answered.
    pub(crate) judge: Option<(String, f64)>,
    /// The question kind the plan routed to.
    pub(crate) kind: Option<&'static str>,
    pub(crate) ops: usize,
    pub(crate) answered: usize,
    /// Bytes of the brief that was rendered; `0` when none was.
    pub(crate) bytes: usize,
    /// The block carried the search receipt.
    pub(crate) receipt: bool,
    /// The block carried answer excerpts.
    pub(crate) answer: bool,
    /// Bytes of those excerpts.
    pub(crate) excerpt_bytes: usize,
    pub(crate) elapsed_ms: u64,
    /// Where the evidence came from: `daemon` or `local`.
    pub(crate) route: Option<&'static str>,
    /// What the daemon start the prompt gave came to, when it gave one and
    /// the hook saw the outcome (`launched`, `running`, `skipped: …`).
    pub(crate) daemon: Option<&'static str>,
    /// The typed text, masked and bounded.
    pub(crate) typed: String,
    /// SHA-256 of the whole typed text, lowercase hex.
    pub(crate) sha256: String,
}

/// Lowercase hex SHA-256 of `text`.
pub(crate) fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
            out.push_str(word);
        }
    }
    out
}

/// The typed text as a log line carries it: the paste tail only, credential
/// shapes masked, then bounded. Masking first, so a secret cut by the bound
/// is never left half visible.
pub(crate) fn logged_typed(typed: &str) -> String {
    let masked = mask_keys(&pixel_git::redact(super::brief_task(typed)));
    masked.chars().take(LOGGED_TYPED_CHARS).collect()
}

impl Record {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "ts": self.ts_ms,
            "signal": self.signal,
            "gate": self.gate,
            "enforced": self.enforced,
            "reason": self.reason,
            "score": self.score,
            "tier": self.tier,
            "model": self.model,
            "best_file": self.best_file,
            "features": self.features.as_ref().map(|features| json!({
                "struct_per_mille": features.struct_per_mille,
                "question": features.question,
                "ops_share": features.ops_share,
                "struct_ratio": features.struct_ratio,
                "total_weight": features.total_weight,
                "best_structural_weight": features.best_structural_weight,
                "files_considered": features.files_considered,
                "structural_files": features.structural_files,
                "keywords": features.keywords,
                "informative": features.informative,
                "shared": features.shared,
            })),
            "judge": self.judge.as_ref().map(|(label, confidence)| {
                json!({"label": label, "confidence": confidence})
            }),
            "kind": self.kind,
            "ops": self.ops,
            "answered": self.answered,
            "bytes": self.bytes,
            "receipt": self.receipt,
            "answer": self.answer,
            "excerpt_bytes": self.excerpt_bytes,
            "elapsed_ms": self.elapsed_ms,
            "route": self.route,
            "daemon": self.daemon,
            "typed": self.typed,
            "sha256": self.sha256,
        })
    }

    /// The record as one line, without its newline.
    pub(crate) fn line(&self) -> String {
        self.to_json().to_string()
    }
}

/// Whether an environment value leaves a feature on (this log, the brief's
/// receipt and excerpts): only an explicit off word turns it off.
pub(crate) fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// The log of `root`, or `None` when `PIXEL_BRIEF_LOG` switches it off.
pub(crate) fn path_for(root: &Path) -> Option<PathBuf> {
    enabled(std::env::var(LOG_ENV).ok().as_deref())
        .then(|| root.join(pixel_index::index::SHARD_DIR).join(LOG_FILE))
}

/// `existing` with `line` added, keeping the last `cap` lines.
fn with_line(existing: &str, line: &str, cap: usize) -> String {
    let kept: Vec<&str> = existing.lines().collect();
    let skip = (kept.len() + 1).saturating_sub(cap);
    let mut out = String::with_capacity(existing.len() + line.len() + 1);
    for old in kept.iter().skip(skip) {
        out.push_str(old);
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
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
                "{} is a symbolic link; the brief does not log through it",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Add `line` to the log at `path`, keeping its last `cap` lines. Takes an
/// exclusive lock so two hooks never interleave a trim, never creates the
/// directory (a repository without `.pixel` is not one the brief ran in), and
/// never follows a link at the file or its directory: the file holds what the
/// user typed, and a planted link would send it, and the trim, elsewhere. An
/// existing file is brought to the owner-only mode through its descriptor; a
/// file that cannot be (another user's, writable by this one) takes no line.
pub(crate) fn append(path: &Path, line: &str, cap: usize) -> io::Result<()> {
    refuse_linked_dir(path)?;
    let mut file = pixel_git::nofollow::open_lock(path)?;
    file.lock_exclusive()?;
    file.set_permissions(Permissions::from_mode(pixel_git::nofollow::PRIVATE_MODE))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let existing = String::from_utf8_lossy(&bytes);
    let next = with_line(&existing, line, cap);
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(next.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-brief-log-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record() -> Record {
        Record {
            ts_ms: 1_700_000_000_000,
            signal: Some("prose"),
            gate: "open",
            enforced: true,
            reason: Some("2 of 3 key terms".into()),
            score: Some(0.75),
            tier: Some("high"),
            model: "test model",
            best_file: Some("src/a.rs".into()),
            features: Some(relevance::Features {
                struct_per_mille: 3.5,
                question: true,
                ops_share: 0.25,
                struct_ratio: 0.75,
                total_weight: 8.0,
                best_structural_weight: 6.0,
                files_considered: 1161,
                structural_files: 30,
                keywords: 4,
                informative: 4,
                shared: 3,
            }),
            judge: Some(("question".into(), 0.9)),
            kind: Some("lookup"),
            ops: 3,
            answered: 2,
            bytes: 412,
            receipt: true,
            answer: true,
            excerpt_bytes: 1300,
            elapsed_ms: 188,
            route: Some("daemon"),
            daemon: Some("running"),
            typed: "how does the daemon start".into(),
            sha256: sha256_hex("how does the daemon start"),
        }
    }

    #[test]
    fn sha256_hex_should_be_the_lowercase_digest_of_the_text() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    #[test]
    fn to_json_should_carry_every_field_under_its_name() {
        let value = record().to_json();
        assert_eq!(
            value,
            json!({
                "ts": 1_700_000_000_000_u64,
                "signal": "prose",
                "gate": "open",
                "enforced": true,
                "reason": "2 of 3 key terms",
                "score": 0.75,
                "tier": "high",
                "model": "test model",
                "best_file": "src/a.rs",
                "features": {
                    "struct_per_mille": 3.5,
                    "question": true,
                    "ops_share": 0.25,
                    "struct_ratio": 0.75,
                    "total_weight": 8.0,
                    "best_structural_weight": 6.0,
                    "files_considered": 1161,
                    "structural_files": 30,
                    "keywords": 4,
                    "informative": 4,
                    "shared": 3,
                },
                "judge": {"label": "question", "confidence": 0.9},
                "kind": "lookup",
                "ops": 3,
                "answered": 2,
                "bytes": 412,
                "receipt": true,
                "answer": true,
                "excerpt_bytes": 1300,
                "elapsed_ms": 188,
                "route": "daemon",
                "daemon": "running",
                "typed": "how does the daemon start",
                "sha256": sha256_hex("how does the daemon start"),
            })
        );
        let bare = Record {
            signal: None,
            reason: None,
            score: None,
            best_file: None,
            features: None,
            tier: None,
            judge: None,
            kind: None,
            ..record()
        };
        let value = bare.to_json();
        for key in [
            "signal",
            "reason",
            "score",
            "tier",
            "best_file",
            "features",
            "judge",
            "kind",
        ] {
            assert_eq!(value[key], Value::Null, "{key}");
        }
    }

    #[test]
    fn line_should_be_one_json_line_that_reads_back() {
        let mut noisy = record();
        noisy.typed = "two\nlines \"quoted\"".into();
        let line = noisy.line();
        assert!(!line.contains('\n'), "{line}");
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back, noisy.to_json());
        assert_eq!(back["typed"], "two\nlines \"quoted\"");
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
    fn logged_typed_should_keep_only_the_last_paragraph_of_a_pasted_log() {
        let pasted = format!("{}\n\nwhy does it fail", "error line\n".repeat(60));
        assert_eq!(logged_typed(&pasted), "why does it fail");
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
        let path = "src/execution_brief/decision_log.rs";
        assert!(path.len() >= KEY_RUN_CHARS);
        assert_eq!(mask_keys(path), path);
        assert_eq!(
            mask_keys("how does the daemon start"),
            "how does the daemon start"
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
        let path = dir.join(LOG_FILE);
        for n in 0..5 {
            append(&path, &format!("{{\"n\":{n}}}"), 3).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_keep_five_hundred_lines_by_default() {
        let dir = scratch("default-cap");
        let path = dir.join(LOG_FILE);
        for n in 0..=MAX_LINES {
            append(&path, &format!("{n}"), MAX_LINES).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 500);
        assert_eq!(lines.first(), Some(&"1"));
        assert_eq!(lines.last(), Some(&"500"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_be_owner_only_and_not_create_a_missing_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let path = dir.join(LOG_FILE);
        append(&path, "x", 3).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let missing = dir.join("absent").join(LOG_FILE);
        assert!(append(&missing, "x", 3).is_err());
        assert!(!dir.join("absent").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_should_tighten_an_existing_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("tighten");
        let path = dir.join(LOG_FILE);
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
        let path = dir.join(LOG_FILE);
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
        assert!(append(&sidecar.join(LOG_FILE), "leak", 3).is_err());
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
