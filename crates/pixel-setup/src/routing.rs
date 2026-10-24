// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The hook registrations PIXEL writes into the agents whose settings share
//! Claude Code's schema (Claude Code, Codex, Devin): one command per event,
//! `'<exe>' hook <event> --host <host>[ --part K/N] || true`
//! (`specs/hosts.md`). Installation is not proof that a host has fired the
//! hooks; doctor reports that boundary separately.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::hosts::{Event, Host};
use crate::{InstallError, config, install};

/// The events registered as hooks, in the order they are written. The edit
/// events join them when edit hooks are switched on.
pub(crate) const REGISTERED: [Event; 4] = [
    Event::SessionStart,
    Event::PromptSubmit,
    Event::PostTool,
    Event::Stop,
];

/// The command that runs `event` for `host`: `'<exe>' hook <event> --host
/// <host>[ --part K/N] || true`.
pub(crate) fn hook_command(exe: &Path, event: Event, host: Host, part: Option<(u8, u8)>) -> String {
    let part = part.map_or_else(String::new, |(k, n)| format!(" --part {k}/{n}"));
    format!(
        "{} hook {} --host {}{part}{FAIL_OPEN}",
        quoted_executable(exe),
        event.as_str(),
        host.as_str()
    )
}

/// The timeout in seconds registered for `event`: the brief waits on the
/// daemon, the rest never do.
pub(crate) fn hook_timeout(event: Event) -> u64 {
    if event == Event::PromptSubmit {
        config::HOOK_TIMEOUT
    } else {
        config::QUICK_HOOK_TIMEOUT
    }
}

/// The settings file of `host` under `home`; `None` for a host whose
/// registrations are not in the Claude schema.
pub(crate) fn settings_path(host: Host, home: &Path) -> Option<PathBuf> {
    Some(home.join(match host {
        Host::Claude => ".claude/settings.json",
        Host::Codex => config::CODEX_HOOKS_FILE,
        Host::Devin => ".config/devin/config.json",
        _ => return None,
    }))
}

/// Ends a hook command so that it exits 0 whatever the binary does. Claude
/// Code blocks the user's prompt when a `UserPromptSubmit` hook exits 2, and an
/// older `pixel` exits 2 on a flag it does not know (`--part`), so every command
/// runs through the shell (every host runs hook commands in one) with this
/// tail. Recognition accepts the command with or without it, so entries of
/// either form are replaced, counted and removed alike.
pub(crate) const FAIL_OPEN: &str = " || true";

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
pub(crate) fn unquoted_executable(executable: &str) -> Option<PathBuf> {
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

/// A `hook` verb taken apart: the event and host words and, for a split
/// prompt-submit brief, the `--part K/N` it serves.
struct HookVerb<'a> {
    event: &'a str,
    host: &'a str,
    part: Option<(u8, u8)>,
}

/// `K/N` of a `--part` flag: `1 <= K <= N <= PARTS_MAX`, and `N >= 2` (a single
/// part is spelled without the flag).
fn parse_part(text: &str) -> Option<(u8, u8)> {
    let (k, n) = text.split_once('/')?;
    let (k, n) = (k.parse::<u8>().ok()?, n.parse::<u8>().ok()?);
    ((2..=config::BRIEF_PARTS_MAX).contains(&n) && (1..=n).contains(&k)).then_some((k, n))
}

fn parse_verb(verb: &str) -> Option<HookVerb<'_>> {
    let words: Vec<_> = verb.split_whitespace().collect();
    let (event, host, part) = match words.as_slice() {
        [event, "--host", host] => (*event, *host, None),
        [event, "--host", host, "--part", part] if *event == "prompt-submit" => {
            (*event, *host, Some(parse_part(part)?))
        }
        _ => return None,
    };
    (host.parse::<Host>().is_ok() && event.parse::<Event>().is_ok()).then_some(HookVerb {
        event,
        host,
        part,
    })
}

