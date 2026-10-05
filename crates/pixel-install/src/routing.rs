// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Provider-specific hook configuration. Installation is not proof that a
//! harness has fired the hooks; doctor reports that boundary separately.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

use crate::{InstallError, config, install};

pub(crate) const RTK_BACKUP: &str = ".claude/pixel-rtk-hooks.json";
/// Claude Code's team-shared project settings, usually committed.
pub(crate) const CLAUDE_SHARED_SETTINGS: &str = ".claude/settings.json";
/// Claude Code's personal per-project settings (gitignored by its convention):
/// the repo guard carries this machine's binary path, so it lives here.
pub(crate) const CLAUDE_LOCAL_SETTINGS: &str = ".claude/settings.local.json";
/// Devin CLI's personal per-project config; its `hooks` key is read like
/// `.devin/hooks.v1.json`, but it is not shared with the team.
pub(crate) const DEVIN_LOCAL_CONFIG: &str = ".devin/config.local.json";
/// Where an earlier `pixel install --repo` wrote the Devin guard. Devin CLI
/// never reads this file; install and uninstall take pixel's entry out of it.
pub(crate) const DEVIN_LEGACY_HOOKS: &str = ".devin/hooks.json";
/// Project-local snapshot of Codex `PreToolUse` groups adopted by the composed
/// guard.  It deliberately lives next to the project hook config so a runtime
/// never has to discover or execute the currently mutable hook configuration.
pub(crate) const CODEX_COMPOSED_BACKUP: &str = "pixel-composed-guard-backup.json";
#[cfg(test)]
const CODEX_COMPOSED_BACKUP_VERSION: u64 = 1;

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

    /// Matchers for legacy routing configurations and the remaining Devin
    /// integration. Native-default Claude and Codex install paths never add
    /// these retrieval guards.
    fn shell_matcher(self) -> &'static str {
        match self {
            Self::Codex => "Bash|shell|unified_exec|local_shell",
            // Devin's native read/search tools bypass exec rewrites and must
            // reach the guard so it can return its documented block response.
            Self::Devin => "exec|read|grep|glob|find_file_by_name",
            Self::Claude => "Bash|Read|Grep",
        }
    }

    pub(crate) fn compact(self) -> &'static str {
        if self == Self::Devin {
            "PostCompaction"
        } else {
            "SessionStart"
        }
    }
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
/// the `run-hook` verb (`session-start`, `guard --provider claude`, ...) or,
/// for an install older than `run-hook`, the script's file name.
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
    // Installs before `pixel run-hook` registered standalone scripts under
    // `~/.claude/hooks/`. An install that does not recognise them keeps the
    // script entry next to the new `run-hook` one: two SessionStart hooks.
    if let Some(script) = executable_name(command).and_then(|name| {
        [
            config::GUARD_HOOK,
            config::OLD_GUARD_HOOK,
            config::SESSION_START_HOOK,
            config::PROMPT_SUBMIT_HOOK,
            config::POST_COMPACTION_HOOK,
        ]
        .into_iter()
        .find(|script| *script == name)
    }) {
        return Some(script);
    }
    let own = exe.file_name().map(|n| n.to_string_lossy());
    // Entries written before the command rename say `pixel hook <verb>`;
    // both spellings are pixel's and both must be recognised so an upgrade
    // replaces the old entry instead of stacking a second one next to it.
    command
        .rsplit_once(" run-hook ")
        .or_else(|| command.rsplit_once(" hook "))
        .filter(|(executable, verb)| {
            executable_name(executable).is_some_and(|name| {
                config::PIXEL_EXECUTABLES.contains(&name.as_str())
                    || own.as_deref() == Some(name.as_str())
            }) && ([
                "guard",
                "guard --provider claude",
                "guard --provider codex",
                "guard --provider devin",
                "guard --provider zcode",
                "guard --provider cursor",
                "guard --provider claude --delegate-rtk",
                "composed-guard --provider codex",
                "session-start",
                "session-start --provider claude",
                "session-start --provider codex",
                "session-start --provider devin",
                "prompt-submit",
                "prompt-submit --provider claude",
                "prompt-submit --provider codex",
                "prompt-submit --provider devin",
                "post-compaction",
                "post-compaction --provider claude",
                "post-tool-use",
                "post-tool-use --provider claude",
                "metrics --provider codex",
                "metrics --provider claude",
                "metrics --provider devin",
                "metrics --provider cursor",
            ]
            .contains(verb)
                || task_hook_verb(verb)
                || verb
                    .strip_prefix("composed-guard --provider codex --backup ")
                    .is_some_and(|backup| !backup.is_empty()))
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
        .filter(|(executable, _)| {
            executable_name(executable).is_some_and(|name| {
                config::PIXEL_EXECUTABLES.contains(&name.as_str())
                    || own.as_deref() == Some(name.as_str())
            })
        })
        .map(|(_, verb)| verb)
}

/// Recognize our executable commands and legacy script names, not arbitrary
/// commands merely containing a lifecycle verb. See [`pixel_hook_verb`] for
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

fn task_hook_verb(verb: &str) -> bool {
    let words: Vec<_> = verb.split_whitespace().collect();
    matches!(words.as_slice(), ["task-event", "--provider", "claude" | "codex" | "pi", "--event", event]
        if TASK_HOOK_EVENTS.iter().any(|(_, name)| name == event)
            || matches!(*event, "tool-failure" | "interrupt" | "model-response" | "user-bash"))
}

/// Replace only Pixel task registrations, preserving foreign hooks and trust.
pub(crate) fn merge_task_hooks(
    hooks: &mut Map<String, Value>,
    provider: Provider,
    exe: &Path,
) -> Result<(), String> {
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        for group in groups.iter_mut() {
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                inner.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(Value::as_str)
                        .and_then(|command| pixel_hook_verb(command, exe))
                        .is_some_and(task_hook_verb)
                });
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|inner| !inner.is_empty())
        });
    }
    let extra = if provider == Provider::Claude {
        ("PostToolUseFailure", "tool-failure")
    } else {
        ("Interrupt", "interrupt")
    };
    for &(event, name) in TASK_HOOK_EVENTS.iter().chain(std::iter::once(&extra)) {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{event} is not an array"))?;
        let mut group = hook_group(
            format!(
                "{} run-hook task-event --provider {} --event {name}",
                quoted_executable(exe),
                provider.name()
            ),
            None,
        );
        group["hooks"][0]["timeout"] = json!(if matches!(event, "SessionEnd" | "Interrupt") {
            3
        } else {
            10
        });
        groups.push(group);
    }
    Ok(())
}

pub(crate) fn task_hooks_registered(value: &Value, provider: Provider, exe: &Path) -> bool {
    let extra = if provider == Provider::Claude {
        ("PostToolUseFailure", "tool-failure")
    } else {
        ("Interrupt", "interrupt")
    };
    TASK_HOOK_EVENTS
        .iter()
        .chain(std::iter::once(&extra))
        .all(|(event, name)| {
            let expected = format!("task-event --provider {} --event {name}", provider.name());
            value
                .get("hooks")
                .and_then(|hooks| hooks.get(*event))
                .and_then(Value::as_array)
                .is_some_and(|groups| {
                    groups.iter().any(|group| {
                        group
                            .get("matcher")
                            .and_then(Value::as_str)
                            .is_none_or(|matcher| matches!(matcher, "" | "*" | ".*"))
                            && group
                                .get("hooks")
                                .and_then(Value::as_array)
                                .is_some_and(|inner| {
                                    inner.iter().any(|hook| {
                                        hook.get("async").and_then(Value::as_bool) != Some(true)
                                            && hook
                                                .get("command")
                                                .and_then(Value::as_str)
                                                .and_then(|command| pixel_hook_verb(command, exe))
                                                == Some(expected.as_str())
                                    })
                                })
                    })
                })
        })
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

/// Remove Pixel's flat-schema hook entries (Cursor's `hooks.<event>` arrays,
/// where every entry carries its `command` directly with no nested `hooks`
/// sub-array), matching by executable ownership via [`is_pixel_hook`] rather
/// than a command substring. A foreign command whose text merely contains a
/// pixel verb (say a tool named `run-hook guard-stats`) survives untouched.
/// Deleted events drop out of the map; events left with a foreign entry stay.
pub(crate) fn remove_flat_pixel_hooks(hooks: &mut Map<String, Value>, exe: &Path) {
    hooks.retain(|_, entries| {
        let Some(entries) = entries.as_array_mut() else {
            return true;
        };
        entries.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| is_pixel_hook(c, exe))
        });
        !entries.is_empty()
    });
}

fn anchored_literal(name: &str) -> Option<&str> {
    let inner = name
        .strip_prefix('^')?
        .strip_suffix('$')
        .unwrap_or(name.strip_prefix('^')?);
    (!inner.is_empty()
        && inner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(inner)
}

fn shell_overlap(group: &Value, provider: Provider) -> bool {
    let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("");
    let names: Vec<&str> = matcher.split('|').collect();
    let matches_shell = names.iter().copied().any(|name| match provider {
        Provider::Codex => matches!(name, "Bash" | "shell" | "unified_exec" | "local_shell"),
        _ => name == provider.shell(),
    });
    if matches_shell || matches!(matcher, "" | "*" | ".*") {
        return true;
    }
    // A matcher made solely of anchored literal tool-name prefixes is provably
    // disjoint when none can match a current shell tool. This matters for
    // project-local Codex hooks: they shadow global hooks even when they only
    // guard `apply_patch` or a specific MCP edit tool.
    if names.iter().all(|name| anchored_literal(name).is_some()) {
        return names.iter().copied().any(|name| match provider {
            Provider::Codex => matches!(
                anchored_literal(name),
                Some("Bash" | "shell" | "unified_exec" | "local_shell")
            ),
            _ => anchored_literal(name) == Some(provider.shell()),
        });
    }
    // Unknown regexes may match the shell too. Only exact known non-shell
    // alternatives can be ruled out without guessing another engine's regex.
    !matcher.split('|').all(|name| {
        [
            "Read",
            "Grep",
            "Glob",
            "Edit",
            "Write",
            "MultiEdit",
            "NotebookEdit",
            "read",
            "grep",
            "glob",
            "edit",
            "write",
            "apply_patch",
        ]
        .contains(&name)
    })
}

/// CMUX's generated Codex pre-tool feed is an observer: it consumes stdin and
/// returns `{}` when no surface is attached. It has no `updatedInput` or
/// permission decision, so it cannot compete with Pixel's compatibility
/// rewrite. This exact, single-command shape is intentionally the only
/// overlap that may coexist; every unknown command remains a routing blocker.
fn passive_cmux_codex_feed(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Codex
        || !group
            .get("matcher")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                command.ends_with("/cmux-codex-hook-persistent-feed-PreToolUse.sh")
            })
}

