// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Managing plugins: add, remove, enable, trust.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pixel_git::{GitOptions, GitRunner};

use crate::Error;
use crate::discover::{Found, Host, contained_program, find_in};
use crate::hash::dir_digest;
use crate::manifest::{self, MANIFEST_FILE, Manifest};
use crate::state::{Config, TrustStore};

/// A shallow clone of a plugin repository may take a while on a slow link.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
/// How deep below a cloned repository root a manifest is looked for.
const SEARCH_DEPTH: usize = 3;
/// Directories never searched for a manifest or copied into a plugin.
const SKIPPED_DIRS: [&str; 3] = [".git", "target", "node_modules"];
/// Git URL schemes `add` accepts. `ext::` and `fd::` run commands, and a
/// bare path is handled as a directory before this list is consulted.
const URL_PREFIXES: [&str; 4] = ["https://", "ssh://", "file://", "git@"];

/// What `add` installed.
#[derive(Debug, PartialEq, Eq)]
pub struct Added {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

pub fn looks_like_git_url(source: &str) -> bool {
    URL_PREFIXES.iter().any(|prefix| source.starts_with(prefix))
}

/// `source` with the userinfo of a URL (`https://user:token@host/…`) removed:
/// what is recorded in `plugins.toml` and shown in messages.
pub fn redact_source(source: &str) -> String {
    let Some((scheme, rest)) = source.split_once("://") else {
        return source.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => source.to_owned(),
    }
}

/// Install a plugin into `~/.pixel/plugins/<name>/` from a local directory
/// or a git URL. `name` picks one plugin out of a repository that holds
/// several; without it the source must hold exactly one manifest.
pub fn add(host: &Host, source: &str, name: Option<&str>) -> Result<Added, Error> {
    if let Some(name) = name
        && !manifest::valid_name(name)
    {
        return Err(Error::InvalidName(name.to_owned()));
    }
    let shown = redact_source(source);
    // Held until the copy is done: a clone is removed when it drops.
    let clone;
    let base: PathBuf = if Path::new(source).is_dir() {
        PathBuf::from(source)
    } else if looks_like_git_url(source) {
        clone = tempfile::tempdir().map_err(|e| Error::io("create a clone directory", e))?;
        let checkout = clone.path().join("clone");
        let runner = GitRunner::with_options(
            clone.path(),
            GitOptions {
                timeout: Some(CLONE_TIMEOUT),
                max_output_bytes: Some(pixel_git::DEFAULT_MAX_OUTPUT_BYTES),
            },
        );
        runner
            .run(&["clone", "--depth", "1", "--", source, "clone"])
            .map_err(|e| {
                // Git echoes the URL it was given, userinfo included.
                let reason = e.to_string().replace(source, &shown);
                Error::Source(format!("cannot clone {shown}: {reason}"))
            })?;
        checkout
    } else {
        return Err(Error::Source(format!(
            "{shown} is neither a directory nor a git URL (https://, ssh://, file:// or git@host:path)"
        )));
    };
    let (from, manifest) = locate(&base, name, &shown)?;
    contained_program(&from, &manifest)?;

    let plugins = host.user_plugins();
    let dest = plugins.join(&manifest.name);
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(Error::AlreadyInstalled(manifest.name));
    }
    std::fs::create_dir_all(&plugins)
        .map_err(|e| Error::io(format!("create {}", plugins.display()), e))?;
    let staging = tempfile::Builder::new()
        .prefix(".adding-")
        .tempdir_in(&plugins)
        .map_err(|e| Error::io(format!("stage in {}", plugins.display()), e))?;
    let staged = staging.path().join(&manifest.name);
    copy_tree(&from, &staged)?;
    std::fs::rename(&staged, &dest)
        .map_err(|e| Error::io(format!("install {}", dest.display()), e))?;
    Config::at(host.config_file()).record_source(&manifest.name, &shown)?;
    Ok(Added {
        name: manifest.name.clone(),
        dir: dest,
        manifest,
    })
}

