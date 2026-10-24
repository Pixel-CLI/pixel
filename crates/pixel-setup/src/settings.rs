// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The typed settings of pixel (`specs/state-and-threats.md`, `config.yaml`).
//!
//! Five keys, each with a default, an optional environment override and a
//! kind (a switch, or a count). A value comes from the first layer that sets
//! it: the environment, the repository's `.pixel/config.yaml`, the global
//! `~/.pixel/config.yaml`, the default. A layer that cannot be read is reported
//! (`Settings::problems`) and skipped, never guessed at.
//!
//! The file is edited as text so the comments a person wrote stay: a set
//! replaces the key's line or appends one, an unset removes it.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::{BRIEF_PARTS_DEFAULT, BRIEF_PARTS_KEY, BRIEF_PARTS_MAX, load_config};

/// Where a layer keeps its settings, below the layer's directory.
const CONFIG_FILE: &str = ".pixel/config.yaml";
/// Mode of a settings file: the global one may hold credentials later.
const CONFIG_MODE: u32 = 0o600;
/// What a settings file pixel creates starts with: every setting documented and
/// commented out.
pub const TEMPLATE: &str = include_str!("config.yaml");

/// One setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// The 🟩 lines (the accounting is never switched off).
    Metrics,
    /// Whether a command may start the repository's daemon.
    DaemonAutoStart,
    /// Whether the prompt brief is computed at all.
    Brief,
    /// How many hooks carry the prompt brief.
    BriefParts,
    /// Whether the edit-time and stop-time checks run.
    EditHooks,
}

impl Setting {
    /// Every setting, in the order `pixel config` lists them.
    pub const ALL: [Self; 5] = [
        Self::Metrics,
        Self::DaemonAutoStart,
        Self::Brief,
        Self::BriefParts,
        Self::EditHooks,
    ];

    /// The key as the file and the command spell it.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::DaemonAutoStart => "daemon_auto_start",
            Self::Brief => "brief",
            Self::BriefParts => BRIEF_PARTS_KEY,
            Self::EditHooks => "edit_hooks",
        }
    }

    /// The environment variable that overrides the setting, if it has one.
    #[must_use]
    pub const fn env(self) -> Option<&'static str> {
        match self {
            Self::Metrics => Some("PIXEL_METRICS"),
            Self::DaemonAutoStart => Some("PIXEL_DAEMON_AUTO_START"),
            Self::Brief => Some("PIXEL_BRIEF"),
            Self::BriefParts => None,
            Self::EditHooks => Some("PIXEL_EDIT_HOOKS"),
        }
    }

    /// What the setting is when no layer sets it.
    #[must_use]
    pub const fn default_value(self) -> Value {
        match self {
            Self::Metrics | Self::DaemonAutoStart | Self::Brief => Value::Switch(true),
            Self::BriefParts => Value::Count(BRIEF_PARTS_DEFAULT),
            Self::EditHooks => Value::Switch(false),
        }
    }

    /// One line on what the setting does.
    #[must_use]
    pub const fn about(self) -> &'static str {
        match self {
            Self::Metrics => "show the 🟩 lines (the accounting stays on)",
            Self::DaemonAutoStart => "let a command start the repository's daemon",
            Self::Brief => "compute the prompt brief",
            Self::BriefParts => "hooks that carry the brief, 1 to 8 (run `pixel setup` after)",
            Self::EditHooks => "run the edit-time and stop-time checks",
        }
    }

    /// The setting a key names.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|setting| setting.key() == key)
    }

    /// `text` as a value of this setting, as a person or the environment
    /// writes it: a switch is `on`/`off` (also `true`/`false`, `1`/`0`,
    /// `yes`/`no`), a count a whole number in range. `None` when it is neither.
    #[must_use]
    pub fn parse(self, text: &str) -> Option<Value> {
        let text = text.trim().to_ascii_lowercase();
        match self {
            Self::BriefParts => text
                .parse::<u8>()
                .ok()
                .filter(|count| (1..=BRIEF_PARTS_MAX).contains(count))
                .map(Value::Count),
            _ => match text.as_str() {
                "on" | "true" | "1" | "yes" => Some(Value::Switch(true)),
                "off" | "false" | "0" | "no" => Some(Value::Switch(false)),
                _ => None,
            },
        }
    }

    /// A YAML value read back. A count outside `1..=8` is clamped into it; a
    /// value of another shape counts as unset.
    fn read_yaml(self, value: &serde_json::Value) -> Option<Value> {
        match (self, value) {
            (Self::BriefParts, serde_json::Value::Number(number)) => {
                let count = number.as_u64()?.clamp(1, u64::from(BRIEF_PARTS_MAX));
                u8::try_from(count).ok().map(Value::Count)
            }
            (Self::BriefParts, _) => None,
            (_, serde_json::Value::Bool(on)) => Some(Value::Switch(*on)),
            (_, serde_json::Value::String(text)) => self.parse(text),
            (_, serde_json::Value::Number(number)) => match number.as_u64()? {
                0 => Some(Value::Switch(false)),
                1 => Some(Value::Switch(true)),
                _ => None,
            },
            _ => None,
        }
    }
}

