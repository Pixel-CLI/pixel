// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The first-party brief probes (`probes/` at the repository root), embedded in
//! the binary and written to `~/.pixel/plugins/<name>/` by `pixel setup`, so
//! the prompt-submit brief runs them with nothing else installed. They are user
//! plugins, trusted as first-party: the user tier is not trust-gated. Each
//! directory carries a marker so setup updates its own copies, `--remove`
//! deletes only them, and a plugin of the same name the user wrote is never
//! overwritten. `pixel plugin disable <name>` switches one off without
//! deleting it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::Result;
use crate::install::{CheckStatus, InstallStep, dry_run_summary};

/// The file that says the directory is Pixel\'s own copy.
const MARKER: &str = ".pixel-bundled";
/// Where the user plugins live, under the home directory.
const PLUGINS_REL: &str = ".pixel/plugins";

const BUNDLED: &[(&str, &str, &str)] = &[
    (
        "cand-j-callers",
        include_str!("../../../probes/cand-j-callers/pixel-plugin.toml"),
        include_str!("../../../probes/cand-j-callers/probe"),
    ),
    (
        "cand-j-chain",
        include_str!("../../../probes/cand-j-chain/pixel-plugin.toml"),
        include_str!("../../../probes/cand-j-chain/probe"),
    ),
    (
        "cand-j-stop-cause",
        include_str!("../../../probes/cand-j-stop-cause/pixel-plugin.toml"),
        include_str!("../../../probes/cand-j-stop-cause/probe"),
    ),
    (
        "cand-k-code-sites",
        include_str!("../../../probes/cand-k-code-sites/pixel-plugin.toml"),
        include_str!("../../../probes/cand-k-code-sites/probe"),
    ),
    (
        "cand-k-commit-patch",
        include_str!("../../../probes/cand-k-commit-patch/pixel-plugin.toml"),
        include_str!("../../../probes/cand-k-commit-patch/probe"),
    ),
    (
        "config-registry",
        include_str!("../../../probes/config-registry/pixel-plugin.toml"),
        include_str!("../../../probes/config-registry/probe"),
    ),
    (
        "doc-sections",
        include_str!("../../../probes/doc-sections/pixel-plugin.toml"),
        include_str!("../../../probes/doc-sections/probe"),
    ),
    (
        "error-origin",
        include_str!("../../../probes/error-origin/pixel-plugin.toml"),
        include_str!("../../../probes/error-origin/probe"),
    ),
    (
        "flow-rank",
        include_str!("../../../probes/flow-rank/pixel-plugin.toml"),
        include_str!("../../../probes/flow-rank/probe"),
    ),
    (
        "history-fallback",
        include_str!("../../../probes/history-fallback/pixel-plugin.toml"),
        include_str!("../../../probes/history-fallback/probe"),
    ),
    (
        "name-files",
        include_str!("../../../probes/name-files/pixel-plugin.toml"),
        include_str!("../../../probes/name-files/probe"),
    ),
    (
        "overview-answer-evidence",
        include_str!("../../../probes/overview-answer-evidence/pixel-plugin.toml"),
        include_str!("../../../probes/overview-answer-evidence/probe"),
    ),
    (
        "rank-blocks",
        include_str!("../../../probes/rank-blocks/pixel-plugin.toml"),
        include_str!("../../../probes/rank-blocks/probe"),
    ),
    (
        "test-bodies",
        include_str!("../../../probes/test-bodies/pixel-plugin.toml"),
        include_str!("../../../probes/test-bodies/probe"),
    ),
];

/// The names of the bundled probes.
pub fn names() -> Vec<&'static str> {
    BUNDLED.iter().map(|(name, _, _)| *name).collect()
}

fn write_if_changed(path: &Path, content: &str, mode: u32) -> Result<bool> {
    if fs::read_to_string(path).is_ok_and(|have| have == content)
        && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == mode)
    {
        return Ok(false);
    }
    let staged = path.with_extension("pixel-tmp");
    fs::write(&staged, content)?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(mode))?;
    fs::rename(&staged, path)?;
    Ok(true)
}

