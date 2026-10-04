// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Configuration storage with legacy JSON support and comment-preserving YAML writes.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_json::{Value, json};

pub const FILE_NAME: &str = "config.yaml";
/// The JSON file read until an install or an edit writes the YAML one.
pub const LEGACY_FILE_NAME: &str = "config.json";
const TEMPLATE: &str = include_str!("config.yaml");

pub fn preferred_path(directory: &Path) -> PathBuf {
    let yaml = directory.join(FILE_NAME);
    let legacy = directory.join(LEGACY_FILE_NAME);
    if !yaml.exists() && legacy.exists() {
        legacy
    } else {
        yaml
    }
}

pub fn load(path: &Path) -> Result<Value, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => return Err(format!("cannot read configuration {}", path.display())),
    };
    parse(&text, path)
}

fn parse(text: &str, path: &Path) -> Result<Value, String> {
    // Parser diagnostics can include source lines containing credentials.
    let value: Value = if path.extension().is_some_and(|ext| ext == "json") {
        serde_json::from_str(text).map_err(|_| ())
    } else {
        serde_saphyr::from_str(text).map_err(|_| ())
    }
    .map_err(|()| {
        format!(
            "invalid configuration {}; expected a YAML/JSON mapping",
            path.display()
        )
    })?;
    if value.is_null() {
        return Ok(json!({}));
    }
    if !value.is_object() {
        return Err(format!(
            "configuration {} must be a mapping",
            path.display()
        ));
    }
    Ok(value)
}

/// Create a template or copy legacy values once; never overwrite an existing YAML file.
pub fn ensure(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let legacy = load(&path.with_extension("json"))?;
    let values = if legacy == json!({}) {
        String::new()
    } else {
        serde_saphyr::to_string(&legacy).map_err(|_| "cannot serialize configuration")?
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    finish_create(
        path,
        create_private(path, format!("{TEMPLATE}\n{values}").as_bytes()),
    )
}

/// A concurrent creator wins without clobbering; every other write error remains an error.
fn finish_create(path: &Path, result: std::io::Result<()>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("create {}: {e}", path.display())),
    }
}

pub fn create_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

pub fn render(path: &Path, before: &Value, after: &Value) -> Result<String, String> {
    if path.extension().is_some_and(|ext| ext == "json") {
        return Ok(format!("{after}\n"));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let file = yaml_edit::YamlFile::from_str(&text)
        .map_err(|_| format!("cannot edit YAML in {}", path.display()))?;
    // An all-comment template has no document yet. Its entire text remains the prefix.
    let Some(doc) = file.documents().next() else {
        return Ok(format!(
            "{text}\n{}",
            serde_saphyr::to_string(after).map_err(|_| "cannot serialize configuration")?
        ));
    };
    if let Some(mapping) = doc.as_mapping() {
        patch_mapping(&mapping, before, after)?;
    } else {
        return Err(format!(
            "configuration {} must be a mapping",
            path.display()
        ));
    }
    let result = file.to_string();
    if parse(&result, path)? != *after {
        return Err("YAML edit did not preserve configuration values; file unchanged".into());
    }
    Ok(result)
}

fn patch_mapping(
    mapping: &yaml_edit::Mapping,
    before: &Value,
    after: &Value,
) -> Result<(), String> {
    let old = before.as_object().ok_or("expected mapping")?;
    let new = after.as_object().ok_or("expected mapping")?;
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        mapping.remove(key.as_str());
    }
    for (key, value) in new {
        if old.get(key) == Some(value) {
            continue;
        }
        if let Some(node) = mapping.get(key.as_str())
            && let Some(child) = node.as_mapping()
            && old.get(key).is_some_and(Value::is_object)
            && value.is_object()
        {
            patch_mapping(child, &old[key], value)?;
        } else {
            // JSON is YAML flow syntax and safely quotes every string, including secrets.
            let replacement = yaml_edit::Document::from_str(&format!("value: {value}\n"))
                .map_err(|_| "cannot encode configuration value")?;
            mapping.set(
                key.as_str(),
                replacement.get("value").ok_or("missing encoded value")?,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_should_accept_a_concurrent_winner_but_propagate_write_failures() {
        let path = Path::new("config.yaml");
        assert_eq!(finish_create(path, Ok(())), Ok(()));
        assert_eq!(
            finish_create(path, Err(std::io::ErrorKind::AlreadyExists.into())),
            Ok(())
        );
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::WriteZero,
        ] {
            assert!(
                finish_create(path, Err(kind.into()))
                    .unwrap_err()
                    .contains("create config.yaml")
            );
        }
    }

    #[test]
    fn rendering_should_not_treat_an_unreadable_path_as_an_empty_template() {
        let error =
            render(&std::env::temp_dir(), &json!({}), &json!({"metrics":"off"})).unwrap_err();
        assert!(error.starts_with("read "));
    }
}
