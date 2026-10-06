// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Codex integration through `~/.codex/config.toml`.
//!
//! Codex reads the `developer_instructions` key of its config file and
//! appends it to the developer message of every session, keeping its own
//! system prompt (`model_instructions_file` would replace that prompt: it
//! becomes the base instructions). A config key reaches every Codex front
//! end that loads the file — the CLI, `codex exec`, the desktop app's
//! bundled binary, the VS Code extension, `spawn_agent` sub-agents — where a
//! shell function only fronts interactive shells that sourced the profile.
//!
//! Older versions embedded Pixel's prompt in the value between
//! [`config::MANAGED_BEGIN`] and [`config::MANAGED_END`]. Installation now
//! removes only that retired block, preserving user-owned instructions and
//! the rest of the TOML file.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Value};

use crate::config::{MANAGED_BEGIN, MANAGED_END};
use crate::install::{CheckStatus, InstallStep, Result, dry_run_summary};

/// The key Codex appends to its developer message.
pub const DEVELOPER_INSTRUCTIONS_KEY: &str = "developer_instructions";

/// `config.toml`, relative to the Codex home.
pub const CODEX_CONFIG_FILE: &str = "config.toml";

/// `hooks.json`, relative to the Codex home — Codex's hook registry, in the
/// nested `hooks.<Event>[{hooks: [{command}]}]` shape Claude Code settings
/// also use. (Distinct from `config::CODEX_HOOKS_FILE`, which is
/// home-relative.)
pub const HOOKS_FILE: &str = "hooks.json";

/// Legacy marker for a Codex metrics callback, used in doctor diagnostics.
pub const METRICS_HOOK_MARKER: &str = "run-hook metrics";
pub const PROMPT_SUBMIT_HOOK_MARKER: &str = "run-hook prompt-submit --provider codex";

/// The agent prompt as bundled in the binary.
#[cfg(test)]
const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

/// The Codex home directory: `$CODEX_HOME` when the caller did not pin a
/// home directory (a real `pixel install`, where Codex itself honours the
/// variable), `<home>/.codex` otherwise (tests and explicit overrides, which
/// must not depend on the invoking environment).
pub(crate) fn codex_home(home: &Path, home_was_explicit: bool) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = std::env::var_os("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home.join(".codex")
}

/// A legacy block fixture for cleanup tests.
#[cfg(test)]
fn managed_block() -> String {
    format!("{MANAGED_BEGIN}\n{AGENT_PROMPT_ASSET}{MANAGED_END}\n")
}

/// Byte range of the managed block inside a `developer_instructions` value,
/// from the begin marker to the end of the line holding the end marker.
fn managed_range(value: &str) -> Option<std::ops::Range<usize>> {
    if value.matches(MANAGED_BEGIN).count() != 1 || value.matches(MANAGED_END).count() != 1 {
        return None;
    }
    let start = value.find(MANAGED_BEGIN)?;
    let end_marker = start + value[start..].find(MANAGED_END)?;
    if start >= end_marker || value[start..end_marker].contains('\r') {
        return None;
    }
    let mut end = end_marker + MANAGED_END.len();
    if value[end..].starts_with('\n') {
        end += 1;
    }
    Some(start..end)
}

/// The value `pixel install` writes for a current value of `existing`:
/// the block replaces a previous one in place, or is appended after the
/// user's own text, separated by a blank line.
#[cfg(test)]
fn merged_value(existing: Option<&str>) -> String {
    let block = managed_block();
    match existing {
        None => block,
        Some(current) => match managed_range(current) {
            Some(range) => {
                let mut out = String::with_capacity(current.len() + block.len());
                out.push_str(&current[..range.start]);
                out.push_str(&block);
                out.push_str(&current[range.end..]);
                out
            }
            None if current.trim().is_empty() => block,
            None => format!("{}\n\n{block}", current.trim_end()),
        },
    }
}

/// The value with the managed block removed. `None` when nothing but the
/// block (and whitespace) was there, so the key can go.
pub(crate) fn value_without_block(current: &str) -> Option<String> {
    let range = managed_range(current)?;
    let mut rest = String::new();
    rest.push_str(&current[..range.start]);
    rest.push_str(&current[range.end..]);
    let rest = rest.trim_end();
    if rest.trim().is_empty() {
        None
    } else {
        Some(format!("{rest}\n"))
    }
}

/// A TOML string value for `text`: a literal multi-line string (`'''`) when
/// the text allows it, so the prompt lands in the file byte for byte with
/// no escaping; `toml_edit`'s own escaped representation otherwise.
fn string_value(text: &str) -> Value {
    if !text.contains("'''") && !text.contains('\r') {
        // Parsing a one-key document is the supported way to obtain a value
        // with a chosen representation; the round trip proves the literal
        // form carries `text` unchanged. A newline right after the opening
        // delimiter is trimmed by TOML, so the text starts on its own line.
        if let Ok(doc) = format!("v = '''\n{text}'''\n").parse::<DocumentMut>()
            && let Some(Item::Value(value)) = doc.get("v")
            && value.as_str() == Some(text)
        {
            return value.clone();
        }
    }
    Value::from(text)
}

fn current_value(doc: &DocumentMut) -> std::result::Result<Option<String>, String> {
    match doc.get(DEVELOPER_INSTRUCTIONS_KEY) {
        None => Ok(None),
        Some(Item::Value(Value::String(s))) => Ok(Some(s.value().clone())),
        Some(_) => Err(format!(
            "`{DEVELOPER_INSTRUCTIONS_KEY}` in config.toml is not a string"
        )),
    }
}

fn read_document(path: &Path) -> std::result::Result<DocumentMut, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    text.parse::<DocumentMut>()
        .map_err(|e| format!("{} does not parse as TOML: {e}", path.display()))
}

