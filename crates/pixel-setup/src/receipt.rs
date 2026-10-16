// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ownership receipt for setup: restore unchanged writes and keep later user edits.

use crate::Result;
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
