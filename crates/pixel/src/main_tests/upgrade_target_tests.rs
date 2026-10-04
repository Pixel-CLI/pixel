// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::cargo_profile_dir;

/// The install step must read the binary the build step wrote: a
/// `--build` on another profile (the `dev-release` iteration loop) used
/// to install the stale `target/release/pixel` without any error.
#[test]
fn profile_dir_follows_the_build_command() {
    assert_eq!(
        cargo_profile_dir("cargo build --release -p pixel-cli"),
        "release"
    );
    assert_eq!(cargo_profile_dir("cargo build -r -p pixel-cli"), "release");
    assert_eq!(cargo_profile_dir("cargo build -p pixel-cli"), "debug");
    assert_eq!(
        cargo_profile_dir("cargo build --profile dev-release -p pixel-cli"),
        "dev-release"
    );
    assert_eq!(
        cargo_profile_dir("cargo build --profile=dev-release"),
        "dev-release"
    );
    // `--profile` beats `--release` whichever comes first, as in cargo.
    assert_eq!(
        cargo_profile_dir("cargo build --release --profile dev-release"),
        "dev-release"
    );
    assert_eq!(cargo_profile_dir("cargo build --profile dev"), "debug");
    assert_eq!(cargo_profile_dir("cargo build --profile bench"), "release");
    assert_eq!(
        cargo_profile_dir("~/.cargo/bin/cargo build -p pixel-cli"),
        "debug"
    );
    // Not a cargo invocation: no flag semantics, historical `release`
    // (the upgrade CLI tests fake the build with `/usr/bin/true`).
    assert_eq!(cargo_profile_dir("/usr/bin/true"), "release");
    assert_eq!(cargo_profile_dir("./scripts/build.sh"), "release");
    assert_eq!(
        cargo_profile_dir("./scripts/build.sh --profile fast"),
        "fast"
    );
}
use super::*;