/// Write the document atomically: Codex may read the file at any moment.
fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.pixel-tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_hooks(path: &Path) -> std::result::Result<serde_json::Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} does not parse as JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({"hooks": {}})),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_hooks(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.pixel-tmp");
    fs::write(
        &tmp,
        format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
    )?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn is_retired_codex_hook(command: &str, exe: &Path) -> bool {
    crate::routing::pixel_hook_verb(command, exe)
        .is_some_and(|verb| !verb.starts_with("task-event --provider codex --event "))
}

/// `pixel install` step: keep Codex's task-event lifecycle hooks installed,
/// removing the retired automatic metrics and retrieval guidance hooks.
pub(crate) fn install_task_hooks(
    codex_home: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = Some(format!(
        "path={} task-event lifecycle hooks; automatic metrics and retrieval hooks disabled",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-task-hooks".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut value = match read_hooks(&path) {
        Ok(v) => v,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let hooks = value
        .as_object_mut()
        .map(|o| o.entry("hooks").or_insert_with(|| serde_json::json!({})))
        .and_then(serde_json::Value::as_object_mut);
    let Some(hooks) = hooks else {
        return Ok(step(
            CheckStatus::Red,
            format!(
                "`hooks` in {} is not an object — not touched",
                path.display()
            ),
        ));
    };
    let before = hooks.clone();
    crate::routing::remove_matching_hooks(hooks, |command| is_retired_codex_hook(command, exe));
    if let Err(error) =
        crate::routing::merge_task_hooks(hooks, crate::routing::Provider::Codex, exe)
    {
        return Ok(step(CheckStatus::Red, format!("{error} — not touched")));
    }
    if *hooks == before {
        return Ok(step(
            CheckStatus::Green,
            format!("verified task-event hooks in {}", path.display()),
        ));
    }
    let summary = format!(
        "{} task-event hooks in {}",
        if path.is_file() {
            "updated"
        } else {
            "installed"
        },
        path.display()
    );
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    write_hooks(&path, &value)?;
    Ok(step(CheckStatus::Green, summary))
}

/// `pixel doctor` check: task-event hooks are registered without automatic
/// retrieval or metrics hooks.
pub(crate) fn check_task_hooks(
    codex_home: &Path,
    exe: &Path,
) -> std::result::Result<(String, serde_json::Value), String> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "events": [
            "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
            "Stop", "SessionEnd", "SubagentStart", "SubagentStop", "Interrupt"
        ],
        "retired_markers": [METRICS_HOOK_MARKER, PROMPT_SUBMIT_HOOK_MARKER],
    });
    let value = read_hooks(&path)?;
    let mut has_retired_pixel_hook = false;
    if let Some(events) = value.get("hooks").and_then(serde_json::Value::as_object) {
        for entries in events.values().filter_map(serde_json::Value::as_array) {
            for entry in entries {
                for hook in entry
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    has_retired_pixel_hook |= hook
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|command| is_retired_codex_hook(command, exe));
                }
            }
        }
    }
    if has_retired_pixel_hook {
        return Err(format!(
            "retired automatic Pixel Codex hook remains in {} — run `pixel install`",
            path.display()
        ));
    }
    if !crate::routing::task_hooks_registered(&value, crate::routing::Provider::Codex, exe) {
        return Err(format!(
            "task lifecycle hooks missing or asynchronous in {} — run `pixel install`",
            path.display()
        ));
    }
    Ok((
        format!(
            "task-event hooks registered; automatic retrieval and metrics hooks absent in {}",
            path.display()
        ),
        detail,
    ))
}

/// `pixel uninstall` step: take the managed block out of
/// `developer_instructions`, dropping the key when nothing else was in it.
pub(crate) fn remove_developer_instructions(
    codex_home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(CODEX_CONFIG_FILE);
    let detail = Some(format!(
        "path={} key={DEVELOPER_INSTRUCTIONS_KEY}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-config".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    if !path.is_file() {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no codex config.toml — skipping"),
        ));
    }
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let current = match current_value(&doc) {
        Ok(Some(current)) => current,
        Ok(None) => {
            return Ok(step(
                CheckStatus::Green,
                dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
            ));
        }
        Err(error) => return Ok(step(CheckStatus::Red, format!("{error} — not touched"))),
    };
    if !current.contains(MANAGED_BEGIN) && !current.contains(MANAGED_END) {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
        ));
    }
    if managed_range(&current).is_none() {
        return Ok(step(
            CheckStatus::Red,
            format!("partial Pixel markers in {} — not touched", path.display()),
        ));
    }
    let summary = match value_without_block(&current) {
        Some(rest) => {
            if !dry_run {
                doc[DEVELOPER_INSTRUCTIONS_KEY] = Item::Value(string_value(&rest));
            }
            format!(
                "removed the pixel block from {DEVELOPER_INSTRUCTIONS_KEY} in {}, keeping the rest",
                path.display()
            )
        }
        None => {
            if !dry_run {
                doc.remove(DEVELOPER_INSTRUCTIONS_KEY);
            }
            format!(
                "removed {DEVELOPER_INSTRUCTIONS_KEY} from {}",
                path.display()
            )
        }
    };
    if !dry_run {
        write_document(&path, &doc)?;
    }
    Ok(step(CheckStatus::Green, dry_run_summary(dry_run, &summary)))
}

/// `pixel doctor` check: no always-on Pixel block remains in the setting.
///
/// # Errors
///
/// The file cannot be read or parsed, or the key is not a string.
pub(crate) fn carries_pixel_block(codex_home: &Path) -> std::result::Result<bool, String> {
    let doc = read_document(&codex_home.join(CODEX_CONFIG_FILE))?;
    Ok(current_value(&doc)?
        .is_some_and(|value| value.contains(MANAGED_BEGIN) || value.contains(MANAGED_END)))
}

/// The table Codex keeps its per-project settings in.
const PROJECTS_TABLE: &str = "projects";

/// The per-project key naming the trust state.
const TRUST_LEVEL_KEY: &str = "trust_level";

/// A project's trust state in Codex's own config, from
/// `[projects."<path>"] trust_level`. Codex composes a project-scoped
/// `.codex/` layer — the repo-local hooks `pixel install --repo` writes —
/// only for a project it trusts, so an installed guard stays dormant while
/// the entry is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectTrust {
    /// `trust_level = "trusted"`.
    Trusted,
    /// `trust_level = "untrusted"`: an explicit refusal.
    Untrusted,
    /// No entry for this project, or one that does not set the key.
    Unspecified,
}

/// How Codex's config records `project`'s trust. `project` must be spelled
/// the way Codex keys the entry — the canonical path `pixel doctor` is
/// handed as its repo root — or the entry it looks for is another project's
/// and the answer is [`ProjectTrust::Unspecified`].
///
/// # Errors
///
/// The file cannot be read or parsed, or the entry's `trust_level` is not a
/// string.
pub(crate) fn project_trust(
    codex_home: &Path,
    project: &Path,
) -> std::result::Result<ProjectTrust, String> {
    let doc = read_document(&codex_home.join(CODEX_CONFIG_FILE))?;
    let key = project.to_string_lossy();
    let Some(entry) = doc
        .get(PROJECTS_TABLE)
        .and_then(Item::as_table_like)
        .and_then(|projects| projects.get(key.as_ref()))
    else {
        return Ok(ProjectTrust::Unspecified);
    };
    let Some(level) = entry
        .as_table_like()
        .and_then(|entry| entry.get(TRUST_LEVEL_KEY))
    else {
        return Ok(ProjectTrust::Unspecified);
    };
    let Some(level) = level.as_str() else {
        return Err(format!(
            "`{TRUST_LEVEL_KEY}` for {} in config.toml is not a string",
            project.display()
        ));
    };
    Ok(match level {
        "trusted" => ProjectTrust::Trusted,
        _ => ProjectTrust::Untrusted,
    })
}

