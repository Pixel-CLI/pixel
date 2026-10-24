// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The files a session is already working on: what the brief knows about a
//! prompt that says little ("now fix the test") beyond its words.
//!
//! Two sources, both bounded and both paths only:
//!
//! * the tail of the host's transcript (the last [`TRANSCRIPT_TAIL_BYTES`]):
//!   the files its recent tool calls read or edited, found in the structured
//!   inputs of those calls (`file_path`, `path`, `command`...), newest first;
//! * the dirty files of `git status`, most recently written first.
//!
//! Nothing but a repository file leaves this module: a candidate is kept only
//! when it exists inside the repository once its links are resolved and is not
//! credential-shaped (the check the read-feedback log uses), so transcript text
//! never reaches the brief or a log. Everything runs inside [`CONTEXT_WINDOW`]:
//! a source that has not answered by then adds nothing.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use pixel_io::{GitOptions, GitRunner};
use serde_json::Value;

use pixel_metrics::journal::feedback;

/// How long the session's files may take to collect, from the start of the
/// brief (whose whole window is 3 s by default).
pub(crate) const CONTEXT_WINDOW: Duration = Duration::from_millis(150);
/// How much of the end of the transcript is read.
pub(crate) const TRANSCRIPT_TAIL_BYTES: u64 = 64 * 1024;
/// Transcript lines looked at, newest first.
const MAX_LINES: usize = 400;
/// Files kept from the transcript.
pub(crate) const MAX_RECENT: usize = 16;
/// Files kept from `git status`.
pub(crate) const MAX_DIRTY: usize = 16;
/// Dirty entries whose modification time is looked at.
const MAX_STATUS_ENTRIES: usize = 400;
/// Words of one command, and path strings of one line, looked at.
const MAX_CANDIDATES: usize = 96;
/// How deep a transcript line is walked.
const MAX_DEPTH: usize = 8;
/// What `git status` may take and print.
const STATUS_TIMEOUT: Duration = Duration::from_millis(250);
const STATUS_MAX_BYTES: usize = 256 * 1024;

/// Keys whose string value is a file name.
const PATH_KEYS: &[&str] = &["file_path", "filePath", "path", "notebook_path", "filename"];
/// Keys whose value is a shell command (a string, or the words of one).
const COMMAND_KEYS: &[&str] = &["command", "cmd"];

/// The files of a session, newest first within each source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionPaths {
    /// Read or edited by the session's recent tool calls.
    pub(crate) recent: Vec<String>,
    /// Modified in the working tree and not yet committed.
    pub(crate) dirty: Vec<String>,
}

impl SessionPaths {
    /// Both sources as one ranking: the recent files first, then the dirty ones
    /// they do not repeat. A file the session touched and left dirty ranks by
    /// the touch.
    pub(crate) fn ranked(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for path in self.recent.iter().chain(&self.dirty) {
            if !out.contains(path) {
                out.push(path.clone());
            }
        }
        out
    }
}

/// Add the words of `command` (a string or an array of strings) to `out`.
fn command_candidates(command: &Value, out: &mut Vec<String>) {
    let text = match command {
        Value::String(text) => text.clone(),
        Value::Array(words) => words
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return,
    };
    out.extend(feedback::command_words(&text));
}

/// Collect the file-name candidates of one transcript value: the strings under
/// a path key and the words of a command, and the same inside an `arguments`
/// string that holds JSON (Codex records a call's input that way). Free text
/// (a message, a result) is never looked at.
fn candidates(value: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() >= MAX_CANDIDATES {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                if PATH_KEYS.contains(&key.as_str()) && inner.is_string() {
                    out.extend(inner.as_str().map(str::to_string));
                } else if COMMAND_KEYS.contains(&key.as_str()) {
                    command_candidates(inner, out);
                } else if key == "arguments"
                    && let Some(text) = inner.as_str().filter(|text| text.starts_with('{'))
                {
                    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                        candidates(&parsed, depth + 1, out);
                    }
                } else {
                    candidates(inner, depth + 1, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|inner| candidates(inner, depth + 1, out)),
        _ => {}
    }
}

/// The last [`TRANSCRIPT_TAIL_BYTES`] of a regular file at `path`, from a line
/// boundary: the lines of the tail, oldest first. A file that cannot be read,
/// is not a regular file or is not UTF-8 gives none.
fn tail_lines(path: &Path) -> Vec<String> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if !meta.is_file() {
        return Vec::new();
    }
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let start = meta.len().saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file
        .take(TRANSCRIPT_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // The first line of a tail is usually cut in two.
        lines.next();
    }
    lines.map(str::to_string).collect()
}

