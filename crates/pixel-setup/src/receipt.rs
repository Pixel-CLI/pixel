// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Ownership receipt for setup: restore unchanged writes and keep later user edits.

use crate::Result;
use crate::install::{CheckStatus, InstallStep};
use pixel_model::hash::Hash;
use serde::{Deserialize, Serialize};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<PathBuf>,
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
    roots.sort();
    roots.dedup();
    roots
}

fn read(path: &Path) -> Result<Option<Entry>> {
    let metadata = match fs::metadata(path) {
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
    if !metadata.is_file() {
        return Err(crate::InstallError::Setup(format!(
            "setup output is not a regular file: {}",
            path.display()
        )));
    }
    Ok(Some(Entry::File {
        bytes: fs::read(path)?,
        mode: fs::metadata(path)?.permissions().mode() & 0o777,
    }))
}

fn collect(path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
    collect_tree(path, entries, &mut Vec::new())
}

fn collect_tree(
    path: &Path,
    entries: &mut BTreeMap<PathBuf, Entry>,
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    if let Some(entry) = read(path)? {
        if entry == Entry::Directory {
            let resolved = fs::canonicalize(path)?;
            if path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .is_some_and(|parent| parent.starts_with(&resolved))
            {
                return Err(crate::InstallError::Setup(
                    "setup output links to an ancestor directory".into(),
                ));
            }
            if ancestors.contains(&resolved) {
                return Ok(());
            }
            ancestors.push(resolved);
            for child in fs::read_dir(path)? {
                let child = child?;
                if !child.file_name().to_string_lossy().contains(".pixel-bak.") {
                    collect_tree(&child.path(), entries, ancestors)?;
                }
            }
            ancestors.pop();
        }
        entries.insert(path.to_path_buf(), entry);
    }
    Ok(())
}

fn fingerprint(entry: &Entry) -> String {
    match entry {
        Entry::Directory => "directory".into(),
        Entry::File { bytes, mode } => format!("{mode:o}:{}", Hash::of(bytes).to_hex()),
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
            let location = fs::canonicalize(&path)
                .ok()
                .filter(|target| *target != path);
            let original = self.entries.get(&path);
            if let Some(owned) = receipt.get_mut(&path) {
                // A reinstall starts a new ownership baseline when the user
                // changed the previous output. Removing it must restore that edit.
                if original.is_none_or(|entry| fingerprint(entry) != owned.installed)
                    || owned.location != location
                {
                    owned.original = original.cloned();
                }
                owned.installed = installed;
                owned.location = location;
            } else {
                receipt.insert(
                    path,
                    Owned {
                        original: original.cloned(),
                        installed,
                        location,
                    },
                );
            }
        }
        if receipt.is_empty() {
            return Ok(());
        }
        // The receipt's own directory is setup's output when setup made it, so
        // removal can prune it once the receipt goes.
        if let Some(state_dir) = path.parent()
            && !self.entries.contains_key(state_dir)
            && !receipt.contains_key(state_dir)
        {
            receipt.insert(
                state_dir.to_path_buf(),
                Owned {
                    original: None,
                    installed: "directory".into(),
                    location: None,
                },
            );
        }
        save(home, &receipt)
    }
}

