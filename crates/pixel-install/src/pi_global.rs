// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Installs Pi's explicit existing-graph impact command as a local Pi package.
//!
//! Pixel owns a package directory under `~/.local/share/pixel/` (a
//! `package.json` plus the extension) and declares it in the `packages`
//! array of Pi's user settings, the mechanism `pi install ./local-package`
//! uses. Pi loads a local package from its path without copying it, so a
//! reinstall that rewrites the package is picked up on Pi's next start.
//! Pi's agent directory is `$PI_CODING_AGENT_DIR` when set, else
//! `~/.pi/agent`, as Pi resolves it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result, dry_run_summary};

/// The extension file an earlier install copied into Pi's own extension
/// directory, relative to the agent directory. A managed copy left there
/// beside the package registers the command twice (`/pixel-impact:1`,
/// `/pixel-impact:2`), so install and uninstall remove it.
pub(crate) const LEGACY_EXTENSION: &str = "extensions/pixel-impact.ts";

/// Pixel's Pi package directory, relative to the home directory.
pub(crate) const PACKAGE_DIR: &str = ".local/share/pixel/pi-package";

/// The extension inside the package, relative to the package directory.
const PACKAGE_EXTENSION: &str = "extensions/pixel-impact.ts";

const PACKAGE_MANIFEST: &str = "package.json";

const STEP_ID: &str = "hooks.pi-impact";

/// Where Pi and Pixel's package live for one home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiPaths {
    /// Pi's user configuration directory.
    pub agent_dir: PathBuf,
    /// Pi's user settings file, whose `packages` array names the package.
    pub settings: PathBuf,
    /// Pixel's package directory.
    pub package_dir: PathBuf,
}

impl PiPaths {
    /// `$PI_CODING_AGENT_DIR`, unless `home` was given explicitly (a test
    /// or `--home`), else `<home>/.pi/agent`.
    pub(crate) fn resolve(home: &Path, home_was_explicit: bool) -> Self {
        Self::resolve_with(
            home,
            home_was_explicit,
            std::env::var_os("PI_CODING_AGENT_DIR"),
        )
    }

    fn resolve_with(home: &Path, home_was_explicit: bool, agent_dir: Option<OsString>) -> Self {
        let agent_dir = agent_dir
            .filter(|dir| !home_was_explicit && !dir.is_empty())
            .map_or_else(|| home.join(config::PI_CONFIG_DIR), PathBuf::from);
        Self {
            settings: agent_dir.join("settings.json"),
            agent_dir,
            package_dir: home.join(PACKAGE_DIR),
        }
    }

    fn legacy_extension(&self) -> PathBuf {
        self.agent_dir.join(LEGACY_EXTENSION)
    }

    fn package_extension(&self) -> PathBuf {
        self.package_dir.join(PACKAGE_EXTENSION)
    }

    /// The `packages` entry naming Pixel's package: its absolute path.
    fn package_source(&self) -> String {
        self.package_dir.display().to_string()
    }
}

/// Whether a `pi` executable is on PATH, so a Pi that has not created its
/// agent directory yet still gets the command.
#[cfg_attr(test, mutants::skip)]
// reason: the one-line adapter over the process PATH; `pi_in_paths` holds
// the logic and is tested.
pub(crate) fn pi_on_path() -> bool {
    pi_in_paths(std::env::var_os("PATH").as_deref())
}

fn pi_in_paths(path: Option<&OsStr>) -> bool {
    path.is_some_and(|path| install::find_in_paths("pi", path).is_some())
}

pub(crate) fn extension_source(exe: &Path) -> String {
    include_str!("../assets/pi-impact.ts")
        .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
        .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
        .replace("__MANAGED_END__", config::MANAGED_END)
}

fn package_manifest() -> String {
    let manifest = json!({
        "name": "pixel-impact",
        "private": true,
        "description": "Pixel's explicit /pixel-impact command, managed by `pixel install`",
        "pi": { "extensions": [format!("./{PACKAGE_EXTENSION}")] },
    });
    format!("{manifest:#}\n")
}

fn is_managed(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && fs::read_to_string(path).is_ok_and(|contents| contents.contains(config::MANAGED_BEGIN))
}

fn names_package(entry: &Value, source: &str) -> bool {
    entry.as_str() == Some(source) || entry.get("source").and_then(Value::as_str) == Some(source)
}

fn declares_package(settings: &Value, source: &str) -> bool {
    settings
        .get("packages")
        .and_then(Value::as_array)
        .is_some_and(|packages| packages.iter().any(|entry| names_package(entry, source)))
}

