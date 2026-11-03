// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The brief probes the host offers the prompt-submit brief: the plugins with
//! the `brief-probe` capability that are switched on and may run. Repo probes
//! (`<repo>/.pixel/plugins/<name>/`) run only at the digest `pixel plugin
//! trust` recorded; user probes (`~/.pixel/plugins/<name>/`, the first-party
//! ones included) run as installed. Nothing here executes a probe.

use std::path::PathBuf;

use crate::discover::{Found, Place, contained_program, find_in, is_trusted};
use crate::manifest::{self, ProbeSpec};
use crate::state::Config;
use crate::{Error, Host};

/// A probe that may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub name: String,
    pub place: Place,
    pub dir: PathBuf,
    /// The executable, canonical and inside `dir`.
    pub program: PathBuf,
    pub spec: ProbeSpec,
}

/// The probes of `host` in name order, and why each of the others is not
/// among them (disabled, untrusted, unrunnable): a repo probe shadows a user
/// probe of the same name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProbeSet {
    pub probes: Vec<Probe>,
    pub skipped: Vec<(String, String)>,
}

pub fn probes(host: &Host) -> Result<ProbeSet, Error> {
    let disabled = Config::at(host.config_file()).disabled()?;
    let mut set = ProbeSet::default();
    let mut tiers = Vec::new();
    if let Some(dir) = host.repo_plugins() {
        tiers.push((Place::Repo, dir));
    }
    tiers.push((Place::User, host.user_plugins()));
    for (place, plugins) in tiers {
        let Ok(entries) = std::fs::read_dir(&plugins) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| manifest::valid_name(n))
            .collect();
        names.sort();
        for name in names {
            let found = match find_in(&plugins, &name) {
                Ok(Some(found)) if found.manifest.is_probe() => found,
                Ok(_) => continue,
                Err(e) => {
                    set.skipped.push((name, e.to_string()));
                    continue;
                }
            };
            if set.probes.iter().any(|p| p.name == name)
                || set.skipped.iter().any(|(n, _)| *n == name)
            {
                continue; // shadowed by the repo tier
            }
            match admit(host, place, found, &disabled) {
                Ok(probe) => set.probes.push(probe),
                Err(why) => set.skipped.push((name, why)),
            }
        }
    }
    set.probes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(set)
}

fn admit(host: &Host, place: Place, found: Found, disabled: &[String]) -> Result<Probe, String> {
    let Found { dir, manifest } = found;
    if disabled.contains(&manifest.name) {
        return Err("disabled".to_owned());
    }
    if place == Place::Repo && !is_trusted(host, &manifest.name, &dir).map_err(|e| e.to_string())? {
        return Err(format!(
            "untrusted: run `pixel plugin trust {}`",
            manifest.name
        ));
    }
    let program = contained_program(&dir, &manifest).map_err(|e| e.to_string())?;
    let spec = manifest
        .probe
        .clone()
        .ok_or_else(|| "no [brief-probe] table".to_owned())?;
    Ok(Probe {
        name: manifest.name,
        place,
        dir,
        program,
        spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fixture;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const TOML: &str = "name = \"NAME\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"d\"\nrun = \"probe\"\ncapabilities = [\"brief-probe\"]\n\n[brief-probe]\nowner = \"o\"\nbudget_ms = 500\n";

    fn probe_dir(plugins: &std::path::Path, name: &str) -> PathBuf {
        let dir = plugins.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pixel-plugin.toml"), TOML.replace("NAME", name)).unwrap();
        let run = dir.join("probe");
        fs::write(&run, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn user_probes_are_listed_in_name_order_and_command_plugins_are_not_probes() {
        let f = fixture();
        probe_dir(&f.host.user_plugins(), "b-probe");
        probe_dir(&f.host.user_plugins(), "a-probe");
        crate::testutil::plugin_dir(&f.host.user_plugins(), "cmd", "");
        let set = probes(&f.host).unwrap();
        let names: Vec<&str> = set.probes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a-probe", "b-probe"]);
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
        assert_eq!(set.probes[0].spec.budget_ms, 500);
        assert!(set.probes[0].program.ends_with("probe"));
    }

    // The brief reads every repo probe's manifest at each prompt: one the
    // repository committed as a link is skipped with its reason, never read
    // through, and the other probes still run.
    #[test]
    fn a_repo_probe_whose_manifest_is_a_link_is_skipped_and_the_others_still_run() {
        let f = fixture();
        probe_dir(&f.host.user_plugins(), "q");
        let repo = f.host.repo_plugins().unwrap();
        let linked = probe_dir(&repo, "p");
        let elsewhere = f.tmp.path().join("elsewhere.toml");
        fs::rename(linked.join("pixel-plugin.toml"), &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, linked.join("pixel-plugin.toml")).unwrap();
        let set = probes(&f.host).unwrap();
        let names: Vec<&str> = set.probes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["q"]);
        assert_eq!(set.skipped.len(), 1, "{:?}", set.skipped);
        assert_eq!(set.skipped[0].0, "p");
        assert!(
            set.skipped[0].1.contains("is a symbolic link"),
            "{:?}",
            set.skipped
        );
    }

    #[test]
    fn a_disabled_probe_is_skipped_and_enabling_brings_it_back() {
        let f = fixture();
        probe_dir(&f.host.user_plugins(), "p");
        crate::disable(&f.host, "p").unwrap();
        let set = probes(&f.host).unwrap();
        assert!(set.probes.is_empty());
        assert_eq!(set.skipped, [("p".to_owned(), "disabled".to_owned())]);
        crate::enable(&f.host, "p").unwrap();
        assert_eq!(probes(&f.host).unwrap().probes.len(), 1);
    }

    #[test]
    fn a_repo_probe_runs_only_at_the_digest_trust_recorded_and_shadows_the_user_one() {
        let f = fixture();
        probe_dir(&f.host.user_plugins(), "p");
        let repo = f.host.repo_plugins().unwrap();
        probe_dir(&repo, "p");
        let set = probes(&f.host).unwrap();
        assert!(
            set.probes.is_empty(),
            "the repo claimed the name and is untrusted"
        );
        assert!(
            set.skipped[0].1.starts_with("untrusted"),
            "{:?}",
            set.skipped
        );
        crate::trust(&f.host, "p").unwrap();
        let set = probes(&f.host).unwrap();
        assert_eq!(set.probes.len(), 1);
        assert_eq!(set.probes[0].place, Place::Repo);
        fs::write(repo.join("p").join("probe"), "#!/bin/sh\necho changed\n").unwrap();
        assert!(
            probes(&f.host).unwrap().probes.is_empty(),
            "any change revokes the trust"
        );
    }

    #[test]
    fn a_broken_manifest_or_a_missing_program_is_skipped_with_its_reason() {
        let f = fixture();
        let dir = probe_dir(&f.host.user_plugins(), "p");
        fs::remove_file(dir.join("probe")).unwrap();
        let broken = f.host.user_plugins().join("q");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("pixel-plugin.toml"), "name = \"q\"\n").unwrap();
        let set = probes(&f.host).unwrap();
        assert!(set.probes.is_empty());
        assert_eq!(set.skipped.len(), 2, "{:?}", set.skipped);
    }

    #[test]
    fn only_a_probe_can_be_disabled() {
        let f = fixture();
        crate::testutil::plugin_dir(&f.host.user_plugins(), "cmd", "");
        let err = crate::disable(&f.host, "cmd").unwrap_err().to_string();
        assert!(err.contains("not a brief probe"), "{err}");
    }
}
