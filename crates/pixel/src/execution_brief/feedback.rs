// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What an agent read after the brief: `.pixel/brief-feedback.jsonl`.
//!
//! The decision log says what the brief judged and how big it was. It cannot
//! say whether the brief made the agent's tool calls unnecessary, which is the
//! point of it. This log joins the two from the host's own hooks, per
//! `session_id`:
//!
//! * `prompt`: one line per prompt the brief saw: the brief's size, its gate,
//!   the paths it injected, and the decision log's hash of the typed text;
//! * `part`: the bytes each prompt-submit hook part printed to the host, so a
//!   brief that never reached the agent is told from one that was empty;
//! * `tool`: one line per tool call after it (`PostToolUse`): the tool, whether
//!   it changed a file, and the repository files it named;
//! * `stop`: the agent answered; and, with it, one `turn` line that closes the
//!   prompt: injected paths, read paths, their overlap, and how many tool calls
//!   came before the first answer.
//!
//! `scripts/bench/brief-feedback-summary.py` turns the `turn` lines into the
//! zero-call rate and the injected-versus-read precision and recall.
//!
//! The file holds paths and counts, never prompt text, command text or tool
//! output: a command is read only to find the repository files it names, and
//! only a file that exists inside the repository, is not a credential and is
//! not reached through a link out of it is kept. The session id is stored
//! hashed. It keeps the last [`MAX_LINES`] lines, is written with the
//! owner-only mode through [`decision_log::append_with`] (never through a
//! symbolic link), and `PIXEL_BRIEF_LOG=0` switches it off. Writing is best
//! effort: a failure never reaches the host.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::decision_log;

/// File name under the repository's `.pixel` directory.
pub(crate) const LOG_FILE: &str = "brief-feedback.jsonl";
/// Lines the log keeps; the oldest go first.
pub(crate) const MAX_LINES: usize = 1_500;
/// Paths one line carries at most.
pub(crate) const MAX_PATHS: usize = 32;
/// Words of one shell command that are looked at as a possible file name.
const MAX_TOKENS: usize = 64;
/// Lines of the log a `turn` line looks back over.
const LOOKBACK_LINES: usize = 600;
/// Characters of a tool name a line carries.
const MAX_TOOL_CHARS: usize = 64;

/// The log of `root`, whether or not it is switched on.
fn path_in(root: &Path) -> PathBuf {
    root.join(pixel_index::index::SHARD_DIR).join(LOG_FILE)
}

/// The log of `root`, or `None` when `PIXEL_BRIEF_LOG` switches it off.
pub(crate) fn path_for(root: &Path) -> Option<PathBuf> {
    decision_log::enabled(std::env::var(decision_log::LOG_ENV).ok().as_deref())
        .then(|| path_in(root))
}

/// The session as the log spells it: sixteen hex digits of the SHA-256 of the
/// host's session id, so no host-chosen text reaches the file.
pub(crate) fn session_key(id: &str) -> String {
    decision_log::sha256_hex(id)[..16].to_string()
}

/// A prompt the brief saw, and what it came to.
pub(crate) struct Prompt<'a> {
    pub(crate) session: &'a str,
    pub(crate) provider: &'a str,
    /// SHA-256 of the prompt as the host sent it.
    pub(crate) prompt: &'a str,
    /// The decision log's hash of the typed text, when the brief judged one.
    pub(crate) decision: Option<&'a str>,
    pub(crate) gate: &'a str,
    pub(crate) kind: Option<&'a str>,
    pub(crate) injected: &'a [String],
    pub(crate) bytes: usize,
}

/// The `prompt` line.
pub(crate) fn prompt_line(ts_ms: u64, prompt: &Prompt) -> String {
    json!({
        "t": "prompt", "ts": ts_ms, "session": session_key(prompt.session),
        "provider": prompt.provider, "prompt": prompt.prompt,
        "decision": prompt.decision, "gate": prompt.gate, "kind": prompt.kind,
        "injected": prompt.injected.iter().take(MAX_PATHS).collect::<Vec<_>>(),
        "bytes": prompt.bytes,
    })
    .to_string()
}

/// The `part` line: hook part `index` of `count` printed `bytes` to the host.
/// `late`: the part waited for the brief and did not get it in time.
pub(crate) fn part_line(
    ts_ms: u64,
    session: &str,
    provider: &str,
    prompt: &str,
    (index, count): (u8, u8),
    (bytes, late): (usize, bool),
) -> String {
    json!({
        "t": "part", "ts": ts_ms, "session": session_key(session), "provider": provider,
        "prompt": prompt, "part": index, "of": count, "bytes": bytes, "late": late,
    })
    .to_string()
}

