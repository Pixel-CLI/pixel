// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Installs Pi's explicit existing-graph impact command in the global extension directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result, dry_run_summary};

pub(crate) const EXTENSION: &str = ".pi/agent/extensions/pixel-impact.ts";

pub(crate) fn extension_source(exe: &Path) -> String {
    include_str!("../assets/pi-impact.ts")
        .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
        .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
        .replace("__MANAGED_END__", config::MANAGED_END)
}

fn is_managed(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && fs::read_to_string(path).is_ok_and(|contents| contents.contains(config::MANAGED_BEGIN))
}

pub(crate) fn install(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(EXTENSION);
    let source = extension_source(exe);
    if fs::symlink_metadata(&path).is_ok() && !is_managed(&path) {
        return Ok(InstallStep {
            id: "hooks.pi-impact".into(),
            status: CheckStatus::Yellow,
            summary: format!("left foreign Pi extension untouched at {}", path.display()),
            detail: Some(format!("path={}", path.display())),
        });
    }
    if dry_run {
        return Ok(InstallStep {
            id: "hooks.pi-impact".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(true, "would install explicit Pi impact command"),
            detail: Some(format!("path={}", path.display())),
        });
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    install::write_atomically(&path, &source)?;
    Ok(InstallStep {
        id: "hooks.pi-impact".into(),
        status: CheckStatus::Green,
        summary: "explicit Pi impact command installed".into(),
        detail: Some(format!("path={}", path.display())),
    })
}

pub(crate) fn uninstall(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(EXTENSION);
    if !is_managed(&path) {
        return Ok(InstallStep {
            id: "hooks.pi-impact".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no managed Pi impact extension found"),
            detail: Some(format!("path={}", path.display())),
        });
    }
    if !dry_run {
        config::backup_if_changing(&path, b"removed Pixel Pi impact extension")?;
        fs::remove_file(&path)?;
        remove_empty_dirs(&[
            &home.join(".pi/agent/extensions"),
            &home.join(".pi/agent"),
            &home.join(".pi"),
        ]);
    }
    Ok(InstallStep {
        id: "hooks.pi-impact".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, "removed managed Pi impact extension"),
        detail: Some(format!("path={}", path.display())),
    })
}

fn remove_empty_dirs(dirs: &[&Path]) {
    for dir in dirs {
        let _ = fs::remove_dir(dir);
    }
}

pub(crate) fn installed_state(home: &Path) -> (bool, PathBuf) {
    let path = home.join(EXTENSION);
    (is_managed(&path), path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{EXTENSION, extension_source, install, installed_state, uninstall};
    use crate::config;
    use crate::install::CheckStatus;

    #[test]
    fn extension_source_should_register_the_explicit_existing_graph_command() {
        let source = extension_source(Path::new("/opt/pixel tools/pixel"));
        assert!(source.contains("const PIXEL_BIN = \"/opt/pixel tools/pixel\";"));
        assert!(source.contains("pi.registerCommand(\"pixel-impact\""));
        assert!(source.contains("\"--no-refresh\""));
        assert!(source.contains(config::MANAGED_BEGIN));
        assert!(source.contains(config::MANAGED_END));
        assert!(!source.contains("registerTool("));
    }

    #[test]
    fn install_should_write_and_refresh_only_its_managed_extension() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(EXTENSION);
        let step = install(home.path(), Path::new("/opt/pixel"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            extension_source(Path::new("/opt/pixel"))
        );
        assert_eq!(installed_state(home.path()), (true, path.clone()));

        install(home.path(), Path::new("/opt/pixel-next"), false).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            extension_source(Path::new("/opt/pixel-next"))
        );
    }

    #[test]
    fn install_should_leave_a_foreign_extension_untouched() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(EXTENSION);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user-owned Pi extension").unwrap();

        let step = install(home.path(), Path::new("/opt/pixel"), false).unwrap();

        assert_eq!(step.status, CheckStatus::Yellow);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "user-owned Pi extension"
        );
        assert_eq!(installed_state(home.path()), (false, path));
    }

    #[test]
    fn uninstall_should_remove_managed_extension_and_keep_foreign_file() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(EXTENSION);
        install(home.path(), Path::new("/opt/pixel"), false).unwrap();
        let step = uninstall(home.path(), false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!path.exists());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user-owned Pi extension").unwrap();
        uninstall(home.path(), false).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "user-owned Pi extension");
    }

    #[test]
    fn dry_run_should_not_create_or_remove_the_pi_extension() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(EXTENSION);
        let step = install(home.path(), Path::new("/opt/pixel"), true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!path.exists());
        assert!(!home.path().join(".pi").exists());
    }
}
