// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Finding a plugin by name: repo, then user, then PATH; and the checks a
//! plugin passes before anything of it runs.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::hash::dir_digest;
use crate::manifest::{self, Manifest};
use crate::state::{CONFIG_FILE, Config, TRUST_FILE, TrustStore};
use crate::{Capability, Error};

/// The per-user and per-repository state directory.
const STATE_DIR: &str = ".pixel";
/// Plugin directories, under the state directory.
const PLUGINS_DIR: &str = "plugins";
/// Executable bits of a file on PATH.
const EXEC_BITS: u32 = 0o111;

/// Where this invocation looks. Every input is a field, never read from the
/// process, so a test points the host at a scratch home, repo and PATH.
#[derive(Debug, Clone)]
pub struct Host {
    pub home: PathBuf,
    pub repo_root: Option<PathBuf>,
    pub path: OsString,
}

impl Host {
    /// `~/.pixel`.
    pub fn state_dir(&self) -> PathBuf {
        self.home.join(STATE_DIR)
    }

    /// `~/.pixel/plugins`.
    pub fn user_plugins(&self) -> PathBuf {
        self.state_dir().join(PLUGINS_DIR)
    }

    /// `<repo>/.pixel/plugins`. Not offered when the repository root is the
    /// home directory: that directory is `~/.pixel/plugins`, the user tier,
    /// and must not turn into a trust-gated repo tier.
    pub fn repo_plugins(&self) -> Option<PathBuf> {
        self.repo_root
            .as_ref()
            .filter(|root| **root != self.home)
            .map(|root| root.join(STATE_DIR).join(PLUGINS_DIR))
    }

    pub fn trust_file(&self) -> PathBuf {
        self.state_dir().join(TRUST_FILE)
    }

    pub fn config_file(&self) -> PathBuf {
        self.state_dir().join(CONFIG_FILE)
    }
}

/// Which lookup tier a plugin came from; also the resolution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    Repo,
    User,
    Path,
}

impl Place {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::User => "user",
            Self::Path => "path",
        }
    }
}

/// A plugin that passed every check and can be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub place: Place,
    /// The executable, canonical and inside `dir` for a manifest plugin.
    pub program: PathBuf,
    /// The plugin directory (`None` for `pixel-<name>` on PATH).
    pub dir: Option<PathBuf>,
}

/// A directory plugin as found on disk: manifest read, nothing else checked.
pub(crate) struct Found {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// `<plugins>/<name>/` with a manifest. The directory must be a real
/// directory: a link there could point anywhere, and a repo chooses what its
/// own `.pixel/plugins` holds.
pub(crate) fn find_in(plugins: &Path, name: &str) -> Result<Option<Found>, Error> {
    let dir = plugins.join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return Ok(None),
    }
    Ok(manifest::load(&dir, Some(name))?.map(|manifest| Found { dir, manifest }))
}

/// Resolve `name` for execution. `Ok(None)`: nothing by that name anywhere.
/// A repo plugin that exists but is untrusted is an error, not a fall-through:
/// the repo claimed the name.
pub fn resolve(host: &Host, name: &str) -> Result<Option<Resolved>, Error> {
    if !manifest::valid_name(name) {
        return Ok(None);
    }
    if let Some(plugins) = host.repo_plugins()
        && let Some(found) = find_in(&plugins, name)?
    {
        return runnable(host, Place::Repo, found).map(Some);
    }
    if let Some(found) = find_in(&host.user_plugins(), name)? {
        return runnable(host, Place::User, found).map(Some);
    }
    Ok(find_on_path(name, &host.path).map(|program| Resolved {
        name: name.to_owned(),
        place: Place::Path,
        program,
        dir: None,
    }))
}

fn runnable(host: &Host, place: Place, found: Found) -> Result<Resolved, Error> {
    let Found { dir, manifest } = found;
    if !manifest.has_command() {
        return Err(Error::NoCommandCapability(manifest.name));
    }
    if place == Place::Repo && !is_trusted(host, &manifest.name, &dir)? {
        return Err(Error::Untrusted {
            name: manifest.name,
            dir,
        });
    }
    if manifest.network && !Config::at(host.config_file()).is_enabled(&manifest.name)? {
        return Err(Error::NotEnabled(manifest.name));
    }
    let program = contained_program(&dir, &manifest)?;
    Ok(Resolved {
        name: manifest.name,
        place,
        program,
        dir: Some(dir),
    })
}

