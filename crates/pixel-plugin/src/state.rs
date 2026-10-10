// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The two machine-wide files the host owns under `~/.pixel/`:
//! `trusted.toml` (which repo plugin content may run) and `plugins.toml`
//! (the network opt-in list and where each plugin was installed from).

use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::Error;

/// `~/.pixel/trusted.toml`.
pub const TRUST_FILE: &str = "trusted.toml";
/// `~/.pixel/plugins.toml`.
pub const CONFIG_FILE: &str = "plugins.toml";

/// Read a TOML file; an absent file is an empty document, a file that does
/// not parse is an error (a store that cannot be read must not read as "no
/// restrictions" or "nothing trusted" by accident).
fn load(path: &Path) -> Result<DocumentMut, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.parse().map_err(|e| {
            Error::State(format!(
                "{} is not valid TOML ({e}); fix or delete it",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(Error::io(format!("read {}", path.display()), e)),
    }
}

/// Write beside the target, then rename over it: a reader never sees half a file.
fn save(path: &Path, doc: &DocumentMut) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::State(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::io(format!("create {}", parent.display()), e))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| Error::io(format!("stage {}", path.display()), e))?;
    std::io::Write::write_all(&mut staged, doc.to_string().as_bytes())
        .map_err(|e| Error::io(format!("write {}", path.display()), e))?;
    staged
        .persist(path)
        .map_err(|e| Error::io(format!("replace {}", path.display()), e.error))?;
    Ok(())
}

/// Trust records: one `[[trusted]]` per plugin name, holding the digest of
/// the directory content the user reviewed.
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Whether `name` is recorded with exactly this digest.
    pub fn is_trusted(&self, name: &str, digest: &str) -> Result<bool, Error> {
        let doc = load(&self.path)?;
        let Some(entries) = doc.get("trusted").and_then(Item::as_array_of_tables) else {
            return Ok(false);
        };
        Ok(entries.iter().any(|entry| {
            entry.get("name").and_then(Item::as_str) == Some(name)
                && entry.get("sha256").and_then(Item::as_str) == Some(digest)
        }))
    }

    /// Record `name` at `digest`, replacing what was recorded for that name:
    /// one reviewed version per plugin, so an old one cannot come back.
    pub fn trust(&self, name: &str, digest: &str) -> Result<(), Error> {
        let mut doc = load(&self.path)?;
        let mut entries = ArrayOfTables::new();
        if let Some(existing) = doc.get("trusted").and_then(Item::as_array_of_tables) {
            for entry in existing
                .iter()
                .filter(|e| e.get("name").and_then(Item::as_str) != Some(name))
            {
                entries.push(entry.clone());
            }
        }
        let mut entry = Table::new();
        entry["name"] = value(name);
        entry["sha256"] = value(digest);
        entries.push(entry);
        doc["trusted"] = Item::ArrayOfTables(entries);
        save(&self.path, &doc)
    }
}

/// `plugins.toml`: `enabled = ["name", …]` and a `[sources]` table.
pub struct Config {
    path: PathBuf,
}

impl Config {
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn is_enabled(&self, name: &str) -> Result<bool, Error> {
        Ok(self.enabled()?.iter().any(|n| n == name))
    }

