// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Managing plugins: install (`add`), uninstall (`remove`), enable, trust.

use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pixel_git::{GitOptions, GitRunner};

use crate::Error;
use crate::directory::Directory;
use crate::discover::{Found, Host, contained_program, find_in};
use crate::fetch::{self, Downloaded, Unverified};
use crate::hash::dir_digest;
use crate::manifest::{self, MANIFEST_FILE, Manifest};
use crate::source::{GITHUB_PREFIX, GitHubSource, NPM_PREFIX, NPM_UNSUPPORTED};
use crate::state::{Config, TrustStore};
use rustix::fs::FileType;

/// A shallow clone of a plugin repository may take a while on a slow link.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
/// How deep below a cloned repository root a manifest is looked for.
const SEARCH_DEPTH: usize = 3;
/// Directories never searched for a manifest or copied into a plugin.
const SKIPPED_DIRS: [&str; 3] = [".git", "target", "node_modules"];
/// Git URL schemes `add` accepts. `ext::` and `fd::` run commands, and a
/// bare path is handled as a directory before this list is consulted.
const URL_PREFIXES: [&str; 4] = ["https://", "ssh://", "file://", "git@"];
/// The environment of a plugin clone. `GIT_ALLOW_PROTOCOL` holds git itself
/// to the transports of [`URL_PREFIXES`] (`git@host:path` is ssh) wherever a
/// URL reaches it, not only the one `add` checked: an `insteadOf` rewrite or
/// an HTTP redirect cannot move the clone to `ext::`, `git://` or `http://`.
/// `GIT_TERMINAL_PROMPT=0` turns off git's own terminal prompt, so a source
/// that asks for credentials (GitHub answers a missing repository that way)
/// fails at once instead of waiting on the terminal, unseen by an agent. It
/// is the only prompt turned off: a user's `GIT_ASKPASS` or `core.askPass`
/// program and ssh's own prompt are left alone and wait until
/// [`CLONE_TIMEOUT`]; `GIT_SSH_COMMAND` is not set to force `BatchMode`,
/// because it would shadow the user's `core.sshCommand`.
const CLONE_ENV: [(&str, &str); 2] = [
    ("GIT_ALLOW_PROTOCOL", "https:ssh:file"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// What `add` installed.
#[derive(Debug, PartialEq, Eq)]
pub struct Added {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub binary: Binary,
    /// The plugin's agent skill, read and validated before anything was
    /// written; the caller (`pixel install`) delivers it to the harness
    /// roots. `None` when the manifest declares no `skill`.
    pub skill: Option<String>,
}

/// The state of the program a plugin's `run` names, once installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binary {
    /// The source shipped it.
    Present,
    /// A `github:` source did not ship it; a release asset was downloaded.
    Downloaded(Downloaded),
    /// A `github:` source shipped it and no release asset replaced it: the
    /// plugin runs the repository's own file, which may only find a program
    /// built elsewhere (a launcher), so the build stays one command away.
    Shipped {
        /// The git URL `cargo install --git` builds from.
        git_url: String,
        /// The ref the plugin was cloned at, when one was asked for.
        reference: Option<String>,
        /// Why no release asset was used.
        download_error: String,
    },
    /// A `github:` source did not ship it and no release asset could be
    /// fetched: the plugin is installed and cannot run until it is built.
    Missing {
        /// The git URL `cargo install --git` builds from.
        git_url: String,
        /// The ref the plugin was cloned at, when one was asked for.
        reference: Option<String>,
        /// Why no release asset was used.
        download_error: String,
    },
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

/// Install a plugin into `~/.pixel/plugins/<name>/` from a local directory,
/// a git URL or `github:<owner>/<repo>[@<ref>]`. `name` picks one plugin out
/// of a repository that holds several; without it the source must hold
/// exactly one manifest.
///
/// A `github:` source gets the release asset `pixel-<name>-<target>` as the
/// program its manifest names, over the file the repository ships there (a
/// launcher that finds a separately built program, say). Without one, a
/// shipped program stays ([`Binary::Shipped`]) and an absent one leaves the
/// plugin installed but not runnable ([`Binary::Missing`]); both say how to
/// build it. A release asset with no published `.sha256` is not used: see
/// [`add_with`] to accept one.
pub fn add(host: &Host, source: &str, name: Option<&str>) -> Result<Added, Error> {
    add_with(host, source, name, Unverified::Refuse)
}

/// [`add`], with `unverified` deciding whether a release asset whose release
/// publishes no `.sha256` may still become the plugin's program.
pub fn add_with(
    host: &Host,
    source: &str,
    name: Option<&str>,
    unverified: Unverified,
) -> Result<Added, Error> {
    if let Some(name) = name
        && !manifest::valid_name(name)
    {
        return Err(Error::InvalidName(name.to_owned()));
    }
    if source.starts_with(NPM_PREFIX) {
        return Err(Error::Unsupported(NPM_UNSUPPORTED.to_owned()));
    }
    let github = source
        .strip_prefix(GITHUB_PREFIX)
        .map(GitHubSource::parse)
        .transpose()?;
    let shown = github
        .as_ref()
        .map_or_else(|| redact_source(source), GitHubSource::shown);
    // Held until the copy is done: a clone is removed when it drops.
    let clone;
    let base: PathBuf = if let Some(github) = &github {
        let url = github.clone_url(host.github_base());
        if !looks_like_git_url(&url) {
            return Err(Error::Source(format!(
                "the GitHub base URL {} must start with https://, ssh://, file:// or git@",
                redact_source(host.github_base())
            )));
        }
        clone = clone_repo(&url, github.reference.as_deref(), &shown)?;
        clone.path().join("clone")
    } else if Path::new(source).is_dir() {
        PathBuf::from(source)
    } else if looks_like_git_url(source) {
        clone = clone_repo(source, None, &shown)?;
        clone.path().join("clone")
    } else {
        return Err(Error::Source(format!(
            "{shown} is neither a directory nor a git URL (https://, ssh://, file:// or git@host:path)"
        )));
    };
    let (from, manifest) = locate(&base, name, &shown)?;
    // Read the declared skill before anything is created: a declaration that
    // fails validation installs nothing anywhere, harness roots included.
    let skill = manifest::read_skill(&from, &manifest)?;
    let unbuilt = github.is_some() && program_is_absent(&from, &manifest);
    if !unbuilt {
        contained_program(&from, &manifest)?;
    }

    let plugins = host.user_plugins();
    let dest = plugins.join(&manifest.name);
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(Error::AlreadyInstalled(manifest.name));
    }
    ensure_no_shared_command(&plugins, &manifest)?;
    std::fs::create_dir_all(&plugins)
        .map_err(|e| Error::io(format!("create {}", plugins.display()), e))?;
    let staging = tempfile::Builder::new()
        .prefix(".adding-")
        .tempdir_in(&plugins)
        .map_err(|e| Error::io(format!("stage in {}", plugins.display()), e))?;
    let staged = staging.path().join(&manifest.name);
    copy_tree(&from, &staged)?;
    let binary = match &github {
        Some(github) => fetch_program(host, github, &staged, &manifest, unverified),
        None => Binary::Present,
    };
    std::fs::rename(&staged, &dest)
        .map_err(|e| Error::io(format!("install {}", dest.display()), e))?;
    Config::at(host.config_file()).record_source(&manifest.name, &shown)?;
    Ok(Added {
        name: manifest.name.clone(),
        dir: dest,
        manifest,
        binary,
        skill,
    })
}

/// Refuse a plugin that answers to a command an installed one already does:
/// `pixel <command>` would stop running either.
fn ensure_no_shared_command(plugins: &Path, manifest: &Manifest) -> Result<(), Error> {
    let Ok(entries) = std::fs::read_dir(plugins) else {
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| manifest::valid_name(n))
        .collect();
    names.sort();
    for name in names {
        let Ok(Some(installed)) = find_in(plugins, &name) else {
            continue;
        };
        let incoming = manifest.serves();
        if let Some(command) = installed
            .manifest
            .serves()
            .into_iter()
            .find(|c| incoming.contains(c))
        {
            return Err(Error::Conflict {
                command: command.to_owned(),
                first: installed.manifest.name,
                second: manifest.name.clone(),
                dir: plugins.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Shallow-clone `url` (at `reference`, a branch or tag) into `<tmp>/clone`,
/// over the transports [`CLONE_ENV`] allows and without git's terminal prompt.
/// The directory is removed when the returned handle drops.
fn clone_repo(url: &str, reference: Option<&str>, shown: &str) -> Result<tempfile::TempDir, Error> {
    let clone = tempfile::tempdir().map_err(|e| Error::io("create a clone directory", e))?;
    let runner = GitRunner::with_options(
        clone.path(),
        GitOptions {
            timeout: Some(CLONE_TIMEOUT),
            max_output_bytes: Some(pixel_git::DEFAULT_MAX_OUTPUT_BYTES),
        },
    );
    let mut args = vec!["clone", "--depth", "1"];
    if let Some(reference) = reference {
        args.extend(["--branch", reference]);
    }
    args.extend(["--", url, "clone"]);
    let failed = |e: pixel_git::GitError| {
        // Git echoes the URL it was given, userinfo included.
        let reason = e.to_string().replace(url, shown);
        Error::Source(format!("cannot clone {shown}: {reason}"))
    };
    let output = runner.run_output(&args, &CLONE_ENV).map_err(failed)?;
    if !output.success() {
        return Err(failed(pixel_git::GitError::NonZeroExit {
            args: args.iter().map(ToString::to_string).collect(),
            code: output.code,
            stderr: output.stderr,
        }));
    }
    Ok(clone)
}

/// Whether the file `run` names is absent from `dir` (not merely unusable:
/// a dangling or escaping link is a defect to report, not a build to do).
fn program_is_absent(dir: &Path, manifest: &Manifest) -> bool {
    std::fs::symlink_metadata(dir.join(&manifest.run))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Put the release asset of `manifest` at `<staged>/<run>`, or say why not
/// and what the plugin runs instead.
fn fetch_program(
    host: &Host,
    github: &GitHubSource,
    staged: &Path,
    manifest: &Manifest,
    unverified: Unverified,
) -> Binary {
    let download_error = match fetch_asset(host, github, staged, manifest, unverified) {
        Ok(downloaded) => return Binary::Downloaded(downloaded),
        Err(reason) => reason,
    };
    let git_url = redact_source(&github.clone_url(host.github_base()));
    let reference = github.reference.clone();
    if program_is_absent(staged, manifest) {
        Binary::Missing {
            git_url,
            reference,
            download_error,
        }
    } else {
        Binary::Shipped {
            git_url,
            reference,
            download_error,
        }
    }
}

fn fetch_asset(
    host: &Host,
    github: &GitHubSource,
    staged: &Path,
    manifest: &Manifest,
    unverified: Unverified,
) -> Result<Downloaded, String> {
    let target = fetch::release_target()
        .ok_or_else(|| "no release asset is published for this platform".to_owned())?;
    let asset = fetch::asset_name(&manifest.name, &target);
    let url = format!("{}/{asset}", github.release_url(host.github_base()));
    let parent = fetch::program_parent(staged, &manifest.run)?;
    let file_name = Path::new(&manifest.run)
        .file_name()
        .ok_or_else(|| format!("`run` = \"{}\" names no file", manifest.run))?;
    let dest = parent.join(file_name);
    let allow_file = host.github_base().starts_with("file://");
    let downloaded = fetch::download_program(&url, &dest, allow_file, unverified)?;
    contained_program(staged, manifest).map_err(|e| {
        let _ = std::fs::remove_file(&dest);
        e.to_string()
    })?;
    Ok(downloaded)
}

/// `cargo install --git <url> [--rev <ref>] --root <root>`: builds the
/// plugin's program into `<root>/bin/`, quoted for a POSIX shell.
pub fn cargo_install_command(git_url: &str, reference: Option<&str>, root: &Path) -> String {
    let mut command = format!("cargo install --git {}", shell_quote(git_url));
    if let Some(reference) = reference {
        command.push_str(&format!(" --rev {}", shell_quote(reference)));
    }
    command.push_str(&format!(" --root {}", shell_quote(&root.to_string_lossy())));
    command
}

/// `text` as one POSIX shell word, single-quoted unless it is plainly safe.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:@".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
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

/// Copy `from` to `to`, symlinks kept as symlinks and never followed (their
/// target is judged when `run` is resolved), `.git` left behind. A FIFO, a
/// socket or a device is refused: copying one would wait on it forever or
/// read a device into the plugin.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    copy_tree_with(from, to, &mut |_| {})
}

fn copy_tree_with(from: &Path, to: &Path, before: &mut impl FnMut(&Path)) -> Result<(), Error> {
    let source =
        Directory::open(from).map_err(|e| Error::io(format!("open {}", from.display()), e))?;
    copy_directory(&source, from, to, before)
}

fn copy_directory(
    source: &Directory,
    from: &Path,
    to: &Path,
    before: &mut impl FnMut(&Path),
) -> Result<(), Error> {
    std::fs::create_dir_all(to).map_err(|e| Error::io(format!("create {}", to.display()), e))?;
    let entries = source
        .entries()
        .map_err(|e| Error::io(format!("read {}", from.display()), e))?;
    for entry in entries {
        if entry.name == ".git" {
            continue;
        }
        let (src, dst) = (from.join(&entry.name), to.join(&entry.name));
        before(&src);
        match entry.kind() {
            FileType::Symlink => {
                let target = source
                    .read_link(&entry)
                    .map_err(|e| Error::io(format!("read link {}", src.display()), e))?;
                symlink(&target, &dst)
                    .map_err(|e| Error::io(format!("link {}", dst.display()), e))?;
            }
            FileType::Directory => {
                let child = source
                    .descend(&entry)
                    .map_err(|e| Error::io(format!("open {}", src.display()), e))?;
                copy_directory(&child, &src, &dst, before)?;
            }
            FileType::RegularFile => {
                let file = source
                    .open_file(&entry)
                    .map_err(|e| Error::io(format!("open {}", src.display()), e))?;
                copy_file(file, entry.len(), &dst)
                    .map_err(|e| Error::io(format!("copy {}", src.display()), e))?;
            }
            _ => {
                return Err(Error::Source(format!(
                    "{} is not a file, a directory or a link; a plugin cannot hold it",
                    src.display()
                )));
            }
        }
    }
    Ok(())
}

/// Copy an already verified regular file, with its permissions and at most
/// the size listed; a growing source is refused rather than copied forever.
fn copy_file(source: std::fs::File, listed_len: u64, to: &Path) -> std::io::Result<()> {
    let permissions = source.metadata()?.permissions();
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let copied = std::io::copy(&mut source.take(listed_len + 1), &mut target)?;
    if copied > listed_len {
        return Err(std::io::Error::other("it grew while it was copied"));
    }
    target.set_permissions(permissions)
}

/// The agent skill `pixel install` delivered for the user-tier plugin
/// `name`: the text of the file its manifest declares. Only a match with it
/// proves a harness copy is pixel's, so the caller sweeps only those; a
/// plugin whose manifest or declared file cannot be read reads as
/// `Ok(None)` — nothing to trust, nothing to remove.
///
/// # Errors
///
/// [`Error::InvalidName`].
pub fn installed_skill(host: &Host, name: &str) -> Result<Option<String>, Error> {
    if !manifest::valid_name(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    let dir = host.user_plugins().join(name);
    match manifest::load(&dir, Some(name)) {
        Ok(Some(manifest)) => Ok(manifest::read_skill(&dir, &manifest).ok().flatten()),
        _ => Ok(None),
    }
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

/// Allow the network plugin `name`, or switch the brief probe `name` back on.
/// Looks in the repo, then the user tier.
pub fn enable(host: &Host, name: &str) -> Result<Enabled, Error> {
    let found = locate_installed(host, name)?;
    if found.manifest.is_probe() {
        Config::at(host.config_file()).undisable(name)?;
        return Ok(Enabled::Yes);
    }
    if !found.manifest.network {
        return Ok(Enabled::NotNeeded);
    }
    Config::at(host.config_file()).enable(name)?;
    Ok(Enabled::Yes)
}

/// Switch the brief probe `name` off: the brief stops running it, the files
/// stay. Only a probe can be switched off; a command plugin is removed with
/// `pixel uninstall`.
pub fn disable(host: &Host, name: &str) -> Result<(), Error> {
    let found = locate_installed(host, name)?;
    if !found.manifest.is_probe() {
        return Err(Error::State(format!(
            "`{name}` is not a brief probe: only probes can be disabled (remove a command plugin with `pixel uninstall {name}`)"
        )));
    }
    Config::at(host.config_file()).disable(name)
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
    use crate::hash::hex;
    use crate::testutil::{Fixture, fixture, manifest_text, plugin_dir};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

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

    // `std::fs::copy` opens a FIFO for reading and waits for a writer that
    // never comes: `pixel install ./dir` hung. Refused, it names the entry
    // and installs nothing.
    #[test]
    fn add_refuses_a_fifo_in_the_source_instead_of_waiting_on_it() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        let status = std::process::Command::new("mkfifo")
            .arg(src.join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());
        let (host, source) = (f.host.clone(), src.to_str().unwrap().to_owned());
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(add(&host, &source, None).map(|added| added.name));
        });
        let err = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("add waited on the FIFO")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pipe is not a file, a directory or a link"),
            "{err}"
        );
        assert_eq!(
            fs::read_dir(f.host.user_plugins()).unwrap().count(),
            0,
            "neither the plugin nor its staging directory is left"
        );
    }

    // A link in a plugin is part of what the plugin is (its text is in the
    // trust digest); following it would copy a file from anywhere on the
    // machine into ~/.pixel/plugins.
    #[test]
    fn add_copies_a_link_as_a_link_and_never_what_it_points_at() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        let outside = f.tmp.path().join("outside.txt");
        fs::write(&outside, "not the plugin's").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("data")).unwrap();
        let added = add(&f.host, src.to_str().unwrap(), None).unwrap();
        let copied = added.dir.join("data");
        assert!(fs::symlink_metadata(&copied).unwrap().is_symlink());
        assert_eq!(fs::read_link(&copied).unwrap(), outside);
    }

    #[test]
    fn copy_should_refuse_a_directory_replaced_after_listing() {
        for linked in [true, false] {
            let f = fixture();
            let src = source_dir(&f, "tool", "");
            fs::create_dir(src.join("bin")).unwrap();
            fs::write(src.join("bin/data"), "original").unwrap();
            let outside = f.tmp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("foreign"), "outside secret").unwrap();
            let dst = f.tmp.path().join("copied");
            let mut swapped = false;
            let result = copy_tree_with(&src, &dst, &mut |path| {
                if path == src.join("bin") && !swapped {
                    swapped = true;
                    fs::rename(path, src.join("held")).unwrap();
                    if linked {
                        symlink(&outside, path).unwrap();
                    } else {
                        fs::create_dir(path).unwrap();
                        fs::write(path.join("foreign"), "replacement").unwrap();
                    }
                }
            });
            assert!(swapped);
            assert!(
                result.is_err(),
                "accepted a replaced directory (link={linked})"
            );
            assert!(!dst.join("bin/foreign").exists());
        }
    }

    #[test]
    fn copy_should_keep_reading_a_directory_pinned_before_its_ancestor_is_relinked() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        fs::create_dir(src.join("bin")).unwrap();
        fs::write(src.join("bin/data"), "original").unwrap();
        let outside = f.tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("data"), "outside secret").unwrap();
        let dst = f.tmp.path().join("copied");
        let mut swapped = false;
        copy_tree_with(&src, &dst, &mut |path| {
            if path == src.join("bin/data") && !swapped {
                swapped = true;
                fs::rename(src.join("bin"), src.join("held")).unwrap();
                symlink(&outside, src.join("bin")).unwrap();
            }
        })
        .unwrap();
        assert!(swapped);
        assert_eq!(fs::read(dst.join("bin/data")).unwrap(), b"original");
    }

    #[test]
    fn a_file_copies_with_its_mode_and_never_past_the_size_listed() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        fs::write(&from, "ab").unwrap();
        fs::set_permissions(&from, fs::Permissions::from_mode(0o750)).unwrap();
        let seen = fs::symlink_metadata(&from).unwrap();
        copy_file(
            crate::file::open_regular(&from, Some(&seen)).unwrap(),
            seen.len(),
            &dir.path().join("same"),
        )
        .unwrap();
        assert_eq!(fs::read(dir.path().join("same")).unwrap(), b"ab");
        let mode = fs::metadata(dir.path().join("same"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750);

        // Appended after it was listed: the same inode, more bytes.
        fs::OpenOptions::new()
            .append(true)
            .open(&from)
            .unwrap()
            .write_all(b"cd")
            .unwrap();
        let err = copy_file(
            crate::file::open_regular(&from, Some(&seen)).unwrap(),
            seen.len(),
            &dir.path().join("grown"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("grew while it was copied"),
            "{err}"
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
        assert!(err.to_string().contains("`pixel uninstall tool`"), "{err}");
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

    // `looks_like_git_url` is the first gate; git's own allow-list is the
    // one that holds when a URL reaches the clone some other way (a user's
    // `insteadOf` rewrite, a redirect). Called directly, a `git://` or
    // `http://` URL, which anyone on the path can tamper with, is refused by
    // git before it connects: without the list git dials 127.0.0.1:1 and
    // fails on the connection instead.
    #[test]
    fn a_clone_is_held_to_authenticated_or_local_transports_by_git_itself() {
        for url in ["git://127.0.0.1:1/p.git", "http://127.0.0.1:1/p.git"] {
            let err = clone_repo(url, None, url).unwrap_err().to_string();
            let scheme = url.split_once(':').unwrap().0;
            assert!(
                err.contains(&format!("transport '{scheme}' not allowed")),
                "{url}: {err}"
            );
        }
        assert_eq!(
            CLONE_ENV,
            [
                ("GIT_ALLOW_PROTOCOL", "https:ssh:file"),
                ("GIT_TERMINAL_PROMPT", "0"),
            ],
            "every scheme add accepts, and no terminal prompt"
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

    // ---- github: sources -------------------------------------------------

    /// A `github:`-shaped directory tree under the fixture: `<base>/<owner>/<repo>.git`
    /// is a bare clone of a work repository that `seed` filled.
    fn github_base(f: &mut Fixture, owner: &str, repo: &str, seed: impl FnOnce(&Path)) -> PathBuf {
        let work = f.tmp.path().join(format!("work-{repo}"));
        fs::create_dir_all(&work).unwrap();
        seed(&work);
        crate::testutil::git(&work, &["init", "-q"]);
        crate::testutil::git(&work, &["add", "."]);
        crate::testutil::git(&work, &["commit", "-q", "-m", "seed"]);
        crate::testutil::git(&work, &["tag", "v1"]);
        let base = f.tmp.path().join("gh");
        fs::create_dir_all(base.join(owner)).unwrap();
        let bare = base.join(owner).join(format!("{repo}.git"));
        crate::testutil::git(
            f.tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        f.host.github_base = Some(format!("file://{}", base.display()));
        base
    }

    /// A plugin whose manifest names `bin/tool`, which the repository does not ship.
    fn unbuilt(work: &Path) {
        fs::write(
            work.join("pixel-plugin.toml"),
            manifest_text("tool", "").replace("run.sh", "bin/tool"),
        )
        .unwrap();
    }

    /// The shape of the Pixel-CLI plugins: `run` = `bin/tool` is committed,
    /// a launcher that only finds a binary built elsewhere.
    const LAUNCHER: &str = "#!/bin/sh\necho 'no tool binary installed' >&2\nexit 1\n";

    fn launcher(work: &Path) {
        unbuilt(work);
        fs::create_dir_all(work.join("bin")).unwrap();
        fs::write(work.join("bin/tool"), LAUNCHER).unwrap();
        fs::set_permissions(work.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn publish_asset(base: &Path, tail: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let target = fetch::release_target().expect("a platform with release assets");
        let dir = base.join("acme/tool/releases").join(tail);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(fetch::asset_name(name, &target));
        fs::write(&file, bytes).unwrap();
        file
    }

    #[test]
    fn github_clones_the_default_branch_and_records_the_typed_source() {
        let mut f = fixture();
        github_base(&mut f, "acme", "tool", |work| {
            plugin_dir(work, "tool", "");
        });
        let added = add(&f.host, "github:acme/tool", None).unwrap();
        assert_eq!(added.name, "tool");
        assert!(
            matches!(added.binary, Binary::Shipped { .. }),
            "{:?}",
            added.binary
        );
        assert!(added.dir.join("run.sh").is_file());
        assert!(!added.dir.join(".git").exists());
        assert_eq!(
            Config::at(f.host.config_file())
                .source("tool")
                .unwrap()
                .as_deref(),
            Some("github:acme/tool")
        );
        assert_eq!(
            resolve(&f.host, "tool").unwrap().unwrap().place,
            Place::User
        );
    }

    #[test]
    fn github_at_a_ref_installs_that_tag_not_the_default_branch() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", |work| {
            plugin_dir(work, "tool", "");
        });
        // A later commit on the default branch that the tag does not hold.
        let work = f.tmp.path().join("work-tool");
        fs::write(work.join("tool/later.txt"), "later").unwrap();
        crate::testutil::git(&work, &["add", "."]);
        crate::testutil::git(&work, &["commit", "-q", "-m", "later"]);
        let bare = base.join("acme/tool.git");
        crate::testutil::git(
            &work,
            &[
                "push",
                "-q",
                bare.to_str().unwrap(),
                "HEAD:refs/heads/main2",
            ],
        );
        crate::testutil::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main2"]);

        let pinned = add(&f.host, "github:acme/tool@v1", None).unwrap();
        assert!(!pinned.dir.join("later.txt").exists());
        assert_eq!(
            Config::at(f.host.config_file())
                .source("tool")
                .unwrap()
                .as_deref(),
            Some("github:acme/tool@v1")
        );
        remove(&f.host, "tool").unwrap();
        let newest = add(&f.host, "github:acme/tool", None).unwrap();
        assert!(newest.dir.join("later.txt").is_file());
    }

    #[test]
    fn a_missing_program_is_fetched_from_the_latest_release_and_verified() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", unbuilt);
        let asset = publish_asset(&base, "latest/download", "tool", b"#!/bin/sh\necho built\n");
        let digest = hex(&Sha256::digest(fs::read(&asset).unwrap()));
        fs::write(
            format!("{}.sha256", asset.display()),
            format!("{digest}  asset\n"),
        )
        .unwrap();

        let added = add(&f.host, "github:acme/tool", None).unwrap();
        let Binary::Downloaded(got) = &added.binary else {
            panic!("expected a download: {:?}", added.binary);
        };
        assert!(got.verified);
        assert!(got.url.ends_with(&format!(
            "/acme/tool/releases/latest/download/{}",
            fetch::asset_name("tool", &fetch::release_target().unwrap())
        )));
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            "#!/bin/sh\necho built\n"
        );
        let resolved = resolve(&f.host, "tool").unwrap().unwrap();
        assert_eq!(
            resolved.program,
            added.dir.join("bin/tool").canonicalize().unwrap()
        );
    }

    #[test]
    fn add_should_leave_the_program_unbuilt_when_the_release_publishes_no_digest() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", unbuilt);
        publish_asset(&base, "download/v1", "tool", b"#!/bin/sh\n");
        let added = add(&f.host, "github:acme/tool@v1", None).unwrap();
        let Binary::Missing { download_error, .. } = &added.binary else {
            panic!("expected the build hint: {:?}", added.binary);
        };
        assert!(
            download_error.contains(".sha256 is not published")
                && download_error.ends_with("pass --allow-unverified to install it anyway"),
            "{download_error}"
        );
        assert!(
            !added.dir.join("bin/tool").exists(),
            "the unverifiable file is not installed"
        );
    }

    #[test]
    fn add_with_allow_should_install_a_pinned_release_asset_without_a_digest() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", unbuilt);
        publish_asset(&base, "download/v1", "tool", b"#!/bin/sh\n");
        publish_asset(&base, "latest/download", "tool", b"newest");
        let added = add_with(&f.host, "github:acme/tool@v1", None, Unverified::Allow).unwrap();
        let Binary::Downloaded(got) = &added.binary else {
            panic!("expected a download: {:?}", added.binary);
        };
        assert!(!got.verified, "no .sha256 was published");
        assert_eq!(got.sha256, hex(&Sha256::digest(b"#!/bin/sh\n")));
        assert!(got.url.contains("/releases/download/v1/"), "{}", got.url);
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            "#!/bin/sh\n"
        );
    }

    #[test]
    fn a_wrong_digest_installs_the_plugin_without_the_program() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", unbuilt);
        let asset = publish_asset(&base, "latest/download", "tool", b"payload");
        fs::write(format!("{}.sha256", asset.display()), "00".repeat(32)).unwrap();
        let added = add(&f.host, "github:acme/tool", None).unwrap();
        let Binary::Missing { download_error, .. } = &added.binary else {
            panic!("expected the build hint: {:?}", added.binary);
        };
        assert!(
            download_error.contains("does not match its published sha256"),
            "{download_error}"
        );
        assert!(
            !added.dir.join("bin/tool").exists(),
            "the rejected file is gone"
        );
        assert!(
            added.dir.join("pixel-plugin.toml").is_file(),
            "the plugin itself is installed"
        );
    }

    #[test]
    fn without_a_release_asset_the_plugin_is_installed_but_not_runnable() {
        let mut f = fixture();
        github_base(&mut f, "acme", "tool", unbuilt);
        let added = add(&f.host, "github:acme/tool@v1", None).unwrap();
        let Binary::Missing {
            git_url,
            reference,
            download_error,
        } = &added.binary
        else {
            panic!("expected the build hint: {:?}", added.binary);
        };
        assert_eq!(git_url, &format!("{}/acme/tool.git", f.host.github_base()));
        assert_eq!(reference.as_deref(), Some("v1"));
        assert!(
            download_error.starts_with("cannot download "),
            "{download_error}"
        );

        let err = resolve(&f.host, "tool").unwrap_err().to_string();
        assert!(err.contains("`run` = \"bin/tool\""), "{err}");
        let rows = crate::list(&f.host).unwrap();
        let row = rows.iter().find(|r| r.name == "tool").unwrap();
        let problem = row.problem.as_deref().unwrap();
        assert!(
            problem.starts_with("not runnable: `run` = \"bin/tool\""),
            "{problem}"
        );
        assert!(crate::render(&rows).contains("  ! tool: not runnable:"));
    }

    #[test]
    fn a_release_asset_replaces_the_launcher_the_repository_ships() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", launcher);
        let asset = publish_asset(&base, "latest/download", "tool", b"#!/bin/sh\necho built\n");
        fs::write(
            format!("{}.sha256", asset.display()),
            hex(&Sha256::digest(b"#!/bin/sh\necho built\n")),
        )
        .unwrap();
        let added = add(&f.host, "github:acme/tool", None).unwrap();
        assert!(
            matches!(added.binary, Binary::Downloaded(_)),
            "{:?}",
            added.binary
        );
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            "#!/bin/sh\necho built\n"
        );
        assert!(
            !added.dir.join("bin/.tool.download").exists(),
            "no partial file is left beside the program"
        );
    }

    #[test]
    fn without_a_release_asset_a_shipped_launcher_stays_and_the_build_is_named() {
        let mut f = fixture();
        github_base(&mut f, "acme", "tool", launcher);
        let added = add(&f.host, "github:acme/tool@v1", None).unwrap();
        let Binary::Shipped {
            git_url,
            reference,
            download_error,
        } = &added.binary
        else {
            panic!(
                "expected the shipped program and its build: {:?}",
                added.binary
            );
        };
        assert_eq!(git_url, &format!("{}/acme/tool.git", f.host.github_base()));
        assert_eq!(reference.as_deref(), Some("v1"));
        assert!(
            download_error.starts_with("cannot download "),
            "{download_error}"
        );
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            LAUNCHER,
            "a failed download leaves the shipped file untouched"
        );
        assert!(!added.dir.join("bin/.tool.download").exists());
    }

    #[test]
    fn a_rejected_asset_leaves_the_shipped_launcher_in_place() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", launcher);
        let asset = publish_asset(&base, "latest/download", "tool", b"payload");
        fs::write(format!("{}.sha256", asset.display()), "00".repeat(32)).unwrap();
        let added = add(&f.host, "github:acme/tool", None).unwrap();
        let Binary::Shipped { download_error, .. } = &added.binary else {
            panic!("expected the shipped program: {:?}", added.binary);
        };
        assert!(
            download_error.contains("does not match its published sha256"),
            "{download_error}"
        );
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            LAUNCHER
        );
    }

    #[test]
    fn a_local_source_runs_what_it_ships_without_asking_a_release() {
        let mut f = fixture();
        let base = github_base(&mut f, "acme", "tool", launcher);
        publish_asset(&base, "latest/download", "tool", b"payload");
        let src = f.tmp.path().join("local");
        fs::create_dir_all(&src).unwrap();
        launcher(&src);
        let added = add(&f.host, src.to_str().unwrap(), None).unwrap();
        assert_eq!(added.binary, Binary::Present);
        assert_eq!(
            fs::read_to_string(added.dir.join("bin/tool")).unwrap(),
            LAUNCHER
        );
    }

    #[test]
    fn a_dangling_program_link_is_a_defect_not_a_build_to_do() {
        let mut f = fixture();
        github_base(&mut f, "acme", "tool", |work| {
            plugin_dir(work, "tool", "");
            fs::remove_file(work.join("tool/run.sh")).unwrap();
            std::os::unix::fs::symlink("/no/such/program", work.join("tool/run.sh")).unwrap();
        });
        let err = add(&f.host, "github:acme/tool", None).unwrap_err();
        assert!(matches!(err, Error::BadRun { .. }), "{err}");
        assert!(!f.host.user_plugins().join("tool").exists());
    }

    #[test]
    fn a_download_never_follows_a_link_out_of_the_plugin() {
        let mut f = fixture();
        let outside = f.tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let base = github_base(&mut f, "acme", "tool", |work| {
            unbuilt(work);
            std::os::unix::fs::symlink(&outside, work.join("bin")).unwrap();
        });
        publish_asset(&base, "latest/download", "tool", b"payload");
        let added = add(&f.host, "github:acme/tool", None).unwrap();
        let Binary::Missing { download_error, .. } = &added.binary else {
            panic!("expected the build hint: {:?}", added.binary);
        };
        assert!(
            download_error.contains("not a directory in the plugin"),
            "{download_error}"
        );
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing written outside"
        );
    }

    #[test]
    fn another_source_kind_never_downloads_a_program() {
        let f = fixture();
        let src = source_dir(&f, "tool", "");
        fs::remove_file(src.join("run.sh")).unwrap();
        let err = add(&f.host, src.to_str().unwrap(), None).unwrap_err();
        assert!(matches!(err, Error::BadRun { .. }), "{err}");
    }

    #[test]
    fn a_malformed_github_source_is_refused_before_anything_is_fetched() {
        let f = fixture();
        for bad in [
            "github:",
            "github:acme",
            "github:acme/tool@",
            "github:-x/y",
            "github:a/b/c",
        ] {
            let err = add(&f.host, bad, None).unwrap_err();
            assert!(matches!(err, Error::Source(_)), "{bad}: {err}");
            assert!(
                err.to_string().contains("github:<owner>/<repo>[@<ref>]"),
                "{bad}: {err}"
            );
        }
        assert!(!f.host.user_plugins().exists());
    }

    #[test]
    fn a_github_base_that_is_not_a_git_url_is_refused() {
        let mut f = fixture();
        f.host.github_base = Some("ext::sh -c id".to_owned());
        let err = add(&f.host, "github:acme/tool", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("GitHub base URL ext::sh -c id must start with"),
            "{err}"
        );
        f.host.github_base = Some("https://user:secret@ghe.example".to_owned());
        let err = add(&f.host, "github:acme/tool", None)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn a_failed_github_clone_names_the_source_by_its_typed_form() {
        let mut f = fixture();
        f.host.github_base = Some(format!("file://{}/nowhere", f.tmp.path().display()));
        let err = add(&f.host, "github:acme/tool", None)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("cannot clone github:acme/tool: "), "{err}");
        assert!(!err.contains("file://"), "the URL is shown as typed: {err}");
    }

    #[test]
    fn npm_sources_are_not_supported_yet() {
        let f = fixture();
        let err = add(&f.host, "npm:@acme/tool", None).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "npm sources are not supported yet; use github:<owner>/<repo> or a local path"
        );
        assert!(!f.host.user_plugins().exists());
    }

    #[test]
    fn the_build_command_quotes_what_a_shell_would_split() {
        let root = Path::new("/home/o'neil/.pixel/plugins/tool");
        assert_eq!(
            cargo_install_command("https://github.com/acme/tool.git", None, root),
            "cargo install --git https://github.com/acme/tool.git --root '/home/o'\\''neil/.pixel/plugins/tool'"
        );
        assert_eq!(
            cargo_install_command("file:///a b/tool.git", Some("release/v1"), Path::new("/p")),
            "cargo install --git 'file:///a b/tool.git' --rev release/v1 --root /p"
        );
    }

    #[test]
    fn a_plugin_answering_to_an_installed_plugins_command_is_refused() {
        let f = fixture();
        let git = plugin_dir(
            &f.tmp.path().join("src"),
            "git",
            "commands = [\"commit\", \"push\"]",
        );
        add(&f.host, git.to_str().unwrap(), None).unwrap();
        let other = plugin_dir(&f.tmp.path().join("src"), "other", "commands = [\"push\"]");
        let err = add(&f.host, other.to_str().unwrap(), None).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err}");
        assert!(
            err.to_string().contains("`git`") && err.to_string().contains("`other`"),
            "{err}"
        );
        assert!(!f.host.user_plugins().join("other").exists());
        let named = plugin_dir(&f.tmp.path().join("src"), "commit", "");
        assert!(matches!(
            add(&f.host, named.to_str().unwrap(), None).unwrap_err(),
            Error::Conflict { .. }
        ));
        let fine = plugin_dir(&f.tmp.path().join("src"), "fetch", "");
        add(&f.host, fine.to_str().unwrap(), None).unwrap();
    }
}