fn save(home: &Path, receipt: &Receipt) -> Result<()> {
    let path = home.join(STATE);
    let bytes = serde_json::to_vec(receipt)?;
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
    let mut retained = Receipt::new();
    // Validate the entire receipt before the first mutation, including lexical traversal.
    for (file, owned) in &receipt {
        let directory = owned.installed == "directory" && owned.original.is_none();
        if !allowed
            .iter()
            .any(|root| file.starts_with(root) || (directory && root.starts_with(file)))
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
            if fs::symlink_metadata(file).is_ok_and(|metadata| !metadata.is_dir()) {
                retained.insert(
                    file.clone(),
                    Owned {
                        original: None,
                        installed: "directory".into(),
                        location: None,
                    },
                );
                steps.push(InstallStep {
                    id: "setup-ownership".into(),
                    status: CheckStatus::Yellow,
                    summary: "kept a directory replaced after setup".into(),
                    detail: Some(file.display().to_string()),
                });
            } else if owned.original.is_none() {
                directories.push(file);
            }
            continue;
        }
        let replaced = fs::symlink_metadata(file).is_ok()
            && (fs::canonicalize(file).ok().as_ref()
                != Some(owned.location.as_ref().unwrap_or(file))
                || fs::metadata(file)
                    .is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir()));
        let current = if replaced { None } else { read(file)? };
        let changed = replaced
            || current
                .as_ref()
                .is_some_and(|entry| fingerprint(entry) != owned.installed);
        if changed {
            retained.insert(
                file.clone(),
                Owned {
                    original: owned.original.clone(),
                    installed: owned.installed.clone(),
                    location: owned.location.clone(),
                },
            );
        }
        if !dry_run && !changed && current.is_some() {
            if let Some(Entry::File { bytes, mode }) = &owned.original {
                if let Some(backup) = crate::config::backup_if_changing(file, bytes)? {
                    steps.push(InstallStep {
                        id: "setup-ownership-backup".into(),
                        status: CheckStatus::Green,
                        summary: "saved a recovery copy before restoring the original file".into(),
                        detail: Some(backup.display().to_string()),
                    });
                }
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
        if retained.is_empty() {
            fs::remove_file(path)?;
        } else {
            // Keep the remaining ownership so another remove cannot fall back
            // to the legacy marker-based removers and delete a preserved edit.
            for directory in &directories {
                retained.insert(
                    (*directory).clone(),
                    Owned {
                        original: None,
                        installed: "directory".into(),
                        location: None,
                    },
                );
            }
            save(home, &retained)?;
        }
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
                hooks: true,
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
                fs::write(
                    &skill,
                    fs::read_to_string(&skill).unwrap() + "\nmy changes\n",
                )
                .unwrap();
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
            if edit == "none" {
                assert!(!home.path().join(STATE).exists());
            } else {
                crate::uninstall::uninstall(&crate::uninstall::UninstallOptions {
                    home: Some(home.path().into()),
                    binary_path: Some(home.path().join("absent")),
                    executable_path: Some("/tmp/pixel".into()),
                    ..Default::default()
                })
                .unwrap();
                assert_eq!(
                    read(&skill).unwrap(),
                    kept,
                    "repeated removal keeps the edit"
                );
            }
        }
    }

    #[test]
    fn setup_should_preserve_old_product_hooks_and_remove_only_its_own_install() {
        let home = tempfile::tempdir().unwrap();
        let settings = home.path().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let original = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/old/pixel run-hook task-event --provider claude --event stop"}]}]}}"#;
        fs::write(&settings, original).unwrap();
        let options = crate::install::InstallOptions {
            home: Some(home.path().into()),
            executable_path: Some("/tmp/pixel".into()),
            ..Default::default()
        };
        let selection = crate::install::InstallSelection {
            agents: vec![crate::setup::AgentTarget::ClaudeCode],
            hooks: true,
        };

        for _ in 0..2 {
            let before = Snapshot::begin(&options).unwrap().unwrap();
            crate::install::install_selected(&options, Some(&selection)).unwrap();
            before.finish().unwrap();
            let hooks: serde_json::Value =
                serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
            assert_eq!(
                hooks["hooks"]["Stop"][0]["hooks"][0]["command"],
                "/old/pixel run-hook task-event --provider claude --event stop"
            );
            assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 2);
        }

        crate::uninstall::uninstall(&crate::uninstall::UninstallOptions {
            home: Some(home.path().into()),
            binary_path: Some(home.path().join("absent")),
            executable_path: Some("/tmp/pixel".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn snapshot_should_reject_links_that_would_scan_the_users_entire_home() {
        let home = tempfile::tempdir().unwrap();
        let link = home.path().join(".claude/skills/pixel");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.path(), &link).unwrap();
        assert!(Snapshot::capture(home.path(), true).is_err());
        assert_eq!(fs::read_link(link).unwrap(), home.path());
        assert!(!home.path().join(STATE).exists());
    }

    #[test]
    fn snapshots_should_refuse_special_files_but_removal_should_keep_user_replacements() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".pixel/config.yaml");
        let before = Snapshot::capture(home.path(), true).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "installed").unwrap();
        before.finish().unwrap();
        fs::remove_file(&file).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(&file).unwrap();
        assert!(matches!(Snapshot::capture(home.path(), true),
            Err(crate::InstallError::Setup(message)) if message.contains("not a regular file")));
        for _ in 0..2 {
            let steps = remove(home.path(), true, false).unwrap().unwrap();
            assert!(steps.iter().any(|step| step.status == CheckStatus::Yellow
                && step.detail.as_deref() == file.to_str()));
            assert!(file.exists());
        }
    }

    #[test]
    fn setup_and_remove_should_preserve_chained_dotfile_links_and_their_targets() {
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let original = "my dotfile\n";
        let target = managed.path().join("SKILL.md");
        let intermediate = managed.path().join("skill-link");
        let skill = home.path().join(".claude/skills/pixel/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&target, original).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &intermediate).unwrap();
        std::os::unix::fs::symlink(&intermediate, &skill).unwrap();
        let options = crate::install::InstallOptions {
            home: Some(home.path().into()),
            executable_path: Some("/tmp/pixel".into()),
            ..Default::default()
        };
        let selection = crate::install::InstallSelection {
            agents: vec![crate::setup::AgentTarget::ClaudeCode],
            ..Default::default()
        };
        let before = Snapshot::begin(&options).unwrap().unwrap();
        crate::install::install_selected(&options, Some(&selection)).unwrap();
        before.finish().unwrap();
        assert_eq!(fs::read_link(&skill).unwrap(), intermediate);
        assert_eq!(fs::read_link(&intermediate).unwrap(), target);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            crate::skill::PIXEL_SKILL
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        remove(home.path(), true, false).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), original);
        assert_eq!(fs::read_link(&skill).unwrap(), intermediate);
        assert_eq!(fs::read_link(&intermediate).unwrap(), target);
    }

    #[test]
    fn reinstall_should_restore_the_users_intervening_edit() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".pixel/config.yaml");
        let before = Snapshot::capture(home.path(), true).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "first install").unwrap();
        before.finish().unwrap();
        fs::write(&file, "user's intervening edit").unwrap();
        let before = Snapshot::capture(home.path(), true).unwrap();
        fs::write(&file, "second install").unwrap();
        before.finish().unwrap();
        remove(home.path(), true, false).unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "user's intervening edit");
    }

    #[test]
    fn removal_should_follow_the_original_directory_link_but_not_a_retargeted_link() {
        for retarget in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let original = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let link = home.path().join(".claude/skills/pixel");
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(original.path(), &link).unwrap();
            let before = Snapshot::capture(home.path(), true).unwrap();
            fs::write(link.join("SKILL.md"), "installed").unwrap();
            before.finish().unwrap();
            if retarget {
                fs::write(other.path().join("SKILL.md"), "installed").unwrap();
                fs::remove_file(&link).unwrap();
                std::os::unix::fs::symlink(other.path(), &link).unwrap();
            }
            let steps = remove(home.path(), true, false).unwrap().unwrap();
            assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(link.join("SKILL.md").exists(), retarget);
            assert_eq!(
                steps.iter().any(|step| step.status == CheckStatus::Yellow),
                retarget
            );
            if retarget {
                assert_eq!(
                    fs::read_to_string(other.path().join("SKILL.md")).unwrap(),
                    "installed"
                );
            }
        }
    }

    #[test]
    fn reinstall_should_adopt_the_users_retargeted_link_as_its_baseline() {
        let home = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/skills/pixel");
        let before = Snapshot::capture(home.path(), true).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "first install").unwrap();
        before.finish().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        fs::write(other.path().join("SKILL.md"), "first install").unwrap();
        std::os::unix::fs::symlink(other.path(), &dir).unwrap();
        let before = Snapshot::capture(home.path(), true).unwrap();
        fs::write(dir.join("SKILL.md"), "second install").unwrap();
        before.finish().unwrap();
        remove(home.path(), true, false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "first install"
        );
        assert!(fs::symlink_metadata(&dir).unwrap().is_symlink());
    }

    #[test]
    fn removal_should_keep_a_created_directory_replaced_with_a_symlink() {
        let home = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let before = Snapshot::capture(home.path(), true).unwrap();
        let dir = home.path().join(".claude/skills/pixel");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "installed").unwrap();
        before.finish().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        fs::write(other.path().join("SKILL.md"), "installed").unwrap();
        std::os::unix::fs::symlink(other.path(), &dir).unwrap();
        for _ in 0..2 {
            let steps = remove(home.path(), true, false).unwrap().unwrap();
            assert!(steps.iter().any(|step| step.status == CheckStatus::Yellow));
            assert!(fs::symlink_metadata(&dir).unwrap().is_symlink());
            assert_eq!(
                fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                "installed"
            );
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
                location: None,
            },
        );
        receipt.insert(
            home.path().join(".pixel/plugins/../../outside"),
            Owned {
                original: None,
                installed: "bad".into(),
                location: None,
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

#[cfg(test)]
mod audited_contract_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn receipt_reading_should_treat_absence_as_optional_but_propagate_other_io_errors() {
        let home = tempfile::tempdir().unwrap();
        let looping = home.path().join("loop");
        symlink("loop", &looping).unwrap();
        assert_eq!(read(&home.path().join("missing")).unwrap(), None);
        assert!(read(&looping).is_err());
        fs::create_dir(home.path().join(".pixel")).unwrap();
        symlink("setup-receipt.json", home.path().join(STATE)).unwrap();
        assert!(remove(home.path(), true, false).is_err());
        assert!(
            Snapshot::capture(home.path(), true)
                .unwrap()
                .finish()
                .is_err()
        );
    }

    #[test]
    fn snapshot_begin_should_skip_both_preview_and_repository_installs_independently() {
        let home = tempfile::tempdir().unwrap();
        for (dry_run, repo, expected) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
            (true, true, false),
        ] {
            let options = crate::install::InstallOptions {
                home: Some(home.path().into()),
                dry_run,
                repo: repo.then(|| home.path().join("repo")),
                ..Default::default()
            };
            assert_eq!(Snapshot::begin(&options).unwrap().is_some(), expected);
        }
    }

    #[test]
    fn snapshots_should_stop_at_the_selected_home_directory() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude/skills")).unwrap();
        let snap = Snapshot::capture(home.path(), true).unwrap();
        assert!(snap.entries.contains_key(&home.path().join(".claude")));
        assert!(!snap.entries.contains_key(home.path()));
        assert!(snap.entries.keys().all(|p| p.starts_with(home.path())));
    }

    #[test]
    fn receipt_validation_should_reject_each_escape_shape_before_removing_any_output() {
        for escape in ["home", "root", "parent", "outside"] {
            let home = tempfile::tempdir().unwrap();
            let home = home.path();
            let file = match escape {
                "home" => home.to_path_buf(),
                "root" => PathBuf::from("/"),
                "parent" => home.join(".claude/skills/pixel/../mine"),
                _ => home.join("mine"),
            };
            fs::write(home.join("mine"), b"keep").unwrap();
            let receipt = Receipt::from([(
                file,
                Owned {
                    original: None,
                    installed: "directory".into(),
                    location: None,
                },
            )]);
            save(home, &receipt).unwrap();
            assert!(remove(home, true, false).is_err(), "{escape}");
            assert_eq!(fs::read(home.join("mine")).unwrap(), b"keep");
        }
    }
}