/// The table under `[hooks]` where Codex records each hook's review.
const HOOK_STATE_TABLE: &str = "state";

/// The key a reviewed hook's entry carries.
const TRUSTED_HASH_KEY: &str = "trusted_hash";

/// The label Codex spells an event with in a `hooks.state` key
/// (`hook_event_key_label` in codex-rs `hooks/src/lib.rs`).
pub(crate) fn hook_event_label(event: &str) -> Option<&'static str> {
    Some(match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        "Interrupt" => "interrupt",
        _ => return None,
    })
}

/// Pixel's hooks in one Codex `hooks.json`, and those Codex has not reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookReview {
    /// Pixel hook handlers in the file, as `Event #group.handler`.
    pub pixel: Vec<String>,
    /// Handlers without an enabled state and exact trusted identity, or whose
    /// configuration shape is not supported by the bounded verifier.
    pub unreviewed: Vec<String>,
}

/// Recompute the normalized identity Codex uses for its hook trust decision.
/// Pixel's generated command shape is intentionally the supported boundary;
/// unknown fields fail closed so an altered hook is never treated as approved.
pub(crate) fn codex_hook_hash(
    event: &str,
    group: &JsonValue,
    handler: &JsonValue,
) -> Option<String> {
    let label = hook_event_label(event)?;
    let command = handler.get("command")?.as_str()?;
    if handler.get("type")?.as_str()? != "command"
        || handler
            .as_object()?
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "command" | "timeout" | "async"))
    {
        return None;
    }
    let timeout = match handler.get("timeout") {
        None => None,
        Some(value) => Some(value.as_u64()?),
    };
    let timeout = match event {
        "SessionEnd" | "Interrupt" => timeout.unwrap_or(1).clamp(1, 3),
        _ => timeout.unwrap_or(600).max(1),
    };
    let is_async = match handler.get("async") {
        None => false,
        Some(value) => value.as_bool()?,
    };
    let matcher = match group.get("matcher") {
        None => None,
        Some(JsonValue::String(matcher)) => Some(matcher.clone()),
        Some(_) => return None,
    };
    if group
        .as_object()?
        .keys()
        .any(|key| !matches!(key.as_str(), "matcher" | "hooks"))
    {
        return None;
    }

    // Codex serializes `NormalizedHookIdentity { event_name, group }` where
    // the group contains one normalized handler. `None` matcher/options are
    // omitted by the TOML serializer before it fingerprints the JSON value.
    let mut identity = json!({
        "event_name": label,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": timeout,
            "async": is_async,
        }],
    });
    if let Some(matcher) = matcher {
        identity["matcher"] = JsonValue::String(matcher);
    }
    identity.sort_all_objects();
    let serialized = serde_json::to_vec(&identity).ok()?;
    let digest = Sha256::digest(serialized);
    Some(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn hook_state_entry<'a>(
    doc: &'a DocumentMut,
    hooks_path: &Path,
    event: &str,
    group_index: usize,
    handler_index: usize,
) -> Option<&'a Item> {
    let label = hook_event_label(event)?;
    let suffix = format!(":{label}:{group_index}:{handler_index}");
    // Codex records the key under the path it resolved (a symlinked
    // `CODEX_HOME`, `/var` -> `/private/var`), so compare canonical paths.
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let file = canonical(hooks_path);
    doc.get("hooks")?
        .as_table_like()?
        .get(HOOK_STATE_TABLE)?
        .as_table_like()?
        .iter()
        .find_map(|(key, entry)| {
            let source = Path::new(key.strip_suffix(&suffix)?);
            (source == hooks_path || canonical(source) == file).then_some(entry)
        })
}

fn codex_hook_is_enabled_and_trusted(
    doc: &DocumentMut,
    hooks_path: &Path,
    event: &str,
    group_index: usize,
    handler_index: usize,
    group: &JsonValue,
    handler: &JsonValue,
) -> bool {
    let Some(current_hash) = codex_hook_hash(event, group, handler) else {
        return false;
    };
    let Some(state) = hook_state_entry(doc, hooks_path, event, group_index, handler_index) else {
        return false;
    };
    let Some(state) = state.as_table_like() else {
        return false;
    };
    let enabled = match state.get("enabled") {
        None => Some(true),
        Some(value) => value.as_bool(),
    };
    enabled == Some(true)
        && state
            .get(TRUSTED_HASH_KEY)
            .and_then(Item::as_str)
            .is_some_and(|trusted_hash| trusted_hash == current_hash)
}

fn matching_task_hook_is_approved(
    groups: &[JsonValue],
    doc: &DocumentMut,
    hooks_path: &Path,
    event: &str,
    task_verb: &str,
    exe: &Path,
) -> bool {
    groups.iter().enumerate().any(|(group_index, group)| {
        if group.get("matcher").is_some() {
            return false;
        }
        let Some(handlers) = group.get("hooks").and_then(JsonValue::as_array) else {
            return false;
        };
        if handlers.len() != 1 {
            return false;
        }
        let handler = &handlers[0];
        let is_expected = handler
            .get("command")
            .and_then(JsonValue::as_str)
            .and_then(|command| crate::routing::pixel_hook_verb(command, exe))
            .is_some_and(|verb| verb == task_verb);
        let is_synchronous = handler
            .get("async")
            .is_none_or(|value| value.as_bool() == Some(false));
        let expected_timeout = if matches!(event, "SessionEnd" | "Interrupt") {
            3
        } else {
            10
        };
        is_expected
            && is_synchronous
            && handler.get("timeout").and_then(JsonValue::as_u64) == Some(expected_timeout)
            && codex_hook_is_enabled_and_trusted(
                doc,
                hooks_path,
                event,
                group_index,
                0,
                group,
                handler,
            )
    })
}

