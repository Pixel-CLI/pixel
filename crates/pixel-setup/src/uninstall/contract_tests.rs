// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the uninstall steps: what each step removes, what it
//! must leave in place (the user's own entries and text, a binary a package
//! manager owns), what a dry run must not touch, and the undo copy every
//! destructive write leaves behind.

use super::*;
use serde_json::json;

/// A Claude-style hook group whose single command is `command`.
fn group(matcher: &str, command: &str) -> serde_json::Value {
    json!({"matcher": matcher, "hooks": [{"type": "command", "command": command}]})
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

// -- binary --------------------------------------------------------------------

#[test]
fn removal_target_should_prefer_the_named_binary_then_the_running_one_then_local_bin() {
    let home = Path::new("/home/u");
    let named = UninstallOptions {
        binary_path: Some("/x/pixel".into()),
        running_binary: Some("/y/pixel".into()),
        ..UninstallOptions::default()
    };
    assert_eq!(
        removal_target(&named, home),
        RemovalTarget {
            path: "/x/pixel".into(),
            explicit: true
        }
    );
    let running = UninstallOptions {
        running_binary: Some("/y/pixel".into()),
        ..UninstallOptions::default()
    };
    assert_eq!(
        removal_target(&running, home),
        RemovalTarget {
            path: "/y/pixel".into(),
            explicit: false
        }
    );
    assert_eq!(
        removal_target(&UninstallOptions::default(), home),
        RemovalTarget {
            path: "/home/u/.local/bin/pixel".into(),
            explicit: false
        }
    );
}

#[test]
fn package_manager_owner_should_name_homebrew_and_mise_trees_and_nothing_else() {
    assert_eq!(
        package_manager_owner(Path::new(
            "/nonexistent/homebrew/Cellar/pixel/0.7.0/bin/pixel"
        )),
        Some(("Homebrew", "`brew uninstall pixel`".to_string()))
    );
    assert_eq!(
        package_manager_owner(Path::new(
            "/nonexistent/.local/share/mise/installs/ubi-pixel-cli-pixel/0.7.0/pixel"
        )),
        Some((
            "mise",
            "`mise uninstall` on the tool installed under `installs/ubi-pixel-cli-pixel`"
                .to_string()
        ))
    );
    assert_eq!(
        package_manager_owner(Path::new("/nonexistent/.local/bin/pixel")),
        None
    );
    assert_eq!(
        package_manager_owner(Path::new("/nonexistent/Cellar")),
        None,
        "a marker with nothing after it names no formula"
    );
}

#[cfg(unix)]
#[test]
fn package_manager_owner_should_follow_a_symlink_into_the_cellar() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("Cellar/pixel/0.7.0/bin/pixel");
    write(&real, "bin");
    let link = root.path().join("bin/pixel");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        package_manager_owner(&link),
        Some(("Homebrew", "`brew uninstall pixel`".to_string())),
        "the brew-linked `bin/pixel` is the Cellar file"
    );
}

#[test]
fn remove_binary_should_skip_when_the_target_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel");
    let step = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: false,
        },
        false,
    )
    .unwrap();
    assert_eq!(step.status, CheckStatus::Green);
    assert_eq!(step.summary, "no binary found — skipping");
    assert_eq!(step.detail, Some(format!("path={}", path.display())));
}

#[test]
fn remove_binary_should_leave_a_package_manager_binary_unless_the_user_named_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("Cellar/pixel/0.7.0/bin/pixel");
    write(&path, "bin");

    let implicit = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: false,
        },
        false,
    )
    .unwrap();
    assert_eq!(implicit.status, CheckStatus::Yellow);
    assert_eq!(
        implicit.summary,
        "left the pixel binary to Homebrew: remove it with `brew uninstall pixel`"
    );
    assert!(
        path.is_file(),
        "Homebrew still lists it: its command removes both"
    );

    let named = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: true,
        },
        false,
    )
    .unwrap();
    assert_eq!(named.status, CheckStatus::Green);
    assert_eq!(named.summary, "removed pixel binary");
    assert!(
        !path.exists(),
        "a binary the user named goes, whoever owns it"
    );
}

#[test]
fn remove_binary_should_delete_an_unmanaged_binary_but_not_on_a_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel");
    write(&path, "bin");
    let target = RemovalTarget {
        path: path.clone(),
        explicit: false,
    };

    let dry = remove_binary(&target, true).unwrap();
    assert_eq!(dry.summary, "[dry-run] would report: removed pixel binary");
    assert!(path.is_file());

    remove_binary(&target, false).unwrap();
    assert!(!path.exists());
}

// -- settings files --------------------------------------------------------------

#[test]
fn remove_pixel_hooks_from_settings_should_report_nothing_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    assert_eq!(
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap(),
        (0, None)
    );
    assert!(!path.exists());
}

#[test]
fn remove_pixel_hooks_from_settings_should_drop_the_hooks_key_once_every_event_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    install::write_settings(
        &path,
        &json!({"keep": 1, "hooks": {"Stop": [group("", "pixel hook stop --host claude")]}}),
        false,
    )
    .unwrap();
    let (removed, backup) =
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap();
    assert_eq!(removed, 1);
    assert!(backup.is_some());
    assert_eq!(install::read_settings(&path).unwrap(), json!({"keep": 1}));
}

#[test]
fn remove_pixel_hooks_from_settings_should_count_but_not_write_when_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let original = json!({"hooks": {"Stop": [group("", "pixel hook stop --host claude")]}});
    install::write_settings(&path, &original, false).unwrap();
    let (removed, backup) =
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), true).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(backup, None);
    assert_eq!(install::read_settings(&path).unwrap(), original);
}

#[test]
fn remove_pixel_hooks_from_settings_should_leave_a_file_without_hooks_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write(&path, "{\"model\":\"x\"}");
    assert_eq!(
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap(),
        (0, None)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"model\":\"x\"}");
}
