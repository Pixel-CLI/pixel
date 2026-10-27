// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Provider-specific hook configuration. Installation is not proof that a
//! harness has fired the hooks; doctor reports that boundary separately.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::{InstallError, config, install};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
    Devin,
}

impl Provider {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Devin => "devin",
        }
    }

    pub(crate) fn path(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Claude => ".claude/settings.json",
            Self::Codex => config::CODEX_HOOKS_FILE,
            Self::Devin => ".config/devin/config.json",
        })
    }

    pub(crate) fn shell(self) -> &'static str {
        if self == Self::Devin { "exec" } else { "Bash" }
    }
}

/// Ends every hook command so that it exits 0 whatever the binary does.
/// A host treats exit 2 as a block: Claude Code drops the user's prompt on
/// `UserPromptSubmit`, refuses the tool call on `PreToolUse` and keeps the
/// session from ending on `Stop`, and Gemini blocks the turn. A binary that
/// does not know the verb or a flag it is called with (an older or a newer
/// `pixel` behind the same path, `--part`) exits 2, so every command Pixel
/// writes runs through the shell (every host runs hook commands in one) with
/// this tail. Pixel's hooks block through the JSON they print, never through
/// their exit status, so the tail removes no intended block. Recognition
/// accepts the command with or without it, so entries of either form are
/// replaced, counted and removed alike.
pub(crate) const FAIL_OPEN: &str = " || true";

/// `command` with [`FAIL_OPEN`] appended: the form of every hook command
/// `pixel setup` writes.
pub(crate) fn fail_open(command: &str) -> String {
    format!("{command}{FAIL_OPEN}")
}

/// `(executable, verb)` with a trailing [`FAIL_OPEN`] taken off the verb.
fn without_fail_open<'a>((executable, verb): (&'a str, &'a str)) -> (&'a str, &'a str) {
    (executable, verb.strip_suffix(FAIL_OPEN).unwrap_or(verb))
}

pub(crate) fn quoted_executable(exe: &Path) -> String {
    format!("'{}'", exe.to_string_lossy().replace('\'', "'\\''"))
}

/// The path of an unquoted executable token, or of one single-quoted the way
/// [`quoted_executable`] writes it. An unquoted token holding whitespace is a
/// command line, not an executable, and has none.
fn unquoted_executable(executable: &str) -> Option<PathBuf> {
    if let Some(inner) = executable
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Some(PathBuf::from(inner.replace("'\\''", "'")));
    }
    if executable.chars().any(char::is_whitespace) {
        return None;
    }
    Some(PathBuf::from(executable))
}

/// The file name of an executable token (see [`unquoted_executable`]).
fn executable_name(executable: &str) -> Option<String> {
    unquoted_executable(executable)?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// What makes `command` one of pixel's hooks, or `None` for a foreign one:
/// its `run-hook` verb (`task-event --provider claude --event stop`,
/// `metrics --provider codex`).
///
/// A `run-hook` command is pixel's when its executable carries one of the
/// names pixel installs itself under ([`config::PIXEL_EXECUTABLES`]: `pixel`
/// for a release, `pixel-dev` for `self-update --dev`) or the file name of
/// `exe`, the binary installing or uninstalling now. Without them a build under
/// another name never recognises the entries it wrote, nor a release the dev
/// build's: each install appends a new set beside the old one and uninstall
/// leaves them all. Only those exact names count, never any name merely
/// containing `pixel`.
pub(crate) fn pixel_hook_verb<'a>(command: &'a str, exe: &Path) -> Option<&'a str> {
    let own = exe.file_name().map(|n| n.to_string_lossy());
    command
        .rsplit_once(" run-hook ")
        .map(without_fail_open)
        .filter(|(executable, verb)| {
            executable_name(executable).is_some_and(|name| {
                config::PIXEL_EXECUTABLES.contains(&name.as_str())
                    || own.as_deref() == Some(name.as_str())
            }) && (["metrics --provider claude", "metrics --provider codex"].contains(verb)
                || task_hook_verb(verb)
                || edit_hook_verb(verb))
        })
        .map(|(_, verb)| verb)
}

/// The verb after `run-hook` in `command` when its executable is one of
/// pixel's (the names [`pixel_hook_verb`] accepts), whatever the verb: a
/// caller owning verbs that table does not list, such as Antigravity's
/// `guard --provider antigravity`, compares the verb itself.
pub(crate) fn pixel_run_hook_verb<'a>(command: &'a str, exe: &Path) -> Option<&'a str> {
    let own = exe.file_name().map(|n| n.to_string_lossy());
    command
        .rsplit_once(" run-hook ")
        .map(without_fail_open)
        .filter(|(executable, _)| {
            executable_name(executable).is_some_and(|name| {
                config::PIXEL_EXECUTABLES.contains(&name.as_str())
                    || own.as_deref() == Some(name.as_str())
            })
        })
        .map(|(_, verb)| verb)
}

/// Recognize our executable commands, not arbitrary commands merely
/// containing a lifecycle verb. See [`pixel_hook_verb`] for
/// which executables count as pixel's.
pub(crate) fn is_pixel_hook(command: &str, exe: &Path) -> bool {
    pixel_hook_verb(command, exe).is_some()
}

/// Supported native boundaries; Codex has no PostToolUseFailure event.
pub(crate) const TASK_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
    ("SubagentStart", "subagent-start"),
    ("SubagentStop", "subagent-stop"),
];

/// The task-event boundaries `provider` accepts: the shared table plus the
/// provider's extra boundary (`PostToolUseFailure` for Claude, `Interrupt`
/// for Codex). Devin accepts only PreToolUse, PostToolUse,
/// PermissionRequest, UserPromptSubmit, Stop, PostCompaction, SessionStart
/// and SessionEnd and rejects a `hooks` object that names any other event,
/// so its set drops the subagent boundaries and takes no extra.
fn provider_task_events(provider: Provider) -> Vec<(&'static str, &'static str)> {
    let mut events = TASK_HOOK_EVENTS.to_vec();
    match provider {
        Provider::Claude => events.push(("PostToolUseFailure", "tool-failure")),
        Provider::Devin => {
            events.retain(|(event, _)| !matches!(*event, "SubagentStart" | "SubagentStop"));
        }
        Provider::Codex => events.push(("Interrupt", "interrupt")),
    }
    events
}