fn sandbox(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pixel-upgrade-target-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn touch(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
    path.canonicalize().unwrap()
}

#[test]
fn explicit_flag_wins_verbatim() {
    let t = resolve_upgrade_target(
        Some(PathBuf::from("/opt/x/pixel")),
        Some(PathBuf::from("/nope")),
        None,
        Path::new("/home/u"),
    );
    assert_eq!(t.path, PathBuf::from("/opt/x/pixel"));
    assert_eq!(t.source, "--install-path");
    assert!(t.explicit);
}

/// The point of the change: on a machine where `pixel` is a managed
/// install behind a shim, the running binary is that install, and the
/// upgrade must land there, not in a `~/.local/bin` that shadows or
/// misses PATH.
#[test]
fn running_binary_is_the_install_location() {
    let d = sandbox("running");
    let managed = touch(&d.join("mise/installs/pixel/rev-abc/bin/pixel"));
    let t = resolve_upgrade_target(None, Some(managed.clone()), None, &d);
    assert_eq!(t.path, managed);
    assert_eq!(t.source, "running binary");
    assert!(!t.explicit, "a resolved default is subject to the refusal");
    let _ = std::fs::remove_dir_all(&d);
}

/// `target/release/pixel upgrade` (or a test binary) must never make
/// the build output the install location.
#[test]
fn cargo_target_binary_falls_through_to_path_then_default() {
    let d = sandbox("target");
    let built = touch(&d.join("repo/target/release/pixel"));
    let on_path = touch(&d.join("cellar/bin/pixel"));
    let shim = touch(&d.join("mise/shims/pixel"));
    let path_var = std::env::join_paths([
        shim.parent().unwrap().to_path_buf(),
        d.join("repo/target/release"),
        on_path.parent().unwrap().to_path_buf(),
    ])
    .unwrap();
    let t = resolve_upgrade_target(None, Some(built.clone()), Some(&path_var), &d);
    assert_eq!(t.path, on_path, "shim dir and target dir skipped");
    assert_eq!(t.source, "first pixel on PATH");
    assert!(!t.explicit);

    let t = resolve_upgrade_target(None, Some(built), None, &d);
    assert_eq!(t.path, d.join(".local/bin/pixel"));
    assert_eq!(t.source, "default");
    assert!(!t.explicit);
    let _ = std::fs::remove_dir_all(&d);
}

/// #513: `target/` symlinked into a build cache (or a
/// `CARGO_TARGET_DIR` elsewhere) canonicalizes to a path with no
/// `target` component. Cargo's `CACHEDIR.TAG` above it still marks it
/// as build output, for the running binary and a PATH entry alike; a
/// cache tag written by another tool does not.
#[test]
fn a_build_dir_reached_through_a_symlink_is_still_build_output() {
    let d = sandbox("symlinked-target");
    let build = d.join("cache/build");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(
        build.join("CACHEDIR.TAG"),
        [
            CARGO_CACHEDIR_TAG,
            b"\n# For information about cache directory tags see https://bford.info/cachedir/\n",
        ]
        .concat(),
    )
    .unwrap();
    let built = touch(&build.join("debug/pixel"));
    std::fs::create_dir_all(d.join("repo")).unwrap();
    std::os::unix::fs::symlink(&build, d.join("repo/target")).unwrap();
    let via_link = d.join("repo/target/debug/pixel");
    assert_eq!(via_link.canonicalize().unwrap(), built);
    assert!(!built.components().any(|c| c.as_os_str() == "target"));
    assert!(is_cargo_target_path(&built));

    let path_var = std::env::join_paths([built.parent().unwrap()]).unwrap();
    let t = resolve_upgrade_target(None, Some(via_link), Some(&path_var), &d);
    assert_eq!(
        t.path,
        d.join(".local/bin/pixel"),
        "neither the exe nor PATH"
    );
    assert_eq!(t.source, "default");

    std::fs::write(
        build.join("CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n# Created by some other tool.\n",
    )
    .unwrap();
    assert!(!is_cargo_target_path(&built));
    let t = resolve_upgrade_target(None, Some(built.clone()), None, &d);
    assert_eq!((t.path, t.source), (built, "running binary"));
    let _ = std::fs::remove_dir_all(&d);
}

/// #530: cargo writes `CACHEDIR.TAG` only into a target directory it
/// creates, so a build cache that creates `target/` itself leaves none,
/// and the running test binary was overwritten again. Cargo still takes
/// its lock in the profile directory beside the binary, whoever created
/// `target/`: either lock file, alone, marks build output. A directory
/// that merely has the lock's name does not.
#[test]
fn a_build_cache_target_without_the_tag_is_still_build_output() {
    for lock in [".cargo-lock", ".cargo-artifact-lock"] {
        let d = sandbox("untagged-target");
        let build = d.join("cache/build");
        let built = touch(&build.join("debug/pixel"));
        std::fs::write(build.join("debug").join(lock), b"").unwrap();
        std::fs::create_dir_all(d.join("repo")).unwrap();
        std::os::unix::fs::symlink(&build, d.join("repo/target")).unwrap();
        assert!(!build.join("CACHEDIR.TAG").exists());
        assert!(is_cargo_target_path(&built), "{lock}");

        let via_link = d.join("repo/target/debug/pixel");
        let path_var = std::env::join_paths([built.parent().unwrap()]).unwrap();
        let t = resolve_upgrade_target(None, Some(via_link), Some(&path_var), &d);
        assert_eq!(
            (t.path, t.source),
            (d.join(".local/bin/pixel"), "default"),
            "{lock}: neither the exe nor PATH"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    let d = sandbox("lock-named-dir");
    let installed = touch(&d.join("opt/bin/pixel"));
    std::fs::create_dir_all(d.join("opt/bin/.cargo-lock")).unwrap();
    assert!(!is_cargo_target_path(&installed));
    let t = resolve_upgrade_target(None, Some(installed.clone()), None, &d);
    assert_eq!((t.path, t.source), (installed, "running binary"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn shadow_is_reported_only_when_a_different_pixel_comes_first() {
    let d = sandbox("shadow");
    let stale = touch(&d.join("local/bin/pixel"));
    let managed = touch(&d.join("mise/installs/pixel/bin/pixel"));
    let link_dir = d.join("linkdir");
    std::fs::create_dir_all(&link_dir).unwrap();
    std::os::unix::fs::symlink(&managed, link_dir.join("pixel")).unwrap();

    let stale_first = std::env::join_paths([
        stale.parent().unwrap().to_path_buf(),
        managed.parent().unwrap().to_path_buf(),
    ])
    .unwrap();
    assert_eq!(
        upgrade_shadowed_by(&managed, Some(&stale_first)),
        Some(stale.clone())
    );

    let managed_first = std::env::join_paths([
        managed.parent().unwrap().to_path_buf(),
        stale.parent().unwrap().to_path_buf(),
    ])
    .unwrap();
    assert_eq!(upgrade_shadowed_by(&managed, Some(&managed_first)), None);

    // A symlink to the installed binary is the same file, not a shadow.
    let link_first =
        std::env::join_paths([link_dir, stale.parent().unwrap().to_path_buf()]).unwrap();
    assert_eq!(upgrade_shadowed_by(&managed, Some(&link_first)), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// `pixel upgrade --dev` exists to never touch `pixel`: a `pixel` earlier
/// on PATH is not shadowing `pixel-dev`, so warning about it would be a
/// false alarm on every dev install.
#[test]
fn a_binary_under_another_name_is_never_shadowed() {
    let d = sandbox("devshadow");
    let managed = touch(&d.join("mise/installs/pixel/bin/pixel"));
    let dev = touch(&d.join("local/bin/pixel-dev"));
    let path_var = std::env::join_paths([managed.parent().unwrap()]).unwrap();
    assert_eq!(upgrade_shadowed_by(&dev, Some(&path_var)), None);
    assert_eq!(
        dev_install_path(Path::new("/home/u")),
        PathBuf::from("/home/u/.local/bin/pixel-dev")
    );
    let _ = std::fs::remove_dir_all(&d);
}

fn roots_of(roots: &[ManagedRoot]) -> Vec<(PathBuf, &'static str)> {
    roots
        .iter()
        .map(|r| (r.root.clone(), r.manager.name()))
        .collect()
}

/// The trees a bare upgrade must not write into: mise's default and
/// relocated `installs/`, the macOS and Linuxbrew Cellars, and the Cellar
/// `brew shellenv` exports. An unset or empty variable adds nothing
/// (an empty `HOMEBREW_CELLAR` would otherwise make `""` a root).
#[test]
fn package_manager_roots_cover_mise_and_homebrew() {
    let home = Path::new("/nonexistent-home");
    let base = roots_of(&package_manager_roots(home, None, None));
    assert_eq!(
        base,
        vec![
            (home.join(".local/share/mise/installs"), "mise"),
            (PathBuf::from("/opt/homebrew/Cellar"), "Homebrew"),
            (PathBuf::from("/usr/local/Cellar"), "Homebrew"),
            (
                PathBuf::from("/home/linuxbrew/.linuxbrew/Cellar"),
                "Homebrew"
            ),
        ]
    );
    let empty = std::ffi::OsStr::new("");
    assert_eq!(
        roots_of(&package_manager_roots(home, Some(empty), Some(empty))),
        base
    );
    let with_env = roots_of(&package_manager_roots(
        home,
        Some(std::ffi::OsStr::new("/nonexistent-mise")),
        Some(std::ffi::OsStr::new("/nonexistent-cellar")),
    ));
    assert!(with_env.contains(&(PathBuf::from("/nonexistent-mise/installs"), "mise")));
    assert!(with_env.contains(&(PathBuf::from("/nonexistent-cellar"), "Homebrew")));
    assert_eq!(with_env.len(), 6);
}

fn target(path: PathBuf, explicit: bool) -> UpgradeTarget {
    UpgradeTarget {
        path,
        source: "running binary",
        explicit,
    }
}

/// The refusal is the guard against clobbering a managed install: a
/// resolved path under a root is refused (directly or through a
/// symlink), a sibling that merely shares a name prefix is not, and an
/// explicit `--install-path` is always the user's call.
#[test]
fn refusal_follows_symlinks_into_managed_roots_and_spares_explicit_paths() {
    let d = sandbox("refusal");
    let cellar = d.join("Cellar");
    let keg = touch(&cellar.join("pixel/1.0/bin/pixel"));
    let roots = vec![ManagedRoot {
        root: cellar.canonicalize().unwrap(),
        manager: ManagedBy::Homebrew,
    }];

    let reason = upgrade_target_refusal(&target(keg.clone(), false), &roots).unwrap();
    assert!(reason.contains(&keg.display().to_string()), "{reason}");
    assert!(reason.contains("(running binary)"), "{reason}");
    assert!(reason.contains("Homebrew"), "{reason}");
    assert!(
        reason.contains("brew update && brew upgrade LivioGama/tap/pixel"),
        "{reason}"
    );

    let link = d.join("bin/pixel");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&keg, &link).unwrap();
    let reason = upgrade_target_refusal(&target(link, false), &roots).unwrap();
    assert!(
        reason.contains(&keg.display().to_string()),
        "resolved: {reason}"
    );

    assert!(upgrade_target_refusal(&target(keg, true), &roots).is_none());
    let sibling = touch(&d.join("Cellarx/pixel"));
    assert!(upgrade_target_refusal(&target(sibling, false), &roots).is_none());
    // A path that does not exist yet (the `~/.local/bin/pixel` default)
    // is compared as given.
    let missing = cellar.canonicalize().unwrap().join("new/pixel");
    assert!(upgrade_target_refusal(&target(missing, false), &roots).is_some());
    let _ = std::fs::remove_dir_all(&d);
}

/// Each manager's refusal names its own update command: a mise or
/// Homebrew install is updated through its package manager, not by
/// `pixel self-update`, and that command is the actionable half of the
/// refusal.
#[test]
fn refusal_names_the_owning_managers_update_command() {
    let d = sandbox("manager-command");
    let mise_root = d.join("Mise");
    let mise_keg = touch(&mise_root.join("pixel/1.0/bin/pixel"));
    let cellar = d.join("Cellar");
    let brew_keg = touch(&cellar.join("pixel/1.0/bin/pixel"));
    let roots = vec![
        ManagedRoot {
            root: mise_root.canonicalize().unwrap(),
            manager: ManagedBy::Mise,
        },
        ManagedRoot {
            root: cellar.canonicalize().unwrap(),
            manager: ManagedBy::Homebrew,
        },
    ];

    let reason = upgrade_target_refusal(&target(mise_keg, false), &roots).unwrap();
    assert!(reason.contains("mise upgrade pixel"), "{reason}");

    let reason = upgrade_target_refusal(&target(brew_keg, false), &roots).unwrap();
    assert!(
        reason.contains("brew update && brew upgrade LivioGama/tap/pixel"),
        "{reason}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
