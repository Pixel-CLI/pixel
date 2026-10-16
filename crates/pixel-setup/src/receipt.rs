// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ownership receipt for setup: restore unchanged writes and keep later user edits.

use crate::Result;
use crate::install::{CheckStatus, InstallStep};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const STATE: &str = ".pixel/setup-receipt.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Entry {
    Directory,
    File { bytes: Vec<u8>, mode: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
struct Owned {
    original: Option<Entry>,
    installed: String,
}

type Receipt = BTreeMap<PathBuf, Owned>;

/// A snapshot limited to setup's output paths, never a scan of the user's home.
#[derive(Debug)]
pub struct Snapshot {
    home: PathBuf,
    roots: Vec<PathBuf>,
    entries: BTreeMap<PathBuf, Entry>,
}

fn roots(home: &Path, explicit: bool) -> Vec<PathBuf> {
    let mut roots = vec![
        home.join(".claude/settings.json"),
        home.join(crate::config::DEVIN_CONFIG_DIR)
            .join(crate::config::DEVIN_CONFIG_FILE),
        crate::codex_config::codex_home(home, explicit).join("hooks.json"),
        home.join(crate::config::GEMINI_SETTINGS_FILE),
        home.join(".gemini/config/config.json"),
        home.join(".gemini/config/hooks.json"),
        home.join(".gemini/config/import_manifest.json"),
        crate::antigravity::plugin_dir(home),
        crate::antigravity::cli_plugin_dir(home),
        crate::opencode_config::opencode_config_dir(home, explicit).join("plugins/pixel-brief.js"),
        home.join(crate::pi_global::PACKAGE_DIR),
        home.join(crate::CLASSIFY_PACKAGE_DIR),
        home.join(".pixel/config.yaml"),
    ];
    let pi = crate::pi_global::PiPaths::resolve(home, explicit);
    roots.push(pi.settings);
    roots.push(pi.agent_dir.join("skills/pixel"));
    for harness in [".claude"].iter().chain(crate::config::SKILL_ROOTS) {
        for skill in ["pixel", "pixel-classify"] {
            roots.push(home.join(harness).join("skills").join(skill));
        }
    }
    for name in crate::bundled_probes::names() {
        roots.push(home.join(".pixel/plugins").join(name));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn read(path: &Path) -> Result<Option<Entry>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        return Ok(Some(Entry::Directory));
    }
    Ok(Some(Entry::File {
        bytes: fs::read(path)?,
        mode: fs::metadata(path)?.permissions().mode() & 0o777,
    }))
}

fn collect(path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
    if let Some(entry) = read(path)? {
        if entry == Entry::Directory {
            for child in fs::read_dir(path)? {
                let child = child?;
                if !child.file_name().to_string_lossy().contains(".pixel-bak.") {
                    collect(&child.path(), entries)?;
                }
            }
        }
        entries.insert(path.to_path_buf(), entry);
    }
    Ok(())
}

fn fingerprint(entry: &Entry) -> String {
    match entry {
        Entry::Directory => "directory".into(),
        Entry::File { bytes, mode } => format!(
            "{mode:o}:{}",
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

impl Snapshot {
    /// Start a global CLI setup receipt; previews and repository installs write no receipt.
    pub fn begin(options: &crate::install::InstallOptions) -> Result<Option<Self>> {
        if options.repo.is_some() || options.dry_run {
            return Ok(None);
        }
        let home = options
            .home
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or(crate::InstallError::NoHome)?;
        Self::capture(&home, options.home.is_some()).map(Some)
    }

    /// Capture only known output files, their owned trees and existing parent directories.
    pub fn capture(home: &Path, explicit: bool) -> Result<Self> {
        let roots = roots(home, explicit);
        let mut entries = BTreeMap::new();
        for root in &roots {
            collect(root, &mut entries)?;
            for parent in root
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != home && parent.parent().is_some())
            {
                if parent.is_dir() {
                    entries.insert(parent.to_path_buf(), Entry::Directory);
                }
            }
        }
        Ok(Self {
            home: home.to_path_buf(),
            roots,
            entries,
        })
    }

    /// Record changed outputs, retaining the original ownership across reinstalls.
    pub fn finish(self) -> Result<()> {
        let home = &self.home;
        let path = home.join(STATE);
        let mut receipt: Receipt = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        let mut after = BTreeMap::new();
        for root in &self.roots {
            collect(root, &mut after)?;
            for parent in root
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != home && parent.parent().is_some())
            {
                if parent.is_dir() {
                    after.insert(parent.to_path_buf(), Entry::Directory);
                }
            }
        }
        for (path, entry) in after {
            if self.entries.get(&path) == Some(&entry) {
                continue;
            }
            let installed = fingerprint(&entry);
            receipt
                .entry(path.clone())
                .and_modify(|owned| owned.installed.clone_from(&installed))
                .or_insert_with(|| Owned {
                    original: self.entries.get(&path).cloned(),
                    installed,
                });
        }
        if receipt.is_empty() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&receipt)?;
        if fs::read(&path).is_ok_and(|current| current == bytes) {
            return Ok(());
        }
        fs::create_dir_all(path.parent().expect("receipt parent"))?;
        let temporary = crate::install::unique_temp_path(&path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

/// Undo only unchanged writes from a receipt; older installations use the legacy removers.
pub(crate) fn remove(
    home: &Path,
    explicit: bool,
    dry_run: bool,
) -> Result<Option<Vec<InstallStep>>> {
    let path = home.join(STATE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    let allowed = roots(home, explicit);
    let mut steps = Vec::new();
    let mut directories = Vec::new();
    // Validate the entire receipt before the first mutation, including lexical traversal.
    for file in receipt.keys() {
        if !allowed
            .iter()
            .any(|root| file.starts_with(root) || root.starts_with(file))
            || file == home
            || file.parent().is_none()
            || file
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(crate::InstallError::Setup(
                "receipt names a path outside setup outputs".into(),
            ));
        }
    }
    for (file, owned) in &receipt {
        if owned.installed == "directory" {
            if owned.original.is_none() {
                directories.push(file);
            }
            continue;
        }
        let current = read(file)?;
        let changed = current
            .as_ref()
            .is_some_and(|entry| fingerprint(entry) != owned.installed);
        if !dry_run && !changed && current.is_some() {
            if let Some(Entry::File { bytes, mode }) = &owned.original {
                crate::config::backup_if_changing(file, bytes)?;
                let resolved = crate::install::resolve_symlink_target(file);
                let target = resolved.as_deref().unwrap_or(file);
                let temporary = crate::install::unique_temp_path(target);
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                output.write_all(bytes)?;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(*mode))?;
                fs::rename(temporary, target)?;
            } else {
                fs::remove_file(file)?;
            }
        }
        steps.push(InstallStep {
            id: "setup-ownership".into(),
            status: if changed {
                CheckStatus::Yellow
            } else {
                CheckStatus::Green
            },
            summary: crate::install::dry_run_summary(
                dry_run,
                if changed {
                    "kept a file changed after setup"
                } else {
                    "undid an unchanged setup write"
                },
            ),
            detail: Some(file.display().to_string()),
        });
    }
    if !dry_run {
        fs::remove_file(path)?;
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            match fs::remove_dir(directory) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(Some(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_should_restore_owned_outputs_but_keep_later_content_or_mode_edits() {
        for edit in ["none", "content", "mode"] {
            let home = tempfile::tempdir().unwrap();
            let config = home.path().join(".claude/settings.json");
            fs::create_dir_all(home.path().join(".claude/skills")).unwrap();
            fs::write(&config, "{ \"mine\": true }\n").unwrap();
            fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
            let before = Snapshot::capture(home.path(), true).unwrap();
            let expected = before.entries.clone();
            let options = crate::install::InstallOptions {
                home: Some(home.path().into()),
                executable_path: Some("/tmp/pixel".into()),
                ..Default::default()
            };
            let selection = crate::install::InstallSelection {
                agents: vec![crate::setup::AgentTarget::ClaudeCode],
                task_hooks: true,
                ..Default::default()
            };
            crate::install::install_selected(&options, Some(&selection)).unwrap();
            before.finish().unwrap();
            let first = fs::read(home.path().join(STATE)).unwrap();
            let again = Snapshot::begin(&options).unwrap().unwrap();
            crate::install::install_selected(&options, Some(&selection)).unwrap();
            again.finish().unwrap();
            assert_eq!(
                fs::read(home.path().join(STATE)).unwrap(),
                first,
                "reinstall retains original ownership"
            );
            let skill = home.path().join(".claude/skills/pixel/SKILL.md");
            if edit == "content" {
                fs::write(&skill, "my changes").unwrap();
            }
            if edit == "mode" {
                fs::set_permissions(&skill, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let kept = read(&skill).unwrap();
            let report = crate::uninstall::uninstall(&crate::uninstall::UninstallOptions {
                home: Some(home.path().into()),
                binary_path: Some(home.path().join("absent")),
                executable_path: Some("/tmp/pixel".into()),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(fs::read_to_string(&config).unwrap(), "{ \"mine\": true }\n");
            assert_eq!(
                fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o640
            );
            if edit == "none" {
                assert_eq!(
                    Snapshot::capture(home.path(), true).unwrap().entries,
                    expected
                );
            } else {
                assert_eq!(read(&skill).unwrap(), kept);
                assert!(
                    report
                        .steps
                        .iter()
                        .any(|step| step.status == CheckStatus::Yellow
                            && step.detail.as_deref() == skill.to_str())
                );
            }
            assert!(!home.path().join(STATE).exists());
        }
    }

    #[test]
    fn remove_should_validate_every_path_before_mutating_any_file() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude/settings.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::create_dir_all(home.path().join(".pixel")).unwrap();
        fs::write(&config, "unchanged").unwrap();
        let mut receipt = Receipt::new();
        receipt.insert(
            config.clone(),
            Owned {
                original: None,
                installed: fingerprint(&read(&config).unwrap().unwrap()),
            },
        );
        receipt.insert(
            home.path().join(".pixel/plugins/../../outside"),
            Owned {
                original: None,
                installed: "bad".into(),
            },
        );
        let state = home.path().join(STATE);
        let bytes = serde_json::to_vec(&receipt).unwrap();
        fs::write(&state, &bytes).unwrap();
        assert!(remove(home.path(), true, false).is_err());
        assert_eq!(fs::read_to_string(config).unwrap(), "unchanged");
        assert_eq!(fs::read(state).unwrap(), bytes);
    }
}