/// Whether the directory's current content is the content `name` was
/// trusted at.
pub(crate) fn is_trusted(host: &Host, name: &str, dir: &Path) -> Result<bool, Error> {
    let digest = dir_digest(dir).map_err(|e| Error::io(format!("hash {}", dir.display()), e))?;
    TrustStore::at(host.trust_file()).is_trusted(name, &digest)
}

/// The manifest's `run`, canonicalised, required to be a file inside the
/// plugin directory once every symlink is resolved.
pub fn contained_program(dir: &Path, manifest: &Manifest) -> Result<PathBuf, Error> {
    let bad = |why: &str| Error::BadRun {
        name: manifest.name.clone(),
        run: manifest.run.clone(),
        why: why.to_owned(),
    };
    let root = dir
        .canonicalize()
        .map_err(|e| Error::io(format!("resolve {}", dir.display()), e))?;
    let program = dir
        .join(&manifest.run)
        .canonicalize()
        .map_err(|e| bad(&format!("cannot be resolved ({e})")))?;
    if !program.starts_with(&root) {
        return Err(bad("resolves outside the plugin directory"));
    }
    if !program.is_file() {
        return Err(bad("is not a file"));
    }
    Ok(program)
}

/// The first executable `pixel-<name>` along `path`. An empty PATH entry
/// means the current directory to a shell; it is skipped here, so a file in
/// the working directory never becomes a plugin by accident.
pub fn find_on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    let file = format!("pixel-{name}");
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(&file))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & EXEC_BITS != 0)
}

/// One line of `pixel plugin list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub place: Place,
    pub version: String,
    pub api: String,
    pub capabilities: Vec<Capability>,
    /// `None` when the plugin has no network access to opt into.
    pub enabled: Option<bool>,
    /// `None` outside the repo tier, where nothing is trust-gated.
    pub trusted: Option<bool>,
    /// Why the plugin cannot run, when its manifest is invalid.
    pub problem: Option<String>,
}

/// Every plugin visible from `host`, in resolution order, a shadowed one
/// included (so `list` shows why a name resolves where it does).
pub fn list(host: &Host) -> Result<Vec<Row>, Error> {
    let mut rows = Vec::new();
    if let Some(plugins) = host.repo_plugins() {
        rows.extend(dir_rows(host, Place::Repo, &plugins)?);
    }
    rows.extend(dir_rows(host, Place::User, &host.user_plugins())?);
    rows.extend(path_rows(&host.path));
    Ok(rows)
}

fn dir_rows(host: &Host, place: Place, plugins: &Path) -> Result<Vec<Row>, Error> {
    let Ok(entries) = std::fs::read_dir(plugins) else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| manifest::valid_name(n))
        .collect();
    names.sort();
    let mut rows = Vec::new();
    for name in names {
        match find_in(plugins, &name) {
            Ok(None) => {}
            Ok(Some(found)) => rows.push(manifest_row(host, place, &found)?),
            Err(Error::Manifest { message, .. }) => rows.push(Row {
                name,
                place,
                version: "-".to_owned(),
                api: "-".to_owned(),
                capabilities: Vec::new(),
                enabled: None,
                trusted: None,
                problem: Some(message),
            }),
            Err(e) => return Err(e),
        }
    }
    Ok(rows)
}

fn manifest_row(host: &Host, place: Place, found: &Found) -> Result<Row, Error> {
    let m = &found.manifest;
    let enabled = if m.network {
        Some(Config::at(host.config_file()).is_enabled(&m.name)?)
    } else {
        None
    };
    let trusted = if place == Place::Repo {
        Some(is_trusted(host, &m.name, &found.dir)?)
    } else {
        None
    };
    Ok(Row {
        name: m.name.clone(),
        place,
        version: m.version.clone(),
        api: m.api.to_string(),
        capabilities: m.capabilities.clone(),
        enabled,
        trusted,
        problem: None,
    })
}

fn path_rows(path: &OsStr) -> Vec<Row> {
    let mut names = std::collections::BTreeSet::new();
    for dir in std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty()) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let file = entry.file_name();
            let Some(name) = file.to_str().and_then(|f| f.strip_prefix("pixel-")) else {
                continue;
            };
            if manifest::valid_name(name) && is_executable_file(&entry.path()) {
                names.insert(name.to_owned());
            }
        }
    }
    names
        .into_iter()
        .map(|name| Row {
            name,
            place: Place::Path,
            version: "-".to_owned(),
            api: "1".to_owned(),
            capabilities: vec![Capability::Command],
            enabled: None,
            trusted: None,
            problem: None,
        })
        .collect()
}