/// Write (or update) the bundled probes under `home`.
pub(crate) fn install(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let plugins = home.join(PLUGINS_REL);
    let (mut changed, mut kept, mut foreign) = (0usize, 0usize, Vec::new());
    for (name, manifest, program) in BUNDLED {
        let dir = plugins.join(name);
        if dir.exists() && !dir.join(MARKER).is_file() {
            foreign.push(*name);
            continue;
        }
        if dry_run {
            // What a real run would write: a file missing or different.
            let differs = |file: &str, content: &str| {
                fs::read_to_string(dir.join(file)).map_or(true, |have| have != content)
            };
            if differs("pixel-plugin.toml", manifest)
                || differs("probe", program)
                || !dir.join(MARKER).is_file()
            {
                changed += 1;
            } else {
                kept += 1;
            }
            continue;
        }
        fs::create_dir_all(&dir)?;
        // The marker goes first: an install cut short after it is still Pixel's
        // and the next run completes it, rather than leaving a "user" plugin.
        let mut touched = write_if_changed(
            &dir.join(MARKER),
            "first-party probe shipped with pixel; `pixel setup` keeps it current\n",
            0o644,
        )?;
        touched |= write_if_changed(&dir.join("pixel-plugin.toml"), manifest, 0o644)?;
        touched |= write_if_changed(&dir.join("probe"), program, 0o755)?;
        if touched {
            changed += 1;
        } else {
            kept += 1;
        }
    }
    let mut summary = format!(
        "{} first-party brief probes in ~/.pixel/plugins ({changed} written, {kept} current)",
        BUNDLED.len() - foreign.len()
    );
    if !foreign.is_empty() {
        summary.push_str(&format!(
            "; left alone, a plugin of your own has the name: {}",
            foreign.join(", ")
        ));
    }
    Ok(InstallStep {
        id: "brief-probes".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: None,
    })
}