/// A setting's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// `on` or `off`.
    Switch(bool),
    /// A whole number.
    Count(u8),
}

impl Value {
    /// The value as a YAML scalar.
    fn yaml(self) -> String {
        match self {
            Self::Switch(on) => on.to_string(),
            Self::Count(count) => count.to_string(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Switch(true) => f.write_str("on"),
            Self::Switch(false) => f.write_str("off"),
            Self::Count(count) => write!(f, "{count}"),
        }
    }
}

/// Which layer a value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The environment variable of that name.
    Env(&'static str),
    /// The repository's settings file.
    Repo(PathBuf),
    /// The global settings file.
    Global(PathBuf),
    /// Nothing set it.
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "environment {name}"),
            Self::Repo(path) | Self::Global(path) => write!(f, "{}", path.display()),
            Self::Default => f.write_str("default"),
        }
    }
}

/// A setting with the value in force and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub setting: Setting,
    pub value: Value,
    pub source: Source,
}

/// The five settings as they stand for one repository.
#[derive(Clone, Debug)]
pub struct Settings {
    /// One entry per [`Setting::ALL`], in that order.
    pub entries: Vec<Resolved>,
    /// Layers that could not be read, one line each.
    pub problems: Vec<String>,
}

impl Settings {
    /// Resolves every setting: `env` answers the override variables, `home`
    /// holds the global layer and `repo` (when given) the repository's.
    #[must_use]
    pub fn load(home: &Path, repo: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut problems = Vec::new();
        let mut layer = |dir: &Path| match load_config(dir) {
            Ok(document) => Some(document),
            Err(problem) => {
                problems.push(problem);
                None
            }
        };
        let repo_layer = repo.and_then(|dir| layer(dir).map(|doc| (dir, doc)));
        let global_layer = layer(home).map(|doc| (home, doc));
        let entries = Setting::ALL
            .into_iter()
            .map(|setting| {
                let from_env = setting.env().and_then(|name| {
                    let value = setting.parse(&env(name)?)?;
                    Some((value, Source::Env(name)))
                });
                let from_file = |layer: Option<&(&Path, serde_json::Value)>,
                                 source: fn(PathBuf) -> Source| {
                    let (dir, document) = layer?;
                    let value = setting.read_yaml(document.get(setting.key())?)?;
                    Some((value, source(config_path(dir))))
                };
                let (value, source) = from_env
                    .or_else(|| from_file(repo_layer.as_ref(), Source::Repo))
                    .or_else(|| from_file(global_layer.as_ref(), Source::Global))
                    .unwrap_or((setting.default_value(), Source::Default));
                Resolved {
                    setting,
                    value,
                    source,
                }
            })
            .collect();
        Self { entries, problems }
    }