/// Orca's generated wrapper is an observer only while its provider-specific
/// target script is absent: its explicit `else` branch consumes stdin and
/// emits no hook response. If Orca is later installed, this returns false on
/// the next install/doctor pass and Pixel defers rather than racing a newly
/// active hook.
fn inactive_orca_observer(group: &Value, provider: Provider) -> bool {
    if !matches!(provider, Provider::Claude | Provider::Codex)
        || group.get("matcher").and_then(Value::as_str).unwrap_or("")
            != if provider == Provider::Claude {
                "*"
            } else {
                ""
            }
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    let Some(command) = (hooks.len() == 1)
        .then(|| hooks[0].get("command").and_then(Value::as_str))
        .flatten()
    else {
        return false;
    };
    let Some(path) = command
        .strip_prefix("if [ -f '")
        .and_then(|rest| rest.split_once("' "))
        .map(|(path, _)| path)
    else {
        return false;
    };
    path.ends_with(&format!("/.orca/agent-hooks/{}-hook.sh", provider.name()))
        && command.contains("else { command -p cat")
        && !Path::new(path).is_file()
}

/// The one RTK registration pixel adopts: `rtk hook claude` on `Bash`.
fn rtk_group() -> Value {
    json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]})
}

fn exact_rtk(group: &Value) -> bool {
    group == &rtk_group()
}

/// Vibe Island's generated Claude bridge is a passive observer for the
/// compatible Bash subset. The exact wrapper exits successfully when the
/// bridge is absent; when present, its current PreToolUse probe produces no
/// response, so it cannot compete with Pixel's `updatedInput` rewrite. Keep
/// this deliberately narrow: any additional handler or a different command is
/// an unknown mutator and must still block routing.
fn passive_vibe_claude_bridge(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Claude || group.get("matcher").and_then(Value::as_str) != Some("*") {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                command.contains(".vibe-island/bin/vibe-island-bridge")
                    && command.contains("--source claude")
                    && command.contains("&&")
                    && command.ends_with("exit 0'")
            })
}

/// GitNexus's generated hook augments Claude context only. Its implementation
/// writes `hookSpecificOutput.additionalContext`; it never returns
/// `updatedInput` or a permission decision. Its graph context can therefore be
/// merged with Pixel's deterministic compatibility rewrite.
fn passive_gitnexus_claude_hook(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Claude
        || group.get("matcher").and_then(Value::as_str) != Some("Grep|Glob|Bash")
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.ends_with("/.claude/hooks/gitnexus/gitnexus-hook.cjs\""))
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
        format!(
            "{} run-hook metrics --provider {}",
            quoted_executable(exe),
            provider.name()
        ),
        Some(provider.shell()),
    )
}

/// Whether `value` registers Pixel's metrics relay for `provider` under
/// PostToolUse with that provider's shell matcher.
pub(crate) fn has_pixel_metrics_relay(value: &Value, provider: Provider, exe: &Path) -> bool {
    let verb = format!("run-hook metrics --provider {}", provider.name());
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("PostToolUse"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| group.get("matcher").and_then(Value::as_str) == Some(provider.shell()))
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| is_pixel_hook(command, exe) && command.contains(&verb))
}

fn hook_group(command: String, matcher: Option<&str>) -> Value {
    let mut group =
        json!({"hooks":[{"type":"command","command":command,"timeout":config::HOOK_TIMEOUT}]});
    if let Some(matcher) = matcher {
        group["matcher"] = matcher.into();
    }
    group
}

pub(crate) fn has_delegate(hooks: &Map<String, Value>, exe: &Path) -> bool {
    hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|inner| {
                        inner.iter().any(|hook| {
                            hook.get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|command| {
                                    is_pixel_hook(command, exe)
                                        && command.contains(" --delegate-rtk")
                                })
                        })
                    })
            })
        })
}

pub(crate) fn restore_rtk(hooks: &mut Map<String, Value>, saved: &[Value]) {
    let groups = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
    if let Some(groups) = groups.as_array_mut() {
        for group in saved {
            if !groups.contains(group) {
                groups.push(group.clone());
            }
        }
    }
}

pub(crate) fn load_rtk_backup(home: &Path) -> crate::Result<Vec<Value>> {
    let path = home.join(RTK_BACKUP);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let saved: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if !saved.iter().all(exact_rtk) {
        return Err(InstallError::InvalidSettings {
            path: home.join(RTK_BACKUP),
            reason: "unrecognized RTK backup; refusing to restore commands".into(),
        });
    }
    Ok(saved)
}

/// The RTK backup under `home` when no pixel guard in
/// `~/.claude/settings.json` delegates to it: a leftover that `pixel install`
/// never applies, such as one an `install --repo` build wrote under `$HOME`
/// before the repository backup moved into the repository.
pub(crate) fn orphan_rtk_backup(home: &Path, exe: &Path) -> Option<PathBuf> {
    let backup = home.join(RTK_BACKUP);
    let in_use = BACKUP_READERS
        .iter()
        .any(|rel| delegates_rtk(&home.join(rel), exe));
    (backup.is_file() && !in_use).then_some(backup)
}

/// The settings files, relative to a backup root, whose delegate guard reads
/// `<root>/.claude/pixel-rtk-hooks.json`. Under `$HOME` both exist when the
/// repository is `$HOME`: the global file is also its shared one, and the
/// repo guard sits in `settings.local.json`.
const BACKUP_READERS: [&str; 2] = [CLAUDE_SHARED_SETTINGS, CLAUDE_LOCAL_SETTINGS];

/// Whether the settings at `path` hold a delegate guard. A file that cannot
/// be read may hold one, so it counts as delegating: the backup is kept.
fn delegates_rtk(path: &Path, exe: &Path) -> bool {
    install::read_settings(path).map_or(true, |value| {
        value
            .get("hooks")
            .and_then(Value::as_object)
            .is_some_and(|hooks| has_delegate(hooks, exe))
    })
}

/// Separate native task events, legacy retrieval callbacks, and cleanup-only
/// migrations so retiring retrieval never retires independent task gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookScope {
    /// Lifecycle + PreToolUse shell routing (the historical full install).
    #[cfg(test)]
    All,
    /// SessionStart/UserPromptSubmit/PostToolUse/compaction only — no
    /// PreToolUse. Used by integrations retaining the legacy prompt profile.
    LifecycleOnly,
    /// Task-event lifecycle only. Used for native-default global Claude and
    /// installs; retrieval prompts and metrics stay out.
    TaskEventsOnly,
    /// PreToolUse guard only — no lifecycle events. Retained for Devin's
    /// repo-local routing and legacy migration fixtures.
    GuardOnly,
    /// Remove Pixel retrieval callbacks and restore any adopted RTK
    /// registration, without registering replacement callbacks.
    NativeCleanup,
}

/// Apply the pure configuration transform and return (routing enabled, RTK
/// fragments adopted). Unknown overlapping hooks remain untouched.
#[cfg(test)]
fn configure(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    saved: &[Value],
) -> Result<(bool, Vec<Value>), String> {
    configure_scoped(value, provider, exe, saved, HookScope::All, &[])
}

/// A PreToolUse group that can run beside Pixel's guard: it cannot touch a
/// shell call, or it is one of the known observers that never rewrite one.
fn coexists_with_guard(group: &Value, provider: Provider, exe: &Path) -> bool {
    !shell_overlap(group, provider)
        // Task gates can deny but never rewrite the tool's input.
        || group.get("hooks").and_then(Value::as_array).is_some_and(|hooks| {
            !hooks.is_empty() && hooks.iter().all(|hook| {
                hook.get("command").and_then(Value::as_str)
                    .and_then(|command| pixel_hook_verb(command, exe))
                    == Some(format!("task-event --provider {} --event pre-tool-use", provider.name()).as_str())
            })
        })
        || passive_vibe_claude_bridge(group, provider)
        || passive_gitnexus_claude_hook(group, provider)
        || passive_cmux_codex_feed(group, provider)
        || inactive_orca_observer(group, provider)
}

/// The `--provider` suffix on a lifecycle hook command line. Claude's task
/// runtime is session-scoped, so only Claude carries the choice on every
/// lifecycle verb; a Devin prompt-submit must carry its provider to render
/// the Pixel-first guidance instead of the provider-neutral context.
#[cfg_attr(test, mutants::skip)]
// reason: the Devin prompt-submit guard is the only non-Claude provider that
// reaches that arm (Codex prompt-submit is `continue`d before this call), so
// a `provider == Provider::Devin -> true` mutation renders identical command
// lines for every `Provider` enum value — no behavioral test can distinguish it.
fn lifecycle_provider_arg(verb: &str, provider: Provider) -> &'static str {
    match verb {
        "session-start" => match provider {
            Provider::Claude => " --provider claude",
            Provider::Codex => " --provider codex",
            Provider::Devin => " --provider devin",
        },
        "prompt-submit" | "post-compaction" | "post-tool-use" if provider == Provider::Claude => {
            " --provider claude"
        }
        "prompt-submit" if provider == Provider::Devin => " --provider devin",
        _ => "",
    }
}

