// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The plugin host, API 1: one capability, `command`.
//!
//! A plugin is a directory with a `pixel-plugin.toml` manifest (or just a
//! `pixel-<name>` executable on PATH). `pixel <name> [args…]`, for a name the
//! core binary does not know, runs it. Lookup order: a trusted repo plugin
//! (`<repo>/.pixel/plugins/<name>/`), a user plugin
//! (`~/.pixel/plugins/<name>/`), then PATH. A repo plugin runs only after
//! `pixel plugin trust <name>` recorded a digest of its directory in
//! `~/.pixel/trusted.toml`; any change to the directory revokes that. A
//! plugin that declares `network = true` also needs `pixel plugin enable`.
//!
//! Every input the host reads from the world (home, repo root, PATH) is a
//! field of [`Host`]; the CLI fills it from the process, tests from a
//! scratch directory.

mod discover;
mod error;
mod hash;
mod install;
mod invoke;
mod manifest;
mod state;

pub use discover::{
    Host, Place, Resolved, Row, contained_program, find_on_path, list, render, resolve,
};
pub use error::Error;
pub use hash::dir_digest;
pub use install::{Added, Enabled, add, enable, looks_like_git_url, redact_source, remove, trust};
pub use invoke::{
    Env, HINT_EXIT, MOVED_COMMANDS, PIXEL_API, PLUGINS_REPO, command, command_word, moved_hint,
    os_args,
};
pub use manifest::{API_VERSION, Capability, MANIFEST_FILE, Manifest, valid_name};
pub use state::{CONFIG_FILE, Config, TRUST_FILE, TrustStore};

#[cfg(test)]
pub(crate) mod testutil {
    //! Scratch homes, repos and plugin directories for the unit tests.

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::Host;

    pub(crate) struct Fixture {
        pub(crate) tmp: tempfile::TempDir,
        pub(crate) host: Host,
    }

    /// A home, a repo root and a one-directory PATH, all empty.
    pub(crate) fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        let bin = tmp.path().join("bin");
        for dir in [&home, &repo, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let host = Host {
            home,
            repo_root: Some(repo),
            path: bin.into_os_string(),
        };
        Fixture { tmp, host }
    }

    pub(crate) fn manifest_text(name: &str, extra: &str) -> String {
        format!(
            "name = \"{name}\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"d\"\nrun = \"run.sh\"\ncapabilities = [\"command\"]\n{extra}\n"
        )
    }

    /// `<plugins>/<name>/` with a manifest and an executable `run.sh`.
    pub(crate) fn plugin_dir(plugins: &Path, name: &str, extra: &str) -> PathBuf {
        let dir = plugins.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pixel-plugin.toml"), manifest_text(name, extra)).unwrap();
        let run = dir.join("run.sh");
        fs::write(&run, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// Real git in `dir`, with the identity and config isolation the other
    /// crates' fixtures use.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}
