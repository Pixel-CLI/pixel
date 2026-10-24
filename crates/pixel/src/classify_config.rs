// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The global-only settings of the opt-in classifier (`specs/classify.md`):
//! `classify.{enabled, engine, remote_preset, remote_model, remote_base,
//! ollaya}` and `remote_keys.<preset>`, in `~/.pixel/config.yaml`.
//!
//! Written with the comment-preserving YAML edit of [`crate::config_file`],
//! owner-only. A key is never printed back and never reaches a log
//! (`crate::run::logged_args` masks `config remote-key`).

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::decide_remote::Preset;

/// The global configuration file, `~/.pixel/config.yaml`.
pub fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| pixel_setup::settings::config_path(&PathBuf::from(home)))
}

fn read_config_doc(path: &Path) -> Option<Value> {
    crate::config_file::load(path).ok()
}

/// Reads the global document, `None` when there is none or it cannot be read.
fn global_doc() -> Option<Value> {
    read_config_doc(&global_config_path()?)
}

fn write_doc(path: &Path, mutate: impl FnOnce(&mut Value)) -> Result<(), String> {
    let before = crate::config_file::load(path)?;
    let mut doc = before.clone();
    mutate(&mut doc);
    let rendered = crate::config_file::render(path, &before, &doc)?;
    // The file holds provider keys: every write is owner-only, through no link.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    pixel_io::nofollow::write_replace(path, rendered.as_bytes(), pixel_io::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_global(mutate: impl FnOnce(&mut Value)) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, mutate)
}

/// Makes `doc.classify` a mapping (replacing a scalar that was there).
fn classify_mut(doc: &mut Value) -> &mut Value {
    if !doc.get("classify").is_some_and(Value::is_object) {
        doc["classify"] = json!({});
    }
    &mut doc["classify"]
}

/// The key a `pixel config remote-key` value stands for: `-` reads the first
/// line of `stdin`, which keeps the secret out of shell history and `ps`;
/// anything else is the key itself.
pub fn key_from_arg(
    value: Option<String>,
    stdin: &mut dyn BufRead,
) -> Result<Option<String>, String> {
    if value.as_deref() != Some("-") {
        return Ok(value);
    }
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("remote-key: read stdin: {e}"))?;
    Ok(Some(line.trim().to_string()))
}

/// Global kill switch: checked before classify reads input or opens an engine.
pub fn classify_enabled() -> Result<bool, String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    classify_enabled_in(&crate::config_file::load(&path)?)
}

fn classify_enabled_in(doc: &Value) -> Result<bool, String> {
    match doc.get("classify").and_then(|c| c.get("enabled")) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "classify.enabled must be true or false".into()),
    }
}

/// Saves the global classify switch, keeping engine and credential settings.
pub fn set_classify_enabled(enabled: bool) -> Result<(), String> {
    write_global(|doc| classify_mut(doc)["enabled"] = json!(enabled))
}

fn classify_str(name: &str) -> Option<String> {
    global_doc()?
        .get("classify")?
        .get(name)?
        .as_str()
        .map(str::to_string)
}

/// The engine preference (`auto`, `local`, `remote`, …).
pub fn classify_engine() -> Option<String> {
    classify_str("engine")
}

/// The provider selected by the interactive remote setup.
pub fn classify_remote_preset() -> Option<Preset> {
    Preset::parse_name(&classify_str("remote_preset")?)
}

/// The model chosen alongside the remote provider, if the setup asked for one.
pub fn classify_remote_model() -> Option<String> {
    classify_str("remote_model")
}

/// The endpoint base chosen alongside the remote provider, when the setup
/// routed a provider to another host.
pub fn classify_remote_base() -> Option<String> {
    classify_str("remote_base")
}

/// Stores the remote engine and its provider together, plus the model and
/// endpoint base the provider should run; `None` drops each earlier choice so
/// a stale one cannot leak across a provider switch.
pub fn set_classify_remote_model(
    preset: Preset,
    model: Option<String>,
    base: Option<&str>,
) -> Result<(), String> {
    write_global(|doc| {
        let classify = classify_mut(doc);
        classify["engine"] = json!("remote");
        classify["remote_preset"] = json!(preset.display());
        if let Some(map) = classify.as_object_mut() {
            match model.filter(|m| !m.is_empty()) {
                Some(model) => {
                    map.insert("remote_model".to_string(), json!(model));
                }
                None => {
                    map.remove("remote_model");
                }
            }
            match base.filter(|b| !b.is_empty()) {
                Some(base) => {
                    map.insert("remote_base".to_string(), json!(base));
                }
                None => {
                    map.remove("remote_base");
                }
            }
        }
    })
}

/// The recorded local Ollaya daemon launch (base, model name, env, argv).
pub fn ollaya_launch() -> Option<Value> {
    global_doc()?.get("classify")?.get("ollaya").cloned()
}

/// Persists the classify engine preference.
pub fn set_classify_engine(value: &str) -> Result<(), String> {
    write_global(|doc| classify_mut(doc)["engine"] = Value::String(value.to_string()))
}

/// Persists the local Ollaya server launch record.
pub fn set_ollaya_launch(launch: &Value) -> Result<(), String> {
    write_global(|doc| classify_mut(doc)["ollaya"] = launch.clone())
}

/// The stored API key for a remote decision preset, if the global config
/// carries one. Keys live only in the global configuration under
/// `remote_keys`, never in the repo layer, never echoed back by the CLI.
pub fn remote_key(preset: Preset) -> Option<String> {
    global_doc()?
        .get("remote_keys")?
        .get(preset.display())?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `pixel config remote-key <preset> [key]`: with a value, persist it to the
/// global configuration (owner-only: it holds secrets); without one, report
/// whether a key is stored. `--clear` removes it. The key itself is never
/// printed.
pub fn run_remote_key(preset: Preset, value: Option<String>, clear: bool) -> Result<(), String> {
    let name = preset.display();
    let path = global_config_path().ok_or("no HOME for the global config")?;
    let say = |text: String| crate::operation::print(format_args!("{text}\n"));
    if clear {
        write_doc(&path, |doc| {
            if let Some(keys) = doc.get_mut("remote_keys").and_then(Value::as_object_mut) {
                keys.remove(name);
            }
        })?;
        say(format!(
            "remote-key {name}: cleared, wrote {}",
            path.display()
        ));
        return Ok(());
    }
    match value {
        Some(key) if !key.is_empty() => {
            write_doc(&path, |doc| {
                if !doc.get("remote_keys").is_some_and(Value::is_object) {
                    doc["remote_keys"] = json!({});
                }
                doc["remote_keys"][name] = Value::String(key);
            })?;
            say(format!("remote-key {name}: set, wrote {}", path.display()));
            Ok(())
        }
        Some(_) => Err("remote-key: empty key".to_string()),
        None => {
            let state = if remote_key(preset).is_some() {
                "set"
            } else {
                "unset"
            };
            say(format!("remote-key {name}: {state}, {}", path.display()));
            Ok(())
        }
    }
}