/// `inherited` holds the PreToolUse groups another settings file contributes
/// to the same session (the shared project file when the guard goes into the
/// personal one). The harness merges them, so an unknown shell rewriter there
/// blocks the guard as surely as one in `value`; an exact RTK group there
/// blocks too, since it cannot be adopted from a file this call does not own.
fn configure_scoped(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    saved: &[Value],
    scope: HookScope,
    inherited: &[Value],
) -> Result<(bool, Vec<Value>), String> {
    let root = value
        .as_object_mut()
        .ok_or("settings root is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks is not an object")?;
    let delegated = has_delegate(hooks, exe);
    if delegated && saved.is_empty() {
        return Err("RTK delegate backup missing; refusing to lose its registration".into());
    }
    if scope == HookScope::NativeCleanup {
        remove_matching_hooks(hooks, |command| {
            pixel_hook_verb(command, exe).is_some_and(|verb| !task_hook_verb(verb))
        });
    } else {
        remove_pixel_hooks(hooks, exe);
    }
    if delegated {
        // The delegate guard that ran RTK is gone: put RTK back where it was,
        // whatever the scope. Without a delegate the backup is only a record
        // of an earlier state and is not applied (the user may have removed
        // RTK since); `install_at_scoped` retires it.
        restore_rtk(hooks, saved);
    }
    let mut enabled = true;
    let mut adopted = Vec::new();
    let installs_guard = scope == HookScope::GuardOnly;
    #[cfg(test)]
    let installs_guard = installs_guard || scope == HookScope::All;
    if installs_guard {
        let pre = hooks
            .entry("PreToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("PreToolUse is not an array")?;
        let blocked = pre.iter().any(|group| {
            !(coexists_with_guard(group, provider, exe)
                || provider == Provider::Claude && exact_rtk(group))
        }) || inherited
            .iter()
            .any(|group| !coexists_with_guard(group, provider, exe));
        if !blocked {
            if provider == Provider::Claude {
                pre.retain(|group| {
                    if exact_rtk(group) {
                        adopted.push(group.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            let delegate = if adopted.is_empty() {
                ""
            } else {
                " --delegate-rtk"
            };
            pre.push(hook_group(
                format!(
                    "{} run-hook guard --provider {}{delegate}",
                    quoted_executable(exe),
                    provider.name()
                ),
                Some(provider.shell_matcher()),
            ));
        }
        enabled = !blocked;
    }
    if scope == HookScope::GuardOnly {
        return Ok((enabled, adopted));
    }
    if scope == HookScope::TaskEventsOnly {
        merge_task_hooks(hooks, provider, exe)?;
        return Ok((enabled, adopted));
    }
    if scope == HookScope::NativeCleanup {
        return Ok((enabled, adopted));
    }
    for (event, verb, matcher) in [
        (
            "PostToolUse",
            "post-tool-use",
            if provider == Provider::Claude {
                Some("Edit")
            } else {
                None
            },
        ),
        ("SessionStart", "session-start", None),
        ("UserPromptSubmit", "prompt-submit", None),
        (
            provider.compact(),
            "post-compaction",
            if provider == Provider::Devin {
                None
            } else {
                Some("compact")
            },
        ),
    ] {
        // Devin's post-tool-use behavior (the metrics relay) is repo-scoped
        // (`install_project_devin_at`): a global one would run beside the
        // repo-local one in every installed repository and double the relay.
        // The post-edit advisory is provider-neutral and has no Devin entry
        // of its own, so a global one would be the only double.
        if provider == Provider::Devin && verb == "post-tool-use" {
            continue;
        }
        // Codex's prompt-submit guidance is the global install's
        // (`install_metrics_hook` owns every `$CODEX_HOME/hooks.json` entry):
        // Codex combines matching global and project hooks, so a repo-local
        // copy would deliver the same guidance twice per prompt in every
        // installed repository — the same doubled-relay shape Devin avoids.
        if provider == Provider::Codex && verb == "prompt-submit" {
            continue;
        }
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{event} is not an array"))?;
        // Claude's task runtime is session-scoped. Make that provider choice
        // explicit at the lifecycle boundary. Session-start and prompt-submit
        // carry the provider for every host: Codex rejects unknown output
        // fields, and a Devin prompt-submit without its provider renders the
        // provider-neutral context instead of the Pixel-first guidance.
        let provider_arg = lifecycle_provider_arg(verb, provider);
        groups.push(hook_group(
            format!("{} run-hook {verb}{provider_arg}", quoted_executable(exe)),
            matcher,
        ));
    }
    if provider == Provider::Claude {
        hooks
            .entry("PostToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("PostToolUse is not an array")?
            .push(metrics_relay_group(exe, provider));
        merge_task_hooks(hooks, provider, exe)?;
    }
    Ok((enabled, adopted))
}

#[cfg(test)]
pub(crate) fn install_provider(
    home: &Path,
    exe: &Path,
    provider: Provider,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    install_at_scoped(
        home,
        &provider.path(home),
        exe,
        provider,
        HookScope::All,
        &[],
        dry_run,
    )
}

pub(crate) fn composed_backup_path(config_path: &Path) -> Result<PathBuf, String> {
    let parent = config_path
        .parent()
        .ok_or_else(|| "Codex hook configuration has no parent directory".to_owned())?;
    let mut missing = Vec::new();
    let mut existing = parent;
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| "Codex hook configuration parent cannot be resolved".to_owned())?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| "Codex hook configuration parent cannot be resolved".to_owned())?;
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| format!("cannot resolve Codex hook configuration parent: {error}"))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved.join(CODEX_COMPOSED_BACKUP))
}

#[cfg(test)]
fn composed_codex_group(exe: &Path, backup: &Path) -> Value {
    hook_group(
        format!(
            "{} run-hook composed-guard --provider codex --backup {}",
            quoted_executable(exe),
            quoted_executable(backup)
        ),
        None,
    )
}

#[cfg(test)]
fn composed_backup(groups: Vec<Value>, managed_pre_tool_use: Value) -> Value {
    json!({
        "version": CODEX_COMPOSED_BACKUP_VERSION,
        "provider": "codex",
        "pre_tool_use": groups,
        "managed_pre_tool_use": managed_pre_tool_use,
    })
}

/// Persist the immutable runtime input before installing the command that can
/// consume it. `persist` is an atomic same-directory rename; write mode is
/// tightened before the file becomes visible.
#[cfg(test)]
fn write_composed_backup(
    path: &Path,
    groups: &[Value],
    managed_pre_tool_use: Value,
    dry_run: bool,
) -> crate::Result<()> {
    use std::{
        fs::OpenOptions,
        io::{self, Write},
        time::{SystemTime, UNIX_EPOCH},
    };

    if dry_run {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| InstallError::InvalidSettings {
        path: path.into(),
        reason: "composed Codex backup has no parent directory".into(),
    })?;
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary_path = parent.join(format!(
        ".pixel-composed-guard-{stamp}-{}.tmp",
        std::process::id()
    ));
    let mut temporary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&composed_backup(groups.to_vec(), managed_pre_tool_use))?
        )
        .as_bytes(),
    )?;
    temporary.flush()?;
    drop(temporary);
    fs::rename(&temporary_path, path)?;
    Ok(())
}

/// Retire project-local Codex retrieval callbacks and preserve existing task
/// hooks without duplicating the global task suite. If an older install wrapped PreToolUse, restore its exact snapshot
/// before removing the callback.
pub(crate) fn install_project_codex_at(
    home: &Path,
    path: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    match crate::uninstall::restore_project_codex_composed_guard(path, dry_run)? {
        crate::uninstall::ComposedGuardRestore::Conflict => {
            return Err(InstallError::InvalidSettings {
                path: path.into(),
                reason: "composed Codex hooks or their private backup changed; preserving both for manual reconciliation".into(),
            });
        }
        crate::uninstall::ComposedGuardRestore::Restored
        | crate::uninstall::ComposedGuardRestore::NotManaged => {}
    }
    install_at_scoped(
        home,
        path,
        exe,
        Provider::Codex,
        HookScope::NativeCleanup,
        &[],
        dry_run,
    )
}

/// Install ZCode's user-level hooks only when its config already exists.
/// ZCode ignores project-level hooks; preserve every unrelated config key and
/// hook group, replacing only Pixel's own matching entries.
pub(crate) fn install_zcode_at(
    home: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let path = home.join(config::ZCODE_CONFIG_FILE);
    if !path.is_file() {
        return Ok(install::InstallStep {
            id: "hooks.zcode".into(),
            status: install::CheckStatus::Green,
            summary: "no ZCode CLI config — skipped (live unverified)".into(),
            detail: None,
        });
    }
    let mut value = install::read_settings(&path)?;
    let hooks = value
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.clone(),
            reason: "expected a JSON object".into(),
        })?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.clone(),
            reason: "hooks must be an object".into(),
        })?;
    let enabled = hooks
        .entry("enabled")
        .or_insert(Value::Bool(true))
        .as_bool()
        == Some(true);
    let events = hooks
        .entry("events")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.clone(),
            reason: "hooks.events must be an object".into(),
        })?;
    let command = format!("{} run-hook guard --provider zcode", quoted_executable(exe));
    for (event, matcher) in [
        ("PreToolUse", "Bash|exec"),
        ("PermissionRequest", "Bash|exec"),
    ] {
        let groups = events
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| InstallError::InvalidSettings {
                path: path.clone(),
                reason: format!("hooks.events.{event} must be an array"),
            })?;
        groups.retain(|group| {
            !group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry.get("command").and_then(Value::as_str) == Some(command.as_str())
                    })
                })
        });
        groups.push(json!({
            "matcher": matcher,
            "hooks": [{"type":"command", "command": command, "timeout":10}]
        }));
    }
    install::write_settings(&path, &value, dry_run)?;
    Ok(install::InstallStep {
        id: "hooks.zcode".into(),
        status: if enabled {
            install::CheckStatus::Green
        } else {
            install::CheckStatus::Yellow
        },
        summary: if enabled {
            "ZCode Pixel rewrite and retrieval-only approval hooks configured (live unverified)"
                .into()
        } else {
            "ZCode hooks remain disabled by the existing hooks.enabled=false setting".into()
        },
        detail: Some(path.display().to_string()),
    })
}