/// The repository files the recent tool calls of the transcript at `path`
/// read or edited, newest first.
pub(crate) fn transcript_paths(root: &Path, path: &Path) -> Vec<String> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let lines = tail_lines(path);
    let mut out: Vec<String> = Vec::new();
    for line in lines.iter().rev().take(MAX_LINES) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let mut found = Vec::new();
        candidates(&value, 0, &mut found);
        // The last call of a line is its newest.
        for candidate in found.iter().rev() {
            if let Some(file) = feedback::repository_file(&root, &root, candidate)
                && !out.contains(&file)
            {
                out.push(file);
            }
        }
        if out.len() >= MAX_RECENT {
            break;
        }
    }
    out.truncate(MAX_RECENT);
    out
}

/// The files of `git status` that exist, are not generated or credential-shaped
/// and are not under `.pixel`, most recently written first.
pub(crate) fn dirty_paths(root: &Path) -> Vec<String> {
    dirty_paths_within(root, STATUS_TIMEOUT)
}

/// [`dirty_paths`] with the time `git status` gets given: a test on a loaded
/// machine must not read a slow status as an empty tree.
fn dirty_paths_within(root: &Path, timeout: Duration) -> Vec<String> {
    let Ok(canonical) = root.canonicalize() else {
        return Vec::new();
    };
    let runner = GitRunner::with_options(
        root,
        GitOptions {
            timeout: Some(timeout),
            max_output_bytes: Some(STATUS_MAX_BYTES),
        },
    );
    let mut files: Vec<(SystemTime, String)> = runner
        .status_porcelain()
        .into_iter()
        .filter(|(xy, _)| !xy.contains('D'))
        .take(MAX_STATUS_ENTRIES)
        .filter(|(_, path)| !path.starts_with(".pixel/") && !crate::chain::is_generated(path))
        .filter_map(|(_, path)| {
            let file = feedback::repository_file(&canonical, &canonical, &path)?;
            let written = std::fs::metadata(canonical.join(&file))
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((written, file))
        })
        .collect();
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    files
        .into_iter()
        .map(|(_, file)| file)
        .take(MAX_DIRTY)
        .collect()
}

