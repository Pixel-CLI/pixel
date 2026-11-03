// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel-plugin.toml`: what a plugin directory declares about itself.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};
use std::sync::{Mutex, OnceLock, PoisonError};

use toml_edit::{DocumentMut, Item};

use crate::Error;

/// The manifest's file name, at the root of a plugin directory.
pub const MANIFEST_FILE: &str = "pixel-plugin.toml";

/// The plugin API this host speaks; a manifest must declare exactly it.
pub const API_VERSION: i64 = 1;

/// Longest plugin name; it becomes a directory name and a `pixel-<name>` binary.
const MAX_NAME_LEN: usize = 64;
/// Largest manifest read, 16 KiB, pixel-mvp's bound: the largest this
/// repository ships (`probes/_template`) is 744 bytes.
pub const MAX_MANIFEST_BYTES: u64 = 16_384;

/// Largest skill file read, 64 KiB: one text file a plugin may ship for an
/// agent to load on demand, which `pixel install` delivers. The core `pixel`
/// skill is 2.6 KiB and the largest skill this repository ships
/// (`pixel-classify`) is 8.9 KiB.
pub const MAX_SKILL_BYTES: u64 = 65_536;

/// What a plugin can be asked to do. API 1 knows two capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `pixel <name> [args…]` runs the plugin.
    Command,
    /// The prompt-submit brief runs the plugin and merges the blocks it
    /// prints into the brief (the `[brief-probe]` table says when).
    BriefProbe,
}

impl Capability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::BriefProbe => "brief-probe",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "command" => Some(Self::Command),
            "brief-probe" => Some(Self::BriefProbe),
            _ => None,
        }
    }
}

/// Longest wall time a probe may ask for.
pub const MAX_PROBE_BUDGET_MS: u64 = 2_500;

/// Where a probe's blocks go relative to the brief's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Before the brief's blocks.
    Lead,
    /// After them.
    Append,
}

/// The `[brief-probe]` table: when a probe runs and how long it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub owner: String,
    /// Run for these question kinds; with `when_prompt`, either is enough.
    pub when_kinds: Vec<String>,
    /// Never run for these kinds.
    pub skip_kinds: Vec<String>,
    /// Run when the prompt matches this regex (searched, not anchored).
    pub when_prompt: Option<String>,
    pub when_gate_closed: bool,
    pub budget_ms: u64,
    pub mode: ProbeMode,
}

impl ProbeSpec {
    /// Whether the probe runs for a brief of `kind` (`None`: the brief did
    /// not settle on one) asked `prompt`. A kind in `skip_kinds` never runs
    /// it; with neither `when_kinds` nor `when_prompt` it always runs.
    pub fn applies(&self, kind: Option<&str>, prompt: &str) -> bool {
        if kind.is_some_and(|k| self.skip_kinds.iter().any(|s| s == k)) {
            return false;
        }
        if self.when_kinds.is_empty() && self.when_prompt.is_none() {
            return true;
        }
        kind.is_some_and(|k| self.when_kinds.iter().any(|w| w == k))
            || self
                .when_prompt
                .as_deref()
                .and_then(compiled)
                .is_some_and(|re| re.is_match(prompt))
    }
}

/// `pattern` compiled once per process: every brief asks each probe and step
/// whether it applies, and compiling their patterns afresh took 76 ms of a
/// debug build's window. `None` for a pattern that does not compile.
fn compiled(pattern: &str) -> Option<regex::Regex> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<regex::Regex>>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache
        .entry(pattern.to_owned())
        .or_insert_with(|| regex::Regex::new(pattern).ok())
        .clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub api: i64,
    pub description: String,
    /// Relative path of the executable, inside the plugin directory.
    pub run: String,
    pub capabilities: Vec<Capability>,
    /// A plugin that reaches the network needs `pixel plugin enable`.
    pub network: bool,
    /// The commands one binary serves. Empty: the plugin serves the command
    /// named like itself and is run as `<run> <args…>`. Non-empty: it serves
    /// exactly these and is run as `<run> <command> <args…>`.
    pub commands: Vec<String>,
    /// The optional agent skill: one path inside the plugin directory whose
    /// text `pixel install` delivers as `skills/<plugin>/SKILL.md` under
    /// every configured harness root. Read, never written, by the host.
    pub skill: Option<String>,
    /// The `[brief-probe]` table, present exactly when the plugin has the
    /// `brief-probe` capability.
    pub probe: Option<ProbeSpec>,
}