/// One `task-event` hook verb taken apart: the provider and event words and,
/// for a split prompt-submit brief, the `--part K/N` it serves.
struct TaskVerb<'a> {
    provider: &'a str,
    event: &'a str,
    part: Option<(u8, u8)>,
}

/// `K/N` of a `--part` flag: `1 <= K <= N <= config::BRIEF_PARTS_MAX`, and
/// `N >= 2` (a single part is spelled without the flag).
fn parse_part(text: &str) -> Option<(u8, u8)> {
    let (k, n) = text.split_once('/')?;
    let (k, n) = (k.parse::<u8>().ok()?, n.parse::<u8>().ok()?);
    ((2..=config::BRIEF_PARTS_MAX).contains(&n) && (1..=n).contains(&k)).then_some((k, n))
}

fn parse_task_verb(verb: &str) -> Option<TaskVerb<'_>> {
    let words: Vec<_> = verb.split_whitespace().collect();
    let (provider, event, part) = match words.as_slice() {
        ["task-event", "--provider", provider, "--event", event] => (*provider, *event, None),
        [
            "task-event",
            "--provider",
            provider,
            "--event",
            "prompt-submit",
            "--part",
            part,
        ] => (*provider, "prompt-submit", Some(parse_part(part)?)),
        _ => return None,
    };
    (matches!(
        provider,
        "claude" | "codex" | "pi" | "devin" | "gemini" | "antigravity"
    ) && (TASK_HOOK_EVENTS.iter().any(|(_, name)| *name == event)
        || matches!(
            event,
            "tool-failure" | "interrupt" | "model-response" | "user-bash"
        )))
    .then_some(TaskVerb {
        provider,
        event,
        part,
    })
}

fn task_hook_verb(verb: &str) -> bool {
    parse_task_verb(verb).is_some()
}

/// The edit hooks, off unless `edit_hooks` is on: the host event each rides
/// and the `edit-event` word it passes. They sit beside the task-event
/// entries of the same host event, with no matcher, because each host names
/// its edit tools differently and `edit-event` ignores any other tool.
pub(crate) const EDIT_HOOK_EVENTS: &[(&str, &str)] =
    &[("PreToolUse", "pre-edit"), ("PostToolUse", "post-edit")];

fn edit_hook_verb(verb: &str) -> bool {
    match verb.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["edit-event", "--provider", provider, "--event", event] => {
            matches!(*provider, "claude" | "codex" | "devin")
                && EDIT_HOOK_EVENTS.iter().any(|(_, name)| name == event)
        }
        _ => false,
    }
}

/// Takes out every Pixel entry whose verb satisfies `owned`, keeping foreign
/// hooks and the rest of each group, and returns where each event's first
/// emptied group sat.
///
/// Codex keys a hook's trust by its group index, so Pixel's new groups go
/// where its old ones were, not after the user's.
fn purge_verbs(
    hooks: &mut Map<String, Value>,
    exe: &Path,
    owned: fn(&str) -> bool,
) -> HashMap<String, usize> {
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let mut kept = Vec::with_capacity(groups.len());
        for mut group in std::mem::take(groups) {
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(Value::as_str)
                        .and_then(|command| pixel_hook_verb(command, exe))
                        .is_some_and(owned)
                });
                if inner.is_empty() {
                    if before > 0 {
                        slots.entry(event.clone()).or_insert(kept.len());
                    }
                    continue;
                }
            }
            kept.push(group);
        }
        *groups = kept;
    }
    slots
}

/// Replaces Pixel's edit hooks (see [`EDIT_HOOK_EVENTS`]) in `hooks`: always
/// takes the old ones out, and writes them again only when `enabled`. With
/// `enabled` false it removes nothing but the edit hooks, so a setup with the
/// switch off writes what it wrote before the switch existed.
pub(crate) fn merge_edit_hooks(
    hooks: &mut Map<String, Value>,
    provider: Provider,
    exe: &Path,
    enabled: bool,
) -> Result<(), String> {
    let slots = purge_verbs(hooks, exe, edit_hook_verb);
    hooks.retain(|event, groups| {
        !(slots.contains_key(event) && groups.as_array().is_some_and(Vec::is_empty))
    });
    if !enabled {
        return Ok(());
    }
    for &(event, name) in EDIT_HOOK_EVENTS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{event} is not an array"))?;
        let command = fail_open(&format!(
            "{} run-hook edit-event --provider {} --event {name}",
            quoted_executable(exe),
            provider.name()
        ));
        let at = slots
            .get(event)
            .copied()
            .unwrap_or(groups.len())
            .min(groups.len());
        groups.insert(at, hook_group(command, None));
    }
    Ok(())
}

/// Replace only Pixel task registrations, preserving foreign hooks and trust.
///
/// `prompt_parts` is how many `UserPromptSubmit` entries carry the brief for
/// a host with a per-hook cap on injected context (Claude Code and Codex;
/// every other provider keeps one): each is the same command with
/// `--part K/N`, so a brief larger than one hook may inject still arrives.
pub(crate) fn merge_task_hooks(
    hooks: &mut Map<String, Value>,
    provider: Provider,
    exe: &Path,
    prompt_parts: u8,
) -> Result<(), String> {
    let slots = purge_verbs(hooks, exe, task_hook_verb);
    if provider == Provider::Devin {
        // A previous install wrote the events Devin does not accept; the
        // purge above emptied their groups, and a key left empty still makes
        // Devin reject the whole `hooks` object.
        for event in [
            "SubagentStart",
            "SubagentStop",
            "Interrupt",
            "PostToolUseFailure",
        ] {
            if hooks
                .get(event)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                hooks.remove(event);
            }
        }
    }
    let split = if matches!(provider, Provider::Claude | Provider::Codex) {
        prompt_parts.clamp(1, config::BRIEF_PARTS_MAX)
    } else {
        1
    };
    for &(event, name) in &provider_task_events(provider) {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{event} is not an array"))?;
        let command = format!(
            "{} run-hook task-event --provider {} --event {name}",
            quoted_executable(exe),
            provider.name()
        );
        let commands = if name == "prompt-submit" && split > 1 {
            (1..=split)
                .map(|part| fail_open(&format!("{command} --part {part}/{split}")))
                .collect()
        } else {
            vec![fail_open(&command)]
        };
        let mut fresh = Vec::new();
        for command in commands {
            let mut group = hook_group(command, None);
            group["hooks"][0]["timeout"] = json!(if matches!(event, "SessionEnd" | "Interrupt") {
                3
            } else {
                10
            });
            if provider == Provider::Devin && name == "prompt-submit" {
                // Devin draws neither `systemMessage` nor a hook's stdout or
                // stderr; `show_output` makes it print `✱ Hook completed:
                // <command>`, the only on-screen trace that the brief ran.
                group["hooks"][0]["show_output"] = json!(true);
            }
            fresh.push(group);
        }
        let at = slots
            .get(event)
            .copied()
            .unwrap_or(groups.len())
            .min(groups.len());
        groups.splice(at..at, fresh);
    }
    Ok(())
}