/// The session's files, collected by `deadline`: the transcript tail on this
/// thread, `git status` beside it. A source that is late adds nothing.
pub(crate) fn collect(root: &Path, transcript: Option<&Path>, deadline: Instant) -> SessionPaths {
    let (send, receive) = mpsc::channel();
    let dirty_root = root.to_path_buf();
    // The thread is left to finish on its own when the window ends first.
    let _ = std::thread::Builder::new()
        .name("pixel-brief-dirty".into())
        .spawn(move || {
            let _ = send.send(dirty_paths(&dirty_root));
        });
    let recent = transcript
        .map(|path| transcript_paths(root, path))
        .unwrap_or_default();
    let dirty = receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_default();
    SessionPaths { recent, dirty }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-session-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir.canonicalize().unwrap()
    }

    fn write(root: &Path, name: &str) {
        std::fs::write(root.join(name), "x\n").unwrap();
    }

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }

    fn lines(values: &[Value]) -> String {
        values.iter().map(|v| format!("{v}\n")).collect()
    }

    #[test]
    fn ranked_should_put_the_recent_files_first_and_not_repeat_a_dirty_one() {
        let paths = SessionPaths {
            recent: vec!["a.rs".into(), "b.rs".into()],
            dirty: vec!["c.rs".into(), "a.rs".into()],
        };
        assert_eq!(paths.ranked(), vec!["a.rs", "b.rs", "c.rs"]);
        assert!(SessionPaths::default().ranked().is_empty());
    }

    #[test]
    fn the_transcript_should_give_the_files_of_recent_tool_calls_newest_first_and_no_text() {
        let root = scratch("transcript");
        for name in ["src/a.rs", "src/b.rs", "src/c.rs", "README.md", ".env"] {
            write(&root, name);
        }
        let abs_b = root.join("src/b.rs").to_string_lossy().to_string();
        let transcript = root.join("session.jsonl");
        std::fs::write(
            &transcript,
            lines(&[
                json!({"type":"user","message":{"content":"please open src/c.rs and the secret plan"}}),
                json!({"type":"assistant","message":{"content":[
                    {"type":"tool_use","name":"Read","input":{"file_path": "src/a.rs"}}]}}),
                json!({"type":"assistant","message":{"content":[
                    {"type":"tool_use","name":"Bash","input":{"command":"cat README.md .env | head"}},
                    {"type":"tool_use","name":"Edit","input":{"file_path": abs_b, "old_string":"private text"}}]}}),
                json!({"type":"response_item","payload":{"type":"function_call","name":"shell",
                    "arguments": "{\"command\":[\"bash\",\"-lc\",\"sed -n 1,9p src/a.rs\"]}"}}),
                json!({"type":"tool_result","content":"src/c.rs appeared in a result"}),
            ]),
        )
        .unwrap();
        let recent = transcript_paths(&root, &transcript);
        // Newest first: the Codex call (a.rs), then the Claude line's last call
        // (b.rs), its earlier command (README.md; .env is a credential), and
        // a.rs's earlier read is a repeat. c.rs is only ever named in text.
        assert_eq!(recent, vec!["src/a.rs", "src/b.rs", "README.md"]);
        assert!(
            !recent
                .iter()
                .any(|path| path.contains("secret") || path.contains("c.rs"))
        );
    }

    #[test]
    fn the_transcript_tail_should_skip_a_cut_first_line_and_stay_inside_the_cap() {
        let root = scratch("tail");
        write(&root, "src/old.rs");
        write(&root, "src/new.rs");
        let transcript = root.join("session.jsonl");
        let old = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"src/old.rs"}}]}});
        let padding = json!({"type":"user","message":{"content":"x".repeat(2_000)}});
        let new = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"src/new.rs"}}]}});
        let mut text = format!("{old}\n");
        let filler = (TRANSCRIPT_TAIL_BYTES as usize / 2_000) + 4;
        for _ in 0..filler {
            text.push_str(&format!("{padding}\n"));
        }
        text.push_str(&format!("{new}\n"));
        std::fs::write(&transcript, text).unwrap();
        assert_eq!(transcript_paths(&root, &transcript), vec!["src/new.rs"]);
    }

    #[test]
    fn a_missing_or_irregular_transcript_gives_nothing() {
        let root = scratch("irregular");
        assert!(transcript_paths(&root, &root.join("absent.jsonl")).is_empty());
        assert!(
            transcript_paths(&root, &root).is_empty(),
            "a directory is not a transcript"
        );
        let transcript = root.join("garbage.jsonl");
        std::fs::write(&transcript, b"\xff\xfe not json\n{\"input\": 3}\n").unwrap();
        assert!(transcript_paths(&root, &transcript).is_empty());
    }

    #[test]
    fn dirty_paths_should_list_modified_and_new_files_without_generated_or_credential_ones() {
        let root = scratch("dirty");
        git(&root, &["init", "-q"]);
        for name in ["src/kept.rs", "src/gone.rs", "Cargo.lock"] {
            write(&root, name);
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("src/kept.rs"), "changed\n").unwrap();
        std::fs::remove_file(root.join("src/gone.rs")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "changed\n").unwrap();
        write(&root, "src/new.rs");
        write(&root, ".env");
        let mut dirty = dirty_paths_within(&root, Duration::from_secs(15));
        dirty.sort();
        assert_eq!(dirty, vec!["src/kept.rs", "src/new.rs"]);
    }

    #[test]
    fn collect_should_return_both_sources_inside_the_window() {
        let root = scratch("collect");
        git(&root, &["init", "-q"]);
        write(&root, "src/seen.rs");
        write(&root, "src/edited.rs");
        let transcript = root.join("session.jsonl");
        std::fs::write(
            &transcript,
            lines(&[json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"Read","input":{"file_path":"src/seen.rs"}}]}})]),
        )
        .unwrap();
        let paths = collect(
            &root,
            Some(&transcript),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(paths.recent, vec!["src/seen.rs"]);
        assert!(
            paths.dirty.contains(&"src/edited.rs".to_string()),
            "{paths:?}"
        );
        let none = collect(&root, None, Instant::now() + Duration::from_secs(5));
        assert!(none.recent.is_empty());
    }

    #[test]
    fn collect_should_not_wait_past_its_deadline_for_git() {
        let root = scratch("late");
        let started = Instant::now();
        let paths = collect(&root, None, started);
        assert!(paths.recent.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an expired window waits for nothing"
        );
    }
}