impl Manifest {
    pub fn has_command(&self) -> bool {
        self.capabilities.contains(&Capability::Command)
    }

    pub fn is_probe(&self) -> bool {
        self.capabilities.contains(&Capability::BriefProbe)
    }

    /// The `pixel <command>` words this plugin answers to.
    pub fn serves(&self) -> Vec<&str> {
        if self.commands.is_empty() {
            vec![self.name.as_str()]
        } else {
            self.commands.iter().map(String::as_str).collect()
        }
    }

    /// Whether `pixel <command>` runs this plugin.
    pub fn serves_command(&self, command: &str) -> bool {
        self.serves().contains(&command)
    }
}

/// A name usable as a directory and as `pixel-<name>` on PATH.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse and validate manifest text. The error is the reason, without a path.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let doc: DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    let string = |key: &str| -> Result<String, String> {
        doc.get(key)
            .and_then(Item::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{key}` is required and must be a string"))
    };
    let name = string("name")?;
    if !valid_name(&name) {
        return Err(Error::InvalidName(name).to_string());
    }
    let version = string("version")?;
    if version.trim().is_empty() {
        return Err("`version` must not be empty".to_owned());
    }
    let api = doc
        .get("api")
        .and_then(Item::as_integer)
        .ok_or_else(|| "`api` is required and must be an integer".to_owned())?;
    if api != API_VERSION {
        return Err(format!(
            "`api` = {api}, but this pixel speaks plugin API {API_VERSION}"
        ));
    }
    let description = string("description")?;
    let run = string("run")?;
    path_is_relative("run", &run)?;
    let capabilities = parse_capabilities(&doc)?;
    let network = match doc.get("network") {
        None => false,
        Some(item) => item
            .as_bool()
            .ok_or_else(|| "`network` must be true or false".to_owned())?,
    };
    let commands = parse_commands(&doc)?;
    let skill = parse_skill(&doc)?;
    let probe = if capabilities.contains(&Capability::BriefProbe) {
        if network {
            return Err(
                "a brief probe uses no network: `network` must be absent or false".to_owned(),
            );
        }
        Some(parse_probe(&doc)?)
    } else if doc.get("brief-probe").is_some() {
        return Err("a `[brief-probe]` table needs the \"brief-probe\" capability".to_owned());
    } else {
        None
    };
    Ok(Manifest {
        name,
        version,
        api,
        description,
        run,
        capabilities,
        network,
        commands,
        skill,
        probe,
    })
}

/// The `[brief-probe]` table of a plugin with the `brief-probe` capability.
fn parse_probe(doc: &DocumentMut) -> Result<ProbeSpec, String> {
    let table = doc
        .get("brief-probe")
        .and_then(Item::as_table)
        .ok_or_else(|| "the \"brief-probe\" capability needs a `[brief-probe]` table".to_owned())?;
    for (key, _) in table.iter() {
        if ![
            "owner",
            "when_kinds",
            "skip_kinds",
            "when_prompt",
            "when_gate_closed",
            "budget_ms",
            "mode",
        ]
        .contains(&key)
        {
            return Err(format!("[brief-probe] has an unknown key `{key}`"));
        }
    }
    let owner = table
        .get("owner")
        .and_then(Item::as_str)
        .filter(|owner| !owner.trim().is_empty())
        .ok_or_else(|| "[brief-probe] `owner` is required and must be a string".to_owned())?
        .to_owned();
    let list = |key: &str| -> Result<Vec<String>, String> {
        let Some(item) = table.get(key) else {
            return Ok(Vec::new());
        };
        item.as_array()
            .and_then(|array| {
                array
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .ok_or_else(|| format!("[brief-probe] `{key}` must be an array of strings"))
    };
    let when_prompt = match table.get("when_prompt") {
        None => None,
        Some(item) => {
            let pattern = item
                .as_str()
                .ok_or_else(|| "[brief-probe] `when_prompt` must be a string".to_owned())?;
            regex::Regex::new(pattern)
                .map_err(|e| format!("[brief-probe] `when_prompt` is not a regex: {e}"))?;
            Some(pattern.to_owned())
        }
    };
    let when_gate_closed = match table.get("when_gate_closed") {
        None => false,
        Some(item) => item
            .as_bool()
            .ok_or_else(|| "[brief-probe] `when_gate_closed` must be a boolean".to_owned())?,
    };
    if when_gate_closed && when_prompt.as_deref().is_none_or(str::is_empty) {
        return Err(
            "[brief-probe] `when_gate_closed` requires a nonempty `when_prompt`".to_owned(),
        );
    }
    let budget_ms = table
        .get("budget_ms")
        .and_then(Item::as_integer)
        .and_then(|ms| u64::try_from(ms).ok())
        .filter(|ms| (1..=MAX_PROBE_BUDGET_MS).contains(ms))
        .ok_or_else(|| {
            format!(
                "[brief-probe] `budget_ms` is required, an integer from 1 to {MAX_PROBE_BUDGET_MS}"
            )
        })?;
    let mode = match table.get("mode").and_then(Item::as_str) {
        Some("lead") => ProbeMode::Lead,
        Some("append") | None => ProbeMode::Append,
        Some(other) => {
            return Err(format!(
                "[brief-probe] `mode` must be \"lead\" or \"append\", not \"{other}\""
            ));
        }
    };
    Ok(ProbeSpec {
        owner,
        when_kinds: list("when_kinds")?,
        skip_kinds: list("skip_kinds")?,
        when_prompt,
        when_gate_closed,
        budget_ms,
        mode,
    })
}

/// The optional `commands` array: command names, each a valid plugin name
/// (it is looked up as `pixel <name>`), without repeats.
fn parse_commands(doc: &DocumentMut) -> Result<Vec<String>, String> {
    let Some(item) = doc.get("commands") else {
        return Ok(Vec::new());
    };
    let list = item
        .as_array()
        .ok_or_else(|| "`commands` must be an array of command names".to_owned())?;
    let mut out: Vec<String> = Vec::new();
    for entry in list {
        let text = entry
            .as_str()
            .ok_or_else(|| "`commands` must hold strings only".to_owned())?;
        if !valid_name(text) {
            return Err(format!(
                "`commands` holds \"{text}\", which is not a command name: use lowercase letters, digits, `-` and `_`"
            ));
        }
        if out.iter().any(|seen| seen == text) {
            return Err(format!("`commands` lists \"{text}\" twice"));
        }
        out.push(text.to_owned());
    }
    Ok(out)
}

/// The optional `skill` key: one file, inside the plugin directory, whose
/// text is delivered to the harness roots. Its shape is checked here; the
/// file itself (regular, no link, at most [`MAX_SKILL_BYTES`], UTF-8) is
/// read by [`read_skill`] when the plugin is installed.
fn parse_skill(doc: &DocumentMut) -> Result<Option<String>, String> {
    let Some(item) = doc.get("skill") else {
        return Ok(None);
    };
    let skill = item
        .as_str()
        .ok_or_else(|| "`skill` must be a string".to_owned())?;
    path_is_relative("skill", skill)?;
    Ok(Some(skill.to_owned()))
}

fn parse_capabilities(doc: &DocumentMut) -> Result<Vec<Capability>, String> {
    let list = doc
        .get("capabilities")
        .and_then(Item::as_array)
        .ok_or_else(|| "`capabilities` is required and must be an array of strings".to_owned())?;
    let mut out = Vec::new();
    for entry in list {
        let text = entry
            .as_str()
            .ok_or_else(|| "`capabilities` must hold strings only".to_owned())?;
        let capability = Capability::parse(text).ok_or_else(|| {
            format!(
                "unknown capability \"{text}\": plugin API 1 knows \"command\" and \"brief-probe\""
            )
        })?;
        if !out.contains(&capability) {
            out.push(capability);
        }
    }
    Ok(out)
}

/// A manifest key naming a path inside the plugin directory: never empty,
/// never absolute, never `..`. `run`'s resolved path is checked again at run
/// time and `skill`'s file when it is read; this is the early, readable
/// refusal.
fn path_is_relative(key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("`{key}` must not be empty"));
    }
    let path = Path::new(value);
    let escapes = path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return Err(format!(
            "`{key}` = \"{value}\" must be a relative path inside the plugin directory"
        ));
    }
    Ok(())
}

/// Read `<dir>/pixel-plugin.toml`, at most [`MAX_MANIFEST_BYTES`] of it,
/// opened without following a link or blocking (`file::open_regular`): the
/// prompt-submit brief reads every repo plugin's manifest, so one committed
/// as a link to `/dev/zero` or `/dev/tty` must be refused, not read.
/// `Ok(None)` when the file is absent (the directory is not a plugin); a
/// manifest that exists but is a link, oversized, invalid, or names another
/// plugin than its directory, is an error.
pub fn load(dir: &Path, expected_name: Option<&str>) -> Result<Option<Manifest>, Error> {
    let path = dir.join(MANIFEST_FILE);
    let file = match crate::file::open_regular(&path, None) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // A link, a FIFO, a device or a directory at the manifest's name is
        // that plugin's invalid manifest, like an oversized one: `list` shows
        // it under its row instead of failing for every plugin beside it.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            return Err(Error::Manifest {
                path,
                message: e.to_string(),
            });
        }
        Err(e) => return Err(Error::io(format!("read {}", path.display()), e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io(format!("read {}", path.display()), e))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::Manifest {
            path,
            message: format!("larger than {MAX_MANIFEST_BYTES} bytes"),
        });
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(Error::Manifest {
            path,
            message: "not UTF-8".to_owned(),
        });
    };
    let manifest = parse(&text).map_err(|message| Error::Manifest {
        path: path.clone(),
        message,
    })?;
    if let Some(expected) = expected_name
        && manifest.name != expected
    {
        return Err(Error::Manifest {
            path,
            message: format!(
                "`name` = \"{}\" does not match its directory \"{expected}\"",
                manifest.name
            ),
        });
    }
    Ok(Some(manifest))
}

/// Read the file `manifest.skill` names, under `dir`: the text `pixel
/// install` delivers to the harness roots. Opened like the manifest itself
/// (`file::open_regular`: never a link, never blocking, a regular file), so
/// a skill committed as a link to a file elsewhere or to `/dev/tty` cannot
/// put that file's content, or an endless read, where the agent's skill is
/// expected; at most [`MAX_SKILL_BYTES`] of it, UTF-8.
///
/// # Errors
///
/// [`Error::BadSkill`], naming the plugin, the key and why the file cannot
/// be delivered. A manifest without the key reads as `Ok(None)`.
pub fn read_skill(dir: &Path, manifest: &Manifest) -> Result<Option<String>, Error> {
    let Some(skill) = &manifest.skill else {
        return Ok(None);
    };
    let bad = |why: String| Error::BadSkill {
        name: manifest.name.clone(),
        skill: skill.clone(),
        why,
    };
    let path = dir.join(skill);
    let file = match crate::file::open_regular(&path, None) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(bad("does not exist".to_owned()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => return Err(bad(e.to_string())),
        Err(e) => return Err(Error::io(format!("read {}", path.display()), e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SKILL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io(format!("read {}", path.display()), e))?;
    if bytes.len() as u64 > MAX_SKILL_BYTES {
        return Err(bad(format!("is larger than {MAX_SKILL_BYTES} bytes")));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(bad("is not UTF-8".to_owned()));
    };
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
name = "flow"
version = "1.2.3"
api = 1
description = "Replay flows"
run = "bin/pixel-flow"
capabilities = ["command"]
network = true
"#;

    #[test]
    fn a_complete_manifest_parses_to_every_field() {
        let m = parse(GOOD).unwrap();
        assert_eq!(
            m,
            Manifest {
                name: "flow".into(),
                version: "1.2.3".into(),
                api: 1,
                description: "Replay flows".into(),
                run: "bin/pixel-flow".into(),
                capabilities: vec![Capability::Command],
                network: true,
                commands: vec![],
                skill: None,
                probe: None,
            }
        );
        assert!(m.has_command());
        assert_eq!(m.serves(), ["flow"]);
        assert!(m.serves_command("flow") && !m.serves_command("ultraflow"));
    }

    const PROBE: &str = r#"
name = "history-fallback"
version = "0.1.0"
api = 1
description = "d"
run = "probe"
capabilities = ["brief-probe"]

[brief-probe]
owner = "pixel"
when_kinds = ["history"]
skip_kinds = ["overview"]
when_prompt = "(?i)\\bwhen\\b"
budget_ms = 800
mode = "lead"
"#;

    #[test]
    fn a_brief_probe_manifest_parses_its_table_and_decides_when_it_runs() {
        let m = parse(PROBE).unwrap();
        assert!(m.is_probe() && !m.has_command());
        let spec = m.probe.unwrap();
        assert!(!spec.when_gate_closed, "the omitted option defaults off");
        assert_eq!(
            (spec.owner.as_str(), spec.budget_ms, spec.mode),
            ("pixel", 800, ProbeMode::Lead)
        );
        assert!(spec.applies(Some("history"), "anything"));
        assert!(
            spec.applies(Some("lookup"), "so WHEN did it land"),
            "the prompt regex is enough"
        );
        assert!(!spec.applies(Some("lookup"), "where is it"));
        assert!(
            !spec.applies(Some("overview"), "when"),
            "a skipped kind never runs it"
        );
        assert!(
            !spec.applies(None, "where is it"),
            "no kind: only the regex can run it"
        );
        let always = parse(
            &PROBE
                .replace("when_kinds = [\"history\"]\n", "")
                .replace("when_prompt = \"(?i)\\\\bwhen\\\\b\"\n", ""),
        )
        .unwrap();
        assert!(always.probe.unwrap().applies(None, "x"));
    }

    #[test]
    fn a_closed_gate_probe_parses_only_with_a_nonempty_prompt_pattern() {
        let enabled = PROBE.replace(
            "when_prompt = \"(?i)\\\\bwhen\\\\b\"",
            "when_prompt = \"(?i)\\\\bwhen\\\\b\"\nwhen_gate_closed = true",
        );
        let spec = parse(&enabled).unwrap().probe.unwrap();
        assert!(spec.when_gate_closed);

        let without_prompt = PROBE
            .replace("when_prompt = \"(?i)\\\\bwhen\\\\b\"\n", "")
            .replace(
                "budget_ms = 800",
                "when_gate_closed = true\nbudget_ms = 800",
            );
        let error = parse(&without_prompt).unwrap_err();
        assert!(
            error.contains("requires a nonempty `when_prompt`"),
            "{error}"
        );

        let empty_prompt = PROBE
            .replace("when_prompt = \"(?i)\\\\bwhen\\\\b\"", "when_prompt = \"\"")
            .replace(
                "budget_ms = 800",
                "when_gate_closed = true\nbudget_ms = 800",
            );
        let error = parse(&empty_prompt).unwrap_err();
        assert!(
            error.contains("requires a nonempty `when_prompt`"),
            "{error}"
        );
    }

    #[test]
    fn a_broken_probe_table_is_refused_with_its_reason() {
        for (from, to, expect) in [
            ("budget_ms = 800", "budget_ms = 5000", "budget_ms"),
            ("budget_ms = 800", "budget_ms = 2501", "budget_ms"),
            ("budget_ms = 800", "budget_ms = 0", "budget_ms"),
            ("budget_ms = 800\n", "", "budget_ms"),
            ("owner = \"pixel\"\n", "", "owner"),
            ("mode = \"lead\"", "mode = \"middle\"", "mode"),
            (
                "budget_ms = 800",
                "budget_ms = 800\nwhen_gate_closed = \"true\"",
                "when_gate_closed` must be a boolean",
            ),
            (
                "when_kinds = [\"history\"]",
                "when_kinds = \"history\"",
                "when_kinds",
            ),
            (
                "budget_ms = 800",
                "budget_ms = 800\nnetwork = true",
                "unknown key",
            ),
            (
                "when_prompt = \"(?i)\\\\bwhen\\\\b\"",
                "when_prompt = \"(\"",
                "not a regex",
            ),
        ] {
            let err = parse(&PROBE.replace(from, to)).unwrap_err();
            assert!(err.contains(expect), "{from} -> {to}: {err}");
        }
        let no_table = PROBE.split("[brief-probe]").next().unwrap();
        assert!(
            parse(no_table)
                .unwrap_err()
                .contains("needs a `[brief-probe]` table")
        );
        let stray = GOOD.to_owned() + "\n[brief-probe]\nowner = \"x\"\nbudget_ms = 1\n";
        assert!(
            parse(&stray)
                .unwrap_err()
                .contains("needs the \"brief-probe\" capability")
        );
    }

    #[test]
    fn a_commands_list_replaces_the_plugin_name_as_what_it_serves() {
        let text = GOOD.replace("network = true", "commands = [\"commit\", \"push\"]");
        let m = parse(&text).unwrap();
        assert_eq!(m.commands, ["commit", "push"]);
        assert_eq!(m.serves(), ["commit", "push"]);
        assert!(m.serves_command("push"));
        assert!(
            !m.serves_command("flow"),
            "the plugin name is not a command then"
        );
    }

    #[test]
    fn a_malformed_commands_list_is_refused_with_its_reason() {
        for (value, expect) in [
            ("\"commit\"", "must be an array"),
            ("[1]", "strings only"),
            ("[\"Bad Name\"]", "not a command name"),
            ("[\"../x\"]", "not a command name"),
            ("[\"a\", \"a\"]", "twice"),
        ] {
            let text = GOOD.replace("network = true", &format!("commands = {value}"));
            let err = parse(&text).unwrap_err();
            assert!(err.contains(expect), "{value}: {err}");
        }
        let empty = GOOD.replace("network = true", "commands = []");
        assert_eq!(parse(&empty).unwrap().serves(), ["flow"]);
    }

    #[test]
    fn network_defaults_to_false_and_capabilities_may_be_empty() {
        let text = GOOD
            .replace("network = true", "")
            .replace("[\"command\"]", "[]");
        let m = parse(&text).unwrap();
        assert!(!m.network);
        assert!(m.capabilities.is_empty());
        assert!(!m.has_command());
    }

    #[test]
    fn duplicate_capabilities_collapse() {
        let text = GOOD.replace("[\"command\"]", "[\"command\", \"command\"]");
        assert_eq!(
            parse(&text).unwrap().capabilities,
            vec![Capability::Command]
        );
    }

    #[test]
    fn each_required_field_is_named_when_missing() {
        for key in [
            "name",
            "version",
            "api",
            "description",
            "run",
            "capabilities",
        ] {
            let text: String = GOOD
                .lines()
                .filter(|l| !l.starts_with(&format!("{key} =")))
                .collect::<Vec<_>>()
                .join("\n");
            let err = parse(&text).unwrap_err();
            assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        }
    }

    #[test]
    fn only_api_one_is_accepted() {
        for api in ["0", "2", "\"1\""] {
            let err = parse(&GOOD.replace("api = 1", &format!("api = {api}"))).unwrap_err();
            assert!(err.contains("`api`"), "{api}: {err}");
        }
        let err = parse(&GOOD.replace("api = 1", "api = 2")).unwrap_err();
        assert!(err.contains("`api` = 2") && err.contains("API 1"), "{err}");
    }

    #[test]
    fn an_unknown_capability_is_refused_by_name() {
        let err =
            parse(&GOOD.replace("[\"command\"]", "[\"command\", \"chain-step\"]")).unwrap_err();
        assert!(err.contains("\"chain-step\""), "{err}");
        let err = parse(&GOOD.replace("[\"command\"]", "[1]")).unwrap_err();
        assert!(err.contains("strings only"), "{err}");
    }

    #[test]
    fn run_must_stay_relative_and_inside() {
        for run in ["/bin/sh", "../x", "a/../../x", "./ok/../../x"] {
            let err = parse(&GOOD.replace("bin/pixel-flow", run)).unwrap_err();
            assert!(err.contains("relative path"), "{run}: {err}");
        }
        assert!(parse(&GOOD.replace("bin/pixel-flow", "./run.sh")).is_ok());
        let err = parse(&GOOD.replace("bin/pixel-flow", "")).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn a_blank_version_and_a_non_boolean_network_are_refused() {
        let err = parse(&GOOD.replace("1.2.3", "  ")).unwrap_err();
        assert!(err.contains("`version`"), "{err}");
        let err = parse(&GOOD.replace("network = true", "network = \"yes\"")).unwrap_err();
        assert!(err.contains("`network`"), "{err}");
    }

    #[test]
    fn invalid_toml_is_reported_as_such() {
        assert!(parse("name = ").unwrap_err().contains("not valid TOML"));
    }

    #[test]
    fn names_are_directory_safe() {
        for good in ["flow", "a", "0", "plan-rollback", "a_b", &"a".repeat(64)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Flow",
            "-x",
            "_x",
            "a b",
            "a/b",
            "..",
            "a.b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        let err = parse(&GOOD.replace("\"flow\"", "\"../x\"")).unwrap_err();
        assert!(err.contains("invalid plugin name"), "{err}");
    }

    #[test]
    fn load_distinguishes_absent_invalid_and_misnamed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), None).unwrap(), None);

        std::fs::write(dir.path().join(MANIFEST_FILE), GOOD).unwrap();
        assert_eq!(
            load(dir.path(), Some("flow")).unwrap().unwrap().name,
            "flow"
        );
        let err = load(dir.path(), Some("other")).unwrap_err().to_string();
        assert!(
            err.contains("does not match its directory \"other\""),
            "{err}"
        );

        std::fs::write(dir.path().join(MANIFEST_FILE), "api = 9").unwrap();
        let err = load(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains(MANIFEST_FILE), "{err}");
    }

    // A repository chooses what its `.pixel/plugins/` holds, and every
    // prompt reads its manifests: a manifest linked to a valid one outside
    // the plugin would make the plugin declare what it does not hold, and
    // one linked to `/dev/zero` would grow the brief's memory without end.
    #[test]
    fn a_manifest_reached_through_a_link_is_refused_and_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.toml");
        std::fs::write(&elsewhere, GOOD).unwrap();
        let plugin = dir.path().join("flow");
        std::fs::create_dir(&plugin).unwrap();
        for target in [elsewhere.as_path(), Path::new("/dev/zero")] {
            let link = plugin.join(MANIFEST_FILE);
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let err = load(&plugin, Some("flow")).unwrap_err().to_string();
            assert!(err.contains("is a symbolic link"), "{target:?}: {err}");
        }
    }

    #[test]
    fn a_manifest_is_read_up_to_the_bound_and_refused_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE);
        let at_bound = |len: u64| {
            let mut text = GOOD.to_owned();
            text.push('#');
            let pad = usize::try_from(len).unwrap() - text.len() - 1;
            text.push_str(&"x".repeat(pad));
            text.push('\n');
            assert_eq!(text.len() as u64, len);
            text
        };
        std::fs::write(&path, at_bound(MAX_MANIFEST_BYTES)).unwrap();
        assert_eq!(load(dir.path(), None).unwrap().unwrap().name, "flow");
        std::fs::write(&path, at_bound(MAX_MANIFEST_BYTES + 1)).unwrap();
        let err = load(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains("larger than 16384 bytes"), "{err}");
        std::fs::write(&path, b"name = \"\xff\"").unwrap();
        let err = load(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains("not UTF-8"), "{err}");
    }

    #[test]
    fn a_manifest_may_declare_one_skill_and_defaults_to_none() {
        let text = GOOD.replace(
            "network = true",
            "skill = \"skills/pixel-flow/SKILL.md\"\nnetwork = true",
        );
        assert_eq!(
            parse(&text).unwrap().skill.as_deref(),
            Some("skills/pixel-flow/SKILL.md")
        );
        assert_eq!(parse(GOOD).unwrap().skill, None, "the key is optional");
    }

    #[test]
    fn a_skill_must_name_one_relative_file() {
        for skill in ["/etc/SKILL.md", "../x", "a/../../x", "./ok/../../x"] {
            let text = GOOD.replace("network = true", &format!("skill = \"{skill}\""));
            let err = parse(&text).unwrap_err();
            assert!(
                err.contains(&format!("`skill` = \"{skill}\"")) && err.contains("relative path"),
                "{skill}: {err}"
            );
        }
        assert!(parse(&GOOD.replace("network = true", "skill = \"./SKILL.md\"")).is_ok());
        for (value, expect) in [
            ("1", "`skill` must be a string"),
            ("\"\"", "`skill` must not be empty"),
        ] {
            let text = GOOD.replace("network = true", &format!("skill = {value}"));
            let err = parse(&text).unwrap_err();
            assert!(err.contains(expect), "{value}: {err}");
        }
    }

    #[test]
    fn read_skill_reads_the_declared_file_and_none_without_the_key() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_skill(dir.path(), &parse(GOOD).unwrap()).unwrap(),
            None,
            "a manifest without the key has nothing to read"
        );

        let manifest = parse(&GOOD.replace("network = true", "skill = \"SKILL.md\"")).unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "know-how\n").unwrap();
        assert_eq!(
            read_skill(dir.path(), &manifest).unwrap().as_deref(),
            Some("know-how\n")
        );
    }

    // A repository or a source directory holds whatever it holds: the file
    // the key names may be absent, a link to a device or to a file elsewhere,
    // not text, or larger than any skill needs to be.
    #[test]
    fn read_skill_refuses_an_undeclarable_file_naming_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        let manifest = parse(&GOOD.replace("network = true", "skill = \"SKILL.md\"")).unwrap();

        let err = read_skill(dir.path(), &manifest).unwrap_err().to_string();
        assert_eq!(err, "plugin `flow`: `skill` = \"SKILL.md\" does not exist");

        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "secret").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        let err = read_skill(dir.path(), &manifest).unwrap_err().to_string();
        assert!(err.contains("is a symbolic link"), "{err}");

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"name = \"\xff\"").unwrap();
        let err = read_skill(dir.path(), &manifest).unwrap_err().to_string();
        assert!(err.contains("is not UTF-8"), "{err}");

        let at = usize::try_from(MAX_SKILL_BYTES).unwrap();
        std::fs::write(&path, "x".repeat(at)).unwrap();
        assert_eq!(
            read_skill(dir.path(), &manifest)
                .unwrap()
                .as_deref()
                .map(str::len),
            Some(at),
            "a file exactly at the cap is read"
        );
        std::fs::write(&path, "x".repeat(at + 1)).unwrap();
        let err = read_skill(dir.path(), &manifest).unwrap_err().to_string();
        assert!(err.contains("is larger than 65536 bytes"), "{err}");
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    #[test]
    fn manifest_io_errors_should_remain_io_errors_instead_of_invalid_manifest_errors() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, "content").unwrap();
        assert!(matches!(load(&file, None), Err(Error::Io { .. })));
    }
}