/// The text `pixel plugin list` prints.
pub fn render(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "no plugins\n".to_owned();
    }
    let yes_no = |value: Option<bool>| match value {
        None => "-",
        Some(true) => "yes",
        Some(false) => "no",
    };
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let caps = if r.capabilities.is_empty() {
                "-".to_owned()
            } else {
                r.capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            [
                r.name.clone(),
                r.place.as_str().to_owned(),
                r.version.clone(),
                r.api.clone(),
                caps,
                yes_no(r.enabled).to_owned(),
                yes_no(r.trusted).to_owned(),
            ]
        })
        .collect();
    let header = [
        "NAME",
        "PLACE",
        "VERSION",
        "API",
        "CAPABILITIES",
        "ENABLED",
        "TRUSTED",
    ];
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            cells
                .iter()
                .map(|row| row[i].len())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |row: &[&str]| {
        let padded: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut out = line(&header);
    for (row, cell) in rows.iter().zip(&cells) {
        out.push_str(&line(&cell.iter().map(String::as_str).collect::<Vec<_>>()));
        if let Some(problem) = &row.problem {
            out.push_str(&format!("  ! {}: {problem}\n", row.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture, plugin_dir};
    use std::fs;
    use std::os::unix::fs::symlink;

    fn path_exe(host: &Host, name: &str, mode: u32) -> PathBuf {
        let file = PathBuf::from(&host.path).join(format!("pixel-{name}"));
        fs::write(&file, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    fn trust(host: &Host, name: &str) {
        let dir = host.repo_plugins().unwrap().join(name);
        let digest = dir_digest(&dir).unwrap();
        TrustStore::at(host.trust_file())
            .trust(name, &digest)
            .unwrap();
    }

    #[test]
    fn a_repository_rooted_at_home_has_no_repo_tier() {
        let mut f = fixture();
        f.host.repo_root = Some(f.host.home.clone());
        assert_eq!(f.host.repo_plugins(), None);
        plugin_dir(&f.host.user_plugins(), "mine", "");
        assert_eq!(
            resolve(&f.host, "mine").unwrap().unwrap().place,
            Place::User
        );
        f.host.repo_root = Some(f.host.home.join("work"));
        assert_eq!(
            f.host.repo_plugins(),
            Some(f.host.home.join("work/.pixel/plugins"))
        );
    }

    #[test]
    fn a_name_nobody_provides_resolves_to_nothing() {
        let f = fixture();
        assert_eq!(resolve(&f.host, "ghost").unwrap(), None);
        assert_eq!(
            resolve(&f.host, "../etc").unwrap(),
            None,
            "invalid names never look anywhere"
        );
    }

    #[test]
    fn a_user_plugin_runs_without_trust() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "tool", "");
        let got = resolve(&f.host, "tool").unwrap().unwrap();
        assert_eq!(got.place, Place::User);
        assert_eq!(got.program, dir.join("run.sh").canonicalize().unwrap());
        assert_eq!(got.dir.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn a_repo_plugin_is_refused_until_trusted_and_again_after_an_edit() {
        let f = fixture();
        let dir = plugin_dir(&f.host.repo_plugins().unwrap(), "tool", "");
        let err = resolve(&f.host, "tool").unwrap_err();
        assert!(matches!(err, Error::Untrusted { .. }), "{err}");
        assert!(
            err.to_string().contains("`pixel plugin trust tool`"),
            "{err}"
        );

        trust(&f.host, "tool");
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().place,
            Place::Repo
        );

        fs::write(dir.join("run.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(matches!(
            resolve(&f.host, "tool").unwrap_err(),
            Error::Untrusted { .. }
        ));
    }

    #[test]
    fn an_untrusted_repo_plugin_does_not_fall_through_to_the_user_one() {
        let f = fixture();
        plugin_dir(&f.host.repo_plugins().unwrap(), "tool", "");
        plugin_dir(&f.host.user_plugins(), "tool", "");
        path_exe(&f.host, "tool", 0o755);
        assert!(matches!(
            resolve(&f.host, "tool").unwrap_err(),
            Error::Untrusted { .. }
        ));
    }

    #[test]
    fn order_is_trusted_repo_then_user_then_path() {
        let f = fixture();
        let repo = plugin_dir(&f.host.repo_plugins().unwrap(), "tool", "");
        let user = plugin_dir(&f.host.user_plugins(), "tool", "");
        let exe = path_exe(&f.host, "tool", 0o755);
        trust(&f.host, "tool");
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().dir,
            Some(repo.clone())
        );
        fs::remove_dir_all(&repo).unwrap();
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().dir,
            Some(user.clone())
        );
        fs::remove_dir_all(&user).unwrap();
        let got = resolve(&f.host, "tool").unwrap().unwrap();
        assert_eq!((got.place, got.program, got.dir), (Place::Path, exe, None));
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_plugin() {
        let f = fixture();
        fs::create_dir_all(f.host.repo_plugins().unwrap().join("tool")).unwrap();
        plugin_dir(&f.host.user_plugins(), "tool", "");
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().place,
            Place::User
        );
    }

    #[test]
    fn a_plugin_directory_that_is_a_symlink_is_ignored() {
        let f = fixture();
        let elsewhere = tempfile::tempdir().unwrap();
        plugin_dir(elsewhere.path(), "tool", "");
        fs::create_dir_all(f.host.user_plugins()).unwrap();
        symlink(
            elsewhere.path().join("tool"),
            f.host.user_plugins().join("tool"),
        )
        .unwrap();
        assert_eq!(resolve(&f.host, "tool").unwrap(), None);
    }

    #[test]
    fn a_network_plugin_needs_enable_in_every_tier_that_has_a_manifest() {
        let f = fixture();
        plugin_dir(&f.host.user_plugins(), "net", "network = true");
        let err = resolve(&f.host, "net").unwrap_err();
        assert!(matches!(err, Error::NotEnabled(_)), "{err}");
        assert!(
            err.to_string().contains("`pixel plugin enable net`"),
            "{err}"
        );
        Config::at(f.host.config_file()).enable("net").unwrap();
        assert!(resolve(&f.host, "net").unwrap().is_some());
    }

    #[test]
    fn a_trusted_network_repo_plugin_still_needs_enable() {
        let f = fixture();
        plugin_dir(&f.host.repo_plugins().unwrap(), "net", "network = true");
        trust(&f.host, "net");
        assert!(matches!(
            resolve(&f.host, "net").unwrap_err(),
            Error::NotEnabled(_)
        ));
    }

    #[test]
    fn a_plugin_without_the_command_capability_cannot_be_invoked() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "idle", "");
        let text = fs::read_to_string(dir.join("pixel-plugin.toml"))
            .unwrap()
            .replace("[\"command\"]", "[]");
        fs::write(dir.join("pixel-plugin.toml"), text).unwrap();
        assert!(matches!(
            resolve(&f.host, "idle").unwrap_err(),
            Error::NoCommandCapability(_)
        ));
    }

    #[test]
    fn a_run_that_escapes_through_a_symlink_is_refused() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "esc", "");
        fs::remove_file(dir.join("run.sh")).unwrap();
        symlink("/bin/sh", dir.join("run.sh")).unwrap();
        let err = resolve(&f.host, "esc").unwrap_err();
        assert!(matches!(err, Error::BadRun { .. }), "{err}");
        assert!(
            err.to_string().contains("outside the plugin directory"),
            "{err}"
        );
    }

    #[test]
    fn a_symlink_that_stays_inside_the_directory_is_fine() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "ok", "");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::rename(dir.join("run.sh"), dir.join("bin/real.sh")).unwrap();
        symlink("bin/real.sh", dir.join("run.sh")).unwrap();
        let got = resolve(&f.host, "ok").unwrap().unwrap();
        assert_eq!(got.program, dir.join("bin/real.sh").canonicalize().unwrap());
    }

    #[test]
    fn a_missing_or_non_file_run_is_refused() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "gone", "");
        fs::remove_file(dir.join("run.sh")).unwrap();
        let err = resolve(&f.host, "gone").unwrap_err();
        assert!(err.to_string().contains("cannot be resolved"), "{err}");
        fs::create_dir(dir.join("run.sh")).unwrap();
        let err = resolve(&f.host, "gone").unwrap_err();
        assert!(err.to_string().contains("is not a file"), "{err}");
    }

    #[test]
    fn an_invalid_manifest_is_an_error_naming_the_file() {
        let f = fixture();
        let dir = plugin_dir(&f.host.user_plugins(), "bad", "");
        fs::write(dir.join("pixel-plugin.toml"), "api = 7").unwrap();
        let err = resolve(&f.host, "bad").unwrap_err().to_string();
        assert!(err.contains("pixel-plugin.toml"), "{err}");
    }

    #[test]
    fn path_lookup_needs_an_executable_file_and_skips_empty_entries() {
        let f = fixture();
        path_exe(&f.host, "plain", 0o644);
        assert_eq!(find_on_path("plain", &f.host.path), None, "not executable");
        let exe = path_exe(&f.host, "tool", 0o755);
        assert_eq!(find_on_path("tool", &f.host.path), Some(exe));
        // A PATH of "" means "the working directory" to a shell: never here.
        let cwd_exe = std::env::current_dir().unwrap().join("pixel-cwdprobe");
        assert!(!cwd_exe.exists());
        assert_eq!(find_on_path("cwdprobe", OsStr::new("")), None);
        assert_eq!(find_on_path("cwdprobe", OsStr::new(":")), None);
        // A directory named pixel-<name> is not a plugin.
        fs::create_dir(PathBuf::from(&f.host.path).join("pixel-dir")).unwrap();
        assert_eq!(find_on_path("dir", &f.host.path), None);
    }

    #[test]
    fn path_lookup_takes_the_first_directory_that_has_it() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [&a, &b] {
            let file = dir.path().join("pixel-x");
            fs::write(&file, "").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(find_on_path("x", &path), Some(a.path().join("pixel-x")));
    }

    #[test]
    fn list_reports_every_tier_with_its_flags() {
        let f = fixture();
        assert_eq!(list(&f.host).unwrap(), []);
        assert_eq!(render(&[]), "no plugins\n");

        plugin_dir(&f.host.repo_plugins().unwrap(), "r", "");
        plugin_dir(&f.host.user_plugins(), "u", "network = true");
        path_exe(&f.host, "p", 0o755);
        path_exe(&f.host, "notexec", 0o644);
        let rows = list(&f.host).unwrap();
        let summary: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.place,
                    r.enabled,
                    r.trusted,
                    r.version.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("r", Place::Repo, None, Some(false), "0.1.0"),
                ("u", Place::User, Some(false), None, "0.1.0"),
                ("p", Place::Path, None, None, "-"),
            ]
        );
        assert_eq!(rows[0].api, "1");
        assert_eq!(rows[0].capabilities, [Capability::Command]);

        trust(&f.host, "r");
        Config::at(f.host.config_file()).enable("u").unwrap();
        let rows = list(&f.host).unwrap();
        assert_eq!((rows[0].trusted, rows[1].enabled), (Some(true), Some(true)));
    }

    #[test]
    fn list_shows_a_shadowed_plugin_and_an_invalid_manifest() {
        let f = fixture();
        plugin_dir(&f.host.repo_plugins().unwrap(), "dup", "");
        plugin_dir(&f.host.user_plugins(), "dup", "");
        let bad = plugin_dir(&f.host.user_plugins(), "bad", "");
        fs::write(bad.join("pixel-plugin.toml"), "api = 7").unwrap();
        let rows = list(&f.host).unwrap();
        let places: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.place)).collect();
        assert_eq!(
            places,
            [
                ("dup", Place::Repo),
                ("bad", Place::User),
                ("dup", Place::User)
            ]
        );
        let bad_row = &rows[1];
        assert!(bad_row.problem.as_deref().unwrap().contains("`name`"));
        assert_eq!(bad_row.capabilities, []);
    }

    #[test]
    fn render_aligns_columns_and_flags_problems() {
        let rows = vec![
            Row {
                name: "flow".into(),
                place: Place::Repo,
                version: "1.0.0".into(),
                api: "1".into(),
                capabilities: vec![Capability::Command],
                enabled: Some(true),
                trusted: Some(false),
                problem: None,
            },
            Row {
                name: "x".into(),
                place: Place::User,
                version: "-".into(),
                api: "-".into(),
                capabilities: vec![],
                enabled: None,
                trusted: None,
                problem: Some("broken".into()),
            },
        ];
        let text = render(&rows);
        assert_eq!(
            text,
            "NAME  PLACE  VERSION  API  CAPABILITIES  ENABLED  TRUSTED\n\
             flow  repo   1.0.0    1    command       yes      no\n\
             x     user   -        -    -             -        -\n  ! x: broken\n"
        );
    }
}