/// Pi's settings as an object, `{}` when the file is absent; `Err` names
/// why a present file cannot be edited safely.
fn read_settings_object(path: &Path) -> std::result::Result<Value, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    if !value.is_object() {
        return Err(format!("{} is not a JSON object", path.display()));
    }
    if value
        .get("packages")
        .is_some_and(|packages| !packages.is_array())
    {
        return Err(format!("{} has a non-array `packages`", path.display()));
    }
    Ok(value)
}

fn step(status: CheckStatus, summary: String, paths: &PiPaths) -> InstallStep {
    InstallStep {
        id: STEP_ID.into(),
        status,
        summary,
        detail: Some(format!(
            "settings={} package={}",
            paths.settings.display(),
            paths.package_dir.display()
        )),
    }
}

/// A non-directory where Pi's agent directory belongs, which install and
/// doctor leave to the user.
fn agent_dir_occupied(paths: &PiPaths) -> Option<String> {
    (fs::symlink_metadata(&paths.agent_dir).is_ok() && !paths.agent_dir.is_dir()).then(|| {
        format!(
            "Pi configuration path is not a directory; left untouched at {}",
            paths.agent_dir.display()
        )
    })
}

/// Write Pixel's package and declare it in Pi's settings. Skips a machine
/// where Pi is neither configured nor on PATH; leaves a non-directory agent
/// path and unparsable settings untouched.
pub(crate) fn install(
    paths: &PiPaths,
    exe: &Path,
    pi_installed: bool,
    dry_run: bool,
) -> Result<InstallStep> {
    if let Some(reason) = agent_dir_occupied(paths) {
        return Ok(step(CheckStatus::Yellow, reason, paths));
    }
    if !paths.agent_dir.exists() && !pi_installed {
        return Ok(step(
            CheckStatus::Green,
            "Pi is not installed; skipped explicit impact command".into(),
            paths,
        ));
    }
    let mut settings = match read_settings_object(&paths.settings) {
        Ok(settings) => settings,
        Err(reason) => {
            return Ok(step(
                CheckStatus::Yellow,
                format!("{reason}; Pi settings left untouched"),
                paths,
            ));
        }
    };
    let source = paths.package_source();
    let declared = declares_package(&settings, &source);
    if !declared
        && let Some(root) = settings.as_object_mut()
        && let Some(packages) = root
            .entry("packages")
            .or_insert_with(|| json!([]))
            .as_array_mut()
    {
        packages.push(Value::String(source));
    }
    let legacy = paths.legacy_extension();
    let foreign_legacy = fs::symlink_metadata(&legacy).is_ok() && !is_managed(&legacy);
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(true, "would install the explicit Pi impact package"),
            paths,
        ));
    }
    if let Err(error) = fs::create_dir_all(&paths.agent_dir) {
        return Ok(step(
            CheckStatus::Yellow,
            format!(
                "cannot create Pi's configuration directory {}: {error}; left untouched",
                paths.agent_dir.display()
            ),
            paths,
        ));
    }
    if let Some(dir) = paths.package_extension().parent() {
        fs::create_dir_all(dir)?;
    }
    install::write_atomically(
        &paths.package_dir.join(PACKAGE_MANIFEST),
        &package_manifest(),
    )?;
    install::write_atomically(&paths.package_extension(), &extension_source(exe))?;
    if !declared {
        install::write_settings(&paths.settings, &settings, false)?;
    }
    if is_managed(&legacy) {
        config::backup_if_changing(&legacy, b"removed Pixel Pi impact extension")?;
        fs::remove_file(&legacy)?;
    }
    if foreign_legacy {
        return Ok(step(
            CheckStatus::Yellow,
            format!(
                "explicit Pi impact package installed; a foreign {} may register /pixel-impact a second time",
                legacy.display()
            ),
            paths,
        ));
    }
    Ok(step(
        CheckStatus::Green,
        "explicit Pi impact package installed".into(),
        paths,
    ))
}

