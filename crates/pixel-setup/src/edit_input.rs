// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The files an agent's edit tool is about to write and the text they will
//! hold (`specs/hooks.md`, "Before an edit"). Each host names its edit tools
//! and their arguments differently; this reduces them to a path and, when the
//! call carries enough to know it, the file's whole new content.
//!
//! The new content is computed from the arguments and the file as it is now
//! (`read`): a replacement is applied only when it is unambiguous, a patch hunk
//! only when its old lines are found. Anything less is a path with no
//! content, which the check answers by recording the file's baseline and
//! staying silent. A shell command is never parsed for content.

use serde_json::Value;

/// A file an edit tool will write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    /// The path as the call spelled it (absolute or relative).
    pub path: String,
    /// The file's whole new content, when the call determines it.
    pub proposed: Option<String>,
}

/// Keys under which a tool names its file.
const PATH_KEYS: &[&str] = &["file_path", "path", "filePath", "filename", "file"];
/// Keys under which a write tool carries the new content.
const CONTENT_KEYS: &[&str] = &["content", "file_text", "text", "contents"];
/// Keys of the text a replacement removes and the text it puts in.
const OLD_KEYS: &[&str] = &["old_string", "oldText", "old_str", "oldString", "old"];
const NEW_KEYS: &[&str] = &["new_string", "newText", "new_str", "newString", "new"];
/// Keys under which a patch tool carries the patch.
const PATCH_KEYS: &[&str] = &["input", "patch", "command", "cmd", "patchText"];

fn text<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| input.get(*key)?.as_str())
}

/// The patch text of an `apply_patch` call: a string, or the last string of an
/// argv-shaped array (`["apply_patch", "*** Begin Patch…"]`).
fn patch_text(input: &Value) -> Option<&str> {
    PATCH_KEYS.iter().find_map(|key| match input.get(*key)? {
        Value::String(patch) if patch.contains("*** Begin Patch") => Some(patch.as_str()),
        Value::Array(words) => words
            .iter()
            .rev()
            .filter_map(Value::as_str)
            .find(|word| word.contains("*** Begin Patch")),
        _ => None,
    })
}

/// `content` with `old` replaced by `new`: the only occurrence, or every one
/// with `all`. `None` when `old` is empty or absent, or ambiguous without `all`.
fn replace(content: &str, old: &str, new: &str, all: bool) -> Option<String> {
    if old.is_empty() {
        return None;
    }
    match content.matches(old).count() {
        0 => None,
        1 => Some(content.replacen(old, new, 1)),
        _ if all => Some(content.replace(old, new)),
        _ => None,
    }
}

/// The replacements an edit call makes, in order: the `edits` array of a
/// multi-edit, or the one pair at the top level.
fn replacements(input: &Value) -> Vec<(String, String, bool)> {
    let one = |value: &Value| {
        let old = text(value, OLD_KEYS)?;
        let new = text(value, NEW_KEYS).unwrap_or("");
        let all = ["replace_all", "replaceAll"]
            .iter()
            .any(|key| value.get(*key).and_then(Value::as_bool) == Some(true));
        Some((old.to_owned(), new.to_owned(), all))
    };
    match input.get("edits").and_then(Value::as_array) {
        Some(edits) => edits.iter().filter_map(one).collect(),
        None => one(input).into_iter().collect(),
    }
}

/// The content after every replacement, `None` if one cannot be applied.
fn apply_replacements(content: &str, edits: &[(String, String, bool)]) -> Option<String> {
    edits
        .iter()
        .try_fold(content.to_owned(), |text, (old, new, all)| {
            replace(&text, old, new, *all)
        })
}

