// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel install <source>`, `pixel uninstall <plugin>`, `pixel plugin …` and the
//! dispatch of an unknown subcommand to a plugin (`specs/plugins.md`).
//!
//! The host itself (manifests, trust, lookup) is `pixel-plugin`; this file
//! reads the process (HOME, PATH, the working directory), prints, and execs.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus};
use std::time::{Duration, Instant};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use pixel_plugin::{Binary, Env, Error, Host};
use serde_json::{Value, json};

use crate::cli::PluginCmd;
use crate::commands::Done;

/// Environment variable that redirects `github:<owner>/<repo>` sources (and
/// their release downloads) from `https://github.com` to a mirror, a GitHub
/// Enterprise host or a `file://` directory laid out the same way.
const GITHUB_BASE_ENV: &str = "PIXEL_GITHUB_BASE_URL";

/// Exit status of a refusal no retry changes (an unsupported source kind).
const REFUSED_EXIT: i32 = 2;

/// The host as this process sees it. The repo tier is the git root of the
/// working directory, and never the home directory itself (a dotfiles
/// repository there would otherwise turn the user's own plugins into
/// trust-gated "repo" plugins).
fn host() -> Result<Host, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or("HOME is not set: plugins live under ~/.pixel")?;
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let repo_root = pixel_io::discover_root(&cwd).filter(|root| *root != home);
    Ok(Host {
        repo_root,
        home,
        path: std::env::var_os("PATH").unwrap_or_default(),
        github_base: std::env::var(GITHUB_BASE_ENV)
            .ok()
            .filter(|base| !base.is_empty()),
    })
}