    /// The entry of `setting`.
    #[must_use]
    pub fn entry(&self, setting: Setting) -> &Resolved {
        self.entries
            .iter()
            .find(|entry| entry.setting == setting)
            .expect("load resolves every setting")
    }

    /// A switch's state; a count reads as on.
    #[must_use]
    pub fn on(&self, setting: Setting) -> bool {
        !matches!(self.entry(setting).value, Value::Switch(false))
    }

    /// A count's value; a switch reads as its default count.
    #[must_use]
    pub fn count(&self, setting: Setting) -> u8 {
        match self.entry(setting).value {
            Value::Count(count) => count,
            Value::Switch(_) => BRIEF_PARTS_DEFAULT,
        }
    }
}

/// The settings file of the layer whose directory is `dir`.
#[must_use]
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

/// Sets `setting` to `raw` in the settings file of `dir` and returns the value
/// written.
///
/// # Errors
///
/// A value the setting does not take, a file that is not a YAML mapping (it is
/// left as it is), or a write the filesystem refuses, including one through a
/// link.
pub fn write(dir: &Path, setting: Setting, raw: &str) -> Result<Value, String> {
    let value = setting.parse(raw).ok_or_else(|| match setting {
        Setting::BriefParts => format!(
            "{} takes a whole number from 1 to {BRIEF_PARTS_MAX}, not {raw:?}",
            setting.key()
        ),
        _ => format!("{} takes on or off, not {raw:?}", setting.key()),
    })?;
    let text = read_text(dir)?;
    store(dir, &with_line(&text, setting.key(), &value.yaml()))?;
    Ok(value)
}

/// Removes `setting` from the settings file of `dir`; whether it was there.
///
/// # Errors
///
/// A file that is not a YAML mapping, or a write the filesystem refuses.
pub fn remove(dir: &Path, setting: Setting) -> Result<bool, String> {
    let text = read_text(dir)?;
    if !text.lines().any(|line| is_entry(line, setting.key())) {
        return Ok(false);
    }
    store(dir, &without_line(&text, setting.key()))?;
    Ok(true)
}

/// The text of the settings file of `dir`, empty when there is none. Refuses a
/// file that does not parse as a mapping, so an edit never lands on one.
fn read_text(dir: &Path) -> Result<String, String> {
    load_config(dir)?;
    let path = config_path(dir);
    let mut text = String::new();
    match pixel_io::nofollow::open_read(&path) {
        Ok(mut file) => {
            file.read_to_string(&mut text)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    Ok(text)
}

/// Replaces the settings file of `dir` with `text`, owner-only, through no link.
fn store(dir: &Path, text: &str) -> Result<(), String> {
    let path = config_path(dir);
    let fail = |error: std::io::Error| format!("write {}: {error}", path.display());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(fail)?;
    }
    pixel_io::nofollow::write_replace(&path, text.as_bytes(), CONFIG_MODE).map_err(fail)
}

/// Whether `line` is the top-level `key:` entry.
fn is_entry(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

/// `text` with the `key:` line set to `value`: replaced in place when there is
/// one, appended otherwise (after a header when the file is new).
fn with_line(text: &str, key: &str, value: &str) -> String {
    let entry = format!("{key}: {value}");
    if text.lines().any(|line| is_entry(line, key)) {
        let mut replaced = false;
        let mut out = String::new();
        for line in text.lines() {
            if !replaced && is_entry(line, key) {
                out.push_str(&entry);
                replaced = true;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        return out;
    }
    let mut out = if text.is_empty() {
        format!("{TEMPLATE}\n")
    } else {
        text.to_owned()
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&entry);
    out.push('\n');
    out
}

/// `text` without its `key:` line.
fn without_line(text: &str, key: &str) -> String {
    text.lines()
        .filter(|line| !is_entry(line, key))
        .fold(String::new(), |mut out, line| {
            out.push_str(line);
            out.push('\n');
            out
        })
}

#[cfg(test)]
mod tests;
