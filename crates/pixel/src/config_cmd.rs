// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel config`: the settings in force and where each value comes from, or a
//! set or unset in the repository's `.pixel/config.yaml` or the global
//! `~/.pixel/config.yaml` (`pixel_setup::settings`).

use std::path::Path;

use pixel_setup::settings::{self, Resolved, Setting, Settings, Source};
use serde_json::{Value, json};

use crate::classify_config;
use crate::cli::ConfigCmd;
use crate::client;
use crate::commands::Done;
use crate::decide_remote::Preset;

/// What `pixel config` was asked.
pub(crate) struct Request<'a> {
    pub key: Option<&'a str>,
    pub value: Option<&'a str>,
    pub global: bool,
    pub unset: bool,
    pub path: &'a Path,
    pub json: bool,
}

/// The setting a key names, or the error that lists the keys there are.
fn setting_of(key: &str) -> Result<Setting, String> {
    Setting::from_key(key).ok_or_else(|| {
        let known: Vec<&str> = Setting::ALL.iter().map(|setting| setting.key()).collect();
        format!(
            "unknown setting {key:?}; the settings are {}",
            known.join(", ")
        )
    })
}

fn entry_json(entry: &Resolved) -> Value {
    json!({
        "key": entry.setting.key(),
        "value": entry.value.to_string(),
        "source": entry.source.to_string(),
        "env": entry.setting.env(),
        "about": entry.setting.about(),
    })
}

/// The table `pixel config` prints: key, value, then where the value comes from.
fn table(settings: &Settings) -> String {
    let mut out = String::new();
    for entry in &settings.entries {
        let name = entry.setting.key();
        let value = entry.value.to_string();
        let source = entry.source.to_string();
        out.push_str(&format!("{name:<18} {value:<4} {source}\n"));
        out.push_str(&format!("{:<18} {}\n", "", entry.setting.about()));
    }
    out
}

/// A note when a layer above the one just written still decides the value.
fn shadowed(settings: &Settings, setting: Setting, written: &Source) -> Option<String> {
    let entry = settings.entry(setting);
    (entry.source != *written).then(|| {
        format!(
            "{} is still {} here: {} takes precedence",
            setting.key(),
            entry.value,
            entry.source
        )
    })
}

pub(crate) fn run(request: &Request) -> Result<Done, String> {
    let root = client::discover_root(request.path).ok();
    let repo = if request.global {
        None
    } else {
        root.as_deref()
    };
    let settings = crate::settings::load(repo);
    let notes = settings.problems.clone();
    let with_notes = |done: Done| {
        notes
            .iter()
            .cloned()
            .fold(done, |done, note| done.note(Some(note)))
    };

    let Some(key) = request.key else {
        let classify = classify_config::classify_enabled().unwrap_or(false);
        let engine = classify_config::classify_engine();
        let value = json!({
            "settings": settings.entries.iter().map(entry_json).collect::<Vec<_>>(),
            "classify": {"enabled": classify, "engine": engine},
            "problems": settings.problems,
        });
        let mut text = table(&settings);
        text.push_str(&format!(
            "{:<18} {:<4} global only; engine {} (`pixel config setup`)\n",
            "classify",
            if classify { "on" } else { "off" },
            engine.as_deref().unwrap_or("auto"),
        ));
        return Ok(with_notes(Done::new(value, text, request.json)));
    };
    let setting = setting_of(key)?;

    if request.value.is_none() && !request.unset {
        let entry = settings.entry(setting);
        let text = format!("{}\n", entry.value);
        return Ok(with_notes(Done::new(entry_json(entry), text, request.json)));
    }

    let dir = if request.global {
        crate::settings::home()
    } else {
        root.clone().ok_or_else(|| {
            "not inside a repository; pass --global to change the global settings".to_owned()
        })?
    };
    let path = settings::config_path(&dir);
    let written = if request.global {
        Source::Global(path.clone())
    } else {
        Source::Repo(path.clone())
    };
    let mut note = None;
    let text = if let Some(raw) = request.value {
        let value = settings::write(&dir, setting, raw)?;
        note = shadowed(&crate::settings::load(root.as_deref()), setting, &written);
        format!("{} {value} ({})\n", setting.key(), path.display())
    } else if settings::remove(&dir, setting)? {
        format!("{} unset ({})\n", setting.key(), path.display())
    } else {
        format!("{} was not set in {}\n", setting.key(), path.display())
    };
    let after = crate::settings::load(repo);
    let mut done = Done::new(entry_json(after.entry(setting)), text, request.json).note(note);
    if setting == Setting::BriefParts && request.value.is_some() {
        done = done.note(Some(
            "run `pixel setup` so the hooks carry the new count".to_owned(),
        ));
    }
    Ok(with_notes(done))
}