/// The directory under `base` that holds the plugin to install, and its
/// manifest: `base` itself, else the one manifest below it that matches.
fn locate(base: &Path, name: Option<&str>, shown: &str) -> Result<(PathBuf, Manifest), Error> {
    let mut found: Vec<(PathBuf, Manifest)> = Vec::new();
    let root = manifest::load(base, None)?;
    if let Some(m) = root
        && name.is_none_or(|n| n == m.name)
    {
        return Ok((base.to_path_buf(), m));
    }
    search(base, 0, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut matching = found
        .into_iter()
        .filter(|(_, m)| name.is_none_or(|n| n == m.name));
    match (matching.next(), matching.next()) {
        (Some(only), None) => Ok(only),
        (None, _) => Err(Error::Source(match name {
            Some(n) => {
                format!("{shown} holds no plugin named `{n}` (no {MANIFEST_FILE} with that name)")
            }
            None => format!("{shown} holds no {MANIFEST_FILE}"),
        })),
        (Some(first), Some(second)) => {
            let mut names = vec![first.1.name, second.1.name];
            names.extend(matching.map(|(_, m)| m.name));
            Err(Error::Source(format!(
                "{shown} holds several plugins ({}): choose one with --name",
                names.join(", ")
            )))
        }
    }
}

fn search(dir: &Path, depth: usize, found: &mut Vec<(PathBuf, Manifest)>) {
    if depth >= SEARCH_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let skipped = entry
            .file_name()
            .to_str()
            .is_none_or(|n| SKIPPED_DIRS.contains(&n));
        if skipped || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if let Ok(Some(m)) = manifest::load(&path, None) {
            found.push((path.clone(), m));
        }
        search(&path, depth + 1, found);
    }
}

/// Copy `from` to `to`, symlinks kept as symlinks (their target is judged
/// when `run` is resolved), `.git` left behind.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(to).map_err(|e| Error::io(format!("create {}", to.display()), e))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| Error::io(format!("read {}", from.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("read {}", from.display()), e))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry
            .file_type()
            .map_err(|e| Error::io(format!("stat {}", src.display()), e))?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&src)
                .map_err(|e| Error::io(format!("read link {}", src.display()), e))?;
            symlink(&target, &dst).map_err(|e| Error::io(format!("link {}", dst.display()), e))?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .map_err(|e| Error::io(format!("copy {}", src.display()), e))?;
        }
    }
    Ok(())
}

/// Delete `~/.pixel/plugins/<name>/` and forget its source and opt-in.
pub fn remove(host: &Host, name: &str) -> Result<(), Error> {
    if !manifest::valid_name(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    let dir = host.user_plugins().join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(Error::State(format!(
                "{} is not a plugin directory; remove it by hand",
                dir.display()
            )));
        }
        Err(_) => return Err(Error::NotFound(name.to_owned())),
    }
    std::fs::remove_dir_all(&dir).map_err(|e| Error::io(format!("remove {}", dir.display()), e))?;
    Config::at(host.config_file()).forget(name)
}

/// What `enable` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Enabled {
    /// The plugin is now on the opt-in list.
    Yes,
    /// The plugin declares no network access: nothing to opt into.
    NotNeeded,
}

/// Allow the network plugin `name`. Looks in the repo, then the user tier.
pub fn enable(host: &Host, name: &str) -> Result<Enabled, Error> {
    let found = locate_installed(host, name)?;
    if !found.manifest.network {
        return Ok(Enabled::NotNeeded);
    }
    Config::at(host.config_file()).enable(name)?;
    Ok(Enabled::Yes)
}