/// Whether `verb` is a hook verb: `<event> --host <host>`, or for a split
/// prompt-submit brief `prompt-submit --host <host> --part K/N`.
fn is_hook_verb(verb: &str) -> bool {
    parse_verb(verb).is_some()
}

/// What makes `command` one of PIXEL's hooks, or `None` for a foreign one: its
/// `hook` verb (`prompt-submit --host claude --part 1/3`).
///
/// A `hook` command is PIXEL's when its executable carries one of the names PIXEL
/// installs itself under ([`config::PIXEL_EXECUTABLES`]: `pixel` for a release,
/// `pixel-dev` for a side build) or the file name of `exe`, the binary
/// installing or uninstalling now. Without them a build under another name
/// never recognises the entries it wrote, nor a release the dev build's: each
/// install appends a new set beside the old one and uninstall leaves them all.
/// Only those exact names count, never any name merely containing `pixel`.
pub(crate) fn pixel_hook_verb<'a>(command: &'a str, exe: &Path) -> Option<&'a str> {
    let own = exe.file_name().map(|n| n.to_string_lossy());
    command
        .rsplit_once(" hook ")
        .map(without_fail_open)
        .filter(|(executable, verb)| {
            executable_name(executable).is_some_and(|name| {
                config::PIXEL_EXECUTABLES.contains(&name.as_str())
                    || own.as_deref() == Some(name.as_str())
            }) && is_hook_verb(verb)
        })
        .map(|(_, verb)| verb)
}

/// Recognize our executable commands, not arbitrary commands merely
/// containing a lifecycle verb. See [`pixel_hook_verb`] for which executables
/// count as PIXEL's.
pub(crate) fn is_pixel_hook(command: &str, exe: &Path) -> bool {
    pixel_hook_verb(command, exe).is_some()
}

