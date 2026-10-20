// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Codex integration: the task-event and metrics hooks in
//! `~/.codex/hooks.json`, and the review state Codex keeps for them in
//! `~/.codex/config.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::install::{CheckStatus, InstallStep, Result, dry_run_summary};

/// `config.toml`, relative to the Codex home.
pub const CODEX_CONFIG_FILE: &str = "config.toml";

/// `hooks.json`, relative to the Codex home — Codex's hook registry, in the
/// nested `hooks.<Event>[{hooks: [{command}]}]` shape Claude Code settings
/// also use. (Distinct from `config::CODEX_HOOKS_FILE`, which is
/// home-relative.)
pub const HOOKS_FILE: &str = "hooks.json";

/// Marker of a Codex metrics callback.
pub const METRICS_HOOK_MARKER: &str = "run-hook metrics";

/// The Codex home directory: `$CODEX_HOME` when the caller did not pin a
/// home directory (a real `pixel setup`, where Codex itself honours the
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

fn read_document(path: &Path) -> std::result::Result<DocumentMut, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    text.parse::<DocumentMut>()
        .map_err(|e| format!("{} does not parse as TOML: {e}", path.display()))
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
    crate::install::write_settings(path, value, false)?;
    Ok(())
}

/// The PostToolUse entry `pixel setup` merges into `hooks.json`: Codex runs
/// it after every tool call; `pixel run-hook metrics` self-filters to shell
/// calls that invoked `pixel` and re-emits the finalized 🟩 line — the relay
/// Codex's stderr-less tool results cannot show.
fn metrics_hook_entry(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("{} run-hook metrics --provider codex", crate::routing::quoted_executable(exe)),
            "timeout": 10,
        }]
    })
}