/// The verbs of Pixel's hooks for `event` that run on every occurrence of it:
/// no restricting matcher, a command, synchronous.
fn eligible_verbs<'a>(value: &'a Value, event: &str, exe: &Path) -> Vec<&'a str> {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| {
            group.get("matcher").is_none()
                || group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .is_some_and(|matcher| matches!(matcher, "" | "*" | ".*"))
        })
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|hook| {
            hook.get("type").and_then(Value::as_str) == Some("command")
                && (hook.get("async").is_none()
                    || hook.get("async").and_then(Value::as_bool) == Some(false))
        })
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .filter_map(|command| pixel_hook_verb(command, exe))
        .collect()
}

/// [`prompt_parts`]' reason when no `UserPromptSubmit` entry carries the brief.
pub(crate) const NO_PROMPT_HOOK: &str = "no prompt-submit brief hook";

/// How many `UserPromptSubmit` entries carry `provider`'s brief: `1` for the
/// whole brief in one hook, `N` for a complete set `--part 1/N` .. `N/N`.
///
/// # Errors
///
/// No entry, whole and split entries mixed, parts of different counts, or a
/// part of the set missing; the text names which, for `pixel doctor`.
pub(crate) fn prompt_parts(value: &Value, provider: Provider, exe: &Path) -> Result<u8, String> {
    let entries: Vec<Option<(u8, u8)>> = eligible_verbs(value, "UserPromptSubmit", exe)
        .into_iter()
        .filter_map(parse_task_verb)
        .filter(|task| task.provider == provider.name() && task.event == "prompt-submit")
        .map(|task| task.part)
        .collect();
    let split: Vec<(u8, u8)> = entries.iter().flatten().copied().collect();
    if entries.is_empty() {
        return Err(NO_PROMPT_HOOK.to_owned());
    }
    if split.is_empty() {
        return Ok(1);
    }
    if split.len() != entries.len() {
        return Err("the whole brief and brief parts are both registered".to_owned());
    }
    let count = split[0].1;
    if split.iter().any(|&(_, n)| n != count) {
        return Err("brief parts of different counts are registered".to_owned());
    }
    let missing: Vec<String> = (1..=count)
        .filter(|part| !split.iter().any(|&(k, _)| k == *part))
        .map(|part| format!("{part}/{count}"))
        .collect();
    if missing.is_empty() {
        Ok(count)
    } else {
        Err(format!("brief part {} missing", missing.join(", ")))
    }
}

pub(crate) fn task_hooks_registered(value: &Value, provider: Provider, exe: &Path) -> bool {
    provider_task_events(provider).iter().all(|(event, name)| {
        if *name == "prompt-submit" {
            return prompt_parts(value, provider, exe).is_ok();
        }
        let expected = format!("task-event --provider {} --event {name}", provider.name());
        eligible_verbs(value, event, exe).contains(&expected.as_str())
    })
}

/// Whether every edit hook of `provider` is registered, synchronous and
/// unrestricted.
pub(crate) fn edit_hooks_registered(value: &Value, provider: Provider, exe: &Path) -> bool {
    EDIT_HOOK_EVENTS.iter().all(|(event, name)| {
        let expected = format!("edit-event --provider {} --event {name}", provider.name());
        eligible_verbs(value, event, exe).contains(&expected.as_str())
    })
}

/// Whether any edit hook is registered, whatever its provider or state.
pub(crate) fn edit_hooks_present(value: &Value, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|events| events.values())
        .filter_map(Value::as_array)
        .flat_map(|groups| commands(groups))
        .filter_map(|command| pixel_hook_verb(command, exe))
        .any(edit_hook_verb)
}

/// Preserve outer matchers and co-located foreign/security hooks.
/// Every entry [`is_pixel_hook`] recognises goes, so one pass also collapses
/// entries an earlier install stacked.
pub(crate) fn remove_pixel_hooks(hooks: &mut Map<String, Value>, exe: &Path) {
    remove_matching_hooks(hooks, |command| is_pixel_hook(command, exe));
}

/// Remove matching commands without disturbing foreign hooks or group metadata.
pub(crate) fn remove_matching_hooks(hooks: &mut Map<String, Value>, remove: impl Fn(&str) -> bool) {
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        groups.retain_mut(|group| {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            inner.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(&remove)
            });
            !inner.is_empty()
        });
        !groups.is_empty()
    });
}

/// The PostToolUse group that relays the finalized 🟩 metrics line as hook
/// output (`systemMessage` for the user, `additionalContext` for the model)
/// after a shell call — Codex's fallback for the rare host whose tool result
/// drops the merged stderr its exec layer normally carries. When the tool
/// result already carries the box, Codex and Devin stay silent while Claude
/// still gets the line as `systemMessage` only (its Bash result surfaces
/// stderr the user never sees), so a host that shows stderr does not print it
/// twice for the model. Codex's own entry lives in `codex_config`.
fn metrics_relay_group(exe: &Path, provider: Provider) -> Value {
    hook_group(
        fail_open(&format!(
            "{} run-hook metrics --provider {}",
            quoted_executable(exe),
            provider.name()
        )),
        Some(provider.shell()),
    )
}

fn hook_group(command: String, matcher: Option<&str>) -> Value {
    let mut group =
        json!({"hooks":[{"type":"command","command":command,"timeout":config::HOOK_TIMEOUT}]});
    if let Some(matcher) = matcher {
        group["matcher"] = matcher.into();
    }
    group
}

