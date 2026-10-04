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
//! Codex 0.154 has no file-backed variant of the key, so the prompt is
//! embedded in the file as a TOML literal multi-line string. The value is
//! managed the way the Markdown agent configs were: the Pixel prompt sits
//! between [`config::MANAGED_BEGIN`] and [`config::MANAGED_END`] marker
//! lines, and text the user keeps outside the markers survives every
//! `pixel install`. The rest of the file is rewritten by `toml_edit` with its
//! formatting and comments preserved, because the desktop app writes to the
//! same file.

use std::fs;
use std::path::{Path, PathBuf};

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

/// Substring unique to the installed metrics-relay command, used for
/// idempotent merge and uninstall removal.
pub const METRICS_HOOK_MARKER: &str = "run-hook metrics";
pub const PROMPT_SUBMIT_HOOK_MARKER: &str = "run-hook prompt-submit --provider codex";

/// The agent prompt as bundled in the binary.
pub(crate) const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

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

/// The managed block exactly as `pixel install` embeds it: markers on their
/// own lines around the bundled prompt.
pub(crate) fn managed_block() -> String {
    format!("{MANAGED_BEGIN}\n{AGENT_PROMPT_ASSET}{MANAGED_END}\n")
}

/// Byte range of the managed block inside a `developer_instructions` value,
/// from the begin marker to the end of the line holding the end marker.
fn managed_range(value: &str) -> Option<std::ops::Range<usize>> {
    let start = value.find(MANAGED_BEGIN)?;
    let end_marker = start + value[start..].find(MANAGED_END)?;
    let mut end = end_marker + MANAGED_END.len();
    if value[end..].starts_with('\n') {
        end += 1;
    }
    Some(start..end)
}

/// The value `pixel install` writes for a current value of `existing`:
/// the block replaces a previous one in place, or is appended after the
/// user's own text, separated by a blank line.
pub(crate) fn merged_value(existing: Option<&str>) -> String {
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

/// `pixel install` step: put the managed block into `developer_instructions`.
pub(crate) fn install_developer_instructions(
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
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        // A file Codex itself could not load is not ours to repair; a
        // rewrite from a failed parse would drop whatever it holds.
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let existing = match current_value(&doc) {
        Ok(existing) => existing,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let wanted = merged_value(existing.as_deref());
    if existing.as_deref() == Some(wanted.as_str()) {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "verified {DEVELOPER_INSTRUCTIONS_KEY} in {}",
                path.display()
            ),
        ));
    }
    let kept_user_text = wanted != managed_block();
    let verb = match &existing {
        None => "installed",
        Some(current) if managed_range(current).is_some() => "updated",
        Some(_) => "appended",
    };
    let summary = format!(
        "{} {DEVELOPER_INSTRUCTIONS_KEY} in {}{}",
        verb,
        path.display(),
        if kept_user_text {
            ", keeping the text outside the pixel markers"
        } else {
            ""
        }
    );
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    doc[DEVELOPER_INSTRUCTIONS_KEY] = Item::Value(string_value(&wanted));
    write_document(&path, &doc)?;
    Ok(step(CheckStatus::Green, summary))
}

/// The PostToolUse entry `pixel install` merges into `hooks.json`: Codex
/// runs it after every tool call; `pixel run-hook metrics` self-filters to
/// shell calls that invoked `pixel` and re-emits the finalized 🟩 line as
/// `additionalContext` — a fallback for the rare host whose tool result
/// drops the merged stderr Codex's exec layer normally carries.
fn metrics_hook_entry(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("{} run-hook metrics --provider codex", crate::routing::quoted_executable(exe)),
            "timeout": 10,
        }]
    })
}

fn prompt_submit_hook_entry(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("{} run-hook prompt-submit --provider codex", crate::routing::quoted_executable(exe)),
            "timeout": 10,
        }]
    })
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