/// Remove Pixel's package, its `packages` entry and a managed legacy copy;
/// foreign settings, packages and extensions stay.
pub(crate) fn uninstall(paths: &PiPaths, dry_run: bool) -> Result<InstallStep> {
    let source = paths.package_source();
    let mut settings = read_settings_object(&paths.settings)
        .ok()
        .filter(|settings| declares_package(settings, &source));
    let legacy = paths.legacy_extension();
    let legacy_managed = is_managed(&legacy);
    if settings.is_none() && !legacy_managed && !paths.package_dir.exists() {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no Pixel Pi impact package found"),
            paths,
        ));
    }
    if !dry_run {
        if let Some(settings) = settings.as_mut() {
            if let Some(packages) = settings.get_mut("packages").and_then(Value::as_array_mut) {
                packages.retain(|entry| !names_package(entry, &source));
            }
            install::write_settings(&paths.settings, settings, false)?;
        }
        for file in [
            paths.package_extension(),
            paths.package_dir.join(PACKAGE_MANIFEST),
        ] {
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        // Only empty directories go: anything else in them is not Pixel's.
        if let Some(dir) = paths.package_extension().parent() {
            let _ = fs::remove_dir(dir);
        }
        let _ = fs::remove_dir(&paths.package_dir);
        if legacy_managed {
            config::backup_if_changing(&legacy, b"removed Pixel Pi impact extension")?;
            fs::remove_file(&legacy)?;
            // Keep the directory containing the recoverable sibling backup.
        }
    }
    Ok(step(
        CheckStatus::Green,
        dry_run_summary(dry_run, "removed the Pixel Pi impact package"),
        paths,
    ))
}

/// How the installed command compares with what `pixel install` writes for
/// `exe`, as doctor reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PiImpactState {
    /// Pi is neither configured nor on PATH: nothing is expected.
    NotInstalled,
    /// The package is current and declared, with no duplicate.
    Current,
    /// A user-owned path blocks or shadows the install; never repaired.
    Manual(String),
    /// `pixel install` would change something.
    NeedsInstall(String),
}