/// The classifier's own subcommands of `pixel config`: the global switch, the
/// engine, the remote preset and its key, and the guided setup.
pub(crate) fn classify_run(cmd: &ConfigCmd) -> Result<Done, String> {
    let said = |text: String| {
        Ok(Done::new(
            json!({"stored": text}),
            format!("{text}\n"),
            false,
        ))
    };
    match cmd {
        ConfigCmd::Setup => setup(),
        ConfigCmd::Classify { value } => {
            classify_config::set_classify_enabled(value == "on")?;
            said(format!("classify: {value}"))
        }
        ConfigCmd::RemoteKey {
            preset,
            value,
            clear,
        } => {
            let key = classify_config::key_from_arg(value.clone(), &mut std::io::stdin().lock())?;
            classify_config::run_remote_key(*preset, key, *clear)?;
            Ok(Done::new(json!({}), String::new(), false))
        }
        ConfigCmd::RemotePreset {
            preset,
            model,
            base,
        } => {
            classify_config::set_classify_remote_model(*preset, model.clone(), base.as_deref())?;
            said(format!(
                "classify remote preset: {} stored",
                preset.display()
            ))
        }
        ConfigCmd::ClassifyEngine { value } => {
            // `jev` and the `clef-*` names are remote engines on that preset.
            let hosted = Preset::parse_name(value)
                .filter(|preset| *preset == Preset::Jev || preset.is_clef());
            if let Some(preset) = hosted {
                classify_config::set_classify_remote_model(preset, None, None)?;
                said(format!(
                    "classify engine: remote ({}) stored",
                    preset.display()
                ))
            } else {
                classify_config::set_classify_engine(value)?;
                said(format!("classify engine: {value} stored"))
            }
        }
    }
}

/// `pixel config setup`: ask whether the classifier is allowed and, when it is,
/// let `classify_setup` choose and install an engine. A failed first-time
/// installation does not leave the classifier enabled.
#[cfg_attr(test, mutants::skip)] // terminal adapter; the policy is `setup_with`
fn setup() -> Result<Done, String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(
            "setup needs a terminal; edit ~/.pixel/config.yaml or run pixel config classify off"
                .into(),
        );
    }
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stderr().lock();
    let message = setup_with(&mut input, &mut output, |input, output| {
        crate::classify_setup::install_step(true, input, output)
    })?;
    Ok(Done::new(
        json!({"stored": message}),
        format!("{message}\n"),
        false,
    ))
}

fn setup_with(
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    install: impl FnOnce(&mut dyn std::io::BufRead, &mut dyn std::io::Write) -> Result<(), String>,
) -> Result<String, String> {
    let was_enabled = classify_config::classify_enabled().unwrap_or(false);
    writeln!(
        output,
        "Classify is optional AI classification, separate from code search: a bounded question about \
         text, with probabilities for your labels. Local runs offline after setup; remote providers \
         receive your text and may charge."
    )
    .map_err(|e| e.to_string())?;
    let enabled = pixel_setup::setup::wizard::ask(input, output, "Allow pixel classify?", true)
        .map_err(|e| e.to_string())?;
    classify_config::set_classify_enabled(enabled)?;
    if !enabled {
        return Ok("classify: off".to_owned());
    }
    if let Err(error) = install(input, output) {
        if !was_enabled {
            classify_config::set_classify_enabled(false).map_err(|rollback| {
                format!("{error}; could not disable classification: {rollback}")
            })?;
        }
        return Err(error);
    }
    Ok("classify: on".to_owned())
}