/// The files a call to `tool` with `input` is about to write.
#[must_use]
pub fn targets(
    tool: &str,
    input: &Value,
    read: &dyn Fn(&str) -> Option<String>,
) -> Vec<EditTarget> {
    let path = text(input, PATH_KEYS).map(str::to_owned);
    match tool.to_ascii_lowercase().as_str() {
        "write" | "write_file" | "create_file" => path
            .map(|path| EditTarget {
                proposed: text(input, CONTENT_KEYS).map(str::to_owned),
                path,
            })
            .into_iter()
            .collect(),
        "edit" | "multiedit" | "replace" | "edit_file" => path
            .map(|path| {
                let edits = replacements(input);
                let proposed = (!edits.is_empty())
                    .then(|| read(&path).and_then(|now| apply_replacements(&now, &edits)))
                    .flatten();
                EditTarget { path, proposed }
            })
            .into_iter()
            .collect(),
        "str_replace_editor" => match input.get("command").and_then(Value::as_str) {
            Some("view") => Vec::new(),
            Some("create") => path
                .map(|path| EditTarget {
                    proposed: text(input, CONTENT_KEYS).map(str::to_owned),
                    path,
                })
                .into_iter()
                .collect(),
            _ => path
                .map(|path| {
                    let edits = replacements(input);
                    let proposed = (!edits.is_empty())
                        .then(|| read(&path).and_then(|now| apply_replacements(&now, &edits)))
                        .flatten();
                    EditTarget { path, proposed }
                })
                .into_iter()
                .collect(),
        },
        "apply_patch" => patch_text(input)
            .map(|patch| patch_targets(patch, read))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// One file section of a patch.
enum Section {
    Update(String, Vec<Vec<String>>),
    Add(String, Vec<String>),
    Delete(String),
}

/// The sections of a Codex `apply_patch` text.
fn sections(patch: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            out.push(Section::Update(path.trim().to_owned(), Vec::new()));
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            out.push(Section::Add(path.trim().to_owned(), Vec::new()));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            out.push(Section::Delete(path.trim().to_owned()));
        } else if line.starts_with("*** ") {
            // `*** Begin Patch`, `*** End Patch`, `*** End of File`, `*** Move to:`.
        } else if let Some(last) = out.last_mut() {
            match last {
                Section::Update(_, hunks) => {
                    if line.starts_with("@@") || hunks.is_empty() {
                        hunks.push(Vec::new());
                    }
                    if !line.starts_with("@@")
                        && let Some(hunk) = hunks.last_mut()
                    {
                        hunk.push(line.to_owned());
                    }
                }
                Section::Add(_, lines) => {
                    if let Some(added) = line.strip_prefix('+') {
                        lines.push(added.to_owned());
                    }
                }
                Section::Delete(_) => {}
            }
        }
    }
    out
}

/// `content` with one hunk applied: its context and removed lines, as a block
/// of whole lines, replaced by its context and added lines.
fn apply_hunk(content: &str, hunk: &[String]) -> Option<String> {
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for line in hunk {
        match line.chars().next() {
            Some('+') => after.push(line[1..].to_owned()),
            Some('-') => before.push(line[1..].to_owned()),
            Some(' ') => {
                before.push(line[1..].to_owned());
                after.push(line[1..].to_owned());
            }
            None => {
                before.push(String::new());
                after.push(String::new());
            }
            Some(_) => return None,
        }
    }
    if before.is_empty() {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let at = (0..=lines.len().checked_sub(before.len())?).find(|&start| {
        lines[start..start + before.len()]
            .iter()
            .zip(&before)
            .all(|(have, want)| have == want)
    })?;
    let mut out: Vec<&str> = lines[..at].to_vec();
    out.extend(after.iter().map(String::as_str));
    out.extend(&lines[at + before.len()..]);
    let mut joined = out.join("\n");
    if content.ends_with('\n') || content.is_empty() {
        joined.push('\n');
    }
    Some(joined)
}

fn patch_targets(patch: &str, read: &dyn Fn(&str) -> Option<String>) -> Vec<EditTarget> {
    sections(patch)
        .into_iter()
        .map(|section| match section {
            Section::Add(path, lines) => EditTarget {
                proposed: Some(format!("{}\n", lines.join("\n"))),
                path,
            },
            Section::Delete(path) => EditTarget {
                proposed: Some(String::new()),
                path,
            },
            Section::Update(path, hunks) => {
                let proposed = read(&path).and_then(|now| {
                    hunks
                        .iter()
                        .try_fold(now, |text, hunk| apply_hunk(&text, hunk))
                });
                EditTarget { path, proposed }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