/// Register the task-event hooks of `provider` in the settings file `path`,
/// replacing only Pixel's own entries and keeping every foreign one. Claude
/// also receives its `PostToolUse` metrics relay. `prompt_parts` is how many
/// hooks carry the prompt brief (see [`merge_task_hooks`]); `edit_hooks` adds
/// the edit hooks (see [`merge_edit_hooks`]).
pub(crate) fn install_task_events(
    path: &Path,
    exe: &Path,
    provider: Provider,
    prompt_parts: u8,
    edit_hooks: bool,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let mut value = install::read_settings(path)?;
    configure_task_events(&mut value, provider, exe, prompt_parts, edit_hooks).map_err(
        |reason| InstallError::InvalidSettings {
            path: path.into(),
            reason,
        },
    )?;
    let backup = install::write_settings(path, &value, dry_run)?;
    Ok(install::InstallStep {
        id: format!("hooks.{}", provider.name()),
        status: install::CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &format!(
                "{} task-event hooks configured (live unverified)",
                provider.name()
            ),
        ),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

/// The pure transform behind [`install_task_events`].
fn configure_task_events(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    prompt_parts: u8,
    edit_hooks: bool,
) -> Result<(), String> {
    let root = value
        .as_object_mut()
        .ok_or("settings root is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks is not an object")?;
    remove_pixel_hooks(hooks, exe);
    if provider == Provider::Claude {
        hooks
            .entry("PostToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("PostToolUse is not an array")?
            .push(metrics_relay_group(exe, provider));
    }
    merge_task_hooks(hooks, provider, exe, prompt_parts)?;
    merge_edit_hooks(hooks, provider, exe, edit_hooks)
}

/// Every hook command of `groups`, in order.
fn commands(groups: &[Value]) -> impl Iterator<Item = &str> {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
}

/// Pixel hooks a settings value registers more than once for the same event
/// and verb, as `Event→verb ×n` lines: the trace of installs that
/// appended their entries beside earlier ones instead of replacing them, so
/// the harness runs that hook `n` times.
pub(crate) fn stacked_pixel_hooks(value: &Value, exe: &Path) -> Vec<String> {
    let Some(events) = value.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut stacked = Vec::new();
    for (event, groups) in events {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for verb in groups
            .as_array()
            .into_iter()
            .flat_map(|groups| commands(groups))
            .filter_map(|command| pixel_hook_verb(command, exe))
        {
            match counts.iter_mut().find(|(seen, _)| *seen == verb) {
                Some((_, count)) => *count += 1,
                None => counts.push((verb, 1)),
            }
        }
        stacked.extend(
            counts
                .into_iter()
                .filter(|&(_, count)| count > 1)
                .map(|(verb, count)| format!("{event}→{verb} ×{count}")),
        );
    }
    stacked
}

/// The executables pixel's `run-hook` entries in a settings value run that
/// are not `exe`, each once, in the order met. A `pixel-dev setup` leaves
/// every hook on the side build and a package-manager upgrade can leave them
/// on the previous version's path; either way the hooks are complete and
/// counted once, so only the executable tells the managed binary that its
/// sessions are not running it. Two paths name the same binary when they
/// canonicalize to one file (a symlink, a shim-less PATH entry).
pub(crate) fn pixel_hooks_running_other_binaries(value: &Value, exe: &Path) -> Vec<PathBuf> {
    let Some(events) = value.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let own = exe.canonicalize().ok();
    let mut others: Vec<PathBuf> = Vec::new();
    for command in events
        .values()
        .filter_map(Value::as_array)
        .flat_map(|groups| commands(groups))
        .filter(|command| is_pixel_hook(command, exe))
    {
        let Some(path) = command
            .rsplit_once(" run-hook ")
            .and_then(|(executable, _)| unquoted_executable(executable))
        else {
            continue;
        };
        let same = path == exe || (own.is_some() && path.canonicalize().ok() == own);
        if !same && !others.contains(&path) {
            others.push(path);
        }
    }
    others
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_hooks_should_keep_every_group_index_when_setup_runs_again() {
        let exe = Path::new("/tmp/pixel");
        let user = json!({"hooks":[{"type":"command","command":"user-check"}]});
        for pixel_first in [true, false] {
            let mut value = json!({"hooks":{}});
            merge_task_hooks(
                value["hooks"].as_object_mut().unwrap(),
                Provider::Codex,
                exe,
                1,
            )
            .unwrap();
            let pixel = value["hooks"]["PreToolUse"][0].clone();
            value["hooks"]["PreToolUse"] = if pixel_first {
                json!([pixel, user])
            } else {
                json!([user, pixel])
            };
            let before = value.clone();
            merge_task_hooks(
                value["hooks"].as_object_mut().unwrap(),
                Provider::Codex,
                exe,
                1,
            )
            .unwrap();
            assert_eq!(value, before);
            let user_at = usize::from(pixel_first);
            assert_eq!(value["hooks"]["PreToolUse"][user_at], user);
        }
    }

    #[test]
    fn task_hooks_should_replace_only_owned_entries_and_cover_native_boundaries() {
        let exe = Path::new("/tmp/pixel");
        for provider in [Provider::Claude, Provider::Codex] {
            let foreign =
                json!({"type":"command","command":"security-check","trusted_hash":"keep"});
            let mut value = json!({"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[foreign.clone(),{"command":"pixel run-hook task-event --provider claude --event pre-tool-use"}]}]}});
            merge_task_hooks(value["hooks"].as_object_mut().unwrap(), provider, exe, 1).unwrap();
            assert_eq!(
                value["hooks"]["PreToolUse"][0]["hooks"],
                json!([foreign.clone()])
            );
            assert!(task_hooks_registered(&value, provider, exe));
            let first = value.clone();
            merge_task_hooks(value["hooks"].as_object_mut().unwrap(), provider, exe, 1).unwrap();
            assert_eq!(value, first);
            assert!(stacked_pixel_hooks(&value, exe).is_empty());
            let mut restricted = value.clone();
            restricted["hooks"]["PreToolUse"][1]["matcher"] = json!("Read");
            assert!(!task_hooks_registered(&restricted, provider, exe));
            value["hooks"]["Stop"][0]["hooks"][0]["async"] = json!(true);
            assert!(!task_hooks_registered(&value, provider, exe));
            remove_pixel_hooks(value["hooks"].as_object_mut().unwrap(), exe);
            assert_eq!(
                value,
                json!({"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[foreign]}]}})
            );
        }
        for command in [
            "foreign run-hook task-event --provider codex --event stop",
            "pixel run-hook task-event --provider codex --event stop && security-check",
            "pixel run-hook task-event --provider unknown --event stop",
        ] {
            assert!(!is_pixel_hook(command, exe), "{command}");
        }
    }

    /// What `configure_task_events` writes for `provider` into a fresh file.
    fn written(provider: Provider, edit_hooks: bool) -> Value {
        let mut value = json!({});
        configure_task_events(&mut value, provider, Path::new("/tmp/pixel"), 1, edit_hooks)
            .unwrap();
        value
    }

    #[test]
    fn edit_hooks_off_should_write_what_task_hooks_alone_write() {
        let exe = Path::new("/tmp/pixel");
        for provider in [Provider::Claude, Provider::Codex, Provider::Devin] {
            // The body of `configure_task_events` before the switch existed.
            let mut expected = json!({"hooks": {}});
            if provider == Provider::Claude {
                expected["hooks"]["PostToolUse"] = json!([metrics_relay_group(exe, provider)]);
            }
            merge_task_hooks(expected["hooks"].as_object_mut().unwrap(), provider, exe, 1).unwrap();
            let written = written(provider, false);
            assert_eq!(written, expected, "{provider:?}");
            assert!(!edit_hooks_present(&written, exe));
        }
    }

    #[test]
    fn edit_hooks_on_should_add_two_failing_open_entries_recognised_everywhere() {
        let exe = Path::new("/tmp/pixel");
        for provider in [Provider::Claude, Provider::Codex, Provider::Devin] {
            let on = written(provider, true);
            let off = written(provider, false);
            assert!(edit_hooks_registered(&on, provider, exe), "{provider:?}");
            assert!(!edit_hooks_registered(&off, provider, exe));
            assert!(edit_hooks_present(&on, exe), "{provider:?}");
            assert!(!edit_hooks_present(&off, exe), "{provider:?}");
            assert!(task_hooks_registered(&on, provider, exe));
            assert!(stacked_pixel_hooks(&on, exe).is_empty());
            for (event, name) in EDIT_HOOK_EVENTS {
                let command = format!(
                    "'/tmp/pixel' run-hook edit-event --provider {} --event {name}{FAIL_OPEN}",
                    provider.name()
                );
                assert!(
                    on["hooks"][*event].to_string().contains(&command),
                    "{provider:?} {event}"
                );
                assert!(is_pixel_hook(&command, exe));
                // Recognised with or without the tail, so an older entry is replaced.
                assert!(is_pixel_hook(command.trim_end_matches(FAIL_OPEN), exe));
            }
            // Writing again changes nothing; switching off gives the off file back.
            let mut again = on.clone();
            configure_task_events(&mut again, provider, exe, 1, true).unwrap();
            assert_eq!(again, on);
            merge_edit_hooks(
                again["hooks"].as_object_mut().unwrap(),
                provider,
                exe,
                false,
            )
            .unwrap();
            assert_eq!(again, off, "{provider:?}");
            // Removal takes the edit hooks with the rest.
            let mut removed = on;
            remove_pixel_hooks(removed["hooks"].as_object_mut().unwrap(), exe);
            assert!(!edit_hooks_present(&removed, exe));
        }
    }

    /// An edit-event verb is Pixel's only for a host that has edit hooks and
    /// an event of the edit pair: either half wrong disowns it.
    #[test]
    fn edit_hook_verb_should_need_an_edit_host_and_an_edit_event() {
        for provider in ["claude", "codex", "devin"] {
            for (_, event) in EDIT_HOOK_EVENTS {
                let verb = format!("edit-event --provider {provider} --event {event}");
                assert!(edit_hook_verb(&verb), "{verb}");
            }
            assert!(!edit_hook_verb(&format!(
                "edit-event --provider {provider} --event session-start"
            )));
        }
        assert!(!edit_hook_verb("edit-event --provider pi --event pre-edit"));
        assert!(!edit_hook_verb(
            "task-event --provider claude --event pre-edit"
        ));
    }

    #[test]
    fn edit_hooks_should_leave_foreign_hooks_and_their_group_index_alone() {
        let exe = Path::new("/tmp/pixel");
        let foreign = json!({"matcher":"Edit","hooks":[{"type":"command","command":"my-guard"}]});
        let mut value = json!({"hooks":{"PreToolUse":[foreign.clone()],"PostToolUse":[]}});
        let hooks = value["hooks"].as_object_mut().unwrap();
        merge_task_hooks(hooks, Provider::Codex, exe, 1).unwrap();
        merge_edit_hooks(hooks, Provider::Codex, exe, true).unwrap();
        assert_eq!(value["hooks"]["PreToolUse"][0], foreign);
        let on = value.clone();
        let hooks = value["hooks"].as_object_mut().unwrap();
        merge_edit_hooks(hooks, Provider::Codex, exe, false).unwrap();
        merge_edit_hooks(hooks, Provider::Codex, exe, true).unwrap();
        assert_eq!(value, on);
        let mut restricted = on;
        restricted["hooks"]["PostToolUse"][0]["matcher"] = json!("Read");
        let n = restricted["hooks"]["PostToolUse"].as_array().unwrap().len();
        restricted["hooks"]["PostToolUse"][n - 1]["matcher"] = json!("Read");
        assert!(!edit_hooks_registered(&restricted, Provider::Codex, exe));
    }

    /// A release build's executable: every release installs it as `pixel`.
    fn release() -> &'static Path {
        Path::new("/usr/local/bin/pixel")
    }

    #[test]
    fn routing_ownership_rejects_other_executables_and_command_mentions() {
        assert!(is_pixel_hook(
            "'/tmp/Pixel tools/pixel' run-hook metrics --provider claude",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook task-event --provider claude --event prompt-submit",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook task-event --provider claude --event prompt-submit",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook task-event --provider claude --event stop",
            release()
        ));
        for foreign in [
            "other-pixel hook guard",
            "echo /tmp/pixel hook session-start",
            "echo ~/.claude/hooks/pixel-prompt-submit",
            "echo 'pixel hook guard'",
            "'/tmp/pixel' run-hook task-event --provider claude --event session-start && security-check",
            "'/tmp/pixel' run-hook metrics --provider claude; security-check",
            "'/tmp/pixel' run-hook task-event --provider claude --event prompt-submit > user-log",
        ] {
            assert!(!is_pixel_hook(foreign, release()), "{foreign}");
        }
    }

    /// A build installed under another name than `pixel` (`self-update
    /// --dev` writes `pixel-dev`, a user may rename one) must recognise the
    /// entries it wrote itself, and a release those of `pixel-dev`, or every
    /// install appends a second set and uninstall leaves them all.
    /// Recognising them must not widen ownership to other names that merely
    /// contain `pixel`, nor to commands that chain or redirect.
    #[test]
    fn is_pixel_hook_should_own_the_installing_executable_under_any_name_but_no_other() {
        let dev = Path::new("/Users/dev/.local/bin/pixel-dev");
        for own in [
            "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event session-start",
            "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event post-tool-use",
            // The same build copied elsewhere writes the same file name.
            "'/opt/other place/pixel-dev' run-hook task-event --provider claude --event prompt-submit",
            // A release build's entries stay pixel's for a dev install.
            "'/usr/local/bin/pixel' run-hook metrics --provider claude",
        ] {
            assert!(is_pixel_hook(own, dev), "{own}");
        }
        for foreign in [
            "'/opt/pixel-dev-helper' run-hook task-event --provider claude --event session-start",
            "'/opt/other-pixel' run-hook task-event --provider claude --event session-start",
            "other-pixel hook guard",
            "herdr hook session-start --agent claude",
            "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event session-start && security-check",
            "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event prompt-submit > user-log",
            "'/Users/dev/.local/bin/pixel-dev' run-hook unknown-verb",
            "my-tool ~/.claude/hooks/pixel-session-start",
        ] {
            assert!(!is_pixel_hook(foreign, dev), "{foreign}");
        }
        assert!(
            is_pixel_hook(
                "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event session-start",
                release()
            ),
            "a release install replaces what `self-update --dev` installed"
        );
        let renamed = Path::new("/opt/bin/pixel-livio");
        let livio =
            "'/opt/bin/pixel-livio' run-hook task-event --provider claude --event session-start";
        assert!(
            is_pixel_hook(livio, renamed),
            "a build renamed by hand owns what it wrote"
        );
        assert!(
            !is_pixel_hook(livio, release()),
            "a name pixel never installs under is only its own build's"
        );
        assert!(!is_pixel_hook(
            "'/opt/pixel-dev-helper' run-hook task-event --provider claude --event session-start",
            release()
        ));
    }

    /// A token quoted by [`quoted_executable`] comes back as the path it
    /// quoted, apostrophes included; a bare token is its own path unless it
    /// holds whitespace, which makes it a command line.
    #[test]
    fn unquoted_executable_should_invert_quoted_executable() {
        let odd = Path::new("/opt/it's here/pixel-dev");
        assert_eq!(
            unquoted_executable(&quoted_executable(odd)),
            Some(odd.to_path_buf())
        );
        assert_eq!(
            unquoted_executable("/usr/local/bin/pixel"),
            Some(PathBuf::from("/usr/local/bin/pixel"))
        );
        assert_eq!(unquoted_executable("/opt/my tools/pixel"), None);
    }

    /// Doctor names each stacked pixel hook with its count, whichever of
    /// pixel's names wrote the copies, and never counts a foreign hook or a
    /// pixel hook registered once.
    #[test]
    fn stacked_pixel_hooks_should_name_each_verb_registered_more_than_once() {
        let dev = Path::new("/Users/dev/.local/bin/pixel-dev");
        let entry = |command: &str| json!({"hooks":[{"type":"command","command":command}]});
        let start = entry(
            "'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event session-start",
        );
        let herdr = entry("herdr hook session-start --agent claude");
        let value = json!({"hooks":{
            "SessionStart":[
                start.clone(), start.clone(), start,
                {"matcher":"compact","hooks":[{"type":"command","command":"'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event stop"}]},
            ],
            "Stop":[herdr.clone(), herdr],
            "UserPromptSubmit":[
                entry("'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit"),
                entry("'/Users/dev/.local/bin/pixel-dev' run-hook task-event --provider claude --event prompt-submit"),
            ],
        }});
        assert_eq!(
            stacked_pixel_hooks(&value, dev),
            vec![
                "SessionStart→task-event --provider claude --event session-start ×3".to_owned(),
                "UserPromptSubmit→task-event --provider claude --event prompt-submit ×2".to_owned(),
            ]
        );
        assert_eq!(
            stacked_pixel_hooks(&value, release()),
            stacked_pixel_hooks(&value, dev),
            "a release doctor sees the copies a dev build stacked"
        );
        assert_eq!(stacked_pixel_hooks(&Value::Null, dev), Vec::<String>::new());
    }

    /// The split brief's entries, in order, as the commands they run.
    fn prompt_commands(value: &Value) -> Vec<String> {
        value["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["hooks"][0]["command"].as_str().unwrap().to_owned())
            .collect()
    }

    fn merged(provider: Provider, exe: &Path, parts: u8) -> Value {
        let mut value = json!({"hooks": {}});
        merge_task_hooks(
            value["hooks"].as_object_mut().unwrap(),
            provider,
            exe,
            parts,
        )
        .unwrap();
        value
    }

    #[test]
    fn parse_part_should_accept_only_a_proper_split_inside_the_bounds() {
        assert_eq!(parse_part("1/2"), Some((1, 2)));
        assert_eq!(parse_part("3/3"), Some((3, 3)));
        assert_eq!(
            parse_part(&format!("1/{}", config::BRIEF_PARTS_MAX)),
            Some((1, config::BRIEF_PARTS_MAX))
        );
        for bad in [
            "1/1", "0/3", "4/3", "1/9", "1/", "/3", "a/3", "1/b", "1-3", "", "1/3/3", "-1/3",
            "256/300",
        ] {
            assert_eq!(parse_part(bad), None, "{bad}");
        }
    }

    /// Pixel's own entries are recognised with a part, so an install after a
    /// change of `brief_parts` replaces the old set instead of stacking.
    #[test]
    fn a_split_prompt_entry_should_read_as_a_pixel_hook_of_either_executable() {
        for exe in [
            "'/usr/local/bin/pixel'",
            "'/Users/dev/.local/bin/pixel-dev'",
        ] {
            let command = format!(
                "{exe} run-hook task-event --provider claude --event prompt-submit --part 2/3"
            );
            assert_eq!(
                pixel_hook_verb(&command, release()),
                Some("task-event --provider claude --event prompt-submit --part 2/3")
            );
        }
        assert!(!is_pixel_hook(
            "'/opt/other' run-hook task-event --provider claude --event prompt-submit --part 2/3",
            release()
        ));
    }

    #[test]
    fn merge_should_write_the_brief_parts_in_order_and_only_for_hosts_with_a_cap() {
        let exe = Path::new("/tmp/pixel");
        for provider in [Provider::Claude, Provider::Codex] {
            let name = provider.name();
            let value = merged(provider, exe, 3);
            let base =
                format!("'/tmp/pixel' run-hook task-event --provider {name} --event prompt-submit");
            assert_eq!(
                prompt_commands(&value),
                vec![
                    format!("{base} --part 1/3 || true"),
                    format!("{base} --part 2/3 || true"),
                    format!("{base} --part 3/3 || true"),
                ]
            );
            for group in value["hooks"]["UserPromptSubmit"].as_array().unwrap() {
                assert_eq!(group["hooks"].as_array().unwrap().len(), 1);
                assert_eq!(group["hooks"][0]["timeout"], 10);
                assert_eq!(group["hooks"][0]["type"], "command");
            }
            assert_eq!(
                prompt_commands(&merged(provider, exe, 1)),
                vec![format!("{base} || true")]
            );
            assert_eq!(
                prompt_commands(&merged(provider, exe, 0)),
                vec![format!("{base} || true")]
            );
            assert_eq!(
                prompt_commands(&merged(provider, exe, 200)).len(),
                usize::from(config::BRIEF_PARTS_MAX),
                "clamped to the most parts a brief is split over"
            );
            // Every other boundary is one entry, whatever the parts.
            for event in [
                "SessionStart",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "SubagentStop",
            ] {
                assert_eq!(
                    value["hooks"][event].as_array().unwrap().len(),
                    1,
                    "{event}"
                );
            }
        }
        let devin = merged(Provider::Devin, exe, 3);
        assert_eq!(
            prompt_commands(&devin),
            vec!["'/tmp/pixel' run-hook task-event --provider devin --event prompt-submit || true"],
            "a host with no known cap keeps one hook"
        );
        // Only the brief hook asks Devin to show its run on screen; every
        // other Devin event, and every other host, stays silent.
        for (event, groups) in devin["hooks"].as_object().unwrap() {
            assert_eq!(
                groups[0]["hooks"][0].get("show_output"),
                (event == "UserPromptSubmit").then_some(&json!(true)),
                "{event}"
            );
        }
        let claude = merged(Provider::Claude, exe, 1);
        assert_eq!(
            claude["hooks"]["UserPromptSubmit"][0]["hooks"][0].get("show_output"),
            None
        );
        // Devin accepts only these hook events and rejects a `hooks` object
        // that names any other — no Subagent*/Interrupt/PostToolUseFailure key.
        let keys: std::collections::BTreeSet<_> = devin["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "PostToolUse",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "UserPromptSubmit",
            ]
            .into_iter()
            .collect()
        );
        assert!(task_hooks_registered(&devin, Provider::Devin, exe));
        // An install over the events an older release wrote drops the keys
        // it emptied, not just their groups.
        let mut stale = devin.clone();
        for (event, name) in [
            ("SubagentStart", "subagent-start"),
            ("SubagentStop", "subagent-stop"),
            ("Interrupt", "interrupt"),
        ] {
            stale["hooks"][event] = json!([hook_group(
                format!("'/tmp/pixel' run-hook task-event --provider devin --event {name}"),
                None,
            )]);
        }
        merge_task_hooks(
            stale["hooks"].as_object_mut().unwrap(),
            Provider::Devin,
            exe,
            1,
        )
        .unwrap();
        assert_eq!(stale, devin, "stale Devin event keys are removed");
    }

    #[test]
    fn merge_should_replace_a_previous_split_with_the_new_one() {
        let exe = Path::new("/tmp/pixel");
        let mut value = merged(Provider::Claude, exe, 3);
        for parts in [2, 3, 1, 3] {
            merge_task_hooks(
                value["hooks"].as_object_mut().unwrap(),
                Provider::Claude,
                exe,
                parts,
            )
            .unwrap();
            assert_eq!(prompt_commands(&value).len(), usize::from(parts), "{parts}");
            assert_eq!(prompt_parts(&value, Provider::Claude, exe), Ok(parts));
        }
    }

    #[test]
    fn prompt_parts_should_name_what_is_wrong_with_an_incomplete_set() {
        let exe = Path::new("/tmp/pixel");
        let entry = |tail: &str| {
            json!({"hooks":[{"type":"command","command":format!(
                "'/tmp/pixel' run-hook task-event --provider claude --event prompt-submit{tail}"
            )}]})
        };
        let with = |entries: Vec<Value>| json!({"hooks":{"UserPromptSubmit": entries}});
        assert_eq!(
            prompt_parts(&json!({"hooks":{}}), Provider::Claude, exe),
            Err(NO_PROMPT_HOOK.to_owned())
        );
        assert_eq!(
            prompt_parts(&with(vec![entry("")]), Provider::Claude, exe),
            Ok(1)
        );
        let full = with(vec![
            entry(" --part 1/3"),
            entry(" --part 2/3"),
            entry(" --part 3/3"),
        ]);
        assert_eq!(prompt_parts(&full, Provider::Claude, exe), Ok(3));
        assert_eq!(
            prompt_parts(&full, Provider::Codex, exe),
            Err(NO_PROMPT_HOOK.to_owned()),
            "another host's entries are not this host's brief"
        );
        let gap = with(vec![entry(" --part 1/3"), entry(" --part 3/3")]);
        assert_eq!(
            prompt_parts(&gap, Provider::Claude, exe),
            Err("brief part 2/3 missing".to_owned())
        );
        let two_gaps = with(vec![entry(" --part 2/4")]);
        assert_eq!(
            prompt_parts(&two_gaps, Provider::Claude, exe),
            Err("brief part 1/4, 3/4, 4/4 missing".to_owned())
        );
        let mixed = with(vec![entry(""), entry(" --part 2/2")]);
        assert_eq!(
            prompt_parts(&mixed, Provider::Claude, exe),
            Err("the whole brief and brief parts are both registered".to_owned())
        );
        let counts = with(vec![entry(" --part 1/2"), entry(" --part 2/3")]);
        assert_eq!(
            prompt_parts(&counts, Provider::Claude, exe),
            Err("brief parts of different counts are registered".to_owned())
        );
        for incomplete in [&gap, &two_gaps, &mixed, &counts] {
            let mut all = merged(Provider::Claude, exe, 1);
            all["hooks"]["UserPromptSubmit"] = incomplete["hooks"]["UserPromptSubmit"].clone();
            assert!(!task_hooks_registered(&all, Provider::Claude, exe));
        }
        let mut whole = merged(Provider::Claude, exe, 3);
        assert!(task_hooks_registered(&whole, Provider::Claude, exe));
        whole["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        assert!(!task_hooks_registered(&whole, Provider::Claude, exe));
    }

    /// An asynchronous or matcher-restricted part is not a brief that runs on
    /// every prompt.
    #[test]
    fn prompt_parts_should_ignore_asynchronous_and_restricted_entries() {
        let exe = Path::new("/tmp/pixel");
        let mut value = merged(Provider::Claude, exe, 3);
        value["hooks"]["UserPromptSubmit"][1]["hooks"][0]["async"] = json!(true);
        assert_eq!(
            prompt_parts(&value, Provider::Claude, exe),
            Err("brief part 2/3 missing".to_owned())
        );
        let mut value = merged(Provider::Claude, exe, 3);
        value["hooks"]["UserPromptSubmit"][2]["matcher"] = json!("Bash");
        assert_eq!(
            prompt_parts(&value, Provider::Claude, exe),
            Err("brief part 3/3 missing".to_owned())
        );
    }

    /// The tail that makes a prompt hook exit 0 is part of the command, not of
    /// the verb: entries with and without it are the same hook to install,
    /// doctor and uninstall, so an upgrade replaces the old form.
    #[test]
    fn the_fail_open_tail_is_not_part_of_the_verb() {
        let with = "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 2/3 || true";
        let without = "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 2/3";
        let expected = "task-event --provider claude --event prompt-submit --part 2/3";
        for command in [with, without] {
            assert_eq!(
                pixel_hook_verb(command, release()),
                Some(expected),
                "{command}"
            );
            assert_eq!(
                pixel_run_hook_verb(command, release()),
                Some(expected),
                "{command}"
            );
            assert!(is_pixel_hook(command, release()), "{command}");
        }
        // Only the exact tail, only at the end, and not for a foreign command.
        for foreign in [
            "'/opt/other' run-hook task-event --provider claude --event prompt-submit || true",
            "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit || true || true",
            "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit ||true",
            "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit; true",
        ] {
            assert!(!is_pixel_hook(foreign, release()), "{foreign}");
        }
        // Old whole-brief entries upgrade in place.
        let mut value = json!({"hooks": {"UserPromptSubmit": [
            {"hooks": [{"type": "command", "command": "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit"}]},
            {"hooks": [{"type": "command", "command": "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 1/2"}]},
            {"hooks": [{"type": "command", "command": "other-tool"}]},
        ]}});
        merge_task_hooks(
            value["hooks"].as_object_mut().unwrap(),
            Provider::Claude,
            release(),
            3,
        )
        .unwrap();
        let commands = prompt_commands(&value);
        assert_eq!(commands.len(), 4, "{commands:?}");
        assert_eq!(
            commands[3], "other-tool",
            "the user's group keeps its index"
        );
        assert!(
            commands[..3]
                .iter()
                .all(|c| c.ends_with(" --part 1/3 || true")
                    || c.ends_with(" --part 2/3 || true")
                    || c.ends_with(" --part 3/3 || true"))
        );
        assert_eq!(prompt_parts(&value, Provider::Claude, release()), Ok(3));
        // Remove takes every form.
        remove_pixel_hooks(value["hooks"].as_object_mut().unwrap(), release());
        assert_eq!(prompt_commands(&value), vec!["other-tool"]);
    }

    /// Every boundary carries the tail, not only the prompt: an entry written
    /// before it is replaced where it stood, never left beside the new one.
    #[test]
    fn rerunning_setup_should_replace_every_old_form_entry_in_place() {
        let old = |event: &str| {
            format!("'/usr/local/bin/pixel' run-hook task-event --provider claude --event {event}")
        };
        let mut value = json!({"hooks": {
            "Stop": [
                {"hooks": [{"type": "command", "command": "other-tool"}]},
                {"hooks": [{"type": "command", "command": old("stop")}]},
            ],
            "PreToolUse": [{"hooks": [{"type": "command", "command": old("pre-tool-use")}]}],
        }});
        merge_task_hooks(
            value["hooks"].as_object_mut().unwrap(),
            Provider::Claude,
            release(),
            1,
        )
        .unwrap();
        let commands = |event: &str| -> Vec<String> {
            value["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .map(|group| group["hooks"][0]["command"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            commands("Stop"),
            vec!["other-tool".to_owned(), fail_open(&old("stop"))]
        );
        assert_eq!(
            commands("PreToolUse"),
            vec![fail_open(&old("pre-tool-use"))]
        );
        for event in value["hooks"].as_object().unwrap().keys() {
            for command in commands(event) {
                assert!(
                    command == "other-tool" || command.ends_with(FAIL_OPEN),
                    "{event}: {command}"
                );
            }
        }
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    #[test]
    fn alternate_hook_binaries_should_require_both_difference_and_first_occurrence() {
        let own = Path::new("/missing/own/pixel");
        let value = serde_json::json!({"hooks":{"Start":[{"hooks":[
            {"command":"'/missing/own/pixel' run-hook metrics --provider claude"},
            {"command":"'/missing/other/pixel' run-hook metrics --provider claude"},
            {"command":"'/missing/other/pixel' run-hook task-event --provider claude --event session-start"}
        ]}]}});
        assert_eq!(
            pixel_hooks_running_other_binaries(&value, own),
            [PathBuf::from("/missing/other/pixel")]
        );
    }
}