/// Delete the copies `install` wrote; a directory without the marker is the user\'s.
pub(crate) fn remove(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let plugins = home.join(PLUGINS_REL);
    let mut removed = 0usize;
    for (name, _, _) in BUNDLED {
        let dir = plugins.join(name);
        if dir.join(MARKER).is_file() {
            if !dry_run {
                // Only the files setup wrote; whatever the user put in the directory stays, and so does it.
                for file in ["pixel-plugin.toml", "probe", MARKER] {
                    let _ = fs::remove_file(dir.join(file));
                }
                let _ = fs::remove_dir(&dir);
            }
            removed += 1;
        }
    }
    Ok(InstallStep {
        id: "brief-probes".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(
            dry_run,
            &format!("{removed} first-party brief probes removed from ~/.pixel/plugins"),
        ),
        detail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED_PROBE_NAMES: &[&str] = &[
        "cand-j-callers",
        "cand-j-chain",
        "cand-j-stop-cause",
        "cand-k-code-sites",
        "cand-k-commit-patch",
        "config-registry",
        "doc-sections",
        "error-origin",
        "flow-rank",
        "history-fallback",
        "name-files",
        "overview-answer-evidence",
        "rank-blocks",
        "test-bodies",
    ];

    #[test]
    fn every_bundled_probe_has_a_valid_probe_manifest_and_an_executable_program() {
        let home = tempfile::tempdir().unwrap();
        install(home.path(), false).unwrap();
        let host = pixel_plugin::Host {
            home: home.path().to_path_buf(),
            repo_root: None,
            path: std::ffi::OsString::new(),
            github_base: None,
        };
        let set = pixel_plugin::probes(&host).unwrap();
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
        let found: Vec<&str> = set.probes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names(), SHIPPED_PROBE_NAMES);
        assert_eq!(found, SHIPPED_PROBE_NAMES);
        assert!(
            set.probes
                .iter()
                .all(|p| p.spec.budget_ms <= pixel_plugin::MAX_PROBE_BUDGET_MS)
        );
    }

    #[test]
    fn install_is_idempotent_updates_its_own_copies_and_never_overwrites_a_users_plugin() {
        let home = tempfile::tempdir().unwrap();
        let n = SHIPPED_PROBE_NAMES.len();
        let first = install(home.path(), false).unwrap();
        assert!(
            first
                .summary
                .contains(&format!("{n} first-party brief probes")),
            "{}",
            first.summary
        );
        assert!(
            first.summary.contains(&format!("{n} written, 0 current")),
            "{}",
            first.summary
        );
        let again = install(home.path(), false).unwrap();
        assert!(
            again.summary.contains(&format!("0 written, {n} current")),
            "{}",
            again.summary
        );
        let probe = home.path().join(PLUGINS_REL).join("flow-rank/probe");
        fs::write(&probe, "tampered").unwrap();
        assert!(
            install(home.path(), false)
                .unwrap()
                .summary
                .contains("1 written")
        );
        assert_eq!(
            fs::read_to_string(&probe).unwrap(),
            BUNDLED.iter().find(|b| b.0 == "flow-rank").unwrap().2
        );
        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // A plugin of the user\'s own under a bundled name is left alone.
        let own = home.path().join(PLUGINS_REL).join("name-files");
        fs::remove_dir_all(&own).unwrap();
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("pixel-plugin.toml"), "mine").unwrap();
        let step = install(home.path(), false).unwrap();
        assert!(
            step.summary.contains("left alone") && step.summary.contains("name-files"),
            "{}",
            step.summary
        );
        assert_eq!(
            fs::read_to_string(own.join("pixel-plugin.toml")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn a_probe_dir_holding_only_the_marker_is_pixels_and_the_next_install_completes_it() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(PLUGINS_REL).join("flow-rank");
        // What an install cut short after its first write leaves behind.
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MARKER), "first-party probe\n").unwrap();

        let step = install(home.path(), false).unwrap();

        assert!(!step.summary.contains("left alone"), "{}", step.summary);
        let (_, manifest, program) = BUNDLED.iter().find(|b| b.0 == "flow-rank").unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("pixel-plugin.toml")).unwrap(),
            *manifest
        );
        assert_eq!(fs::read_to_string(dir.join("probe")).unwrap(), *program);
    }

    #[test]
    fn a_dry_run_reports_what_a_real_run_would_write_and_remove_keeps_what_the_user_added() {
        let home = tempfile::tempdir().unwrap();
        let n = SHIPPED_PROBE_NAMES.len();
        assert!(
            install(home.path(), true)
                .unwrap()
                .summary
                .contains(&format!("{n} written, 0 current"))
        );
        install(home.path(), false).unwrap();
        assert!(
            install(home.path(), true)
                .unwrap()
                .summary
                .contains(&format!("0 written, {n} current"))
        );
        let probe = home.path().join(PLUGINS_REL).join("rank-blocks/probe");
        fs::write(&probe, "old version").unwrap();
        assert!(
            install(home.path(), true)
                .unwrap()
                .summary
                .contains(&format!("1 written, {} current", n - 1))
        );
        assert_eq!(
            fs::read_to_string(&probe).unwrap(),
            "old version",
            "a dry run never writes"
        );
        let mine = home.path().join(PLUGINS_REL).join("flow-rank/notes.txt");
        fs::write(&mine, "mine").unwrap();
        remove(home.path(), false).unwrap();
        assert!(
            mine.is_file(),
            "a file of the user's inside a bundled directory survives --remove"
        );
        assert!(
            !home
                .path()
                .join(PLUGINS_REL)
                .join("flow-rank/probe")
                .exists()
        );
        assert!(
            !home
                .path()
                .join(PLUGINS_REL)
                .join("config-registry")
                .exists(),
            "an emptied directory goes"
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_and_remove_deletes_only_what_install_wrote() {
        let home = tempfile::tempdir().unwrap();
        install(home.path(), true).unwrap();
        assert!(!home.path().join(PLUGINS_REL).exists());
        install(home.path(), false).unwrap();
        let own = home.path().join(PLUGINS_REL).join("mine");
        fs::create_dir_all(&own).unwrap();
        fs::remove_file(
            home.path()
                .join(PLUGINS_REL)
                .join("doc-sections")
                .join(MARKER),
        )
        .unwrap();
        let step = remove(home.path(), false).unwrap();
        let removed = format!("{} first-party", SHIPPED_PROBE_NAMES.len() - 1);
        assert!(step.summary.contains(&removed), "{}", step.summary);
        assert!(own.is_dir());
        assert!(
            home.path().join(PLUGINS_REL).join("doc-sections").is_dir(),
            "no marker: not ours"
        );
        assert!(!home.path().join(PLUGINS_REL).join("flow-rank").exists());
    }
}