    pub fn enabled(&self) -> Result<Vec<String>, Error> {
        let doc = load(&self.path)?;
        Ok(doc
            .get("enabled")
            .and_then(Item::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn enable(&self, name: &str) -> Result<(), Error> {
        let mut doc = load(&self.path)?;
        let mut names = self.enabled()?;
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
        doc["enabled"] = value(names.into_iter().collect::<Array>());
        save(&self.path, &doc)
    }

    /// Where `name` was installed from, as recorded by `add`.
    pub fn source(&self, name: &str) -> Result<Option<String>, Error> {
        let doc = load(&self.path)?;
        Ok(doc
            .get("sources")
            .and_then(Item::as_table)
            .and_then(|t| t.get(name))
            .and_then(Item::as_str)
            .map(str::to_owned))
    }

    pub fn record_source(&self, name: &str, source: &str) -> Result<(), Error> {
        let mut doc = load(&self.path)?;
        if doc.get("sources").and_then(Item::as_table).is_none() {
            doc["sources"] = Item::Table(Table::new());
        }
        doc["sources"][name] = value(source);
        save(&self.path, &doc)
    }

    /// Drop `name` from the enable list and the sources (plugin removed).
    pub fn forget(&self, name: &str) -> Result<(), Error> {
        let mut doc = load(&self.path)?;
        let kept: Vec<String> = self.enabled()?.into_iter().filter(|n| n != name).collect();
        if doc.get("enabled").is_some() {
            doc["enabled"] = value(kept.into_iter().collect::<Array>());
        }
        if let Some(sources) = doc.get_mut("sources").and_then(Item::as_table_mut) {
            sources.remove(name);
        }
        save(&self.path, &doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_trusted_before_the_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::at(dir.path().join(TRUST_FILE));
        assert!(!store.is_trusted("x", "aa").unwrap());
    }

    #[test]
    fn a_record_trusts_exactly_that_name_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(TRUST_FILE);
        let store = TrustStore::at(&path);
        store.trust("x", "aa").unwrap();
        assert!(store.is_trusted("x", "aa").unwrap());
        assert!(!store.is_trusted("x", "bb").unwrap(), "another digest");
        assert!(!store.is_trusted("y", "aa").unwrap(), "another name");
    }

    #[test]
    fn re_trusting_replaces_the_old_digest_and_keeps_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::at(dir.path().join(TRUST_FILE));
        store.trust("x", "aa").unwrap();
        store.trust("y", "cc").unwrap();
        store.trust("x", "bb").unwrap();
        assert!(!store.is_trusted("x", "aa").unwrap(), "old version revoked");
        assert!(store.is_trusted("x", "bb").unwrap());
        assert!(store.is_trusted("y", "cc").unwrap());
        let text = std::fs::read_to_string(dir.path().join(TRUST_FILE)).unwrap();
        assert_eq!(text.matches("[[trusted]]").count(), 2, "{text}");
    }

    #[test]
    fn an_unreadable_trust_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TRUST_FILE);
        std::fs::write(&path, "[[trusted\n").unwrap();
        let err = TrustStore::at(&path).is_trusted("x", "aa").unwrap_err();
        assert!(err.to_string().contains("not valid TOML"), "{err}");
        assert!(TrustStore::at(&path).trust("x", "aa").is_err());
    }

    #[test]
    fn enabling_is_per_name_idempotent_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let config = Config::at(&path);
        assert!(!config.is_enabled("a").unwrap());
        config.enable("a").unwrap();
        config.enable("b").unwrap();
        config.enable("a").unwrap();
        assert_eq!(config.enabled().unwrap(), ["a", "b"]);
        assert!(config.is_enabled("a").unwrap());
        assert!(!config.is_enabled("c").unwrap());
    }

    #[test]
    fn sources_are_recorded_per_plugin_and_forgotten_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let config = Config::at(&path);
        assert_eq!(config.source("a").unwrap(), None);
        config.record_source("a", "https://example.test/a").unwrap();
        config.record_source("b", "/tmp/b").unwrap();
        config.enable("a").unwrap();
        assert_eq!(
            config.source("a").unwrap().as_deref(),
            Some("https://example.test/a")
        );
        config.forget("a").unwrap();
        assert_eq!(config.source("a").unwrap(), None);
        assert!(!config.is_enabled("a").unwrap());
        assert_eq!(config.source("b").unwrap().as_deref(), Some("/tmp/b"));
    }

    #[test]
    fn forgetting_a_plugin_that_was_never_recorded_changes_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let config = Config::at(&path);
        config.enable("keep").unwrap();
        config.forget("other").unwrap();
        assert_eq!(config.enabled().unwrap(), ["keep"]);
    }
}
