// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Configuration storage: YAML reads and comment-preserving YAML writes of
//! the nested, global-only keys (`classify`, `remote_keys`); the flat
//! settings are edited as text by `pixel_setup::settings`.

use std::path::Path;
use std::str::FromStr;

use serde_json::{Value, json};

const TEMPLATE: &str = pixel_setup::settings::TEMPLATE;

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
    let value: Value = serde_saphyr::from_str(text).map_err(|_| {
        format!(
            "invalid configuration {}; expected a YAML mapping",
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

pub fn render(path: &Path, before: &Value, after: &Value) -> Result<String, String> {
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
    fn rendering_should_not_treat_an_unreadable_path_as_an_empty_template() {
        let error =
            render(&std::env::temp_dir(), &json!({}), &json!({"metrics":"off"})).unwrap_err();
        assert!(error.starts_with("read "));
    }
}