/// `pixel install <source>`. An unsupported source kind (`npm:`) prints its
/// reason and owns exit status 2.
pub(crate) fn install(
    source: &str,
    name: Option<&str>,
    allow_unverified: bool,
    json: bool,
) -> Result<Done, String> {
    let host = host()?;
    let unverified = if allow_unverified {
        pixel_plugin::Unverified::Allow
    } else {
        pixel_plugin::Unverified::Refuse
    };
    match pixel_plugin::add_with(&host, source, name, unverified) {
        Ok(added) => {
            let mut text = render_installed(&added);
            text.push_str(&deliver_skill(&host, &added)?);
            Ok(Done::new(installed_json(&added), text, json))
        }
        Err(error @ Error::Unsupported(_)) => {
            let mut done = Done::new(json!({"ok": false}), String::new(), json)
                .note(Some(format!("pixel: {error}")));
            done.exit = REFUSED_EXIT;
            Ok(done)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// What `pixel install` prints once the plugin is in place.
fn render_installed(added: &pixel_plugin::Added) -> String {
    let mut out = format!(
        "installed plugin {} {} in {}\n",
        added.name,
        added.manifest.version,
        added.dir.display()
    );
    match &added.binary {
        Binary::Present => {}
        Binary::Downloaded(got) => {
            let checked = if got.verified {
                format!("sha256 {} verified", got.sha256)
            } else {
                format!(
                    "UNVERIFIED: no .sha256 published, installed by --allow-unverified; sha256 {}",
                    got.sha256
                )
            };
            out.push_str(&format!("downloaded {} ({checked})\n", got.url));
        }
        Binary::Shipped {
            git_url,
            reference,
            download_error,
        } => {
            let build =
                pixel_plugin::cargo_install_command(git_url, reference.as_deref(), &added.dir);
            out.push_str(&format!(
                "{name} runs the repository's own `{run}`: no release asset replaced it ({download_error})\nif `{name}` then reports a missing program, build it into the plugin with: {build} --force\n",
                name = added.name,
                run = added.manifest.run,
            ));
        }
        Binary::Missing {
            git_url,
            reference,
            download_error,
        } => {
            let build =
                pixel_plugin::cargo_install_command(git_url, reference.as_deref(), &added.dir);
            out.push_str(&format!(
                "{name} is installed but not runnable: its program `{run}` is not built and no release asset was usable ({download_error})\nbuild it with: {build}\nthe build must leave `{run}` under {dir}; `pixel plugin list` shows when {name} runs\n",
                name = added.name,
                run = added.manifest.run,
                dir = added.dir.display(),
            ));
        }
    }
    if added.manifest.network {
        out.push_str(&format!(
            "{} declares network access: run `pixel plugin enable {}` to allow it\n",
            added.name, added.name
        ));
    }
    out
}

/// What `pixel install --json` prints: the plugin, and how its program got
/// there (`binary.kind`), with the downloaded asset's URL, sha256 and whether
/// a published digest verified it.
fn installed_json(added: &pixel_plugin::Added) -> Value {
    let binary = match &added.binary {
        Binary::Present => json!({ "kind": "present" }),
        Binary::Downloaded(got) => json!({
            "kind": "downloaded",
            "url": got.url,
            "sha256": got.sha256,
            "verified": got.verified,
        }),
        Binary::Shipped {
            git_url,
            reference,
            download_error,
        } => json!({
            "kind": "shipped",
            "git_url": git_url,
            "reference": reference,
            "download_error": download_error,
        }),
        Binary::Missing {
            git_url,
            reference,
            download_error,
        } => json!({
            "kind": "missing",
            "git_url": git_url,
            "reference": reference,
            "download_error": download_error,
        }),
    };
    json!({
        "name": added.name,
        "version": added.manifest.version,
        "dir": added.dir.to_string_lossy(),
        "network": added.manifest.network,
        "binary": binary,
    })
}

/// Deliver a plugin's declared skill to the harness roots once the plugin is
/// in place, listing each destination: the roots rule and the backup
/// semantics are `pixel setup`'s, one spelling, in `pixel-setup`.
fn deliver_skill(host: &Host, added: &pixel_plugin::Added) -> Result<String, String> {
    let Some(skill) = &added.skill else {
        return Ok(String::new());
    };
    let destinations = pixel_setup::skill::deploy_plugin_skill(&host.home, &added.name, skill)
        .map_err(|error| {
            format!(
                "plugin `{name}` is installed, but its skill was not delivered: {error}\n`pixel uninstall {name}` removes the plugin; fix the cause and install it again",
                name = added.name
            )
        })?;
    Ok(destinations
        .iter()
        .map(|destination| format!("delivered skill {}\n", destination.display()))
        .collect())
}

/// `pixel uninstall <plugin>`.
pub(crate) fn uninstall(name: &str) -> Result<Done, String> {
    let host = host()?;
    // What the install delivered, read before the plugin directory goes: a
    // harness copy no longer matching it is the user's and is kept.
    let delivered = pixel_plugin::installed_skill(&host, name).map_err(|e| e.to_string())?;
    pixel_plugin::remove(&host, name).map_err(|e| e.to_string())?;
    let mut text = format!("removed plugin {name}\n");
    let (mut removed, mut kept) = (Vec::new(), Vec::new());
    if let Some(content) = &delivered {
        let sweep = pixel_setup::skill::remove_plugin_skill(&host.home, name, content)
            .map_err(|e| e.to_string())?;
        for file in &sweep.removed {
            text.push_str(&format!("removed skill {}\n", file.display()));
        }
        for file in &sweep.kept {
            text.push_str(&format!(
                "kept skill {} (differs from what was delivered)\n",
                file.display()
            ));
        }
        removed = sweep.removed;
        kept = sweep.kept;
    }
    let value = json!({"removed": name, "skills_removed": removed, "skills_kept": kept});
    Ok(Done::new(value, text, false))
}

/// `pixel plugin …`.
pub(crate) fn run(cmd: &PluginCmd) -> Result<Done, String> {
    let host = host()?;
    let (value, text) = match cmd {
        PluginCmd::List => {
            let rows = pixel_plugin::list(&host).map_err(|e| e.to_string())?;
            (json!({"listed": rows.len()}), pixel_plugin::render(&rows))
        }
        PluginCmd::Enable { name } => {
            match pixel_plugin::enable(&host, name).map_err(|e| e.to_string())? {
                pixel_plugin::Enabled::Yes => {
                    (json!({"enabled": name}), format!("enabled plugin {name}\n"))
                }
                pixel_plugin::Enabled::NotNeeded => (
                    json!({"enabled": null}),
                    format!("plugin {name} declares no network access: nothing to enable\n"),
                ),
            }
        }
        PluginCmd::Disable { name } => {
            pixel_plugin::disable(&host, name).map_err(|e| e.to_string())?;
            (
                json!({"disabled": name}),
                format!("disabled brief probe {name}\n"),
            )
        }
        PluginCmd::Trust { name } => {
            let digest = pixel_plugin::trust(&host, name).map_err(|e| e.to_string())?;
            (
                json!({"trusted": name, "sha256": digest}),
                format!("trusted repo plugin {name} (sha256 {})\n", &digest[..12]),
            )
        }
    };
    Ok(Done::new(value, text, false))
}

/// What an unknown subcommand turned into.
enum Plan {
    /// A plugin to exec.
    Run(Process),
    /// A plugin that cannot run: print this and exit with the code.
    Stop(String, i32),
    /// Not a plugin: clap's own error stands.
    Unknown,
}

/// Decide what `pixel <name> <args…>` does when the core has no such command:
/// a plugin of that name runs, one that cannot says why, and any other name
/// is clap's.
fn plan(host: &Host, name: &str, args: &[OsString], env: Env<'_>) -> Plan {
    match pixel_plugin::resolve(host, name) {
        Ok(Some(plugin)) => Plan::Run(pixel_plugin::command(&plugin, args, env)),
        Ok(None) => Plan::Unknown,
        Err(error) => Plan::Stop(error.to_string(), 1),
    }
}

/// Hand an unrecognized subcommand to a plugin. `Some(code)` when it was
/// handled and the process should exit with `code`; `None` leaves clap's
/// error to be shown.
pub(crate) fn dispatch(error: &clap::Error, argv: &[String]) -> Option<i32> {
    let (index, name) = pixel_plugin::command_word(argv)?;
    if !names_the_unknown_command(error, name) {
        return None;
    }
    let host = host().ok()?;
    let cwd = std::env::current_dir().ok()?;
    let root = pixel_io::discover_root(Path::new(".")).unwrap_or(cwd);
    let graph_db = pixel_io::sidecar::dir(&root).join(pixel_graph::store::DB);
    let bin = std::env::current_exe().ok()?;
    let env = Env {
        repo_root: &root,
        graph_db: &graph_db,
        bin: &bin,
    };
    let args = pixel_plugin::os_args(&argv[index + 1..]);
    match plan(&host, name, &args, env) {
        Plan::Run(process) => Some(run_logged(process, name, argv, &root)),
        Plan::Stop(message, code) => {
            eprintln!("pixel: {message}");
            Some(code)
        }
        Plan::Unknown => None,
    }
}

/// Whether clap rejected exactly the top-level word `name` as an unknown
/// subcommand (and not, say, a misspelt nested one).
fn names_the_unknown_command(error: &clap::Error, name: &str) -> bool {
    error.kind() == ErrorKind::InvalidSubcommand
        && matches!(
            error.get(ContextKind::InvalidSubcommand),
            Some(ContextValue::String(unknown)) if unknown == name
        )
}

/// Run the plugin as a child with this process's stdio, wait for it, and
/// write its run to the repository's action log like any core command: the
/// command word and the arguments through [`crate::run::logged_args`] (a
/// credential in them is masked the same way), the outcome and the duration.
/// The exit code is the plugin's own. A signal that ends the plugin is
/// `128 + signal`, the shell's convention.
fn run_logged(mut process: Process, name: &str, argv: &[String], root: &Path) -> i32 {
    let started = Instant::now();
    let code = match process.status() {
        Ok(status) => exit_code(status),
        Err(failure) => {
            eprintln!("pixel: cannot run plugin `{name}`: {failure}");
            1
        }
    };
    log_run(name, argv, root, code, started.elapsed());
    code
}

/// The code `pixel` exits with for a plugin that ended with `status`.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// One action-log record for a plugin run. The record is on disk before the
/// process exits: `pixel metrics` reads it back.
fn log_run(name: &str, argv: &[String], root: &Path, code: i32, elapsed: Duration) {
    let mut logger = pixel_metrics::ActionLog::spawn_for_root(root);
    let result = if code == 0 {
        Ok(())
    } else {
        Err(format!("{name} exited {code}"))
    };
    let event = crate::run::action(name, crate::run::logged_args(&argv[1..]), "plugin")
        .with_result(&result, elapsed);
    logger.log(event);
    logger.finish_flush();
}

#[cfg(test)]
mod tests;
