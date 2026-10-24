// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Antigravity's deferred metrics relay. A `PostToolUse` hook cannot show the
//! user anything there, so the finalized 🟩 block of an `pixel` call is parked
//! under the conversation's artifact directory and handed over by the
//! `PostInvocation` hook as an injected step, once the invocation that made the
//! call is over. One file per step: `<artifactDirectoryPath>/.pixel-pending-metrics/<stepIdx>.json`.

use std::fs;
use std::path::PathBuf;

use pixel_io::nofollow;
use serde_json::{Value, json};

/// The directory under the artifact path that holds the parked blocks.
const DIR: &str = ".pixel-pending-metrics";

/// The directory for `payload`'s conversation and its id: only a directory
/// under `~/.gemini` is ever written, whatever path the host names.
fn directory(payload: &Value, home: &std::path::Path) -> Option<(PathBuf, String)> {
    let conversation = payload.get("conversationId")?.as_str()?;
    if conversation.is_empty() {
        return None;
    }
    let artifact = payload.get("artifactDirectoryPath")?.as_str()?;
    let gemini_home = home.canonicalize().ok()?.join(".gemini");
    let artifact = artifact
        .strip_prefix("~/")
        .map_or_else(|| PathBuf::from(artifact), |relative| home.join(relative));
    if !artifact.is_absolute() {
        return None;
    }
    let artifact = artifact.canonicalize().ok()?;
    artifact
        .starts_with(&gemini_home)
        .then(|| (artifact.join(DIR), conversation.to_owned()))
}

/// Parks `block` for the step `payload` names. Best effort: no step, no
/// conversation or a path outside `~/.gemini` parks nothing.
pub(super) fn store(payload: &Value, home: &std::path::Path, block: &str) {
    let Some(step) = payload.get("stepIdx").and_then(Value::as_u64) else {
        return;
    };
    let Some((directory, conversation)) = directory(payload, home) else {
        return;
    };
    let record = json!({"conversationId": conversation, "stepIdx": step, "line": block});
    let _ = fs::create_dir_all(&directory).and_then(|()| {
        nofollow::write_replace(
            &directory.join(format!("{step}.json")),
            record.to_string().as_bytes(),
            nofollow::PRIVATE_MODE,
        )
    });
}

/// The blocks parked by the invocation `payload` ends, in step order, taken
/// off disk. A file of another conversation, from before the invocation
/// (`initialNumSteps`) or that does not read back is dropped, not delivered.
pub(super) fn take(payload: &Value, home: &std::path::Path) -> Vec<String> {
    let (Some(conversation), Some(first)) = (
        payload.get("conversationId").and_then(Value::as_str),
        payload.get("initialNumSteps").and_then(Value::as_u64),
    ) else {
        return Vec::new();
    };
    let Some((directory, _)) = directory(payload, home) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut parked: Vec<(u64, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let step = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|name| name.parse::<u64>().ok());
        let record = nofollow::open_read(&path).ok().and_then(|file| {
            serde_json::from_reader::<_, Value>(std::io::Read::take(file, 65_536)).ok()
        });
        let line = step.zip(record).and_then(|(step, record)| {
            (record.get("conversationId").and_then(Value::as_str) == Some(conversation)
                && record.get("stepIdx").and_then(Value::as_u64) == Some(step)
                && step >= first)
                .then(|| {
                    record
                        .get("line")?
                        .as_str()
                        .map(|line| (step, line.to_owned()))
                })
                .flatten()
        });
        let _ = fs::remove_file(&path);
        parked.extend(line);
    }
    let _ = fs::remove_dir(&directory);
    parked.sort_unstable_by_key(|(step, _)| *step);
    parked.into_iter().map(|(_, line)| line).collect()
}

#[cfg(test)]
mod tests;