/// Replaces only PIXEL's registrations, preserving foreign hooks and trust.
///
/// `prompt_parts` is how many `UserPromptSubmit` entries carry the brief for a
/// host with a per-hook cap on injected context (Claude Code and Codex; every
/// other host keeps one): each is the same command with `--part K/N`, so a
/// brief larger than one hook may inject still arrives.
pub(crate) fn merge_hooks(
    hooks: &mut Map<String, Value>,
    host: Host,
    exe: &Path,
    prompt_parts: u8,
) -> Result<(), String> {
    // Codex keys a hook's trust by its group index, so PIXEL's new groups go
    // where its old ones were, not after the user's.
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
                        .is_some_and(|command| is_pixel_hook(command, exe))
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
    let split = if matches!(host, Host::Claude | Host::Codex) {
        prompt_parts.clamp(1, config::BRIEF_PARTS_MAX)
    } else {
        1
    };
    for event in REGISTERED {
        let Some(host_event) = host.registration(event) else {
            continue;
        };
        let groups = hooks
            .entry(host_event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{host_event} is not an array"))?;
        let commands = if event == Event::PromptSubmit && split > 1 {
            (1..=split)
                .map(|part| hook_command(exe, event, host, Some((part, split))))
                .collect()
        } else {
            vec![hook_command(exe, event, host, None)]
        };
        let mut fresh = Vec::new();
        for command in commands {
            let mut group = hook_group(command, None);
            group["hooks"][0]["timeout"] = json!(hook_timeout(event));
            if host == Host::Devin && event == Event::PromptSubmit {
                // Devin draws neither `systemMessage` nor a hook's stdout or
                // stderr; `show_output` makes it print `✱ Hook completed:
                // <command>`, the only on-screen trace that the brief ran.
                group["hooks"][0]["show_output"] = json!(true);
            }
            fresh.push(group);
        }
        let at = slots
            .get(host_event)
            .copied()
            .unwrap_or(groups.len())
            .min(groups.len());
        groups.splice(at..at, fresh);
    }
    Ok(())
}

/// The verbs of PIXEL's hooks for `event` that run on every occurrence of it:
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

/// How many `UserPromptSubmit` entries carry `host`'s brief: `1` for the
/// whole brief in one hook, `N` for a complete set `--part 1/N` .. `N/N`.
///
/// # Errors
///
/// No entry, whole and split entries mixed, parts of different counts, or a
/// part of the set missing; the text names which, for `pixel doctor`.
pub(crate) fn prompt_parts(value: &Value, host: Host, exe: &Path) -> Result<u8, String> {
    let entries: Vec<Option<(u8, u8)>> = eligible_verbs(value, "UserPromptSubmit", exe)
        .into_iter()
        .filter_map(parse_verb)
        .filter(|verb| verb.host == host.as_str() && verb.event == "prompt-submit")
        .map(|verb| verb.part)
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

/// Whether every event PIXEL registers for `host` is registered in `value`.
pub(crate) fn hooks_registered(value: &Value, host: Host, exe: &Path) -> bool {
    REGISTERED.iter().all(|&event| {
        let Some(host_event) = host.registration(event) else {
            return true;
        };
        if event == Event::PromptSubmit {
            return prompt_parts(value, host, exe).is_ok();
        }
        let expected = format!("{} --host {}", event.as_str(), host.as_str());
        eligible_verbs(value, host_event, exe).contains(&expected.as_str())
    })
}

/// Preserve outer matchers and co-located foreign hooks. Every entry
/// [`is_pixel_hook`] recognises goes, so one pass also collapses entries an
/// earlier install stacked.
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

fn hook_group(command: String, matcher: Option<&str>) -> Value {
    let mut group =
        json!({"hooks":[{"type":"command","command":command,"timeout":config::HOOK_TIMEOUT}]});
    if let Some(matcher) = matcher {
        group["matcher"] = matcher.into();
    }
    group
}

/// Register the hooks of `host` in the settings file `path`, replacing only
/// PIXEL's own entries and keeping every foreign one. `prompt_parts` is how many hooks carry
/// the prompt brief (see [`merge_hooks`]).
pub(crate) fn install_hooks(
    path: &Path,
    exe: &Path,
    host: Host,
    prompt_parts: u8,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let mut value = install::read_settings(path)?;
    configure_hooks(&mut value, host, exe, prompt_parts).map_err(|reason| {
        InstallError::InvalidSettings {
            path: path.into(),
            reason,
        }
    })?;
    let backup = install::write_settings(path, &value, dry_run)?;
    Ok(install::InstallStep {
        id: format!("hooks.{}", host.as_str()),
        status: install::CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &format!("{} hooks configured (live unverified)", host.as_str()),
        ),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

/// The pure transform behind [`install_hooks`].
fn configure_hooks(
    value: &mut Value,
    host: Host,
    exe: &Path,
    prompt_parts: u8,
) -> Result<(), String> {
    let root = value
        .as_object_mut()
        .ok_or("settings root is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks is not an object")?;
    merge_hooks(hooks, host, exe, prompt_parts)
}

/// Every hook command of `groups`, in order.
fn commands(groups: &[Value]) -> impl Iterator<Item = &str> {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
}

/// Hooks a settings value registers more than once for the same event and
/// verb, as `Event→verb ×n` lines: the trace of installs that appended their
/// entries beside earlier ones instead of replacing them, so the harness runs
/// that hook `n` times.
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

/// The executables PIXEL's `hook` entries in a settings value run that are not
/// `exe`, each once, in the order met. A side-build setup leaves every hook on
/// the side build and a package-manager upgrade can leave them on the previous
/// version's path; either way the hooks are complete and counted once, so
/// only the executable tells the managed binary that its sessions are not
/// running it. Two paths name the same binary when they canonicalize to one
/// file (a symlink, a shim-less PATH entry).
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
            .rsplit_once(" hook ")
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
mod tests;