/// Apply the selected host profile while preserving foreign hook entries.
///
/// `backup_root` is the directory whose `.claude/pixel-rtk-hooks.json` holds
/// an adopted RTK group: `$HOME` for the global install, the repository for
/// `pixel install --repo`, so a repo's adoption never reaches the global
/// settings. `inherited` is passed through to [`configure_scoped`].
pub(crate) fn install_at_scoped(
    backup_root: &Path,
    path: &Path,
    exe: &Path,
    provider: Provider,
    scope: HookScope,
    inherited: &[Value],
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let mut value = install::read_settings(path)?;
    let saved = if provider == Provider::Claude {
        load_rtk_backup(backup_root)?
    } else {
        Vec::new()
    };
    let (enabled, adopted) = configure_scoped(&mut value, provider, exe, &saved, scope, inherited)
        .map_err(|reason| InstallError::InvalidSettings {
            path: path.into(),
            reason,
        })?;
    // Persist only the adopted fragment, never restore a whole settings file
    // over later user edits. Save before changing its active registration.
    if !adopted.is_empty() {
        install::write_settings(&backup_root.join(RTK_BACKUP), &json!(adopted), dry_run)?;
    }
    let backup = install::write_settings(path, &value, dry_run)?;
    // Nothing delegates to the backup any more: RTK is back in the settings
    // or was dropped by the user. Retire it after the settings are written,
    // unless the other file that reads it still delegates.
    let read_elsewhere = BACKUP_READERS
        .iter()
        .map(|rel| backup_root.join(rel))
        .any(|other| other != path && delegates_rtk(&other, exe));
    if adopted.is_empty() && !saved.is_empty() && !dry_run && !read_elsewhere {
        fs::remove_file(backup_root.join(RTK_BACKUP))?;
    }
    let summary_text = match scope {
        HookScope::LifecycleOnly => {
            format!(
                "{} lifecycle hooks configured (live unverified)",
                provider.name()
            )
        }
        HookScope::TaskEventsOnly => format!(
            "{} task-event hooks configured (live unverified)",
            provider.name()
        ),
        HookScope::NativeCleanup => format!(
            "{} native tools preserved; Pixel retrieval callbacks removed",
            provider.name()
        ),
        HookScope::GuardOnly => format!(
            "{} guard {}",
            provider.name(),
            if enabled {
                "configured (live unverified)"
            } else {
                "not installed: unknown overlapping hook"
            }
        ),
        #[cfg(test)]
        HookScope::All => format!(
            "{} lifecycle configured; shell routing {} (live unverified)",
            provider.name(),
            if enabled {
                "configured"
            } else {
                "not installed: unknown overlapping hook"
            }
        ),
    };
    Ok(install::InstallStep {
        id: format!("hooks.{}", provider.name()),
        status: if enabled {
            install::CheckStatus::Green
        } else {
            install::CheckStatus::Yellow
        },
        summary: install::dry_run_summary(dry_run, &summary_text),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

/// Take Pixel's guard out of the `PreToolUse` event of a settings file and
/// return the groups left there, plus whether the file changed.
///
/// Only `PreToolUse` is touched, so a lifecycle entry the global install
/// wrote survives even when the repository is `$HOME`. A delegate guard gets
/// the RTK group it adopted back: that group has exactly one accepted shape
/// ([`rtk_group`]), so no backup file is needed to restore it.
pub(crate) fn remove_pre_tool_use_guard(
    path: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<(Vec<Value>, bool)> {
    if !path.is_file() {
        return Ok((Vec::new(), false));
    }
    let mut value = install::read_settings(path)?;
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok((Vec::new(), false));
    };
    let Some(before) = hooks.get("PreToolUse").cloned() else {
        return Ok((Vec::new(), false));
    };
    let mut event = Map::new();
    event.insert("PreToolUse".into(), before.clone());
    let delegated = has_delegate(&event, exe);
    remove_matching_hooks(&mut event, |command| {
        pixel_hook_verb(command, exe).is_some_and(|verb| {
            matches!(
                verb.split_whitespace().next(),
                Some("guard" | "composed-guard" | config::GUARD_HOOK | config::OLD_GUARD_HOOK)
            )
        })
    });
    if delegated {
        restore_rtk(&mut event, &[rtk_group()]);
    }
    let after = event.remove("PreToolUse");
    let left = after
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if after.as_ref() == Some(&before) {
        return Ok((left, false));
    }
    match after {
        Some(groups) => {
            hooks.insert("PreToolUse".into(), groups);
        }
        None => {
            hooks.remove("PreToolUse");
        }
    }
    install::write_settings(path, &value, dry_run)?;
    Ok((left, true))
}

/// Remove the retired repo-local Claude retrieval guard, restoring any
/// adopted RTK registration. Native Claude tools remain in control; task
/// lifecycle hooks are installed globally by the normal install path.
pub(crate) fn install_project_claude_at(
    repo: &Path,
    _home: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let shared = repo.join(CLAUDE_SHARED_SETTINGS);
    let (_, migrated) = remove_pre_tool_use_guard(&shared, exe, dry_run)?;
    let mut step = install_at_scoped(
        repo,
        &repo.join(CLAUDE_LOCAL_SETTINGS),
        exe,
        Provider::Claude,
        HookScope::NativeCleanup,
        &[],
        dry_run,
    )?;
    if migrated {
        step.detail = Some(format!(
            "{}; pixel guard removed from shared {}",
            step.detail.unwrap_or_default(),
            shared.display()
        ));
    }
    Ok(step)
}

/// The groups that cannot run beside the Claude guard: shell rewriters,
/// the exact RTK group included, since only the file that holds it can hand
/// it to the guard.
#[cfg(test)]
fn blocking_claude_groups(groups: &[Value], exe: &Path) -> Vec<Value> {
    groups
        .iter()
        .filter(|group| !coexists_with_guard(group, Provider::Claude, exe))
        .cloned()
        .collect()
}

/// Every hook command of `groups`, in order.
fn commands(groups: &[Value]) -> impl Iterator<Item = &str> {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
}

/// Whether any hook command in a settings value (`{"hooks": {<event>: [...]}}`)
/// is one of Pixel's: the evidence that Pixel was installed into that file.
pub(crate) fn has_pixel_hook(value: &Value, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|events| {
            events
                .values()
                .filter_map(Value::as_array)
                .flat_map(|groups| commands(groups))
                .any(|command| is_pixel_hook(command, exe))
        })
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
/// are not `exe`, each once, in the order met. A `pixel-dev install` leaves
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

/// Whether a Pixel `PreToolUse` command in a settings value contains `verb`
/// (`run-hook guard --provider claude`): the guard is registered, not merely
/// some other Pixel hook.
pub(crate) fn has_pixel_guard(value: &Value, verb: &str, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| is_pixel_hook(command, exe) && command.contains(verb))
}

/// Whether Devin's project config registers Pixel's non-blocking prompt context.
pub(crate) fn has_pixel_prompt_context(value: &Value, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("UserPromptSubmit"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| {
            is_pixel_hook(command, exe)
                && command.contains("run-hook prompt-submit --provider devin")
        })
}

/// Whether Devin's local config pre-approves Pixel retrieval via PermissionRequest.
pub(crate) fn has_pixel_permission_approval(value: &Value, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("PermissionRequest"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| group.get("matcher").and_then(Value::as_str) == Some("exec"))
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| {
            is_pixel_hook(command, exe) && command.contains("run-hook guard --provider devin")
        })
}

/// Merge Pixel's silent exec rewrite, retrieval approval, and prompt context hooks into the
/// repository's personal Devin config, `<repo>/.devin/config.local.json`
/// (the command names this machine's binary, so not the shared
/// `.devin/config.json`). Devin accepts multiple independent hook groups, so
/// each Pixel group is appended after any foreign entries and a
/// reinstall replaces only pixel's own group. A guard an earlier install
/// wrote into `.devin/hooks.json`, a file Devin CLI does not read, is removed.
pub(crate) fn install_project_devin_at(
    repo: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    remove_pre_tool_use_guard(&repo.join(DEVIN_LEGACY_HOOKS), exe, dry_run)?;
    let path = &repo.join(DEVIN_LOCAL_CONFIG);
    let mut value = install::read_settings(path)?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.into(),
            reason: "settings root is not an object".into(),
        })?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.into(),
            reason: "hooks is not an object".into(),
        })?;
    let pixel_group = hook_group(
        format!(
            "{} run-hook guard --provider {}",
            quoted_executable(exe),
            Provider::Devin.name()
        ),
        Some(Provider::Devin.shell_matcher()),
    );
    let merged = config::merge_hook_entry(
        hooks.get("PreToolUse"),
        "run-hook guard --provider devin",
        pixel_group,
    );
    let prompt_group = hook_group(
        format!(
            "{} run-hook prompt-submit --provider devin",
            quoted_executable(exe)
        ),
        None,
    );
    let merged_prompt = config::merge_hook_entry(
        hooks.get("UserPromptSubmit"),
        "run-hook prompt-submit --provider devin",
        prompt_group,
    );
    let permission_group = hook_group(
        format!("{} run-hook guard --provider devin", quoted_executable(exe)),
        Some("exec"),
    );
    let merged_permission = config::merge_hook_entry(
        hooks.get("PermissionRequest"),
        "run-hook guard --provider devin",
        permission_group,
    );
    let merged_metrics = config::merge_hook_entry(
        hooks.get("PostToolUse"),
        "run-hook metrics --provider devin",
        metrics_relay_group(exe, Provider::Devin),
    );
    let unchanged = hooks.get("PreToolUse") == Some(&merged)
        && hooks.get("UserPromptSubmit") == Some(&merged_prompt)
        && hooks.get("PermissionRequest") == Some(&merged_permission)
        && hooks.get("PostToolUse") == Some(&merged_metrics);
    let backup = if unchanged {
        None
    } else {
        hooks.insert("PreToolUse".into(), merged);
        hooks.insert("UserPromptSubmit".into(), merged_prompt);
        hooks.insert("PermissionRequest".into(), merged_permission);
        hooks.insert("PostToolUse".into(), merged_metrics);
        install::write_settings(path, &value, dry_run)?
    };
    Ok(install::InstallStep {
        id: "hooks.devin".into(),
        status: install::CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            if unchanged {
                "devin Pixel rewrite, no-prompt retrieval approval, and prompt-context hooks verified (live unverified)"
            } else {
                "devin Pixel rewrite, no-prompt retrieval approval, and prompt-context hooks configured (live unverified)"
            },
        ),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_hooks_should_replace_only_owned_entries_and_cover_native_boundaries() {
        let exe = Path::new("/tmp/pixel");
        for provider in [Provider::Claude, Provider::Codex] {
            let foreign =
                json!({"type":"command","command":"security-check","trusted_hash":"keep"});
            let mut value = json!({"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[foreign.clone(),{"command":"pixel run-hook task-event --provider claude --event pre-tool-use"}]}]}});
            merge_task_hooks(value["hooks"].as_object_mut().unwrap(), provider, exe).unwrap();
            assert_eq!(
                value["hooks"]["PreToolUse"][0]["hooks"],
                json!([foreign.clone()])
            );
            assert!(task_hooks_registered(&value, provider, exe));
            let first = value.clone();
            merge_task_hooks(value["hooks"].as_object_mut().unwrap(), provider, exe).unwrap();
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

    /// A release build's executable: every release installs it as `pixel`.
    fn release() -> &'static Path {
        Path::new("/usr/local/bin/pixel")
    }

    #[test]
    fn routing_ownership_rejects_other_executables_and_command_mentions() {
        assert!(is_pixel_hook(
            "'/tmp/Pixel tools/pixel' run-hook guard --provider claude",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook prompt-submit",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook prompt-submit --provider claude",
            release()
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook post-compaction --provider claude",
            release()
        ));
        for foreign in [
            "other-pixel hook guard",
            "echo /tmp/pixel hook session-start",
            "echo ~/.claude/hooks/pixel-prompt-submit",
            "echo 'pixel hook guard'",
            "'/tmp/pixel' run-hook session-start && security-check",
            "'/tmp/pixel' run-hook guard --provider claude; security-check",
            "'/tmp/pixel' run-hook prompt-submit > user-log",
        ] {
            assert!(!is_pixel_hook(foreign, release()), "{foreign}");
        }
    }

    /// Cursor's flat `hooks.<event>` schema: Pixel's own commands go by
    /// executable ownership, so a foreign command whose text merely contains
    /// a pixel verb (e.g. `run-hook guard` or `run-hook metrics`) survives
    /// an uninstall untouched.
    #[test]
    fn remove_flat_pixel_hooks_preserves_foreign_commands_that_mention_pixel_verbs() {
        let exe = Path::new("/usr/local/bin/pixel");
        let mut value = json!({
            "preToolUse": [
                { "command": "lint" },
                { "command": "/opt/dev/bin/run-hook guard-stats" },
                { "command": format!("{} run-hook guard", quoted_executable(exe)) },
            ],
            "postToolUse": [
                { "command": "tidy" },
                { "command": "some-tool run-hook metrics --provider cursor" },
                { "command": format!(
                    "{} run-hook metrics --provider cursor",
                    quoted_executable(exe)
                ) },
            ],
        });
        remove_flat_pixel_hooks(value.as_object_mut().unwrap(), exe);
        assert_eq!(
            value,
            json!({
                "preToolUse": [
                    { "command": "lint" },
                    { "command": "/opt/dev/bin/run-hook guard-stats" },
                ],
                "postToolUse": [
                    { "command": "tidy" },
                    { "command": "some-tool run-hook metrics --provider cursor" },
                ],
            })
        );
    }

    /// Installs before `pixel run-hook` registered bare scripts under
    /// `~/.claude/hooks/`; each must read as Pixel's so a reinstall replaces
    /// it instead of leaving it beside the new `run-hook` entry.
    #[test]
    fn is_pixel_hook_should_recognise_script_entries_of_pre_run_hook_installs() {
        for legacy in [
            "~/.claude/hooks/pixel-session-start",
            "/Users/dev/.claude/hooks/pixel-prompt-submit",
            "/Users/dev/.claude/hooks/pixel-post-compaction",
            "~/.claude/hooks/pixel-targets-guard",
            "~/.claude/hooks/gitpixel-targets-guard",
            "'/Users/a dev/.claude/hooks/pixel-session-start'",
        ] {
            assert!(is_pixel_hook(legacy, release()), "{legacy}");
        }
        for foreign in [
            "~/.claude/hooks/pixel-session-start-audit",
            "my-tool ~/.claude/hooks/pixel-session-start",
            "/usr/local/bin/session-start",
        ] {
            assert!(!is_pixel_hook(foreign, release()), "{foreign}");
        }
    }

    #[test]
    fn has_pixel_guard_should_need_a_pixel_command_carrying_the_verb() {
        let settings = |command: &str| json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":command}]}]}});
        let verb = "run-hook guard --provider claude";
        assert!(has_pixel_guard(
            &settings("'/p/pixel' run-hook guard --provider claude"),
            verb,
            release()
        ));
        assert!(
            !has_pixel_guard(
                &settings("echo run-hook guard --provider claude"),
                verb,
                release()
            ),
            "a foreign command merely naming the verb is not the guard"
        );
        assert!(
            !has_pixel_guard(
                &settings("'/p/pixel' run-hook guard --provider devin"),
                verb,
                release()
            ),
            "another provider's guard is not this one"
        );
        let lifecycle_only = json!({"hooks":{"SessionStart":[{"hooks":[{"command":"'/p/pixel' run-hook guard --provider claude"}]}]}});
        assert!(
            !has_pixel_guard(&lifecycle_only, verb, release()),
            "only PreToolUse registers a guard"
        );
        assert!(has_pixel_hook(&lifecycle_only, release()));
        assert!(!has_pixel_hook(&settings("keep-security-check"), release()));
        assert!(!has_pixel_hook(&Value::Null, release()));
    }

    #[test]
    fn remove_pre_tool_use_guard_should_keep_lifecycle_and_foreign_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(
            remove_pre_tool_use_guard(&path, release(), false).unwrap(),
            (Vec::new(), false),
            "an absent file is left absent"
        );
        assert!(!path.exists());
        let write = json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write"}]});
        let guard = json!({"matcher":"Bash","hooks":[{"type":"command","command":"'/old/pixel' run-hook guard --provider claude"}]});
        let start =
            json!({"hooks":[{"type":"command","command":"'/p/pixel' run-hook session-start"}]});
        let original =
            json!({"hooks":{"PreToolUse":[write.clone(), guard],"SessionStart":[start.clone()]}});
        install::write_settings(&path, &original, false).unwrap();

        let (left, changed) = remove_pre_tool_use_guard(&path, release(), true).unwrap();
        assert!(changed);
        assert_eq!(left, vec![write.clone()]);
        assert_eq!(
            install::read_settings(&path).unwrap(),
            original,
            "a dry run reports without writing"
        );

        let (left, changed) = remove_pre_tool_use_guard(&path, release(), false).unwrap();
        assert!(changed);
        assert_eq!(left, vec![write.clone()]);
        let after = install::read_settings(&path).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"], json!([write.clone()]));
        assert_eq!(
            after["hooks"]["SessionStart"],
            json!([start]),
            "a lifecycle entry is never touched, even Pixel's own"
        );
        assert_eq!(
            remove_pre_tool_use_guard(&path, release(), false).unwrap(),
            (vec![write], false),
            "nothing left to remove"
        );
    }

    #[test]
    fn shared_guard_cleanup_should_preserve_task_gates_inside_mixed_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let task = json!({"type":"command","command":"'/p/pixel' run-hook task-event --provider claude --event pre-tool-use","timeout":10});
        let foreign = json!({"type":"command","command":"other run-hook guard --provider claude"});
        let retained = json!({"matcher":"Bash","hooks":[task, foreign],"custom":"keep"});
        for command in [
            "'/p/pixel' run-hook guard",
            "'/p/pixel' hook guard --provider claude",
            "'/p/pixel' run-hook guard --provider codex",
            "'/p/pixel' run-hook guard --provider devin",
            "'/p/pixel' run-hook guard --provider zcode",
            "'/p/pixel' run-hook composed-guard --provider codex",
            config::GUARD_HOOK,
            config::OLD_GUARD_HOOK,
        ] {
            let mut mixed = retained.clone(); // Each case mutates an independent fixture.
            mixed["hooks"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"command","command":command}));
            install::write_settings(&path, &json!({"hooks":{"PreToolUse":[mixed]}}), false)
                .unwrap();
            assert_eq!(
                remove_pre_tool_use_guard(&path, release(), false).unwrap(),
                (vec![retained.clone()], true), // Reuse the fixture in later assertions.
                "{command}"
            );
            assert_eq!(
                install::read_settings(&path).unwrap(),
                json!({"hooks":{"PreToolUse":[retained.clone()]}}), // Retain the shared fixture.
                "{command}"
            );
        }
    }

    #[test]
    fn remove_pre_tool_use_guard_should_drop_an_emptied_event_and_restore_a_delegated_rtk() {
        let dir = tempfile::tempdir().unwrap();
        let only_guard = dir.path().join("only.json");
        install::write_settings(
            &only_guard,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude"}]}]}}),
            false,
        )
        .unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&only_guard, release(), false).unwrap(),
            (Vec::new(), true)
        );
        assert_eq!(
            install::read_settings(&only_guard).unwrap(),
            json!({"hooks":{}})
        );

        let delegated = dir.path().join("delegated.json");
        install::write_settings(
            &delegated,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude --delegate-rtk"}]}]}}),
            false,
        )
        .unwrap();
        let (left, changed) = remove_pre_tool_use_guard(&delegated, release(), false).unwrap();
        assert!(changed);
        assert_eq!(
            left,
            vec![rtk_group()],
            "the RTK group the delegate adopted runs again"
        );
        assert_eq!(
            install::read_settings(&delegated).unwrap()["hooks"]["PreToolUse"],
            json!([rtk_group()])
        );

        let no_pre = dir.path().join("no-pre.json");
        let lifecycle = json!({"hooks":{"SessionStart":[{"hooks":[{"command":"x"}]}]}});
        install::write_settings(&no_pre, &lifecycle, false).unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&no_pre, release(), false).unwrap(),
            (Vec::new(), false)
        );
        let no_hooks = dir.path().join("no-hooks.json");
        install::write_settings(&no_hooks, &json!({"theme":"dark"}), false).unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&no_hooks, release(), false).unwrap(),
            (Vec::new(), false)
        );
    }

    /// A shell rewriter the shared project settings register runs in the
    /// same Claude session as the personal file's guard: two rewriters of one
    /// command race, so the guard stays out, exactly as when both sit in one
    /// file. A group that cannot touch a shell call blocks nothing.
    #[test]
    fn configure_scoped_should_treat_inherited_shell_rewriters_as_blocking() {
        let exe = Path::new("/tmp/pixel");
        let security =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
        let write = json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write"}]});
        let gitnexus = json!({"matcher":"Grep|Glob|Bash","hooks":[{"type":"command","command":"node \"/Users/dev/.claude/hooks/gitnexus/gitnexus-hook.cjs\""}]});
        for (inherited, expect_enabled) in [
            (vec![security.clone()], false),
            (vec![rtk_group()], false),
            (vec![write.clone()], true),
            (
                vec![hook_group(
                    "pixel run-hook task-event --provider claude --event pre-tool-use".into(),
                    None,
                )],
                true,
            ),
            (vec![gitnexus], true),
            (Vec::new(), true),
        ] {
            let mut value = json!({});
            let (enabled, adopted) = configure_scoped(
                &mut value,
                Provider::Claude,
                exe,
                &[],
                HookScope::GuardOnly,
                &inherited,
            )
            .unwrap();
            assert_eq!(enabled, expect_enabled, "{inherited:?}");
            assert!(adopted.is_empty(), "an inherited group is never adopted");
            assert_eq!(
                has_pixel_guard(&value, "run-hook guard --provider claude", release()),
                expect_enabled,
                "{value}"
            );
        }
    }

    #[test]
    fn renamed_task_gate_should_coexist_without_accepting_another_executable() {
        let exe = Path::new("/opt/custom tools/our-agent");
        let task = hook_group(
            format!(
                "{} run-hook task-event --provider claude --event pre-tool-use",
                quoted_executable(exe)
            ),
            None,
        );
        let foreign = hook_group(
            "'/opt/foreign-agent' run-hook task-event --provider claude --event pre-tool-use"
                .into(),
            None,
        );
        assert!(blocking_claude_groups(std::slice::from_ref(&task), exe).is_empty());
        assert_eq!(
            blocking_claude_groups(&[task.clone(), foreign.clone()], exe),
            vec![foreign.clone()]
        );
        for (inherited, allowed) in [(vec![task.clone()], true), (vec![task, foreign], false)] {
            let mut value = json!({});
            let (enabled, adopted) = configure_scoped(
                &mut value,
                Provider::Claude,
                exe,
                &[],
                HookScope::GuardOnly,
                &inherited,
            )
            .unwrap();
            assert_eq!(enabled, allowed);
            assert!(adopted.is_empty());
            assert_eq!(
                has_pixel_guard(&value, "run-hook guard --provider claude", exe),
                allowed
            );
        }
    }

    #[test]
    fn routing_provider_transform_preserves_foreign_hooks_and_lifecycle_contracts() {
        for provider in [Provider::Claude, Provider::Codex, Provider::Devin] {
            let foreign = json!({"matcher":"SessionStart","hooks":[{"type":"command","command":"keep-security-check"}]});
            let mut value = json!({"hooks":{"SessionStart":[foreign.clone(), {"hooks":[{"command":"/tmp/pixel hook session-start"}]}], "PostCompaction":[{"hooks":[{"command":"~/.claude/hooks/pixel-post-compaction"}]}]},"unrelated":true});
            let (enabled, _) = configure(
                &mut value,
                provider,
                Path::new("/tmp/Pixel tools/pixel"),
                &[],
            )
            .unwrap();
            assert!(enabled);
            assert_eq!(value["unrelated"], true);
            assert_eq!(value["hooks"]["SessionStart"][0], foreign);
            assert!(value["hooks"]["SessionStart"][1].get("matcher").is_none());
            assert_eq!(
                value["hooks"]["SessionStart"][1]["hooks"][0]["command"],
                format!(
                    "'/tmp/Pixel tools/pixel' run-hook session-start --provider {}",
                    provider.name()
                )
            );
            assert_eq!(
                value["hooks"]["PreToolUse"][0]["matcher"],
                provider.shell_matcher()
            );
            assert!(value["hooks"][provider.compact()].is_array());
            if provider == Provider::Codex {
                // The global install owns Codex's prompt-submit
                // (`install_metrics_hook`); the project file never carries a
                // second copy Codex would merge over it.
                assert!(value["hooks"].get("UserPromptSubmit").is_none(), "{value}");
            } else {
                let prompt = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
                let prompt_command = prompt.first().unwrap()["hooks"][0]["command"]
                    .as_str()
                    .unwrap();
                if provider == Provider::Claude {
                    assert!(prompt_command.ends_with("hook prompt-submit --provider claude"));
                } else {
                    // Without its provider a Devin prompt-submit renders the
                    // provider-neutral context instead of the Pixel-first
                    // guidance.
                    assert!(prompt_command.ends_with("hook prompt-submit --provider devin"));
                }
            }
            if provider != Provider::Devin {
                assert_eq!(value["hooks"]["SessionStart"][2]["matcher"], "compact");
                assert!(value["hooks"].get("PostCompact").is_none());
                let compact_command = value["hooks"]["SessionStart"][2]["hooks"][0]["command"]
                    .as_str()
                    .unwrap();
                if provider == Provider::Claude {
                    assert!(compact_command.ends_with("hook post-compaction --provider claude"));
                } else {
                    assert!(compact_command.ends_with("hook post-compaction"));
                }
            }
            let once = value.clone();
            configure(
                &mut value,
                provider,
                Path::new("/tmp/Pixel tools/pixel"),
                &[],
            )
            .unwrap();
            assert_eq!(value, once);
        }
    }

    #[test]
    fn routing_rtk_adoption_round_trips_without_discarding_user_hooks() {
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let mut value = json!({"hooks":{"PreToolUse":[rtk.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(adopted, vec![rtk.clone()]);
        assert!(has_delegate(value["hooks"].as_object().unwrap(), release()));
        let once = value.clone();
        configure(
            &mut value,
            Provider::Claude,
            Path::new("/tmp/pixel"),
            &adopted,
        )
        .unwrap();
        assert_eq!(value, once);
        let hooks = value["hooks"].as_object_mut().unwrap();
        remove_pixel_hooks(hooks, release());
        restore_rtk(hooks, &adopted);
        assert_eq!(hooks["PreToolUse"], json!([rtk]));
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
            "'/Users/dev/.local/bin/pixel-dev' run-hook session-start",
            "'/Users/dev/.local/bin/pixel-dev' run-hook post-tool-use --provider claude",
            // The same build copied elsewhere writes the same file name.
            "'/opt/other place/pixel-dev' run-hook prompt-submit --provider claude",
            // A release build's entries stay pixel's for a dev install.
            "'/usr/local/bin/pixel' run-hook guard --provider claude",
            "'/p/pixel-dev' run-hook composed-guard --provider codex --backup '/p/b.json'",
        ] {
            assert!(is_pixel_hook(own, dev), "{own}");
        }
        for foreign in [
            "'/opt/pixel-dev-helper' run-hook session-start",
            "'/opt/other-pixel' run-hook session-start",
            "other-pixel hook guard",
            "herdr hook session-start --agent claude",
            "'/Users/dev/.local/bin/pixel-dev' run-hook session-start && security-check",
            "'/Users/dev/.local/bin/pixel-dev' run-hook prompt-submit > user-log",
            "'/Users/dev/.local/bin/pixel-dev' run-hook unknown-verb",
            "'/p/pixel-dev' run-hook composed-guard --provider codex --backup ",
            "'/p/security' run-hook composed-guard --provider codex --backup '/p/b.json'",
            "my-tool ~/.claude/hooks/pixel-session-start",
            "/usr/local/bin/session-start",
        ] {
            assert!(!is_pixel_hook(foreign, dev), "{foreign}");
        }
        assert!(
            is_pixel_hook(
                "'/Users/dev/.local/bin/pixel-dev' run-hook session-start",
                release()
            ),
            "a release install replaces what `self-update --dev` installed"
        );
        let renamed = Path::new("/opt/bin/pixel-livio");
        let livio = "'/opt/bin/pixel-livio' run-hook session-start";
        assert!(
            is_pixel_hook(livio, renamed),
            "a build renamed by hand owns what it wrote"
        );
        assert!(
            !is_pixel_hook(livio, release()),
            "a name pixel never installs under is only its own build's"
        );
        assert!(!is_pixel_hook(
            "'/opt/pixel-dev-helper' run-hook session-start",
            release()
        ));
    }

    /// The RTK delegation of a renamed build's guard is read back by that
    /// build's next install: unrecognised, the old delegate guard stayed as an
    /// unknown shell rewriter, which blocked the new guard, and the adopted
    /// RTK group was never put back.
    #[test]
    fn reinstall_by_a_renamed_build_should_keep_its_rtk_delegation() {
        let renamed = Path::new("/opt/bin/pixel-livio");
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let mut value = json!({"hooks":{"PreToolUse":[rtk.clone()]}});
        let (_, adopted) = configure(&mut value, Provider::Claude, renamed, &[]).unwrap();
        assert_eq!(adopted, vec![rtk.clone()]);
        assert!(has_delegate(value["hooks"].as_object().unwrap(), renamed));
        assert!(!has_delegate(
            value["hooks"].as_object().unwrap(),
            release()
        ));
        let once = value.clone();
        let (enabled, again) = configure(&mut value, Provider::Claude, renamed, &adopted).unwrap();
        assert!(
            enabled,
            "the renamed build's own guard does not block itself"
        );
        assert_eq!(again, vec![rtk]);
        assert_eq!(value, once);
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

    /// The executables pixel's hooks run besides this binary: the side
    /// build's path once however many hooks name it, never a foreign hook, a
    /// legacy script or this binary itself, whether a hook names it by the
    /// same path or through a symlink.
    #[test]
    #[cfg(unix)]
    fn pixel_hooks_running_other_binaries_should_name_each_other_executable_once() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("pixel");
        fs::write(&exe, "").unwrap();
        let link = dir.path().join("pixel");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        let dev = dir.path().join("dev dir/pixel-dev");
        let hook = |exe: &Path, verb: &str| json!({"hooks":[{"type":"command","command":format!("{} run-hook {verb}", quoted_executable(exe))}]});
        let value = json!({"hooks":{
            "SessionStart":[
                hook(&dev, "session-start"),
                {"hooks":[{"type":"command","command":"herdr hook session-start --agent claude"}]},
                {"hooks":[{"type":"command","command":"/opt/foreign/pixel-dev-helper run-hook session-start"}]},
            ],
            "UserPromptSubmit":[
                hook(&dev, "prompt-submit --provider claude"),
                hook(&exe, "prompt-submit --provider claude"),
            ],
            "PostToolUse":[hook(&link, "post-tool-use --provider claude")],
        }});
        assert_eq!(
            pixel_hooks_running_other_binaries(&value, &exe),
            vec![dev.clone()]
        );
        let mut from_dev = pixel_hooks_running_other_binaries(&value, &dev);
        from_dev.sort();
        let mut expected = vec![exe.clone(), link];
        expected.sort();
        assert_eq!(
            from_dev, expected,
            "a binary that does not exist on disk compares by path alone"
        );
        let missing = dir.path().join("gone/pixel");
        assert_eq!(
            pixel_hooks_running_other_binaries(
                &json!({"hooks":{"SessionStart":[hook(&missing, "session-start")]}}),
                &dir.path().join("also-gone/pixel")
            ),
            vec![missing],
            "two missing binaries are not the same one"
        );
        assert_eq!(
            pixel_hooks_running_other_binaries(&Value::Null, &exe),
            Vec::<PathBuf>::new()
        );
    }

    /// Doctor names each stacked pixel hook with its count, whichever of
    /// pixel's names wrote the copies, and never counts a foreign hook or a
    /// pixel hook registered once.
    #[test]
    fn stacked_pixel_hooks_should_name_each_verb_registered_more_than_once() {
        let dev = Path::new("/Users/dev/.local/bin/pixel-dev");
        let entry = |command: &str| json!({"hooks":[{"type":"command","command":command}]});
        let start = entry("'/Users/dev/.local/bin/pixel-dev' run-hook session-start");
        let herdr = entry("herdr hook session-start --agent claude");
        let value = json!({"hooks":{
            "SessionStart":[
                start.clone(), start.clone(), start,
                {"matcher":"compact","hooks":[{"type":"command","command":"'/Users/dev/.local/bin/pixel-dev' run-hook post-compaction --provider claude"}]},
            ],
            "Stop":[herdr.clone(), herdr],
            "UserPromptSubmit":[
                entry("'/usr/local/bin/pixel' run-hook prompt-submit --provider claude"),
                entry("'/Users/dev/.local/bin/pixel-dev' run-hook prompt-submit --provider claude"),
            ],
        }});
        assert_eq!(
            stacked_pixel_hooks(&value, dev),
            vec![
                "SessionStart→session-start ×3".to_owned(),
                "UserPromptSubmit→prompt-submit --provider claude ×2".to_owned(),
            ]
        );
        assert_eq!(
            stacked_pixel_hooks(&value, release()),
            stacked_pixel_hooks(&value, dev),
            "a release doctor sees the copies a dev build stacked"
        );
        assert_eq!(stacked_pixel_hooks(&Value::Null, dev), Vec::<String>::new());
    }

    #[test]
    fn routing_unknown_overlap_is_preserved_without_a_second_mutator() {
        let security =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
        let mut value = json!({"hooks":{"PreToolUse":[security.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(!enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"][0], security);
        assert_eq!(value["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "'/tmp/pixel' run-hook task-event --provider claude --event pre-tool-use"
        );
    }

    #[test]
    fn routing_codex_cmux_observer_can_coexist_with_the_single_rewriter() {
        let cmux = json!({
            "hooks":[{"type":"command","command":"/Users/test/.cmux/hooks/cmux-codex-hook-persistent-feed-PreToolUse.sh"}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[cmux.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"][0], cmux);
        assert_eq!(
            value["hooks"]["PreToolUse"][1]["matcher"],
            "Bash|shell|unified_exec|local_shell"
        );
        assert!(
            value["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook guard --provider codex")
        );
    }

    #[test]
    fn routing_claude_vibe_observer_can_coexist_with_pixel_and_adopted_rtk() {
        let vibe = json!({
            "matcher":"*",
            "hooks":[{"type":"command","command":"/bin/sh -c '[ -x \"$HOME/.vibe-island/bin/vibe-island-bridge\" ] && \"$HOME/.vibe-island/bin/vibe-island-bridge\" --source claude; exit 0'"}]
        });
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let mut value = json!({"hooks":{"PreToolUse":[vibe.clone(), rtk]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(adopted.len(), 1);
        assert_eq!(value["hooks"]["PreToolUse"][0], vibe);
        assert_eq!(value["hooks"]["PreToolUse"][1]["matcher"], "Bash|Read|Grep");
        assert!(
            value["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook guard --provider claude --delegate-rtk")
        );
    }

    #[test]
    fn routing_claude_gitnexus_context_hook_can_coexist_with_pixel() {
        let gitnexus = json!({
            "matcher":"Grep|Glob|Bash",
            "hooks":[{"type":"command","command":"node \"/Users/test/.claude/hooks/gitnexus/gitnexus-hook.cjs\""}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[gitnexus.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"][0], gitnexus);
        assert_eq!(value["hooks"]["PreToolUse"][1]["matcher"], "Bash|Read|Grep");
    }

    #[test]
    fn routing_codex_inactive_orca_wrapper_can_coexist_but_an_active_one_blocks() {
        let missing = std::env::temp_dir()
            .join(format!("pixel-routing-orca-missing-{}", std::process::id()))
            .join(".orca/agent-hooks/codex-hook.sh");
        let command = format!(
            "if [ -f '{}' ] && [ -r '{}' ] && [ -x '{}' ]; then /bin/sh '{}'; else {{ command -p cat 2>/dev/null || cat; }} >/dev/null 2>&1 || :; fi",
            missing.display(),
            missing.display(),
            missing.display(),
            missing.display()
        );
        let orca = json!({"hooks":[{"type":"command","command":command}]});
        let mut value = json!({"hooks":{"PreToolUse":[orca.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);

        std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
        std::fs::write(&missing, "#!/bin/sh\n").unwrap();
        let mut active = json!({"hooks":{"PreToolUse":[orca]}});
        let (enabled, _) =
            configure(&mut active, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(!enabled);
        let _ = std::fs::remove_file(missing);
    }

    #[test]
    fn routing_claude_inactive_orca_wrapper_can_coexist() {
        let missing = std::env::temp_dir()
            .join(format!(
                "pixel-routing-orca-claude-missing-{}",
                std::process::id()
            ))
            .join(".orca/agent-hooks/claude-hook.sh");
        let command = format!(
            "if [ -f '{}' ] && [ -r '{}' ] && [ -x '{}' ]; then /bin/sh '{}'; else {{ command -p cat 2>/dev/null || cat; }} >/dev/null 2>&1 || :; fi",
            missing.display(),
            missing.display(),
            missing.display(),
            missing.display()
        );
        let orca = json!({"matcher":"*","hooks":[{"type":"command","command":command}]});
        let mut value = json!({"hooks":{"PreToolUse":[orca.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(value["hooks"]["PreToolUse"][0], orca);
    }

    #[test]
    fn routing_codex_anchored_non_shell_matcher_is_disjoint_and_kept() {
        let worktree_guard = json!({
            "matcher":"^apply_patch$|^mcp__filesystem__|^mcp__morph-mcp__edit_file$",
            "hooks":[{"type":"command","command":"keep-worktree-path-guard"}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[worktree_guard.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(value["hooks"]["PreToolUse"][0], worktree_guard);
        assert_eq!(
            value["hooks"]["PreToolUse"][1]["matcher"],
            "Bash|shell|unified_exec|local_shell"
        );
    }

    #[test]
    fn project_codex_default_should_keep_foreign_pretooluse_without_pixel_wrapper() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let original = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"deny-unsafe-shell"}]},
            {"matcher":"*","hooks":[{"type":"command","command":"audit-all-tools"}]}
        ]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":original.clone()}}),
            false,
        )
        .unwrap();

        install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).unwrap();
        let mut installed = install::read_settings(&path).unwrap();
        assert_eq!(installed["hooks"]["PreToolUse"], original);
        assert!(
            !installed
                .to_string()
                .contains("task-event --provider codex")
        );
        assert!(!installed.to_string().contains("composed-guard"));
        assert!(!path.parent().unwrap().join(CODEX_COMPOSED_BACKUP).exists());

        let changed =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"later-user-guard"}]});
        installed["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(changed.clone());
        install::write_settings(&path, &installed, false).unwrap();
        install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            json!([original[0].clone(), original[1].clone(), changed])
        );
    }

    #[test]
    fn project_codex_install_should_unwrap_a_legacy_composed_guard() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let exe = Path::new("/tmp/pixel");
        let sidecar = composed_backup_path(&path).unwrap();
        let original = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]}
        ]);
        let managed = json!([composed_codex_group(exe, &sidecar)]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":managed.clone()}}),
            false,
        )
        .unwrap();
        write_composed_backup(&sidecar, original.as_array().unwrap(), managed, false).unwrap();

        install_project_codex_at(home.path(), &path, exe, false).unwrap();

        let installed = install::read_settings(&path).unwrap();
        assert_eq!(installed["hooks"]["PreToolUse"], original);
        assert!(
            !installed
                .to_string()
                .contains("task-event --provider codex")
        );
        assert!(!installed.to_string().contains("composed-guard"));
        assert!(!sidecar.exists());
    }

    #[test]
    fn project_codex_install_should_preserve_a_diverged_legacy_composition() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let exe = Path::new("/tmp/pixel");
        let sidecar = composed_backup_path(&path).unwrap();
        let original = json!([]);
        let managed = json!([composed_codex_group(exe, &sidecar)]);
        let mut changed = managed.clone();
        changed
            .as_array_mut()
            .unwrap()
            .push(json!({"matcher":"Bash","hooks":[{"type":"command","command":"user-change"}]}));
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":changed.clone()}}),
            false,
        )
        .unwrap();
        write_composed_backup(&sidecar, original.as_array().unwrap(), managed, false).unwrap();

        assert!(install_project_codex_at(home.path(), &path, exe, false).is_err());
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            changed
        );
        assert!(sidecar.is_file(), "the recovery snapshot remains intact");
    }

    #[test]
    fn project_codex_install_should_canonicalize_relative_config_aliases() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let path = repo.join(".codex/hooks.json");
        let alias = home.path().join("alias/../repo/.codex/hooks.json");
        let original = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"user-shell-policy"}]}
        ]);
        fs::create_dir_all(home.path().join("alias")).unwrap();
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":original.clone()}}),
            false,
        )
        .unwrap();

        install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).unwrap();
        install_project_codex_at(home.path(), &alias, Path::new("/tmp/pixel"), false).unwrap();

        let installed = install::read_settings(&path).unwrap();
        assert_eq!(installed["hooks"]["PreToolUse"], original);
        assert!(!installed.to_string().contains("composed-guard"));
        assert!(!repo.join(".codex").join(CODEX_COMPOSED_BACKUP).exists());
    }

    #[test]
    fn project_claude_native_cleanup_should_preserve_foreign_hooks_and_restore_rtk() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let local = repo.join(CLAUDE_LOCAL_SETTINGS);
        let foreign = json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "user-shell-policy"}]
        });
        let delegated = json!({
            "matcher": "Bash|shell",
            "hooks": [{
                "type": "command",
                "command": "'/p/pixel' run-hook guard --provider claude --delegate-rtk"
            }]
        });
        let task_gate = json!({
            "hooks": [{
                "type": "command",
                "command": "'/p/pixel' run-hook task-event --provider claude --event pre-tool-use"
            }]
        });
        let task_stop = json!({
            "hooks": [{
                "type": "command",
                "command": "'/p/pixel' run-hook task-event --provider claude --event stop"
            }]
        });
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(
            &local,
            serde_json::to_string(&json!({
                "hooks": {
                    "PreToolUse": [foreign.clone(), delegated, task_gate.clone()],
                    "Stop": [task_stop.clone()]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            repo.join(RTK_BACKUP),
            serde_json::to_string(&json!([rtk_group()])).unwrap(),
        )
        .unwrap();

        let step =
            install_project_claude_at(&repo, home.path(), Path::new("/p/pixel"), false).unwrap();

        assert_eq!(step.status, install::CheckStatus::Green);
        let installed = install::read_settings(&local).unwrap();
        assert_eq!(
            installed["hooks"]["PreToolUse"],
            json!([foreign, task_gate, rtk_group()])
        );
        assert_eq!(installed["hooks"]["Stop"], json!([task_stop]));
        assert!(
            !installed
                .to_string()
                .contains("run-hook guard --provider claude")
        );
        assert!(!repo.join(RTK_BACKUP).exists());
    }

    /// The legacy spelling is not only a canonical path: `self-update --dev`
    /// can point `bin/pixel` at a versioned file whose *name* is not `pixel`.
    /// Such an entry is recognised by its canonicalised path alone, and it too
    /// must migrate the backup, or the next install reads the config and the
    /// backup as disagreeing and refuses.
    #[test]
    #[cfg(unix)]
    fn project_codex_composition_migrates_a_symlink_whose_target_has_another_name() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let real = home.path().join("store/pixel-0.7.0");
        let stable = home.path().join("bin/pixel");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(stable.parent().unwrap()).unwrap();
        fs::write(&real, "pixel").unwrap();
        symlink(&real, &stable).unwrap();
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"keep"}]}]}}),
            false,
        )
        .unwrap();

        let canonical = real.canonicalize().unwrap();
        install_project_codex_at(home.path(), &path, &canonical, false).unwrap();
        let sidecar = path.parent().unwrap().join(CODEX_COMPOSED_BACKUP);
        install_project_codex_at(home.path(), &path, &stable, false).unwrap();

        let installed = install::read_settings(&path).unwrap();
        let command = installed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.starts_with(&format!("'{}'", stable.display())),
            "{command}"
        );
        assert_eq!(
            read_composed_backup(&sidecar).unwrap()["managed_pre_tool_use"],
            installed["hooks"]["PreToolUse"]
        );
    }

    /// A repo install written by a side build (`pixel-dev install --repo .`)
    /// must be taken back by the managed `pixel install --repo .`: the entry is
    /// pixel's own, only under the other executable name. Both the config and
    /// the backup must move to the managed spelling in one pass, or the next
    /// install would read them as disagreeing and refuse.
    #[test]
    fn project_codex_composition_hands_a_side_build_install_back_to_the_managed_pixel() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let side = home.path().join("bin/pixel-dev");
        let managed = home.path().join("bin/pixel");
        let original = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"deny-unsafe-shell"}]}
        ]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":original.clone()}, "keep":true}),
            false,
        )
        .unwrap();

        install_project_codex_at(home.path(), &path, &side, false).unwrap();
        let sidecar = path.parent().unwrap().join(CODEX_COMPOSED_BACKUP);
        let side_managed = install::read_settings(&path).unwrap()["hooks"]["PreToolUse"].clone();

        install_project_codex_at(home.path(), &path, &managed, false).unwrap();

        let installed = install::read_settings(&path).unwrap();
        let command = installed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.starts_with(&format!("'{}'", managed.display())),
            "{command}"
        );
        assert_ne!(installed["hooks"]["PreToolUse"], side_managed);
        assert_eq!(installed["keep"], json!(true));
        let stored = read_composed_backup(&sidecar).unwrap();
        assert_eq!(stored["pre_tool_use"], original);
        assert_eq!(
            stored["managed_pre_tool_use"],
            installed["hooks"]["PreToolUse"]
        );
    }

    /// The hand-back admits exactly one group. A user group added beside the
    /// side build's entry is still a refusal, never a silent overwrite.
    #[test]
    fn project_codex_composition_refuses_a_user_group_added_beside_a_side_build_entry() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let original = json!([{"matcher":"Bash","hooks":[{"type":"command","command":"keep"}]}]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":original.clone()}}),
            false,
        )
        .unwrap();
        install_project_codex_at(
            home.path(),
            &path,
            &home.path().join("bin/pixel-dev"),
            false,
        )
        .unwrap();

        let mut changed = install::read_settings(&path).unwrap();
        changed["hooks"]["PreToolUse"].as_array_mut().unwrap().push(
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"later-user-guard"}]}),
        );
        install::write_settings(&path, &changed, false).unwrap();

        assert!(
            install_project_codex_at(home.path(), &path, &home.path().join("bin/pixel"), false)
                .is_err()
        );
        assert_eq!(
            read_composed_backup(&path.parent().unwrap().join(CODEX_COMPOSED_BACKUP)).unwrap()["pre_tool_use"],
            original
        );
    }

    /// Install a side build's composed group, apply `edit` to it, and assert
    /// the managed `pixel` refuses the hand-back without touching either file.
    fn assert_hand_back_refused_after(edit: impl FnOnce(&mut Value)) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"keep"}]}]}}),
            false,
        )
        .unwrap();
        install_project_codex_at(
            home.path(),
            &path,
            &home.path().join("bin/pixel-dev"),
            false,
        )
        .unwrap();
        let sidecar = path.parent().unwrap().join(CODEX_COMPOSED_BACKUP);
        let mut changed = install::read_settings(&path).unwrap();
        edit(&mut changed["hooks"]["PreToolUse"][0]);
        install::write_settings(&path, &changed, false).unwrap();
        let config_before = fs::read(&path).unwrap();
        let backup_before = fs::read(&sidecar).unwrap();

        assert!(
            install_project_codex_at(home.path(), &path, &home.path().join("bin/pixel"), false)
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), config_before);
        assert_eq!(fs::read(&sidecar).unwrap(), backup_before);
    }

    /// A user hook added *inside* the side build's group is a user edit: the
    /// hand-back admits pixel's group only exactly as pixel wrote it.
    #[test]
    fn project_codex_composition_refuses_a_user_hook_added_inside_a_side_build_group() {
        assert_hand_back_refused_after(|group| {
            group["hooks"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"command","command":"user-audit"}));
        });
    }

    /// A matcher set on the side build's group changes what the composed guard
    /// sees; that is a user edit, not a stale spelling.
    #[test]
    fn project_codex_composition_refuses_a_matcher_set_on_a_side_build_group() {
        assert_hand_back_refused_after(|group| group["matcher"] = json!("*"));
    }

    /// A changed timeout on the side build's entry is a user edit too.
    #[test]
    fn project_codex_composition_refuses_a_changed_timeout_on_a_side_build_entry() {
        assert_hand_back_refused_after(|group| group["hooks"][0]["timeout"] = json!(1));
    }

    /// A foreign executable that merely spells the composed verb is not pixel's,
    /// even with the exact backup argument.
    #[test]
    fn project_codex_composition_refuses_a_foreign_executable_naming_the_composed_verb() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let side = home.path().join("bin/pixel-dev");
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"keep"}]}]}}),
            false,
        )
        .unwrap();
        install_project_codex_at(home.path(), &path, &side, false).unwrap();

        let mut changed = install::read_settings(&path).unwrap();
        let command = changed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .replace(&quoted_executable(&side), "'/usr/bin/notpixel'");
        changed["hooks"]["PreToolUse"][0]["hooks"][0]["command"] = json!(command);
        install::write_settings(&path, &changed, false).unwrap();

        assert!(
            install_project_codex_at(home.path(), &path, &home.path().join("bin/pixel"), false)
                .is_err()
        );
    }

    /// The composed entry must name *this* install's backup; one pointing at
    /// another file is a foreign entry, not a stale spelling.
    #[test]
    fn project_codex_composition_refuses_a_composed_group_naming_another_backup() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("repo/.codex/hooks.json");
        let side = home.path().join("bin/pixel-dev");
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"keep"}]}]}}),
            false,
        )
        .unwrap();
        install_project_codex_at(home.path(), &path, &side, false).unwrap();
        let backup = path.parent().unwrap().join(CODEX_COMPOSED_BACKUP);

        let mut changed = install::read_settings(&path).unwrap();
        let command = changed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .replace(
                &quoted_executable(&backup),
                &quoted_executable(&home.path().join("other-backup.json")),
            );
        changed["hooks"]["PreToolUse"][0]["hooks"][0]["command"] = json!(command);
        install::write_settings(&path, &changed, false).unwrap();

        assert!(
            install_project_codex_at(home.path(), &path, &home.path().join("bin/pixel"), false)
                .is_err()
        );
    }

    fn delegate_guard() -> Value {
        json!({"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude --delegate-rtk"}]})
    }

    /// A home whose `~/.claude/settings.json` holds `pre` and whose backup
    /// holds the exact RTK group.
    fn home_with_backup(pre: &[Value]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(RTK_BACKUP),
            serde_json::to_string(&json!([rtk_group()])).unwrap(),
        )
        .unwrap();
        fs::write(
            Provider::Claude.path(home.path()),
            serde_json::to_string(&json!({"hooks":{"PreToolUse": pre}})).unwrap(),
        )
        .unwrap();
        home
    }

    fn global_install(home: &Path, dry_run: bool) -> Value {
        install_at_scoped(
            home,
            &Provider::Claude.path(home),
            Path::new("/p/pixel"),
            Provider::Claude,
            HookScope::LifecycleOnly,
            &[],
            dry_run,
        )
        .unwrap();
        install::read_settings(&Provider::Claude.path(home)).unwrap()
    }

    /// The backup exists so that a delegation cannot lose RTK. Without one
    /// it is only a record of the past: a user who removed `rtk hook claude`
    /// keeps it removed, and the stale file goes.
    #[test]
    fn global_install_should_not_bring_back_an_rtk_hook_the_user_removed() {
        let home = home_with_backup(&[]);
        let settings = global_install(home.path(), false);
        assert!(
            !settings.to_string().contains("rtk hook claude"),
            "{settings}"
        );
        assert!(!home.path().join(RTK_BACKUP).exists());
    }

    #[test]
    fn global_install_should_restore_a_delegated_rtk_and_retire_its_backup() {
        let home = home_with_backup(&[delegate_guard()]);
        let settings = global_install(home.path(), false);
        assert_eq!(settings["hooks"]["PreToolUse"][0], rtk_group());
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "'/p/pixel' run-hook task-event --provider claude --event pre-tool-use"
        );
        assert!(!home.path().join(RTK_BACKUP).exists());
    }

    #[test]
    fn global_install_dry_run_should_keep_the_backup() {
        let home = home_with_backup(&[]);
        global_install(home.path(), true);
        assert!(home.path().join(RTK_BACKUP).is_file());
    }

    /// A guard that adopts RTK again on the same run still needs the backup.
    #[test]
    fn repo_install_should_keep_the_backup_while_the_guard_delegates() {
        let repo = home_with_backup(&[delegate_guard()]);
        let local = repo.path().join(CLAUDE_LOCAL_SETTINGS);
        fs::rename(Provider::Claude.path(repo.path()), &local).unwrap();
        install_at_scoped(
            repo.path(),
            &local,
            Path::new("/p/pixel"),
            Provider::Claude,
            HookScope::GuardOnly,
            &[],
            false,
        )
        .unwrap();
        let value = install::read_settings(&local).unwrap();
        assert!(
            has_delegate(value["hooks"].as_object().unwrap(), release()),
            "{value}"
        );
        assert_eq!(
            load_rtk_backup(repo.path()).unwrap(),
            vec![rtk_group()],
            "the delegate's backup must survive"
        );
    }

    /// Write `pre` as the `PreToolUse` of `<root>/.claude/settings.local.json`,
    /// the personal file of a repository at `$HOME`.
    fn write_local(root: &Path, pre: &[Value]) {
        fs::write(
            root.join(CLAUDE_LOCAL_SETTINGS),
            serde_json::to_string(&json!({"hooks":{"PreToolUse": pre}})).unwrap(),
        )
        .unwrap();
    }

    fn pixel_command(verb: &str) -> Value {
        json!({"hooks": [{"type": "command", "command": format!("'/p/pixel' {verb}")}]})
    }

    #[test]
    fn devin_context_and_approval_probes_need_pixels_own_command() {
        let exe = Path::new("/p/pixel");
        let prompt = json!({"hooks": {"UserPromptSubmit": [pixel_command("run-hook prompt-submit --provider devin")]}});
        let approval = json!({"hooks": {"PermissionRequest": [{"matcher": "exec", "hooks": [
            {"type": "command", "command": "'/p/pixel' run-hook guard --provider devin"}
        ]}]}});
        assert!(has_pixel_prompt_context(&prompt, exe));
        assert!(has_pixel_permission_approval(&approval, exe));

        for empty in [json!({}), json!({"hooks": {}})] {
            assert!(!has_pixel_prompt_context(&empty, exe), "{empty}");
            assert!(!has_pixel_permission_approval(&empty, exe), "{empty}");
        }
        // Another verb of Pixel's own binary is not the prompt context.
        let guard_only = json!({"hooks": {"UserPromptSubmit": [pixel_command("run-hook guard --provider devin")]}});
        assert!(!has_pixel_prompt_context(&guard_only, exe));
        // A foreign command carrying the same words is not Pixel's hook.
        let foreign_prompt = json!({"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "other run-hook prompt-submit --provider devin"}]}]}});
        assert!(!has_pixel_prompt_context(&foreign_prompt, exe));
        let foreign_approval = json!({"hooks": {"PermissionRequest": [{"matcher": "exec", "hooks": [{"type": "command", "command": "other run-hook guard --provider devin"}]}]}});
        assert!(!has_pixel_permission_approval(&foreign_approval, exe));
        // Only the exec matcher pre-approves a native shell call.
        let read_matcher = json!({"hooks": {"PermissionRequest": [{"matcher": "read", "hooks": [{"type": "command", "command": "'/p/pixel' run-hook guard --provider devin"}]}]}});
        assert!(!has_pixel_permission_approval(&read_matcher, exe));
    }

    #[test]
    fn devin_install_verifies_only_an_untouched_triple_and_repairs_each_group() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".devin")).unwrap();
        let config = repo.join(DEVIN_LOCAL_CONFIG);
        let install = || install_project_devin_at(&repo, release(), false).unwrap();

        assert!(install().summary.contains("configured"));
        assert!(
            install().summary.contains("verified"),
            "an untouched reinstall verifies"
        );
        for event in ["PreToolUse", "UserPromptSubmit", "PermissionRequest"] {
            let mut value: Value =
                serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
            value["hooks"].as_object_mut().unwrap().remove(event);
            fs::write(&config, serde_json::to_string(&value).unwrap()).unwrap();
            let step = install();
            assert!(
                step.summary.contains("configured"),
                "{event} removed: {}",
                step.summary
            );
            assert!(install().summary.contains("verified"), "{event} restored");
        }
    }

    #[test]
    fn orphan_rtk_backup_should_name_a_backup_no_guard_delegates_to() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(orphan_rtk_backup(home.path(), release()), None);

        let home = home_with_backup(&[]);
        assert_eq!(
            orphan_rtk_backup(home.path(), release()),
            Some(home.path().join(RTK_BACKUP))
        );

        let home = home_with_backup(&[delegate_guard()]);
        assert_eq!(orphan_rtk_backup(home.path(), release()), None);
    }

    /// A repository at `$HOME` shares the global backup: a delegate guard in
    /// its `settings.local.json` still needs it, and a file that cannot be
    /// read may hold one.
    #[test]
    fn a_backup_the_home_repository_guard_delegates_to_should_be_kept() {
        let home = home_with_backup(&[]);
        write_local(home.path(), &[delegate_guard()]);
        assert_eq!(orphan_rtk_backup(home.path(), release()), None);
        global_install(home.path(), false);
        assert!(home.path().join(RTK_BACKUP).is_file());

        let home = home_with_backup(&[]);
        fs::write(home.path().join(CLAUDE_LOCAL_SETTINGS), "{ not json").unwrap();
        assert_eq!(orphan_rtk_backup(home.path(), release()), None);
        global_install(home.path(), false);
        assert!(home.path().join(RTK_BACKUP).is_file());
    }

    #[test]
    fn zcode_install_adds_pixel_hooks_and_preserves_foreign_config() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(config::ZCODE_CONFIG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "custom": {"keep": true},
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher":"Write", "hooks":[{"command":"foreign"}]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let exe = Path::new("/tmp/pixel-dev");
        let first = install_zcode_at(home.path(), exe, false).unwrap();
        assert_eq!(first.status, install::CheckStatus::Green);
        let installed = install::read_settings(&path).unwrap();
        assert_eq!(installed["custom"]["keep"], true);
        assert_eq!(
            installed["hooks"]["events"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            installed["hooks"]["events"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        install_zcode_at(home.path(), exe, false).unwrap();
        let rerun = install::read_settings(&path).unwrap();
        assert_eq!(
            rerun["hooks"]["events"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            rerun["hooks"]["events"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn zcode_install_preserves_an_existing_disabled_hooks_setting() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(config::ZCODE_CONFIG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"hooks":{"enabled":false}}"#).unwrap();

        let step = install_zcode_at(home.path(), Path::new("/tmp/pixel-dev"), false).unwrap();
        assert_eq!(step.status, install::CheckStatus::Yellow);
        let installed = install::read_settings(&path).unwrap();
        assert_eq!(installed["hooks"]["enabled"], false);
        assert_eq!(
            installed["hooks"]["events"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