/// `pixel install` step: register Codex's metrics relay and task-boundary
/// context hooks, idempotently alongside existing hook groups.
pub(crate) fn install_metrics_hook(
    codex_home: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = Some(format!(
        "path={} events=PostToolUse,UserPromptSubmit markers={METRICS_HOOK_MARKER},{PROMPT_SUBMIT_HOOK_MARKER}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-metrics-hook".into(),
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
    let merged_metrics = crate::config::merge_hook_entry(
        hooks.get("PostToolUse"),
        METRICS_HOOK_MARKER,
        metrics_hook_entry(exe),
    );
    let merged_prompt = crate::config::merge_hook_entry(
        hooks.get("UserPromptSubmit"),
        PROMPT_SUBMIT_HOOK_MARKER,
        prompt_submit_hook_entry(exe),
    );
    hooks.insert("PostToolUse".to_string(), merged_metrics);
    hooks.insert("UserPromptSubmit".to_string(), merged_prompt);
    if let Err(error) =
        crate::routing::merge_task_hooks(hooks, crate::routing::Provider::Codex, exe)
    {
        return Ok(step(CheckStatus::Red, format!("{error} — not touched")));
    }
    if *hooks == before {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "verified metrics, prompt-submit and task hooks in {}",
                path.display()
            ),
        ));
    }
    let summary = format!(
        "{} metrics, prompt-submit and task hooks in {}",
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

/// `pixel doctor` check: both global Codex hooks are registered.
pub(crate) fn check_metrics_hook(
    codex_home: &Path,
) -> std::result::Result<(String, serde_json::Value), String> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "events": ["PostToolUse", "UserPromptSubmit"],
        "markers": [METRICS_HOOK_MARKER, PROMPT_SUBMIT_HOOK_MARKER],
    });
    let value = read_hooks(&path)?;
    let has_marker = |event: &str, marker: &str| {
        value
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command")
                                    .and_then(serde_json::Value::as_str)
                                    .is_some_and(|command| command.contains(marker))
                            })
                        })
                })
            })
    };
    if !has_marker("PostToolUse", METRICS_HOOK_MARKER)
        || !has_marker("UserPromptSubmit", PROMPT_SUBMIT_HOOK_MARKER)
    {
        return Err(format!(
            "missing Pixel Codex hook in {} — run `pixel install`",
            path.display()
        ));
    }
    if !crate::routing::task_hooks_registered(
        &value,
        crate::routing::Provider::Codex,
        Path::new("pixel"),
    ) {
        return Err(format!(
            "task lifecycle hooks missing or asynchronous in {} — run `pixel install`",
            path.display()
        ));
    }
    Ok((
        format!(
            "metrics, prompt-submit and task hooks registered in {} (runtime activity checked separately)",
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
    let Some(current) = current_value(&doc).ok().flatten() else {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
        ));
    };
    if managed_range(&current).is_none() {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
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

/// `pixel doctor` check: the managed block is present and current.
/// Whether `codex_home`'s config.toml holds pixel's begin marker in
/// `developer_instructions`: the evidence that `pixel install` wrote there.
/// A missing file, a missing key, or a value without the marker is none, so
/// a project that keeps its own Codex config is not a broken install.
///
/// # Errors
///
/// The file cannot be read or parsed, or the key is not a string.
pub(crate) fn carries_pixel_block(codex_home: &Path) -> std::result::Result<bool, String> {
    let doc = read_document(&codex_home.join(CODEX_CONFIG_FILE))?;
    Ok(current_value(&doc)?.is_some_and(|value| value.contains(MANAGED_BEGIN)))
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
fn hook_event_label(event: &str) -> Option<&'static str> {
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
    /// The subset without a `trusted_hash` in Codex's config.
    pub unreviewed: Vec<String>,
}

/// Which of Pixel's hooks in `hooks_path` Codex will skip.
///
/// Codex 0.159 runs a user or project hook only after the user reviewed it
/// (`/hooks` in the TUI), which records `[hooks.state."<file>:<event>:<group>:
/// <handler>"] trusted_hash = "sha256:…"` in `<codex_home>/config.toml`; an
/// unreviewed hook is skipped without a message, even in `codex exec`. The
/// hash is not recomputed here: a present `trusted_hash` counts as reviewed,
/// so a hook Pixel rewrote after the review (Codex's `Modified` state) is not
/// reported. The file part of the key is compared by canonical path.
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
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let file = canonical(hooks_path);
    let reviewed: Vec<String> = doc
        .get("hooks")
        .and_then(Item::as_table_like)
        .and_then(|hooks| hooks.get(HOOK_STATE_TABLE))
        .and_then(Item::as_table_like)
        .map(|state| {
            state
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .as_table_like()
                        .and_then(|entry| entry.get(TRUSTED_HASH_KEY))
                        .and_then(Item::as_str)
                        .is_some()
                })
                .map(|(key, _)| key.to_string())
                .collect()
        })
        .unwrap_or_default();
    let is_reviewed = |suffix: &str| {
        reviewed.iter().any(|key| {
            key.strip_suffix(suffix)
                .is_some_and(|source| canonical(Path::new(source)) == file)
        })
    };
    let mut review = HookReview {
        pixel: Vec::new(),
        unreviewed: Vec::new(),
    };
    let events = hooks.get("hooks").and_then(serde_json::Value::as_object);
    for (event, groups) in events.into_iter().flatten() {
        let Some(label) = hook_event_label(event) else {
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
                if !is_reviewed(&format!(":{label}:{group_index}:{handler_index}")) {
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
            "Codex skips {} of the {} Pixel hook(s) in {file} until you review them ({}): \
             start `codex` in this directory, run `/hooks` and trust them",
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
        return Err(format!(
            "{} not found — run `pixel install`",
            path.display()
        ));
    }
    let doc = read_document(&path)?;
    let Some(current) = current_value(&doc)? else {
        return Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} missing from {} — run `pixel install`",
            path.display()
        ));
    };
    match managed_range(&current) {
        None => Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} in {} carries no pixel block — run `pixel install`",
            path.display()
        )),
        Some(range) if current[range.clone()] != managed_block() => Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} in {} is stale — run `pixel install` to update",
            path.display()
        )),
        Some(_) => Ok((
            format!(
                "{DEVELOPER_INSTRUCTIONS_KEY} carries the agent prompt in {} ({} bytes)",
                path.display(),
                current.len()
            ),
            detail,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ---- metrics PostToolUse hook ----------------------------------------

    fn scratch_codex_home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-codex-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn post_tool_use(home: &Path) -> serde_json::Value {
        let text = fs::read_to_string(home.join(HOOKS_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["hooks"]["PostToolUse"].clone()
    }

    fn user_prompt_submit(home: &Path) -> serde_json::Value {
        let text = fs::read_to_string(home.join(HOOKS_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["hooks"]["UserPromptSubmit"]
            .clone()
    }

    #[test]
    fn metrics_hook_install_verify_and_preserve_foreign_entries() {
        let home = scratch_codex_home("install");
        let exe = Path::new("/opt/pixel tools/pixel");
        // A foreign PostToolUse group survives the merge.
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"PostToolUse": [
                    {"hooks": [{"type": "command", "command": "cmux-feed"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();

        install_metrics_hook(&home, exe, false).unwrap();
        let entries = post_tool_use(&home).as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            3,
            "foreign group preserved + metrics and task hooks added"
        );
        let command = entries[1]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains(METRICS_HOOK_MARKER));
        assert!(
            command.starts_with('\''),
            "the exe path is shell-quoted: {command}"
        );
        let prompt_entries = user_prompt_submit(&home).as_array().unwrap().clone();
        assert_eq!(
            prompt_entries.len(),
            2,
            "prompt-submit guidance plus the task-event group"
        );
        assert!(
            prompt_entries[0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains(PROMPT_SUBMIT_HOOK_MARKER))
        );
        assert!(
            check_metrics_hook(&home).is_ok(),
            "doctor check sees the registration"
        );

        // Second install verifies instead of duplicating.
        let step = install_metrics_hook(&home, exe, false).unwrap();
        assert!(step.summary.contains("verified"), "{}", step.summary);
        assert_eq!(post_tool_use(&home).as_array().unwrap().len(), 3);
        assert_eq!(user_prompt_submit(&home).as_array().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_reinstall_refreshes_a_stale_executable_path() {
        let home = scratch_codex_home("refresh");
        install_metrics_hook(&home, Path::new("/old/pixel"), false).unwrap();
        install_metrics_hook(&home, Path::new("/new/pixel"), false).unwrap();
        let entries = post_tool_use(&home).as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            2,
            "the stale entry is replaced, not appended"
        );
        let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains("/new/pixel"), "{command}");
        assert!(!command.contains("/old/pixel"), "{command}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_install_refuses_unparseable_hooks_json() {
        let home = scratch_codex_home("broken");
        fs::write(home.join(HOOKS_FILE), "not json").unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).unwrap(),
            "not json",
            "an unparseable file is never rewritten"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": [1]}").unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(
            step.status,
            CheckStatus::Red,
            "a non-object hooks key is refused, not silently replaced"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_install_reports_an_unreadable_hooks_json() {
        let home = scratch_codex_home("unreadable");
        // A directory where the file is expected fails the read for every
        // user including root — and is not "absent": it must come back Red,
        // never Ok-treated-as-empty.
        fs::create_dir(home.join(HOOKS_FILE)).unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        assert!(step.summary.contains("not touched"), "{}", step.summary);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_check_is_red_until_registered() {
        let home = scratch_codex_home("check");
        assert!(
            check_metrics_hook(&home).is_err(),
            "absent file is not registered"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": {}}").unwrap();
        assert!(
            check_metrics_hook(&home).is_err(),
            "empty PostToolUse is not registered"
        );
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"PostToolUse": [
                    {"hooks": [{"type": "command", "command": "pixel run-hook metrics --provider codex"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            check_metrics_hook(&home).is_err(),
            "metrics alone satisfies neither the UserPromptSubmit nor the task gate registration"
        );
        install_metrics_hook(&home, Path::new("pixel"), false).unwrap();
        assert!(check_metrics_hook(&home).is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_check_is_red_when_only_the_prompt_submit_marker_is_missing() {
        let home = scratch_codex_home("half-registered");
        install_metrics_hook(&home, Path::new("pixel"), false).unwrap();
        assert!(check_metrics_hook(&home).is_ok());

        // Remove only the prompt-submit guidance entry: the PostToolUse
        // metrics marker and every task gate stay registered, so the check
        // fails iff each marker is required on its own.
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(HOOKS_FILE)).unwrap()).unwrap();
        value["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| {
                !entry["hooks"].as_array().unwrap().iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains(PROMPT_SUBMIT_HOOK_MARKER))
                })
            });
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        assert!(
            check_metrics_hook(&home).is_err(),
            "a missing prompt-submit marker alone fails the check"
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