/// Whether every generated global Codex task hook is enabled and still has
/// the exact identity the user reviewed. Never writes Codex trust state.
pub(crate) fn task_hook_suite_is_enabled_and_trusted(
    codex_home: &Path,
    hooks_path: &Path,
    hooks: &JsonValue,
    exe: &Path,
) -> bool {
    let Ok(doc) = read_document(&codex_home.join(CODEX_CONFIG_FILE)) else {
        return false;
    };
    if let Some(profiles) = doc.get("profiles") {
        let Some(profiles) = profiles.as_table_like() else {
            return false;
        };
        if profiles.iter().next().is_some() {
            return false;
        }
    }
    // `[features] hooks = false` (or the older `codex_hooks` spelling) turns
    // every hook off; an unreadable value does too.
    if let Some(features) = doc.get("features") {
        let Some(features) = features.as_table_like() else {
            return false;
        };
        if let Some(enabled) = features
            .get("hooks")
            .or_else(|| features.get("codex_hooks"))
            && enabled.as_bool() != Some(true)
        {
            return false;
        }
    }
    crate::routing::TASK_HOOK_EVENTS
        .iter()
        .copied()
        .chain(std::iter::once(("Interrupt", "interrupt")))
        .all(|(event, name)| {
            hooks
                .get("hooks")
                .and_then(|events| events.get(event))
                .and_then(JsonValue::as_array)
                .is_some_and(|groups| {
                    matching_task_hook_is_approved(
                        groups,
                        &doc,
                        hooks_path,
                        event,
                        &format!("task-event --provider codex --event {name}"),
                        exe,
                    )
                })
        })
}

/// Which Pixel hooks in `hooks_path` have enabled, verifiable current approval.
///
/// Codex runs an unmanaged hook only when it is enabled and the stored hash
/// exactly matches its normalized event/group/handler identity. The key is
/// scoped to the exact source path and event/group/handler indexes.
///
/// # Errors
///
/// Either file cannot be read or parsed.
pub(crate) fn pixel_hook_review(
    codex_home: &Path,
    hooks_path: &Path,
    exe: &Path,
) -> std::result::Result<HookReview, String> {
    let hooks = read_hooks(hooks_path)?;
    let doc = read_document(&codex_home.join(CODEX_CONFIG_FILE))?;
    let mut review = HookReview {
        pixel: Vec::new(),
        unreviewed: Vec::new(),
    };
    let events = hooks.get("hooks").and_then(serde_json::Value::as_object);
    for (event, groups) in events.into_iter().flatten() {
        let Some(_label) = hook_event_label(event) else {
            continue;
        };
        let groups = groups.as_array().map_or(&[][..], Vec::as_slice);
        for (group_index, group) in groups.iter().enumerate() {
            let handlers = group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for (handler_index, handler) in handlers.iter().enumerate() {
                // The metrics relay is recognised by its marker, as
                // `check_metrics_hook` and uninstall do; the other entries
                // by the verbs `routing::is_pixel_hook` owns.
                let is_pixel = handler
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|command| {
                        command.contains(METRICS_HOOK_MARKER)
                            || crate::routing::is_pixel_hook(command, exe)
                    });
                if !is_pixel {
                    continue;
                }
                let name = format!("{event} #{group_index}.{handler_index}");
                if !codex_hook_is_enabled_and_trusted(
                    &doc,
                    hooks_path,
                    event,
                    group_index,
                    handler_index,
                    group,
                    handler,
                ) {
                    review.unreviewed.push(name.clone());
                }
                review.pixel.push(name);
            }
        }
    }
    Ok(review)
}

/// A `pixel doctor` outcome for Pixel's hooks in one Codex `hooks.json`:
/// yellow while Codex has hooks of Pixel's it will skip, with the one step
/// that clears it, which only the user can take.
pub(crate) fn hook_review_outcome(
    review: &HookReview,
    hooks_path: &Path,
) -> (crate::doctor::CheckStatus, String) {
    let file = hooks_path.display();
    if review.pixel.is_empty() {
        return (
            crate::doctor::CheckStatus::Green,
            format!("no Pixel hook for Codex in {file}"),
        );
    }
    if review.unreviewed.is_empty() {
        return (
            crate::doctor::CheckStatus::Green,
            format!(
                "Codex has reviewed the {} Pixel hook(s) in {file}",
                review.pixel.len()
            ),
        );
    }
    (
        crate::doctor::CheckStatus::Yellow,
        format!(
            "approval for {} of the {} Pixel hook(s) in {file} is missing, stale, disabled, or not verifiable ({}): \
             start `codex` in this directory and inspect `/hooks`",
            review.unreviewed.len(),
            review.pixel.len(),
            review.unreviewed.join(", ")
        ),
    )
}