/// The `tool` line: a tool call that named `paths` (repository files).
pub(crate) fn tool_line(
    ts_ms: u64,
    session: &str,
    provider: &str,
    tool: &str,
    write: bool,
    paths: &[String],
) -> String {
    json!({
        "t": "tool", "ts": ts_ms, "session": session_key(session), "provider": provider,
        "tool": tool.chars().take(MAX_TOOL_CHARS).collect::<String>(),
        "write": write, "paths": paths.iter().take(MAX_PATHS).collect::<Vec<_>>(),
    })
    .to_string()
}

/// The lines a `Stop` adds to the log as it stands (`existing`): the `stop`
/// line and, when the session's open prompt has not been closed yet, the
/// `turn` line that sums it up.
pub(crate) fn stop_lines(existing: &str, ts_ms: u64, session: &str, provider: &str) -> Vec<String> {
    let key = session_key(session);
    let mut lines =
        vec![json!({"t": "stop", "ts": ts_ms, "session": key, "provider": provider}).to_string()];
    if let Some(turn) = turn_summary(existing, &key, ts_ms) {
        lines.push(turn.to_string());
    }
    lines
}

/// Whether `value` is the `t` kind of a log line.
fn is(value: &Value, kind: &str) -> bool {
    value.get("t").and_then(Value::as_str) == Some(kind)
}

/// The strings of `value`'s array `key`, in order.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Append `path` to `out` unless it is there already.
fn push_unique(out: &mut Vec<String>, path: &str) {
    if !out.iter().any(|held| held == path) {
        out.push(path.to_string());
    }
}

/// The turn the session `key` has open in `existing`: its last `prompt` line
/// and the `tool` lines after it, summed up. `None` when no prompt is open
/// (none was seen, or a `turn` line closed it already).
pub(crate) fn turn_summary(existing: &str, key: &str, ts_ms: u64) -> Option<Value> {
    let lines: Vec<&str> = existing.lines().collect();
    let from = lines.len().saturating_sub(LOOKBACK_LINES);
    let mut open: Option<(Value, Vec<Value>)> = None;
    for line in &lines[from..] {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("session").and_then(Value::as_str) != Some(key) {
            continue;
        }
        if is(&value, "prompt") {
            open = Some((value, Vec::new()));
        } else if is(&value, "turn") {
            open = None;
        } else if is(&value, "tool")
            && let Some((_, tools)) = open.as_mut()
        {
            tools.push(value);
        }
    }
    let (prompt, tools) = open?;
    let injected = strings(&prompt, "injected");
    let (mut read, mut edited) = (Vec::new(), Vec::new());
    for tool in &tools {
        let target = if tool.get("write").and_then(Value::as_bool) == Some(true) {
            &mut edited
        } else {
            &mut read
        };
        strings(tool, "paths")
            .iter()
            .for_each(|path| push_unique(target, path));
    }
    let overlap: Vec<&String> = injected
        .iter()
        .filter(|path| read.contains(path) || edited.contains(path))
        .collect();
    Some(json!({
        "t": "turn", "ts": ts_ms, "session": key,
        "provider": prompt["provider"], "prompt": prompt["prompt"],
        "decision": prompt["decision"], "gate": prompt["gate"], "kind": prompt["kind"],
        "briefed": prompt["bytes"].as_u64().unwrap_or(0) > 0,
        "bytes": prompt["bytes"],
        "injected": injected, "read": read, "edited": edited, "overlap": overlap,
        "calls": tools.len(),
    }))
}

/// Whether `path` is a file the log may name: a credential-shaped path never
/// enters it.
fn loggable(path: &Path) -> bool {
    !pixel_index::index::credential_path(path)
}

/// The words of a shell `command` that may be file names: split at blanks and
/// shell operators, quotes dropped, options and anything with an expansion
/// left out.
pub(crate) fn command_words(command: &str) -> Vec<String> {
    command
        .split(|ch: char| ch.is_whitespace() || "|;&<>()".contains(ch))
        .map(|word| word.trim_matches(['\'', '"']))
        .filter(|word| {
            !word.is_empty()
                && !word.starts_with('-')
                && !word.contains(['$', '`', '*', '?', '{', '['])
        })
        .take(MAX_TOKENS)
        .map(str::to_string)
        .collect()
}

/// `candidate` as a path relative to the canonical repository `root`: only a
/// file that exists, whose canonical path is inside `root` (a link out of the
/// repository is not followed), and that is not credential-shaped.
pub(crate) fn repository_file(root: &Path, cwd: &Path, candidate: &str) -> Option<String> {
    if candidate.len() > 1_024 || candidate.contains('\0') {
        return None;
    }
    let given = Path::new(candidate);
    let joined = if given.is_absolute() {
        given.to_path_buf()
    } else {
        cwd.join(given)
    };
    let canonical = joined.canonicalize().ok()?;
    if !canonical.is_file() {
        return None;
    }
    let relative = canonical.strip_prefix(root).ok()?;
    loggable(relative).then(|| relative.to_string_lossy().replace('\\', "/"))
}