pub(crate) fn check(paths: &PiPaths, exe: &Path, pi_installed: bool) -> PiImpactState {
    if let Some(reason) = agent_dir_occupied(paths) {
        return PiImpactState::Manual(reason);
    }
    if !paths.agent_dir.exists() && !pi_installed {
        return PiImpactState::NotInstalled;
    }
    let settings = match read_settings_object(&paths.settings) {
        Ok(settings) => settings,
        Err(reason) => return PiImpactState::Manual(reason),
    };
    let source = paths.package_source();
    if !declares_package(&settings, &source) {
        return PiImpactState::NeedsInstall(format!(
            "{} does not declare the Pixel impact package {source}",
            paths.settings.display()
        ));
    }
    let current =
        |path: &Path, expected: &str| fs::read_to_string(path).is_ok_and(|text| text == expected);
    if !current(
        &paths.package_dir.join(PACKAGE_MANIFEST),
        &package_manifest(),
    ) || !current(&paths.package_extension(), &extension_source(exe))
    {
        return PiImpactState::NeedsInstall(format!(
            "the Pixel Pi impact package at {} is missing, stale or points to a different binary",
            paths.package_dir.display()
        ));
    }
    let legacy = paths.legacy_extension();
    if is_managed(&legacy) {
        return PiImpactState::NeedsInstall(format!(
            "a retired copy at {} registers /pixel-impact twice",
            legacy.display()
        ));
    }
    if fs::symlink_metadata(&legacy).is_ok() {
        return PiImpactState::Manual(format!(
            "a foreign {} may register /pixel-impact a second time",
            legacy.display()
        ));
    }
    PiImpactState::Current
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::json;

    use super::{
        LEGACY_EXTENSION, PACKAGE_DIR, PiImpactState, PiPaths, check, extension_source, install,
        package_manifest, pi_in_paths, uninstall,
    };
    use crate::config;
    use crate::install::CheckStatus;

    fn configure_pi(home: &Path, settings: &serde_json::Value) -> PiPaths {
        let paths = PiPaths::resolve(home, true);
        fs::create_dir_all(&paths.agent_dir).unwrap();
        fs::write(&paths.settings, format!("{settings:#}\n")).unwrap();
        paths
    }

    fn settings(paths: &PiPaths) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(&paths.settings).unwrap()).unwrap()
    }

    fn source(paths: &PiPaths) -> String {
        paths.package_dir.display().to_string()
    }

    #[test]
    fn extension_source_should_register_the_command_in_the_factory() {
        let source = extension_source(Path::new("/opt/pixel tools/pixel"));
        assert!(source.contains("const PIXEL_BIN = \"/opt/pixel tools/pixel\";"));
        assert!(source.contains("pi.registerCommand(\"pixel-impact\""));
        assert!(source.contains("\"--no-refresh\""));
        assert!(source.contains(config::MANAGED_BEGIN));
        assert!(source.contains(config::MANAGED_END));
        assert!(!source.contains("registerTool("));
        assert!(
            !source.contains("session_start"),
            "Pi collects commands while it loads the factory"
        );
    }

    #[test]
    fn package_manifest_should_point_pi_at_the_extension() {
        let manifest: serde_json::Value = serde_json::from_str(&package_manifest()).unwrap();
        assert_eq!(
            manifest["pi"],
            json!({"extensions": ["./extensions/pixel-impact.ts"]})
        );
        assert_eq!(manifest["name"], "pixel-impact");
    }

    #[test]
    fn resolve_should_honour_the_agent_dir_variable_only_for_the_real_home() {
        let home = Path::new("/home/someone");
        let variable = Some("/srv/pi-agent".into());
        let real = PiPaths::resolve_with(home, false, variable.clone());
        assert_eq!(real.agent_dir, Path::new("/srv/pi-agent"));
        assert_eq!(real.settings, Path::new("/srv/pi-agent/settings.json"));
        assert_eq!(real.package_dir, home.join(PACKAGE_DIR));
        assert_eq!(
            PiPaths::resolve_with(home, true, variable).agent_dir,
            home.join(config::PI_CONFIG_DIR)
        );
        for unset in [None, Some("".into())] {
            assert_eq!(
                PiPaths::resolve_with(home, false, unset).agent_dir,
                home.join(config::PI_CONFIG_DIR)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pi_in_paths_should_find_only_an_executable_pi() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(!pi_in_paths(None));
        assert!(!pi_in_paths(Some(dir.path().as_os_str())));
        let pi = dir.path().join("pi");
        fs::write(&pi, "#!/bin/sh\n").unwrap();
        assert!(!pi_in_paths(Some(dir.path().as_os_str())));
        fs::set_permissions(&pi, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(pi_in_paths(Some(dir.path().as_os_str())));
    }

    #[test]
    fn install_should_declare_the_package_and_keep_foreign_settings() {
        let home = tempfile::tempdir().unwrap();
        let paths = configure_pi(
            home.path(),
            &json!({"theme": "dark", "packages": ["npm:other@1"]}),
        );
        let step = install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{step:?}");
        assert_eq!(
            settings(&paths),
            json!({"theme": "dark", "packages": ["npm:other@1", source(&paths)]})
        );
        assert_eq!(
            fs::read_to_string(paths.package_dir.join("package.json")).unwrap(),
            package_manifest()
        );
        assert_eq!(
            fs::read_to_string(paths.package_dir.join("extensions/pixel-impact.ts")).unwrap(),
            extension_source(Path::new("/opt/pixel"))
        );
        assert_eq!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::Current
        );

        // A reinstall for another binary rewrites the package, not the settings.
        let declared = fs::read(&paths.settings).unwrap();
        install(&paths, Path::new("/opt/pixel-next"), false, false).unwrap();
        assert_eq!(fs::read(&paths.settings).unwrap(), declared);
        assert!(matches!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::NeedsInstall(_)
        ));
        assert_eq!(
            check(&paths, Path::new("/opt/pixel-next"), false),
            PiImpactState::Current
        );
    }

    #[test]
    fn install_should_accept_an_object_entry_naming_the_package() {
        let home = tempfile::tempdir().unwrap();
        let source = source(&PiPaths::resolve(home.path(), true));
        let paths = configure_pi(home.path(), &json!({"packages": [{"source": source}]}));
        install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        assert_eq!(settings(&paths), json!({"packages": [{"source": source}]}));
        assert_eq!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::Current
        );
    }

    #[test]
    fn install_should_skip_a_machine_without_pi_and_create_settings_when_pi_is_on_path() {
        let home = tempfile::tempdir().unwrap();
        let paths = PiPaths::resolve(home.path(), true);
        let step = install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("Pi is not installed"), "{step:?}");
        assert!(!home.path().join(".pi").exists());
        assert!(!paths.package_dir.exists());
        assert_eq!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::NotInstalled
        );

        // Pi on PATH before its first run: the agent directory is created.
        assert!(matches!(
            check(&paths, Path::new("/opt/pixel"), true),
            PiImpactState::NeedsInstall(_)
        ));
        install(&paths, Path::new("/opt/pixel"), true, false).unwrap();
        assert_eq!(settings(&paths), json!({"packages": [source(&paths)]}));
        assert_eq!(
            check(&paths, Path::new("/opt/pixel"), true),
            PiImpactState::Current
        );
    }

    #[test]
    fn install_should_leave_unparsable_settings_and_non_directory_agent_paths_untouched() {
        let home = tempfile::tempdir().unwrap();
        let paths = PiPaths::resolve(home.path(), true);
        fs::create_dir_all(&paths.agent_dir).unwrap();
        for text in ["{ not json", "[]", "{\"packages\": \"x\"}"] {
            fs::write(&paths.settings, text).unwrap();
            let step = install(&paths, Path::new("/opt/pixel"), true, false).unwrap();
            assert_eq!(step.status, CheckStatus::Yellow, "{text}");
            assert_eq!(fs::read_to_string(&paths.settings).unwrap(), text);
            assert!(matches!(
                check(&paths, Path::new("/opt/pixel"), true),
                PiImpactState::Manual(_)
            ));
        }
        assert!(!paths.package_dir.exists());

        fs::remove_dir_all(&paths.agent_dir).unwrap();
        fs::write(&paths.agent_dir, "user-owned file").unwrap();
        let step = install(&paths, Path::new("/opt/pixel"), true, false).unwrap();
        assert_eq!(step.status, CheckStatus::Yellow);
        assert!(step.summary.contains("not a directory"));
        assert_eq!(
            fs::read_to_string(&paths.agent_dir).unwrap(),
            "user-owned file"
        );
        assert!(matches!(
            check(&paths, Path::new("/opt/pixel"), true),
            PiImpactState::Manual(_)
        ));
    }

    #[test]
    fn install_should_report_an_agent_dir_it_cannot_create_without_failing() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".pi"), "not a directory").unwrap();
        let paths = PiPaths::resolve(home.path(), true);
        let step = install(&paths, Path::new("/opt/pixel"), true, false).unwrap();
        assert_eq!(step.status, CheckStatus::Yellow, "{step:?}");
        assert!(step.summary.contains("left untouched"), "{step:?}");
        assert!(!paths.package_dir.exists());
        assert_eq!(
            fs::read_to_string(home.path().join(".pi")).unwrap(),
            "not a directory"
        );
    }

    #[test]
    fn install_should_retire_a_managed_legacy_copy_and_keep_a_foreign_one() {
        let home = tempfile::tempdir().unwrap();
        let paths = configure_pi(home.path(), &json!({}));
        let legacy = paths.agent_dir.join(LEGACY_EXTENSION);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, extension_source(Path::new("/opt/old"))).unwrap();
        install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        // Package installed first, then the stale copy is caught by doctor.
        fs::write(&legacy, extension_source(Path::new("/opt/old"))).unwrap();
        assert!(matches!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::NeedsInstall(_)
        ));
        let step = install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(
            !legacy.exists(),
            "a managed copy would register the command twice"
        );
        assert_eq!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::Current
        );

        fs::write(&legacy, "user-owned Pi extension").unwrap();
        let step = install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        assert_eq!(step.status, CheckStatus::Yellow);
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            "user-owned Pi extension"
        );
        assert!(matches!(
            check(&paths, Path::new("/opt/pixel"), false),
            PiImpactState::Manual(_)
        ));
    }

    #[test]
    fn uninstall_should_remove_only_pixels_package_entry_and_files() {
        let home = tempfile::tempdir().unwrap();
        let paths = configure_pi(home.path(), &json!({"packages": ["npm:other@1"]}));
        install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        let legacy = paths.agent_dir.join(LEGACY_EXTENSION);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, extension_source(Path::new("/opt/old"))).unwrap();

        let step = uninstall(&paths, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("removed"), "{step:?}");
        assert_eq!(settings(&paths), json!({"packages": ["npm:other@1"]}));
        assert!(!paths.package_dir.exists());
        assert!(!legacy.exists());

        let step = uninstall(&paths, false).unwrap();
        assert!(
            step.summary.contains("no Pixel Pi impact package found"),
            "{step:?}"
        );
    }

    #[test]
    fn dry_run_should_write_nothing() {
        let home = tempfile::tempdir().unwrap();
        let paths = configure_pi(home.path(), &json!({}));
        let original = fs::read(&paths.settings).unwrap();
        let step = install(&paths, Path::new("/opt/pixel"), false, true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!paths.package_dir.exists());
        assert_eq!(fs::read(&paths.settings).unwrap(), original);

        install(&paths, Path::new("/opt/pixel"), false, false).unwrap();
        let declared = fs::read(&paths.settings).unwrap();
        uninstall(&paths, true).unwrap();
        assert!(paths.package_dir.exists());
        assert_eq!(fs::read(&paths.settings).unwrap(), declared);
    }
}