pub(crate) fn check_developer_instructions(
    codex_home: &Path,
) -> std::result::Result<(String, serde_json::Value), String> {
    let path = codex_home.join(CODEX_CONFIG_FILE);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "key": DEVELOPER_INSTRUCTIONS_KEY,
    });
    if !path.is_file() {
        return Ok((
            "no Pixel developer-instructions block installed".into(),
            detail,
        ));
    }
    let doc = read_document(&path)?;
    let Some(current) = current_value(&doc)? else {
        return Ok((
            "no Pixel developer-instructions block installed".into(),
            detail,
        ));
    };
    if current.contains(MANAGED_BEGIN) || current.contains(MANAGED_END) {
        return Err(format!(
            "retired Pixel block remains in {DEVELOPER_INSTRUCTIONS_KEY} in {} — run `pixel install` to remove it",
            path.display()
        ));
    }
    Ok((
        "no Pixel developer-instructions block installed".into(),
        detail,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn hook_state_entry_should_match_a_key_recorded_under_the_canonical_path() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-codex");
        std::fs::create_dir_all(&real).unwrap();
        let linked = temp.path().join("linked-codex");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        std::fs::write(real.join("hooks.json"), "{}").unwrap();
        let recorded = real.canonicalize().unwrap().join("hooks.json");
        let doc: DocumentMut = format!(
            "[hooks.state.\"{}:stop:1:0\"]\ntrusted_hash = \"sha256:x\"\n",
            recorded.display()
        )
        .parse()
        .unwrap();

        let through_link = linked.join("hooks.json");
        assert!(super::hook_state_entry(&doc, &through_link, "Stop", 1, 0).is_some());
        assert!(super::hook_state_entry(&doc, &recorded, "Stop", 1, 0).is_some());
        assert!(super::hook_state_entry(&doc, &through_link, "Stop", 0, 0).is_none());
        assert!(super::hook_state_entry(&doc, &through_link, "SessionEnd", 1, 0).is_none());
        let other = temp.path().join("other/hooks.json");
        assert!(super::hook_state_entry(&doc, &other, "Stop", 1, 0).is_none());
    }

    /// A `hooks.state` key spells the event the way Codex does
    /// (`hook_event_key_label`); a wrong or missing label reads every
    /// review of that event as absent, or hides the event's Pixel hooks.
    #[test]
    fn hook_event_label_should_spell_every_event_as_codex_keys_it() {
        let table = [
            ("PreToolUse", "pre_tool_use"),
            ("PermissionRequest", "permission_request"),
            ("PostToolUse", "post_tool_use"),
            ("PreCompact", "pre_compact"),
            ("PostCompact", "post_compact"),
            ("SessionStart", "session_start"),
            ("SessionEnd", "session_end"),
            ("UserPromptSubmit", "user_prompt_submit"),
            ("SubagentStart", "subagent_start"),
            ("SubagentStop", "subagent_stop"),
            ("Stop", "stop"),
            ("Interrupt", "interrupt"),
        ];
        for (event, label) in table {
            assert_eq!(hook_event_label(event), Some(label), "{event}");
        }
        assert_eq!(hook_event_label("NotAnEvent"), None);
    }

    #[test]
    fn codex_hook_hash_matches_the_deployed_codex_0160_identity_vectors() {
        let vectors = [
            (
                "Interrupt",
                "interrupt",
                3,
                "sha256:e1b9f0136319de932653a58aba7daadc81ac6ba11087b56e9642264967c6c5bd",
            ),
            (
                "PostToolUse",
                "post-tool-use",
                10,
                "sha256:e823b27c09032bbf495660b2b2768810f9955a2d8c8821ad0ec3b59d483315c5",
            ),
            (
                "PreToolUse",
                "pre-tool-use",
                10,
                "sha256:40528cc1a93d61a88f1ef8def24aeda9a35df3f769ab7ffabc1e23cb15ca68cb",
            ),
            (
                "SessionEnd",
                "session-end",
                3,
                "sha256:592671cbeab88cb52beb144a4a8430e1aa45930d39f97dbefda4efbeb66a3737",
            ),
            (
                "SessionStart",
                "session-start",
                10,
                "sha256:297139f8a7d4ad7c8c5e44305725dab6d58754e3c25fe3d9d338d44b5eef8206",
            ),
            (
                "Stop",
                "stop",
                10,
                "sha256:fed5e0c7cf5936eeac3f2cb180b47e9492031238ee325bab135a25f7e891a864",
            ),
            (
                "SubagentStart",
                "subagent-start",
                10,
                "sha256:226885cf987ba4c08cd94d411e055c9d9f23984042099f943559af79daf350b8",
            ),
            (
                "SubagentStop",
                "subagent-stop",
                10,
                "sha256:9bb4267598af13529d2948086891f50b76bee7728e618358a3aa08d7bdf6da2c",
            ),
            (
                "UserPromptSubmit",
                "prompt-submit",
                10,
                "sha256:72f75c5ee09194fa84fdab917b746b99b0941db9241d660bee579d6048676037",
            ),
        ];
        for (event, verb, timeout, expected) in vectors {
            let group = serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": format!("/usr/local/bin/pixel run-hook task-event --provider codex --event {verb}"),
                    "timeout": timeout,
                }]
            });
            assert_eq!(
                codex_hook_hash(event, &group, &group["hooks"][0]).as_deref(),
                Some(expected),
                "Codex currentHash for {event}"
            );
        }

        let command = "/usr/local/bin/pixel run-hook task-event --provider codex --event stop";
        let absent_timeout = serde_json::json!({"hooks":[{"type":"command","command":command}]});
        let default_timeout =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":600}]});
        assert_eq!(
            codex_hook_hash("Stop", &absent_timeout, &absent_timeout["hooks"][0]),
            codex_hook_hash("Stop", &default_timeout, &default_timeout["hooks"][0]),
            "Codex normalizes an absent regular-event timeout to 600 seconds"
        );
        let short_timeout =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":1}]});
        let zero_timeout =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":0}]});
        assert_eq!(
            codex_hook_hash("Stop", &short_timeout, &short_timeout["hooks"][0]),
            codex_hook_hash("Stop", &zero_timeout, &zero_timeout["hooks"][0]),
            "Codex clamps a zero timeout to one second"
        );
        let session_end_three =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":3}]});
        let session_end_overflow =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":99}]});
        assert_eq!(
            codex_hook_hash(
                "SessionEnd",
                &session_end_three,
                &session_end_three["hooks"][0]
            ),
            codex_hook_hash(
                "SessionEnd",
                &session_end_overflow,
                &session_end_overflow["hooks"][0]
            ),
            "Codex clamps SessionEnd timeouts to three seconds"
        );
        let with_matcher = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":command,"timeout":10}]});
        let without_matcher =
            serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":10}]});
        assert_ne!(
            codex_hook_hash("PreToolUse", &with_matcher, &with_matcher["hooks"][0]),
            codex_hook_hash("PreToolUse", &without_matcher, &without_matcher["hooks"][0]),
            "Codex fingerprints the matcher"
        );
        let unknown_field = serde_json::json!({"hooks":[{"type":"command","command":command,"timeout":10,"custom":true}]});
        assert!(codex_hook_hash("Stop", &unknown_field, &unknown_field["hooks"][0]).is_none());
        let unknown_group_field = serde_json::json!({"custom":true,"hooks":[{"type":"command","command":command,"timeout":10}]});
        assert!(
            codex_hook_hash(
                "Stop",
                &unknown_group_field,
                &unknown_group_field["hooks"][0]
            )
            .is_none()
        );
    }

    #[test]
    fn task_hook_approval_requires_the_same_synchronous_reviewed_handler() {
        let home = tempfile::tempdir().unwrap();
        let codex_home = home.path().join(".codex");
        fs::create_dir_all(&codex_home).unwrap();
        let hooks_path = codex_home.join(HOOKS_FILE);
        let config_path = codex_home.join(CODEX_CONFIG_FILE);
        let exe = Path::new("/usr/local/bin/pixel");
        let mut hooks = serde_json::json!({"hooks": {}});
        let mut trust = String::new();

        for (event, name) in crate::routing::TASK_HOOK_EVENTS
            .iter()
            .copied()
            .chain(std::iter::once(("Interrupt", "interrupt")))
        {
            let mut handler = serde_json::json!({
                "type": "command",
                "command": format!(
                    "/usr/local/bin/pixel run-hook task-event --provider codex --event {name}"
                ),
                "timeout": if matches!(event, "SessionEnd" | "Interrupt") {
                    3
                } else {
                    10
                },
            });
            if event == "SessionStart" {
                handler["async"] = serde_json::json!(false);
            }
            let group = serde_json::json!({"hooks": [handler]});
            hooks["hooks"][event] = serde_json::json!([group]);
            let hash = codex_hook_hash(event, &group, &group["hooks"][0]).unwrap();
            let state_key = format!(
                "{}:{}:0:0",
                hooks_path.display(),
                hook_event_label(event).unwrap()
            );
            trust.push_str(&format!(
                "[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = {hash:?}\n\n"
            ));
        }
        fs::write(&hooks_path, serde_json::to_vec(&hooks).unwrap()).unwrap();
        fs::write(&config_path, &trust).unwrap();

        assert!(
            task_hook_suite_is_enabled_and_trusted(&codex_home, &hooks_path, &hooks, exe,),
            "an explicitly synchronous, exactly reviewed handler must be eligible"
        );

        // A trusted asynchronous callback cannot authorize a separate
        // synchronous callback whose current hash was never reviewed.
        let mut trusted_async = hooks["hooks"]["SessionStart"][0].clone();
        trusted_async["hooks"][0]["async"] = serde_json::json!(true);
        let mut mixed_trust = String::new();
        for (event, _) in crate::routing::TASK_HOOK_EVENTS
            .iter()
            .copied()
            .chain(std::iter::once(("Interrupt", "interrupt")))
        {
            let group = if event == "SessionStart" {
                &trusted_async
            } else {
                &hooks["hooks"][event][0]
            };
            let hash = codex_hook_hash(event, group, &group["hooks"][0]).unwrap();
            let state_key = format!(
                "{}:{}:0:0",
                hooks_path.display(),
                hook_event_label(event).unwrap()
            );
            mixed_trust.push_str(&format!(
                "[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = {hash:?}\n\n"
            ));
        }
        let untrusted_sync = hooks["hooks"]["SessionStart"][0].clone();
        hooks["hooks"]["SessionStart"] = serde_json::json!([trusted_async, untrusted_sync]);
        fs::write(&hooks_path, serde_json::to_vec(&hooks).unwrap()).unwrap();
        fs::write(&config_path, mixed_trust).unwrap();

        assert!(
            !task_hook_suite_is_enabled_and_trusted(&codex_home, &hooks_path, &hooks, exe,),
            "approval for an asynchronous group must not combine with an unreviewed synchronous group"
        );
    }

    #[test]
    fn hook_approval_requires_exact_source_enabled_state_and_current_hash() {
        let home = tempfile::tempdir().unwrap();
        let hooks_path = home.path().join("hooks.json");
        let group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": "/usr/local/bin/pixel run-hook task-event --provider codex --event session-start",
                "timeout": 10,
            }]
        });
        let handler = &group["hooks"][0];
        let hash = codex_hook_hash("SessionStart", &group, handler).unwrap();
        let state_key = format!("{}:session_start:0:0", hooks_path.display());
        let config_path = home.path().join(CODEX_CONFIG_FILE);
        fs::write(
            &config_path,
            format!("[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = {hash:?}\n"),
        )
        .unwrap();
        let doc = read_document(&config_path).unwrap();
        assert!(codex_hook_is_enabled_and_trusted(
            &doc,
            &hooks_path,
            "SessionStart",
            0,
            0,
            &group,
            handler,
        ));

        fs::write(
            &config_path,
            format!("[hooks.state.{state_key:?}]\nenabled = false\ntrusted_hash = {hash:?}\n"),
        )
        .unwrap();
        let doc = read_document(&config_path).unwrap();
        assert!(!codex_hook_is_enabled_and_trusted(
            &doc,
            &hooks_path,
            "SessionStart",
            0,
            0,
            &group,
            handler,
        ));

        fs::write(
            &config_path,
            format!(
                "[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = \"sha256:stale\"\n"
            ),
        )
        .unwrap();
        let doc = read_document(&config_path).unwrap();
        assert!(!codex_hook_is_enabled_and_trusted(
            &doc,
            &hooks_path,
            "SessionStart",
            0,
            0,
            &group,
            handler,
        ));

        let aliased_key = format!(
            "{}:session_start:0:0",
            home.path().join("alias/hooks.json").display()
        );
        fs::write(
            &config_path,
            format!("[hooks.state.{aliased_key:?}]\nenabled = true\ntrusted_hash = {hash:?}\n"),
        )
        .unwrap();
        let doc = read_document(&config_path).unwrap();
        assert!(!codex_hook_is_enabled_and_trusted(
            &doc,
            &hooks_path,
            "SessionStart",
            0,
            0,
            &group,
            handler,
        ));
    }

    #[test]
    fn pixel_hook_review_accepts_only_the_current_enabled_identity() {
        let home = tempfile::tempdir().unwrap();
        let hooks_path = home.path().join("hooks.json");
        let exe = Path::new("/tmp/pixel");
        let group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": "'/tmp/pixel' run-hook task-event --provider codex --event session-start",
                "timeout": 10,
            }]
        });
        let hooks = serde_json::json!({"hooks":{"SessionStart":[group.clone()]}});
        fs::write(&hooks_path, serde_json::to_vec(&hooks).unwrap()).unwrap();
        let hash = codex_hook_hash("SessionStart", &group, &group["hooks"][0]).unwrap();
        let state_key = format!("{}:session_start:0:0", hooks_path.display());
        let config_path = home.path().join(CODEX_CONFIG_FILE);
        fs::write(
            &config_path,
            format!("[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = {hash:?}\n"),
        )
        .unwrap();
        let reviewed = pixel_hook_review(home.path(), &hooks_path, exe).unwrap();
        assert_eq!(reviewed.pixel, ["SessionStart #0.0"]);
        assert!(reviewed.unreviewed.is_empty());

        fs::write(
            &config_path,
            format!(
                "[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = \"sha256:stale\"\n"
            ),
        )
        .unwrap();
        let stale = pixel_hook_review(home.path(), &hooks_path, exe).unwrap();
        assert_eq!(stale.unreviewed, ["SessionStart #0.0"]);
    }

    #[test]
    fn the_asset_survives_a_toml_literal_string() {
        // A literal multi-line string cannot contain its own delimiter and
        // TOML forbids control characters other than tab and newline; either
        // would silently switch the file to the escaped representation or
        // fail to load in Codex.
        let block = managed_block();
        assert!(
            !block.contains("'''"),
            "the managed block must not contain '''"
        );
        assert!(
            !block
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "the managed block must not contain control characters"
        );
        let doc: DocumentMut = format!("{DEVELOPER_INSTRUCTIONS_KEY} = {}\n", string_value(&block))
            .parse()
            .expect("the literal representation parses");
        assert_eq!(
            current_value(&doc).unwrap().as_deref(),
            Some(block.as_str())
        );
    }

    #[test]
    fn merge_keeps_user_text_on_both_sides_and_replaces_only_the_block() {
        let stale = format!("Before.\n\n{MANAGED_BEGIN}\nold prompt\n{MANAGED_END}\nAfter.\n");
        let merged = merged_value(Some(&stale));
        assert_eq!(merged, format!("Before.\n\n{}After.\n", managed_block()));
        assert_eq!(merged_value(Some(&merged)), merged, "idempotent");
        assert_eq!(
            merged_value(Some("Mine.\n")),
            format!("Mine.\n\n{}", managed_block()),
            "a foreign value is kept and the block appended"
        );
        assert_eq!(merged_value(Some("  \n")), managed_block());
        assert_eq!(
            value_without_block(&merged).as_deref(),
            Some("Before.\n\nAfter.\n")
        );
        assert_eq!(value_without_block(&managed_block()), None);
    }

    #[test]
    fn managed_marker_range_rejects_duplicate_stray_and_reordered_markers() {
        let malformed = [
            format!("{MANAGED_BEGIN}\ntext\n"),
            format!("text\n{MANAGED_END}\n"),
            format!("{MANAGED_END}\n{MANAGED_BEGIN}\n"),
            format!("{MANAGED_BEGIN}\r\ntext\n{MANAGED_END}\n"),
            format!("{MANAGED_BEGIN}\none\n{MANAGED_BEGIN}\ntwo\n{MANAGED_END}"),
            format!("{MANAGED_BEGIN}\none\n{MANAGED_END}\n{MANAGED_END}"),
        ];
        for value in malformed {
            assert!(
                managed_range(&value).is_none(),
                "malformed markers must not produce a removable range: {value:?}"
            );
            assert!(
                value_without_block(&value).is_none(),
                "malformed marker content must remain untouched: {value:?}"
            );
        }
    }

    #[test]
    fn removal_refuses_malformed_marker_sets_without_rewriting_codex_config() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(CODEX_CONFIG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for value in [
            format!("{MANAGED_BEGIN}\npartial"),
            format!("{MANAGED_END}\n"),
            format!("{MANAGED_END}\n{MANAGED_BEGIN}\n"),
            format!("{MANAGED_BEGIN}\r\npartial\n{MANAGED_END}\n"),
            format!("{MANAGED_BEGIN}\nfirst\n{MANAGED_BEGIN}\nsecond\n{MANAGED_END}"),
            format!("{MANAGED_BEGIN}\n{MANAGED_END}\n{MANAGED_END}"),
        ] {
            let original = format!("developer_instructions = {}\n", string_value(&value));
            fs::write(&path, &original).unwrap();

            let step = remove_developer_instructions(home.path(), false).unwrap();

            assert_eq!(step.status, CheckStatus::Red, "{value:?}");
            assert!(step.summary.contains("not touched"), "{step:?}");
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    // ---- native-default Codex task hooks --------------------------------

    fn scratch_codex_home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-codex-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hook_events(home: &Path) -> serde_json::Value {
        let text = fs::read_to_string(home.join(HOOKS_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["hooks"].clone()
    }

    #[test]
    fn task_hooks_should_remove_pixel_retrieval_keep_foreign_hooks_and_install_lifecycle() {
        let home = scratch_codex_home("install");
        let exe = Path::new("/opt/pixel tools/pixel");
        // Both foreign hooks and Pixel's retired automatic hooks have to be
        // distinguished: task lifecycle remains, retrieval and metrics leave.
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "PostToolUse": [
                        {"hooks": [{"type": "command", "command": "cmux-feed"}]},
                        {
                            "matcher": "Bash",
                            "timeout": 9,
                            "hooks": [
                                {"type": "command", "command": "metrics-proxy --label 'run-hook metrics --provider codex'"},
                                {"type": "command", "command": "pixel run-hook metrics --provider codex"}
                            ]
                        },
                        {"hooks": [{"type": "command", "command": "pixel run-hook metrics --provider codex"}]}
                    ],
                    "UserPromptSubmit": [
                        {"hooks": [{"type": "command", "command": "prompt-audit --label 'run-hook prompt-submit --provider codex'"}]},
                        {"hooks": [{"type": "command", "command": "pixel run-hook prompt-submit --provider codex"}]}
                    ]
                }}
            ))
            .unwrap(),
        )
        .unwrap();

        install_task_hooks(&home, exe, false).unwrap();
        let hooks = hook_events(&home);
        let post_tool_use = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post_tool_use.len(),
            3,
            "foreign groups and lifecycle survive"
        );
        assert!(
            post_tool_use
                .iter()
                .any(|entry| { entry["hooks"][0]["command"] == "cmux-feed" })
        );
        let mixed = post_tool_use
            .iter()
            .find(|entry| entry.get("matcher").is_some())
            .expect("foreign hook's group remains");
        assert_eq!(mixed["matcher"], "Bash");
        assert_eq!(mixed["timeout"], 9);
        assert_eq!(
            mixed["hooks"],
            serde_json::json!([{
                "type": "command",
                "command": "metrics-proxy --label 'run-hook metrics --provider codex'"
            }]),
            "foreign marker mention survives beside removed Pixel callback"
        );
        assert!(post_tool_use.iter().any(|entry| {
            entry["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains("task-event --provider codex"))
        }));
        assert!(
            hooks["UserPromptSubmit"].is_array(),
            "task-event prompt stays"
        );
        let prompt_hooks = hooks["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(
            prompt_hooks.len(),
            2,
            "foreign prompt hook and lifecycle survive"
        );
        assert_eq!(
            prompt_hooks[0]["hooks"][0]["command"],
            "prompt-audit --label 'run-hook prompt-submit --provider codex'"
        );
        assert!(
            check_task_hooks(&home, exe).is_ok(),
            "doctor check sees task hooks"
        );

        // Second install verifies instead of duplicating.
        let step = install_task_hooks(&home, exe, false).unwrap();
        assert!(step.summary.contains("verified"), "{}", step.summary);
        assert_eq!(
            hook_events(&home)["PostToolUse"].as_array().unwrap().len(),
            3,
            "reinstall preserves both foreign groups without duplication"
        );
        assert!(check_task_hooks(&home, exe).is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_check_rejects_retired_pixel_hook_after_non_retired_hooks() {
        let home = scratch_codex_home("check-retired-order");
        let exe = Path::new("/opt/pixel");
        install_task_hooks(&home, exe, false).unwrap();

        let path = home.join(HOOKS_FILE);
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [{"type": "command", "command": "foreign-session-hook"}]
            }));
        value["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": "/opt/pixel run-hook metrics --provider codex"
                }]
            }));
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error = check_task_hooks(&home, exe).unwrap_err();
        assert!(
            error.contains("retired automatic Pixel Codex hook remains"),
            "the later retired hook must be reported even after a non-retired hook: {error}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn carries_pixel_block_detects_either_orphaned_marker() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(CODEX_CONFIG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for marker in [MANAGED_BEGIN, MANAGED_END] {
            fs::write(
                &path,
                format!("{DEVELOPER_INSTRUCTIONS_KEY} = {}\n", string_value(marker)),
            )
            .unwrap();

            assert!(
                carries_pixel_block(home.path()).unwrap(),
                "an orphaned marker must remain detectable: {marker}"
            );
        }
    }

    #[test]
    fn task_hook_reinstall_refreshes_the_binary_path_without_duplicating_events() {
        let home = scratch_codex_home("refresh");
        install_task_hooks(&home, Path::new("/old/pixel"), false).unwrap();
        install_task_hooks(&home, Path::new("/new/pixel"), false).unwrap();
        let hooks = hook_events(&home);
        let commands = hooks.to_string();
        assert!(commands.contains("/new/pixel"), "{commands}");
        assert!(!commands.contains("/old/pixel"), "{commands}");
        for groups in hooks.as_object().unwrap().values() {
            let count = groups
                .as_array()
                .unwrap()
                .iter()
                .filter(|group| {
                    group["hooks"].as_array().unwrap().iter().any(|hook| {
                        hook["command"]
                            .as_str()
                            .is_some_and(|command| command.contains("run-hook task-event"))
                    })
                })
                .count();
            assert_eq!(count, 1, "no duplicate task-event groups in {groups}");
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_install_should_remove_renamed_pixel_callbacks_only() {
        let home = scratch_codex_home("renamed");
        let exe = Path::new("/opt/pixel custom/pixel-next");
        install_task_hooks(&home, exe, false).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(HOOKS_FILE)).unwrap()).unwrap();
        value["hooks"]["PostToolUse"] = serde_json::json!([
            {"hooks": [{"type": "command", "command": "pixel-next run-hook metrics --provider codex"}]},
            {"hooks": [{"type": "command", "command": "pixel run-hook task-event --provider codex --event post-tool-use"}]}
        ]);
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();

        install_task_hooks(&home, exe, false).unwrap();
        let hooks = hook_events(&home);
        let post = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1, "renamed Pixel callback is removed");
        assert_eq!(
            post[0]["hooks"][0]["command"],
            "'/opt/pixel custom/pixel-next' run-hook task-event --provider codex --event post-tool-use",
            "task lifecycle command is preserved"
        );
        assert!(check_task_hooks(&home, exe).is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_install_refuses_unparseable_hooks_json() {
        let home = scratch_codex_home("broken");
        fs::write(home.join(HOOKS_FILE), "not json").unwrap();
        let step = install_task_hooks(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).unwrap(),
            "not json",
            "an unparseable file is never rewritten"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": [1]}").unwrap();
        let step = install_task_hooks(&home, Path::new("/x"), false).unwrap();
        assert_eq!(
            step.status,
            CheckStatus::Red,
            "a non-object hooks key is refused, not silently replaced"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_install_reports_an_unreadable_hooks_json() {
        let home = scratch_codex_home("unreadable");
        // A directory where the file is expected fails the read for every
        // user including root — and is not "absent": it must come back Red,
        // never Ok-treated-as-empty.
        fs::create_dir(home.join(HOOKS_FILE)).unwrap();
        let step = install_task_hooks(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        assert!(step.summary.contains("not touched"), "{}", step.summary);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_check_requires_registration_and_rejects_retired_hooks() {
        let home = scratch_codex_home("check");
        assert!(
            check_task_hooks(&home, Path::new("pixel")).is_err(),
            "absent file has no task lifecycle"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": {}}").unwrap();
        assert!(
            check_task_hooks(&home, Path::new("pixel")).is_err(),
            "empty hooks object has no task lifecycle"
        );
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"SessionStart": [
                    {"hooks": [{"type": "command", "command": "pixel run-hook task-event --provider codex --event session-start"}]}
                ]}}
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(
            check_task_hooks(&home, Path::new("pixel")).is_err(),
            "a partial task lifecycle is not registered"
        );
        install_task_hooks(&home, Path::new("pixel"), false).unwrap();
        assert!(check_task_hooks(&home, Path::new("pixel")).is_ok());
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(HOOKS_FILE)).unwrap()).unwrap();
        value["hooks"]["PostToolUse"] = serde_json::json!([
            {"hooks":[{"type":"command","command":"pixel run-hook metrics --provider codex"}]}
        ]);
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        assert!(
            check_task_hooks(&home, Path::new("pixel")).is_err(),
            "legacy automatic metric entry is rejected"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn task_hook_install_keeps_the_task_prompt_event_but_removes_pixel_guidance() {
        let home = scratch_codex_home("half-registered");
        install_task_hooks(&home, Path::new("pixel"), false).unwrap();
        assert!(check_task_hooks(&home, Path::new("pixel")).is_ok());
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(HOOKS_FILE)).unwrap()).unwrap();
        value["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| !entry["hooks"].to_string().contains("run-hook task-event"));
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        assert!(
            check_task_hooks(&home, Path::new("pixel")).is_err(),
            "removing the task prompt event breaks the registered lifecycle"
        );
        let _ = fs::remove_dir_all(&home);
    }

    // ---- project trust ---------------------------------------------------

    fn write_config(home: &Path, body: &str) {
        fs::write(home.join(CODEX_CONFIG_FILE), body).unwrap();
    }

    #[test]
    fn project_trust_reads_every_state_and_degrades_on_a_bad_value() {
        let home = scratch_codex_home("trust");
        let project = Path::new("/work/repo");

        // No config.toml at all: Codex has no entry for the project.
        assert_eq!(
            project_trust(&home, project).unwrap(),
            ProjectTrust::Unspecified
        );

        // Another project's entry does not answer for this one.
        write_config(
            &home,
            "[projects.\"/work/other\"]\ntrust_level = \"trusted\"\n",
        );
        assert_eq!(
            project_trust(&home, project).unwrap(),
            ProjectTrust::Unspecified,
            "a key that is not this path says nothing about this project"
        );

        write_config(
            &home,
            "[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n",
        );
        assert_eq!(
            project_trust(&home, project).unwrap(),
            ProjectTrust::Trusted
        );

        write_config(
            &home,
            "[projects.\"/work/repo\"]\ntrust_level = \"untrusted\"\n",
        );
        assert_eq!(
            project_trust(&home, project).unwrap(),
            ProjectTrust::Untrusted,
            "an explicit refusal is its own state, not unspecified"
        );

        // An entry that never sets the key leaves the project unspecified.
        write_config(&home, "[projects.\"/work/repo\"]\n");
        assert_eq!(
            project_trust(&home, project).unwrap(),
            ProjectTrust::Unspecified
        );

        // A wrong-typed value is an error, as for any value in this file.
        write_config(&home, "[projects.\"/work/repo\"]\ntrust_level = true\n");
        let err = project_trust(&home, project).unwrap_err();
        assert!(err.contains("not a string"), "{err}");

        // A file that does not parse is an error too, never a panic.
        write_config(&home, "this is not toml = = =\n");
        assert!(project_trust(&home, project).is_err());

        let _ = fs::remove_dir_all(&home);
    }
}