/// Record the digest of the repo plugin `name` as it is on disk now.
/// Returns that digest.
pub fn trust(host: &Host, name: &str) -> Result<String, Error> {
    if !manifest::valid_name(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    let Some(plugins) = host.repo_plugins() else {
        return Err(Error::State(
            "not inside a repository: only repo plugins (`.pixel/plugins/<name>/`) need trust"
                .to_owned(),
        ));
    };
    let Some(Found { dir, manifest }) = find_in(&plugins, name)? else {
        return Err(Error::State(format!(
            "no repo plugin `{name}` under {}",
            plugins.display()
        )));
    };
    contained_program(&dir, &manifest)?;
    let digest = dir_digest(&dir).map_err(|e| Error::io(format!("hash {}", dir.display()), e))?;
    TrustStore::at(host.trust_file()).trust(name, &digest)?;
    Ok(digest)
}

fn locate_installed(host: &Host, name: &str) -> Result<Found, Error> {
    if !manifest::valid_name(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    if let Some(plugins) = host.repo_plugins()
        && let Some(found) = find_in(&plugins, name)?
    {
        return Ok(found);
    }
    find_in(&host.user_plugins(), name)?.ok_or_else(|| Error::NotFound(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::{Place, resolve};
    use crate::testutil::{Fixture, fixture, plugin_dir};
    use std::fs;

    fn source_dir(f: &Fixture, name: &str, extra: &str) -> PathBuf {
        plugin_dir(&f.tmp.path().join("src"), name, extra)
    }

    #[test]
    fn url_schemes_are_a_whitelist() {
        for ok in [
            "https://github.com/Pixel-CLI/pixel-plugins",
            "ssh://git@host/x.git",
            "file:///tmp/x",
            "git@github.com:a/b.git",
        ] {
            assert!(looks_like_git_url(ok), "{ok}");
        }
        for bad in [
            "ext::sh -c id",
            "fd::3",
            "http://insecure.example/x",
            "-oProxyCommand=x",
            "/tmp/dir",
            "",
        ] {
            assert!(!looks_like_git_url(bad), "{bad}");
        }
    }

    #[test]
    fn userinfo_never_reaches_a_record() {
        assert_eq!(
            redact_source("https://user:ghp_token@github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
        assert_eq!(
            redact_source("https://github.com/a/b"),
            "https://github.com/a/b"
        );
        assert_eq!(
            redact_source("https://github.com/a/b@v1"),
            "https://github.com/a/b@v1",
            "an @ in the path is not userinfo"
        );
        assert_eq!(
            redact_source("git@github.com:a/b.git"),
            "git@github.com:a/b.git"
        );
        assert_eq!(redact_source("/tmp/x"), "/tmp/x");
    }

    #[test]
    fn add_copies_a_local_plugin_and_records_its_source() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        fs::create_dir_all(src.join(".git")).unwrap();
        fs::write(src.join(".git/HEAD"), "ref").unwrap();
        fs::create_dir_all(src.join("lib")).unwrap();
        fs::write(src.join("lib/data.txt"), "data").unwrap();

        let added = add(&f.host, src.to_str().unwrap(), None).unwrap();
        assert_eq!(added.name, "tool");
        assert_eq!(added.dir, f.host.user_plugins().join("tool"));
        assert_eq!(
            fs::read_to_string(added.dir.join("lib/data.txt")).unwrap(),
            "data"
        );
        assert!(!added.dir.join(".git").exists(), "history is not copied");
        assert!(added.dir.join("run.sh").is_file());
        assert_eq!(
            Config::at(f.host.config_file())
                .source("tool")
                .unwrap()
                .as_deref(),
            src.to_str()
        );
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().place,
            Place::User
        );
    }

    #[test]
    fn add_picks_the_named_plugin_out_of_a_repository() {
        let f = fixture();
        let repo = f.tmp.path().join("many");
        plugin_dir(&repo.join("crates"), "one", "");
        plugin_dir(&repo.join("crates"), "two", "");
        fs::create_dir_all(repo.join("target/deep")).unwrap();
        plugin_dir(&repo.join("target"), "three", "");

        let added = add(&f.host, repo.to_str().unwrap(), Some("two")).unwrap();
        assert_eq!(added.name, "two");
        assert!(f.host.user_plugins().join("two/run.sh").is_file());
        assert!(!f.host.user_plugins().join("one").exists());

        let err = add(&f.host, repo.to_str().unwrap(), Some("three"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no plugin named `three`"),
            "target/ is not searched: {err}"
        );
    }

    #[test]
    fn add_without_a_name_needs_exactly_one_plugin() {
        let f = fixture();
        let repo = f.tmp.path().join("many");
        plugin_dir(&repo.join("a"), "one", "");
        plugin_dir(&repo.join("b"), "two", "");
        let err = add(&f.host, repo.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("one, two") && err.contains("--name"), "{err}");

        let empty = f.tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let err = add(&f.host, empty.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("holds no pixel-plugin.toml"), "{err}");

        let only = f.tmp.path().join("only");
        plugin_dir(&only.join("nested"), "solo", "");
        assert_eq!(
            add(&f.host, only.to_str().unwrap(), None).unwrap().name,
            "solo"
        );
    }

    #[test]
    fn a_root_manifest_wins_unless_another_name_was_asked_for() {
        let f = fixture();
        let root = source_dir(&f, "rootp", "");
        plugin_dir(&root.join("sub"), "inner", "");
        let added = add(&f.host, root.to_str().unwrap(), Some("inner")).unwrap();
        assert_eq!(added.name, "inner");
        let added = add(&f.host, root.to_str().unwrap(), Some("rootp")).unwrap();
        assert_eq!(added.name, "rootp");
    }

    #[test]
    fn add_refuses_a_source_that_is_neither_directory_nor_url() {
        let f = fixture();
        for bad in ["/no/such/dir", "ext::sh -c id", "http://x.example/p"] {
            let err = add(&f.host, bad, None).unwrap_err().to_string();
            assert!(
                err.contains("neither a directory nor a git URL"),
                "{bad}: {err}"
            );
        }
        assert!(
            !f.host.user_plugins().exists(),
            "nothing created on refusal"
        );
    }

    #[test]
    fn add_refuses_to_overwrite_and_leaves_the_installed_one_alone() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        add(&f.host, src.to_str().unwrap(), None).unwrap();
        fs::write(f.host.user_plugins().join("tool/marker"), "mine").unwrap();
        let err = add(&f.host, src.to_str().unwrap(), None).unwrap_err();
        assert!(matches!(err, Error::AlreadyInstalled(_)), "{err}");
        assert!(
            err.to_string().contains("`pixel plugin remove tool`"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(f.host.user_plugins().join("tool/marker")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn add_refuses_a_plugin_whose_run_is_missing_or_escapes() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        fs::remove_file(src.join("run.sh")).unwrap();
        assert!(matches!(
            add(&f.host, src.to_str().unwrap(), None).unwrap_err(),
            Error::BadRun { .. }
        ));
        std::os::unix::fs::symlink("/bin/sh", src.join("run.sh")).unwrap();
        let err = add(&f.host, src.to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the plugin directory"), "{err}");
        assert!(!f.host.user_plugins().join("tool").exists());
    }

    #[test]
    fn add_leaves_no_staging_directory_behind() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        add(&f.host, src.to_str().unwrap(), None).unwrap();
        let names: Vec<_> = fs::read_dir(f.host.user_plugins())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["tool"]);
    }

    #[test]
    fn add_rejects_an_invalid_name_before_looking_at_the_source() {
        let f = fixture();
        let err = add(&f.host, "/no/such", Some("../x")).unwrap_err();
        assert!(matches!(err, Error::InvalidName(_)), "{err}");
    }

    #[test]
    fn add_clones_a_git_url_and_never_records_credentials() {
        let f = fixture();
        let origin = f.tmp.path().join("origin");
        plugin_dir(&origin.join("plugins"), "cloned", "");
        crate::testutil::git(&origin, &["init", "-q"]);
        crate::testutil::git(&origin, &["add", "."]);
        crate::testutil::git(&origin, &["commit", "-q", "-m", "seed"]);
        let url = format!("file://{}", origin.display());
        let added = add(&f.host, &url, Some("cloned")).unwrap();
        assert_eq!(added.name, "cloned");
        assert!(added.dir.join("run.sh").is_file());
        assert!(!added.dir.join(".git").exists());
        assert_eq!(
            Config::at(f.host.config_file()).source("cloned").unwrap(),
            Some(url)
        );
    }

    #[test]
    fn a_failed_clone_names_the_url_without_its_userinfo() {
        let f = fixture();
        let err = add(&f.host, "file://user:secret@/no/such/repo", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot clone"), "{err}");
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn remove_deletes_the_directory_and_forgets_it() {
        let f = fixture();
        let src = source_dir(&f, "net", "network = true");
        add(&f.host, src.to_str().unwrap(), None).unwrap();
        enable(&f.host, "net").unwrap();
        remove(&f.host, "net").unwrap();
        assert!(!f.host.user_plugins().join("net").exists());
        let config = Config::at(f.host.config_file());
        assert!(!config.is_enabled("net").unwrap());
        assert_eq!(config.source("net").unwrap(), None);
        assert!(matches!(
            remove(&f.host, "net").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn remove_never_follows_a_link_and_validates_the_name() {
        let f = fixture();
        let keep = tempfile::tempdir().unwrap();
        fs::write(keep.path().join("precious"), "x").unwrap();
        fs::create_dir_all(f.host.user_plugins()).unwrap();
        std::os::unix::fs::symlink(keep.path(), f.host.user_plugins().join("lnk")).unwrap();
        assert!(remove(&f.host, "lnk").is_err());
        assert!(keep.path().join("precious").exists());
        assert!(matches!(
            remove(&f.host, "../x").unwrap_err(),
            Error::InvalidName(_)
        ));
    }

    #[test]
    fn enable_applies_to_network_plugins_only() {
        let f = fixture();
        plugin_dir(&f.host.user_plugins(), "net", "network = true");
        plugin_dir(&f.host.user_plugins(), "calm", "");
        assert_eq!(enable(&f.host, "net").unwrap(), Enabled::Yes);
        assert_eq!(enable(&f.host, "calm").unwrap(), Enabled::NotNeeded);
        let config = Config::at(f.host.config_file());
        assert!(config.is_enabled("net").unwrap());
        assert!(!config.is_enabled("calm").unwrap());
        assert!(matches!(
            enable(&f.host, "ghost").unwrap_err(),
            Error::NotFound(_)
        ));
        assert!(matches!(
            enable(&f.host, "a/b").unwrap_err(),
            Error::InvalidName(_)
        ));
    }

    #[test]
    fn trust_records_the_current_digest_of_a_repo_plugin_only() {
        let f = fixture();
        let dir = plugin_dir(&f.host.repo_plugins().unwrap(), "tool", "");
        let digest = trust(&f.host, "tool").unwrap();
        assert_eq!(digest, dir_digest(&dir).unwrap());
        assert!(
            TrustStore::at(f.host.trust_file())
                .is_trusted("tool", &digest)
                .unwrap()
        );

        plugin_dir(&f.host.user_plugins(), "mine", "");
        let err = trust(&f.host, "mine").unwrap_err().to_string();
        assert!(err.contains("no repo plugin `mine`"), "{err}");
        assert!(matches!(
            trust(&f.host, "../x").unwrap_err(),
            Error::InvalidName(_)
        ));
    }

    #[test]
    fn trust_outside_a_repository_explains_itself() {
        let mut f = fixture();
        f.host.repo_root = None;
        let err = trust(&f.host, "tool").unwrap_err().to_string();
        assert!(err.contains("not inside a repository"), "{err}");
    }

    #[test]
    fn trust_refuses_a_plugin_whose_run_escapes() {
        let f = fixture();
        let dir = plugin_dir(&f.host.repo_plugins().unwrap(), "esc", "");
        fs::remove_file(dir.join("run.sh")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", dir.join("run.sh")).unwrap();
        assert!(matches!(
            trust(&f.host, "esc").unwrap_err(),
            Error::BadRun { .. }
        ));
        assert!(
            !TrustStore::at(f.host.trust_file())
                .is_trusted("esc", "x")
                .unwrap()
        );
    }
}