/// The repository files a tool call named, in order: the `file_path`, `path`
/// or `notebook_path` of its input, and the existing files among the words of
/// its shell `command`. Text of the input other than these never leaves this
/// function.
pub(crate) fn tool_targets(root: &Path, cwd: &Path, input: &Value) -> Vec<String> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut candidates: Vec<String> = ["file_path", "path", "notebook_path"]
        .iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if let Some(command) = ["command", "cmd"]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
    {
        candidates.extend(command_words(command));
    }
    let mut out = Vec::new();
    for candidate in &candidates {
        if let Some(path) = repository_file(&root, cwd, candidate) {
            push_unique(&mut out, &path);
        }
        if out.len() >= MAX_PATHS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-brief-feedback-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn briefed<'a>(session: &'a str, injected: &'a [String], bytes: usize) -> Prompt<'a> {
        Prompt {
            session,
            provider: "codex",
            prompt: "p1",
            decision: Some("d1"),
            gate: "open",
            kind: Some("lookup"),
            injected,
            bytes,
        }
    }

    fn log(lines: &[String]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn session_key_should_be_sixteen_hex_digits_of_the_hashed_id() {
        let key = session_key("session-1");
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(key, decision_log::sha256_hex("session-1")[..16]);
        assert_ne!(key, session_key("session-2"));
        assert!(!key.contains("session"));
    }

    #[test]
    fn a_turn_should_join_the_injected_paths_to_the_paths_read_before_the_answer() {
        let injected = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let existing = log(&[
            prompt_line(1, &briefed("s", &injected, 900)),
            tool_line(2, "s", "codex", "Read", false, &["src/a.rs".to_string()]),
            tool_line(3, "s", "codex", "Edit", true, &["src/c.rs".to_string()]),
            tool_line(4, "s", "codex", "Bash", false, &[]),
        ]);
        let turn = turn_summary(&existing, &session_key("s"), 9).unwrap();
        assert_eq!(turn["t"], "turn");
        assert_eq!(turn["decision"], "d1");
        assert_eq!(turn["briefed"], true);
        assert_eq!(turn["bytes"], 900);
        assert_eq!(turn["calls"], 3);
        assert_eq!(turn["injected"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(turn["read"], json!(["src/a.rs"]));
        assert_eq!(turn["edited"], json!(["src/c.rs"]));
        assert_eq!(turn["overlap"], json!(["src/a.rs"]));
    }

    #[test]
    fn a_turn_with_no_tool_call_before_the_stop_is_a_zero_call_turn() {
        let injected = vec!["src/a.rs".to_string()];
        let existing = log(&[prompt_line(1, &briefed("s", &injected, 500))]);
        let lines = stop_lines(&existing, 5, "s", "codex");
        assert_eq!(lines.len(), 2);
        let stop: Value = serde_json::from_str(&lines[0]).unwrap();
        let turn: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(stop["t"], "stop");
        assert_eq!(turn["calls"], 0);
        assert_eq!(turn["overlap"], json!([]));
    }

    #[test]
    fn a_prompt_without_a_brief_is_a_turn_that_was_not_briefed() {
        let existing = log(&[prompt_line(1, &briefed("s", &[], 0))]);
        let turn = turn_summary(&existing, &session_key("s"), 2).unwrap();
        assert_eq!(turn["briefed"], false);
        assert_eq!(turn["bytes"], 0);
    }

    #[test]
    fn a_second_stop_does_not_sum_the_same_prompt_up_again() {
        let injected = vec!["src/a.rs".to_string()];
        let mut existing = log(&[prompt_line(1, &briefed("s", &injected, 500))]);
        let first = stop_lines(&existing, 2, "s", "codex");
        assert_eq!(first.len(), 2);
        existing.push_str(&log(&first));
        let second = stop_lines(&existing, 3, "s", "codex");
        assert_eq!(second.len(), 1, "only the stop line: the prompt is closed");
    }

    #[test]
    fn a_turn_counts_only_the_session_it_belongs_to() {
        let injected = vec!["src/a.rs".to_string()];
        let existing = log(&[
            prompt_line(1, &briefed("one", &injected, 500)),
            prompt_line(2, &briefed("two", &injected, 500)),
            tool_line(3, "two", "codex", "Read", false, &["src/a.rs".to_string()]),
            tool_line(4, "two", "codex", "Read", false, &["src/z.rs".to_string()]),
        ]);
        let one = turn_summary(&existing, &session_key("one"), 9).unwrap();
        assert_eq!(one["calls"], 0);
        let two = turn_summary(&existing, &session_key("two"), 9).unwrap();
        assert_eq!(two["calls"], 2);
        assert_eq!(two["overlap"], json!(["src/a.rs"]));
        assert!(turn_summary(&existing, &session_key("three"), 9).is_none());
    }

    #[test]
    fn a_newer_prompt_replaces_an_open_one_that_never_stopped() {
        let injected = vec!["src/a.rs".to_string()];
        let existing = log(&[
            prompt_line(1, &briefed("s", &injected, 500)),
            tool_line(2, "s", "codex", "Read", false, &["src/a.rs".to_string()]),
            prompt_line(3, &briefed("s", &injected, 500)),
        ]);
        let turn = turn_summary(&existing, &session_key("s"), 9).unwrap();
        assert_eq!(
            turn["calls"], 0,
            "the interrupted prompt's calls are not carried over"
        );
    }

    #[test]
    fn a_line_holds_the_hashed_session_and_never_the_raw_id() {
        let line = tool_line(1, "raw-session-id", "claude", "Read", false, &[]);
        assert!(!line.contains("raw-session-id"));
        assert!(line.contains(&session_key("raw-session-id")));
        let part = part_line(1, "raw-session-id", "codex", "p", (2, 3), (0, true));
        assert!(!part.contains("raw-session-id"));
        let value: Value = serde_json::from_str(&part).unwrap();
        assert_eq!(
            (value["part"].as_u64(), value["of"].as_u64()),
            (Some(2), Some(3))
        );
        assert_eq!(
            (value["bytes"].as_u64(), value["late"].as_bool()),
            (Some(0), Some(true))
        );
    }

    #[test]
    fn tool_targets_should_keep_existing_repository_files_and_nothing_else() {
        let root = scratch("targets");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        std::fs::write(root.join(".env"), "KEY=secret-value\n").unwrap();
        let input = json!({"command": "sed -n '1,40p' src/a.rs | rg -n \"secret phrase\" README.md .env nope.rs src/"});
        let targets = tool_targets(&root, &root, &input);
        assert_eq!(targets, vec!["src/a.rs", "README.md"]);
        let joined = targets.join(" ");
        assert!(
            !joined.contains("secret"),
            "no command text, no credential path"
        );
    }

    #[test]
    fn tool_targets_should_read_the_file_path_of_a_structured_tool_and_make_it_relative() {
        let root = scratch("structured");
        std::fs::write(root.join("a.rs"), "x\n").unwrap();
        let absolute = root.join("a.rs").to_string_lossy().to_string();
        let input = json!({"file_path": absolute, "content": "private body"});
        assert_eq!(tool_targets(&root, &root, &input), vec!["a.rs"]);
        let nested = root.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            tool_targets(&root, &nested, &json!({"path": "../a.rs"})),
            vec!["a.rs"]
        );
    }

    #[test]
    fn tool_targets_should_not_follow_a_link_out_of_the_repository() {
        let root = scratch("linked");
        let outside = scratch("linked-outside");
        std::fs::write(outside.join("secret.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
        let input = json!({"file_path": "link.txt", "command": "cat link.txt"});
        assert_eq!(tool_targets(&root, &root, &input), Vec::<String>::new());
        let absolute = outside.join("secret.txt").to_string_lossy().to_string();
        assert!(tool_targets(&root, &root, &json!({"file_path": absolute})).is_empty());
    }

    #[test]
    fn append_with_should_keep_the_cap_when_a_stop_adds_two_lines() {
        let dir = scratch("cap");
        std::fs::create_dir_all(dir.join(".pixel")).unwrap();
        let path = dir.join(".pixel").join(LOG_FILE);
        for n in 0..5 {
            decision_log::append(&path, &format!("{{\"n\":{n}}}"), 4).unwrap();
        }
        decision_log::append_with(&path, 4, |existing| {
            assert_eq!(existing.lines().count(), 4);
            vec!["a".to_string(), "b".to_string()]
        })
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\"n\":3}\n{\"n\":4}\na\nb\n");
    }

    #[test]
    fn the_log_lives_beside_the_decision_log_under_dot_pixel() {
        assert_eq!(
            path_in(Path::new("/repo")),
            Path::new("/repo/.pixel/brief-feedback.jsonl")
        );
        assert_eq!(
            path_in(Path::new("/repo")).parent(),
            decision_log::path_for(Path::new("/repo"))
                .as_deref()
                .and_then(Path::parent)
                .or(Some(Path::new("/repo/.pixel")))
        );
    }
}