/// `pixel setup` step: keep Codex's task-event lifecycle hooks and the
/// PostToolUse metrics relay installed.
pub(crate) fn install_task_hooks(
    codex_home: &Path,
    exe: &Path,
    prompt_parts: u8,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = Some(format!(
        "path={} task-event lifecycle hooks and the metrics relay",
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
    if let Err(error) =
        crate::routing::merge_task_hooks(hooks, crate::routing::Provider::Codex, exe, prompt_parts)
    {
        return Ok(step(CheckStatus::Red, format!("{error} — not touched")));
    }
    let metrics = crate::config::merge_hook_entry(
        hooks.get("PostToolUse"),
        METRICS_HOOK_MARKER,
        metrics_hook_entry(exe),
    );
    hooks.insert("PostToolUse".to_string(), metrics);
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

/// `pixel doctor` check: task-event hooks and the metrics relay are
/// registered.
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
        "metrics_marker": METRICS_HOOK_MARKER,
    });
    let value = read_hooks(&path)?;
    if !crate::routing::task_hooks_registered(&value, crate::routing::Provider::Codex, exe) {
        // A brief split over several hooks is whole only with every part.
        if let Err(reason) =
            crate::routing::prompt_parts(&value, crate::routing::Provider::Codex, exe)
            && reason != crate::routing::NO_PROMPT_HOOK
        {
            return Err(format!(
                "{reason} in {} — run `pixel setup`",
                path.display()
            ));
        }
        return Err(format!(
            "task lifecycle hooks missing or asynchronous in {} — run `pixel setup`",
            path.display()
        ));
    }
    // The metrics relay is part of the install: an entry naming its marker
    // must sit under PostToolUse.
    let metrics_registered = value
        .get("hooks")
        .and_then(|hooks| hooks.get("PostToolUse"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(serde_json::Value::as_str))
        .any(|command| command.contains(METRICS_HOOK_MARKER));
    if !metrics_registered {
        return Err(format!(
            "metrics PostToolUse relay missing in {} — run `pixel setup`",
            path.display()
        ));
    }
    Ok((
        format!(
            "task-event hooks and metrics relay registered in {}",
            path.display()
        ),
        detail,
    ))
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

/// The `hooks.state` entries Codex may hold for one handler. Codex looks up
/// the exact key `{source}:{event}:{group}:{handler}`, with `source` the
/// hooks file's path as it resolved it: the path Pixel was given, or its
/// canonical form when Codex canonicalized a symlinked `CODEX_HOME` or
/// repository (`/var` -> `/private/var`). Only those two spellings are
/// looked up, never another alias of the same file, and both are returned
/// when both are recorded, so a caller can require every one to agree.
fn hook_state_entries<'a>(
    doc: &'a DocumentMut,
    hooks_path: &Path,
    event: &str,
    group_index: usize,
    handler_index: usize,
) -> Vec<&'a Item> {
    let Some(label) = hook_event_label(event) else {
        return Vec::new();
    };
    let Some(states) = doc
        .get("hooks")
        .and_then(Item::as_table_like)
        .and_then(|hooks| hooks.get(HOOK_STATE_TABLE))
        .and_then(Item::as_table_like)
    else {
        return Vec::new();
    };
    let mut sources = vec![hooks_path.display().to_string()];
    if let Ok(canonical) = hooks_path.canonicalize() {
        let canonical = canonical.display().to_string();
        if canonical != sources[0] {
            sources.push(canonical);
        }
    }
    sources
        .iter()
        .filter_map(|source| states.get(&format!("{source}:{label}:{group_index}:{handler_index}")))
        .collect()
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
    // Whichever spelling Codex resolved, its entry must approve the hook:
    // with both recorded, a stale or disabled one is not outvoted.
    let states = hook_state_entries(doc, hooks_path, event, group_index, handler_index);
    !states.is_empty()
        && states.iter().all(|state| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn hook_state_entries_should_read_only_the_given_and_canonical_spellings() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-codex");
        std::fs::create_dir_all(&real).unwrap();
        let linked = temp.path().join("linked-codex");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let other_link = temp.path().join("other-link");
        std::os::unix::fs::symlink(&real, &other_link).unwrap();
        std::fs::write(real.join("hooks.json"), "{}").unwrap();
        let canonical = real.canonicalize().unwrap().join("hooks.json");
        let through_link = linked.join("hooks.json");
        let doc = |entries: &[(&Path, &str)]| -> DocumentMut {
            entries
                .iter()
                .map(|(source, body)| {
                    format!("[hooks.state.\"{}:stop:1:0\"]\n{body}\n", source.display())
                })
                .collect::<String>()
                .parse()
                .unwrap()
        };
        let ok = "trusted_hash = \"sha256:x\"";
        let off = "enabled = false\ntrusted_hash = \"sha256:x\"";

        // Codex canonicalized the symlinked home: the canonical key counts.
        let canonical_only = doc(&[(&canonical, ok)]);
        assert_eq!(
            super::hook_state_entries(&canonical_only, &through_link, "Stop", 1, 0).len(),
            1
        );
        assert!(super::hook_state_entries(&canonical_only, &through_link, "Stop", 0, 0).is_empty());
        assert!(
            super::hook_state_entries(&canonical_only, &through_link, "SessionEnd", 1, 0)
                .is_empty()
        );

        // Another alias of the same file is never Codex's key for this path.
        let alias_only = doc(&[(&other_link.join("hooks.json"), ok)]);
        assert!(super::hook_state_entries(&alias_only, &through_link, "Stop", 1, 0).is_empty());

        // Both spellings recorded and disagreeing: both are returned, so a
        // disabled canonical entry is not outvoted by an enabled given one.
        let conflicting = doc(&[(&through_link, ok), (&canonical, off)]);
        let states = super::hook_state_entries(&conflicting, &through_link, "Stop", 1, 0);
        assert_eq!(states.len(), 2);
        assert!(states.iter().any(|state| {
            state
                .as_table_like()
                .unwrap()
                .get("enabled")
                .and_then(Item::as_bool)
                == Some(false)
        }));

        // The trust decision install reads: only a unanimous approval with
        // the current hash counts.
        let group =
            serde_json::json!({"hooks": [{"type": "command", "command": "x", "timeout": 10}]});
        let hash = codex_hook_hash("Stop", &group, &group["hooks"][0]).unwrap();
        let approve = format!("trusted_hash = {hash:?}");
        let disable = format!("enabled = false\ntrusted_hash = {hash:?}");
        let trusted = |entries: &[(&Path, &str)]| {
            super::codex_hook_is_enabled_and_trusted(
                &doc(entries),
                &through_link,
                "Stop",
                1,
                0,
                &group,
                &group["hooks"][0],
            )
        };
        assert!(trusted(&[(&canonical, &approve)]));
        assert!(trusted(&[
            (&through_link, &approve),
            (&canonical, &approve)
        ]));
        assert!(!trusted(&[
            (&through_link, &approve),
            (&canonical, &disable)
        ]));
        assert!(!trusted(&[(&other_link.join("hooks.json"), &approve)]));
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
    fn task_hook_reinstall_refreshes_the_binary_path_without_duplicating_events() {
        let home = scratch_codex_home("refresh");
        install_task_hooks(&home, Path::new("/old/pixel"), 1, false).unwrap();
        install_task_hooks(&home, Path::new("/new/pixel"), 1, false).unwrap();
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
        install_task_hooks(&home, exe, 1, false).unwrap();
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

        install_task_hooks(&home, exe, 1, false).unwrap();
        let hooks = hook_events(&home);
        let post = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post.len(),
            2,
            "renamed Pixel callback is refreshed, the lifecycle hook kept"
        );
        assert_eq!(
            post[0]["hooks"][0]["command"],
            "'/opt/pixel custom/pixel-next' run-hook metrics --provider codex",
            "the metrics relay is re-merged in place under the current executable"
        );
        assert_eq!(
            post[1]["hooks"][0]["command"],
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
        let step = install_task_hooks(&home, Path::new("/x"), 1, false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).unwrap(),
            "not json",
            "an unparseable file is never rewritten"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": [1]}").unwrap();
        let step = install_task_hooks(&home, Path::new("/x"), 1, false).unwrap();
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
        let step = install_task_hooks(&home, Path::new("/x"), 1, false).unwrap();
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
        install_task_hooks(&home, Path::new("pixel"), 1, false).unwrap();
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
        install_task_hooks(&home, Path::new("pixel"), 1, false).unwrap();
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
}
