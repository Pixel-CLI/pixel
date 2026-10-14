// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel install`, `pixel uninstall`, `pixel plugin …` and `pixel <plugin> …`
//! through the real binary, with a scratch HOME, a scratch git repository and
//! a PATH of its own.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use super::support::{Scratch, git, pixel_command};

/// What every fixture plugin prints, and how it exits.
const RUN_SCRIPT: &str = "#!/bin/sh
printf 'args=%s\\n' \"$*\"
printf 'api=%s\\nroot=%s\\ndb=%s\\nbin=%s\\n' \"$PIXEL_API\" \"$PIXEL_REPO_ROOT\" \"$PIXEL_GRAPH_DB\" \"$PIXEL_BIN\"
if [ \"$1\" = fail ]; then exit 7; fi
";

struct World {
    scratch: Scratch,
}

impl World {
    fn new(tag: &str) -> Self {
        let scratch = Scratch::for_test("pixel-plugin-cli", tag);
        for dir in ["home", "repo", "bin"] {
            std::fs::create_dir_all(scratch.join(dir)).unwrap();
        }
        git(&scratch.join("repo"), &["init", "-q"]);
        Self { scratch }
    }

    fn home(&self) -> PathBuf {
        self.scratch.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.scratch.join("repo")
    }

    fn user_plugins(&self) -> PathBuf {
        self.home().join(".pixel/plugins")
    }

    fn repo_plugins(&self) -> PathBuf {
        self.repo().join(".pixel/plugins")
    }

    /// `pixel <args>` from inside the repository.
    fn pixel(&self, args: &[&str]) -> Output {
        self.pixel_with(args, &[])
    }

    /// `pixel <args>` with more environment variables.
    fn pixel_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        pixel_command()
            .current_dir(self.repo())
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.scratch.join("bin").display()),
            )
            .env("PIXEL_METRICS", "0")
            .envs(envs.iter().copied())
            .args(args)
            .output()
            .unwrap()
    }

    /// A plugin source directory outside HOME and the repo.
    fn source(&self, name: &str, extra: &str) -> PathBuf {
        plugin_dir(&self.scratch.join("src"), name, extra)
    }
}

fn plugin_dir(parent: &Path, name: &str, extra: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pixel-plugin.toml"),
        format!(
            "name = \"{name}\"\nversion = \"1.4.2\"\napi = 1\ndescription = \"fixture\"\nrun = \"run.sh\"\ncapabilities = [\"command\"]\n{extra}\n"
        ),
    )
    .unwrap();
    let run = dir.join("run.sh");
    std::fs::write(&run, RUN_SCRIPT).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    stdout(output)
}

#[test]
fn list_is_empty_on_a_clean_machine() {
    let world = World::new("empty");
    let out = world.pixel(&["plugin", "list"]);
    assert_eq!(ok(&out), "no plugins\n");
}

#[test]
fn an_installed_plugin_runs_with_the_args_the_environment_and_its_exit_code() {
    let world = World::new("run");
    let source = world.source("tool", "");
    let added = world.pixel(&["install", source.to_str().unwrap()]);
    assert!(
        ok(&added).starts_with("installed plugin tool 1.4.2 in "),
        "{added:?}"
    );

    let listed = ok(&world.pixel(&["plugin", "list"]));
    let row = listed.lines().find(|l| l.starts_with("tool")).unwrap();
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>(),
        ["tool", "user", "1.4.2", "1", "command", "-", "-", "-"]
    );

    let out = world.pixel(&["--metrics", "off", "tool", "a", "--flag", "b c"]);
    let text = ok(&out);
    let repo = world.repo().canonicalize().unwrap();
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "args=a --flag b c".to_owned(),
            "api=1".to_owned(),
            format!("root={}", repo.display()),
            format!("db={}", repo.join(".pixel/graph.v2.db").display()),
            format!(
                "bin={}",
                Path::new(env!("CARGO_BIN_EXE_pixel"))
                    .canonicalize()
                    .unwrap()
                    .display()
            ),
        ]
    );

    let failing = world.pixel(&["tool", "fail"]);
    assert_eq!(failing.status.code(), Some(7), "{failing:?}");
}

#[test]
fn a_repo_plugin_needs_trust_and_loses_it_when_a_file_changes() {
    let world = World::new("trust");
    let dir = plugin_dir(&world.repo_plugins(), "tool", "");

    let refused = world.pixel(&["tool", "x"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        stderr(&refused).contains("`pixel plugin trust tool`"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(stdout(&refused), "", "nothing of the plugin ran");

    let trusted = ok(&world.pixel(&["plugin", "trust", "tool"]));
    assert!(
        trusted.starts_with("trusted repo plugin tool (sha256 "),
        "{trusted}"
    );
    let ran = ok(&world.pixel(&["tool", "x"]));
    assert!(ran.starts_with("args=x\n"), "{ran}");

    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>()
                == ["tool", "repo", "1.4.2", "1", "command", "-", "yes", "-"]),
        "{listed}"
    );

    std::fs::write(dir.join("extra.txt"), "changed").unwrap();
    let revoked = world.pixel(&["tool", "x"]);
    assert_eq!(revoked.status.code(), Some(1), "{revoked:?}");
    assert!(stderr(&revoked).contains("`pixel plugin trust tool`"));
    assert_eq!(stdout(&revoked), "");
}

#[test]
fn a_run_that_escapes_through_a_symlink_never_executes() {
    let world = World::new("escape");
    let dir = plugin_dir(&world.user_plugins(), "esc", "");
    std::fs::remove_file(dir.join("run.sh")).unwrap();
    let marker = world.scratch.join("outside-ran");
    let outside = world.scratch.join("outside.sh");
    std::fs::write(&outside, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("run.sh")).unwrap();

    let out = world.pixel(&["esc"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("outside the plugin directory"),
        "{}",
        stderr(&out)
    );
    assert!(!marker.exists(), "the escaped script must not have run");
}

#[test]
fn a_network_plugin_is_refused_until_it_is_enabled() {
    let world = World::new("network");
    let source = world.source("net", "network = true");
    let added = ok(&world.pixel(&["install", source.to_str().unwrap()]));
    assert!(added.contains("run `pixel plugin enable net`"), "{added}");

    let refused = world.pixel(&["net"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(stderr(&refused).contains("`pixel plugin enable net`"));

    assert_eq!(
        ok(&world.pixel(&["plugin", "enable", "net"])),
        "enabled plugin net\n"
    );
    assert!(ok(&world.pixel(&["net", "hi"])).starts_with("args=hi\n"));

    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>()
                == ["net", "user", "1.4.2", "1", "command", "yes", "-", "-"]),
        "{listed}"
    );
}

#[test]
fn an_executable_pixel_name_on_path_is_a_plugin_without_a_manifest() {
    let world = World::new("path");
    let exe = world.scratch.join("bin/pixel-hello");
    std::fs::write(&exe, RUN_SCRIPT).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = ok(&world.pixel(&["hello", "world"]));
    assert!(out.starts_with("args=world\napi=1\n"), "{out}");
    assert_eq!(world.pixel(&["hello", "fail"]).status.code(), Some(7));

    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>()
                == ["hello", "path", "-", "1", "command", "-", "-", "-"]),
        "{listed}"
    );
}

#[test]
fn a_user_plugin_shadows_path_and_a_trusted_repo_plugin_shadows_both() {
    let world = World::new("order");
    let exe = world.scratch.join("bin/pixel-who");
    std::fs::write(&exe, "#!/bin/sh\necho from-path\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(ok(&world.pixel(&["who"])), "from-path\n");

    let user = plugin_dir(&world.user_plugins(), "who", "");
    std::fs::write(user.join("run.sh"), "#!/bin/sh\necho from-user\n").unwrap();
    assert_eq!(ok(&world.pixel(&["who"])), "from-user\n");

    let repo = plugin_dir(&world.repo_plugins(), "who", "");
    std::fs::write(repo.join("run.sh"), "#!/bin/sh\necho from-repo\n").unwrap();
    ok(&world.pixel(&["plugin", "trust", "who"]));
    assert_eq!(ok(&world.pixel(&["who"])), "from-repo\n");
}

#[test]
fn uninstall_deletes_the_plugin() {
    let world = World::new("remove");
    let source = world.source("tool", "");
    ok(&world.pixel(&["install", source.to_str().unwrap()]));
    assert_eq!(
        ok(&world.pixel(&["uninstall", "tool"])),
        "removed plugin tool\n"
    );
    assert!(!world.user_plugins().join("tool").exists());
    let gone = world.pixel(&["tool"]);
    assert_eq!(
        gone.status.code(),
        Some(2),
        "back to clap's unknown-command error: {gone:?}"
    );
    let missing = world.pixel(&["uninstall", "tool"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("plugin `tool` is not installed"));
}

#[test]
fn an_unknown_command_without_a_plugin_keeps_clap_error() {
    let world = World::new("unknown");
    let out = world.pixel(&["no-such-command", "x"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        stderr(&out).contains("unrecognized subcommand 'no-such-command'"),
        "{}",
        stderr(&out)
    );
    let nested = world.pixel(&["plugin", "no-such-sub"]);
    assert_eq!(nested.status.code(), Some(2));
}

#[test]
fn install_reports_a_bad_source_and_keeps_the_credentials_of_a_url_out_of_the_log() {
    let world = World::new("addbad");
    let out = world.pixel(&["install", "/definitely/not/here"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("neither a directory nor a git URL"));

    let url = "file://user:ghp_topsecret@/definitely/not/here";
    let out = world.pixel(&["install", url]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(!stderr(&out).contains("ghp_topsecret"), "{}", stderr(&out));
    let log =
        std::fs::read_to_string(world.repo().join(".pixel/actions.jsonl")).unwrap_or_default();
    assert!(!log.contains("ghp_topsecret"), "{log}");
}

/// The pre-rename spellings that stood for a command now shipped as a
/// plugin and not built into the core, with the command they resolve to.
const MOVED_ALIASES: [(&str, &str); 19] = [
    ("branch", "new-branch"),
    ("branches", "list-branches"),
    ("excavate", "dig-history"),
    ("history", "commit-history"),
    ("history-search", "search-history"),
    ("inspect", "repo-state"),
    ("lifecycle", "file-history"),
    ("log", "action-log"),
    ("publish", "commit"),
    ("reconcile", "sync-branch"),
    ("replay-flow", "flow"),
    ("rescue", "plan-rollback"),
    ("review", "review-changes"),
    ("rewrite", "squash-branch"),
    ("savings", "token-savings"),
    ("ship", "commit-and-push"),
    ("sniper", "list-errors"),
    ("sync", "fetch"),
    ("update", "fast-forward"),
];

/// The moved commands the core no longer builds in: the only names the
/// moved-command hint is for. Only `check-release` was kept in the binary.
fn unbuilt_moved() -> Vec<(&'static str, &'static str)> {
    let built_in = super::support::built_in_commands();
    pixel_plugin::MOVED_COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| !built_in.contains(*name))
        .collect()
}

fn moved_hint(name: &str) -> String {
    let (_, repo) = pixel_plugin::MOVED_COMMANDS
        .iter()
        .find(|(command, _)| *command == name)
        .unwrap();
    format!("pixel: \"{name}\" moved to a plugin: pixel install github:Pixel-CLI/{repo}\n")
}

#[test]
fn the_alias_list_is_every_rename_that_points_at_an_unbuilt_moved_command() {
    let unbuilt: Vec<&str> = unbuilt_moved().iter().map(|(name, _)| *name).collect();
    let derived: Vec<(&str, &str)> = pixel_proto::commands::RENAMED_COMMANDS
        .iter()
        .copied()
        .filter(|(_, new)| unbuilt.contains(new))
        .collect();
    assert_eq!(derived, MOVED_ALIASES);
}

#[test]
fn the_unbuilt_moved_commands_are_the_ones_the_core_already_dropped() {
    let unbuilt: Vec<&str> = unbuilt_moved().iter().map(|(name, _)| *name).collect();
    for name in [
        "flow",
        "plan-rollback",
        "token-savings",
        "squash-branch",
        "fast-forward",
    ] {
        assert!(unbuilt.contains(&name), "{name}: {unbuilt:?}");
    }
}

#[test]
fn an_unbuilt_moved_command_and_each_of_its_aliases_without_a_plugin_print_the_hint_and_exit_two() {
    let world = World::new("hints");
    for (name, repo) in unbuilt_moved() {
        let out = world.pixel(&[name, "anything", "--json"]);
        assert_eq!(out.status.code(), Some(2), "{name}: {out:?}");
        assert_eq!(
            stderr(&out),
            format!("pixel: \"{name}\" moved to a plugin: pixel install github:Pixel-CLI/{repo}\n"),
            "{name}"
        );
        assert_eq!(stdout(&out), "", "{name}");
    }
    for (alias, name) in MOVED_ALIASES {
        let out = world.pixel(&[alias, "--help"]);
        assert_eq!(out.status.code(), Some(2), "{alias}: {out:?}");
        assert_eq!(
            stderr(&out),
            moved_hint(name),
            "{alias} names the canonical plugin, not itself"
        );
        assert_eq!(stdout(&out), "", "{alias}");
    }
}

#[test]
fn the_hint_names_the_repository_each_command_moves_to() {
    let world = World::new("hint-repos");
    for (name, repo) in [
        ("flow", "pixel-flow"),
        ("plan-rollback", "pixel-git"),
        ("squash-branch", "pixel-git"),
        ("fast-forward", "pixel-git"),
        ("token-savings", "pixel-telemetry"),
    ] {
        let out = world.pixel(&[name]);
        assert_eq!(
            stderr(&out),
            format!("pixel: \"{name}\" moved to a plugin: pixel install github:Pixel-CLI/{repo}\n")
        );
    }
}

#[test]
fn a_moved_command_that_is_still_built_in_runs_instead_of_hinting() {
    let world = World::new("still-built-in");
    let help = world.pixel(&["check-release", "--help"]);
    assert_eq!(help.status.code(), Some(0), "{help:?}");
    assert!(
        stdout(&help).contains("Usage: pixel check-release"),
        "{}",
        stdout(&help)
    );
    assert!(
        !stderr(&help).contains("moved to a plugin"),
        "{}",
        stderr(&help)
    );
    assert!(super::support::built_in_commands().contains("check-release"));
}

#[test]
fn check_release_is_the_only_moved_command_the_core_still_builds_in() {
    let built_in = super::support::built_in_commands();
    let still_built_in: Vec<&str> = pixel_plugin::MOVED_COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| built_in.contains(*name))
        .collect();
    assert_eq!(still_built_in, ["check-release"]);
    assert!(pixel_plugin::stays_in_core("check-release"));
    assert_eq!(unbuilt_moved().len(), 29, "30 moved, one kept");
    assert_eq!(pixel_plugin::MOVED_COMMANDS.len(), 30);
}

#[test]
fn the_pruned_surface_lists_no_unbuilt_moved_command() {
    let listed = super::support::built_in_commands();
    assert!(listed.contains("plugin"), "{listed:?}");
    assert!(
        listed.contains("install") && listed.contains("uninstall"),
        "{listed:?}"
    );
    assert!(listed.contains("setup"), "{listed:?}");
    for (name, _) in unbuilt_moved() {
        assert!(!listed.contains(name), "{name} is still listed: {listed:?}");
    }
}

/// A plugin that reports how it was called, on PATH as `pixel-<name>`.
fn path_plugin(world: &World, name: &str) {
    let exe = world.scratch.join(format!("bin/pixel-{name}"));
    std::fs::write(&exe, RUN_SCRIPT).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn an_unbuilt_moved_command_runs_the_plugin_by_name_and_by_alias_with_its_own_args_and_exit_code() {
    let world = World::new("moved-run");
    for (name, _) in unbuilt_moved() {
        path_plugin(&world, name);
    }
    for (name, _) in unbuilt_moved() {
        let out = world.pixel(&[name, "list", "--json"]);
        assert!(
            ok(&out).starts_with("args=list --json\napi=1\n"),
            "{name}: {out:?}"
        );
        assert_eq!(stderr(&out), "", "{name}: no note for the current name");
        assert_eq!(
            world.pixel(&[name, "fail"]).status.code(),
            Some(7),
            "{name}"
        );
    }
    for (alias, name) in MOVED_ALIASES {
        let out = world.pixel(&[alias, "list", "--json"]);
        assert!(
            ok(&out).starts_with("args=list --json\napi=1\n"),
            "{alias}: {out:?}"
        );
        assert_eq!(
            stderr(&out),
            format!("note: '{alias}' is now '{name}'; the old name stays accepted until 1.0\n"),
            "{alias}"
        );
        assert_eq!(
            world.pixel(&[alias, "fail"]).status.code(),
            Some(7),
            "{alias}"
        );
        let help = world.pixel(&[alias, "--help"]);
        assert_eq!(stderr(&help), "", "{alias} --help prints no rename note");
    }
}

#[test]
fn an_alias_resolves_to_the_plugin_of_the_canonical_name_only() {
    let world = World::new("moved-canonical");
    // `pixel-rescue` is not the plugin `rescue` stands for; `pixel-plan-rollback` is.
    path_plugin(&world, "rescue");
    let out = world.pixel(&["rescue", "x"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(stderr(&out), moved_hint("plan-rollback"));
    path_plugin(&world, "plan-rollback");
    assert!(ok(&world.pixel(&["rescue", "x"])).starts_with("args=x\n"));
}

#[test]
fn an_installed_plugin_serves_the_moved_name() {
    let world = World::new("moved-add");
    let source = world.source("flow", "");
    ok(&world.pixel(&["install", source.to_str().unwrap()]));
    assert!(ok(&world.pixel(&["flow", "list"])).starts_with("args=list\napi=1\n"));
    let by_alias = world.pixel(&["replay-flow", "list"]);
    assert!(
        ok(&by_alias).starts_with("args=list\napi=1\n"),
        "{by_alias:?}"
    );
    // Another unbuilt moved command is still only a hint.
    assert_eq!(
        stderr(&world.pixel(&["plan-rollback"])),
        moved_hint("plan-rollback")
    );
}

/// Every record of the repository's action log, parsed.
fn action_log(world: &World) -> Vec<serde_json::Value> {
    std::fs::read_to_string(world.repo().join(".pixel/actions.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_plugin_run_is_in_the_action_log_like_a_core_command_with_its_outcome_and_masked_args() {
    let world = World::new("plugin-log");
    path_plugin(&world, "flow");
    path_plugin(&world, "plan-rollback");
    let secret = "auth_url=https://login.test/cb?code=s3cret&state=t0ken";
    let done = world.pixel(&["flow", "replay", "login", "--var", secret]);
    assert!(
        ok(&done).starts_with("args=replay login --var auth_url="),
        "{done:?}"
    );
    // A pre-rename spelling is logged under the command it stands for.
    let failed = world.pixel(&["rescue", "fail"]);
    assert_eq!(failed.status.code(), Some(7), "{failed:?}");

    let log = action_log(&world);
    assert_eq!(log.len(), 2, "{log:#?}");
    let (first, second) = (&log[0], &log[1]);
    assert_eq!(first["command"], "flow");
    assert_eq!(first["args"], "flow replay login --var auth_url=<redacted>");
    assert_eq!(first["outcome"], "ok");
    assert!(first.get("error").is_none(), "{first}");
    assert_eq!(second["command"], "plan-rollback");
    assert_eq!(second["args"], "rescue fail");
    assert_eq!(second["outcome"], "error");
    assert_eq!(second["error"], "plan-rollback exited 7");
    for record in &log {
        assert_eq!(
            record["cwd"],
            world.repo().canonicalize().unwrap().display().to_string(),
            "{record}"
        );
        assert!(record["pid"].as_u64().unwrap() > 0, "{record}");
        assert!(record["ts_ms"].as_i64().unwrap() > 0, "{record}");
        assert!(record["invocation_id"].as_str().is_some(), "{record}");
    }
    let text = std::fs::read_to_string(world.repo().join(".pixel/actions.jsonl")).unwrap();
    assert!(
        !text.contains("s3cret") && !text.contains("t0ken"),
        "{text}"
    );
}

#[test]
fn a_moved_command_hint_is_not_a_plugin_run_and_leaves_no_log_record() {
    let world = World::new("plugin-log-hint");
    let out = world.pixel(&["flow", "list"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(action_log(&world).is_empty());
}

#[test]
fn plugin_keeps_list_enable_and_trust_only() {
    let world = World::new("plugin-subs");
    for retired in ["add", "remove"] {
        let out = world.pixel(&["plugin", retired, "x"]);
        assert_eq!(out.status.code(), Some(2), "plugin {retired}: {out:?}");
        assert!(
            stderr(&out).contains(&format!("unrecognized subcommand '{retired}'")),
            "{}",
            stderr(&out)
        );
    }
    let help = ok(&world.pixel(&["plugin", "--help"]));
    let commands = help
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("Options:").next())
        .unwrap();
    let listed: Vec<&str> = commands
        .lines()
        .filter_map(|line| line.strip_prefix("  "))
        .filter(|line| !line.starts_with(' '))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .collect();
    assert_eq!(listed, ["list", "enable", "trust"]);
}

#[test]
fn install_of_an_npm_package_says_not_yet_and_exits_two() {
    let world = World::new("npm");
    let out = world.pixel(&["install", "npm:@acme/tool"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        stderr(&out),
        "pixel: npm sources are not supported yet; use github:<owner>/<repo> or a local path\n"
    );
    assert_eq!(stdout(&out), "");
    assert!(!world.user_plugins().exists(), "nothing installed");
}

/// A `github:`-shaped base directory: `<base>/acme/<repo>.git` is a bare
/// clone of a work repository holding the plugin source.
fn github_base(world: &World, repo: &str, manifest_run: &str, tracked_run: bool) -> PathBuf {
    let work = world.scratch.join(format!("work-{repo}"));
    std::fs::create_dir_all(work.join("bin")).unwrap();
    std::fs::write(
        work.join("pixel-plugin.toml"),
        format!(
            "name = \"{repo}\"\nversion = \"2.0.0\"\napi = 1\ndescription = \"fixture\"\nrun = \"{manifest_run}\"\ncapabilities = [\"command\"]\n"
        ),
    )
    .unwrap();
    if tracked_run {
        let run = work.join(manifest_run);
        std::fs::write(&run, RUN_SCRIPT).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        std::fs::write(work.join("bin/.gitkeep"), "").unwrap();
    }
    git(&work, &["init", "-q"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "seed"]);
    let base = world.scratch.join("gh");
    std::fs::create_dir_all(base.join("acme")).unwrap();
    let bare = base.join(format!("acme/{repo}.git"));
    git(
        &world.scratch,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    base
}

fn base_url(base: &Path) -> String {
    format!("file://{}", base.display())
}

#[test]
fn install_github_clones_the_repo_and_the_plugin_runs() {
    let world = World::new("github");
    let base = github_base(&world, "hello", "run.sh", true);
    let out = world.pixel_with(
        &["install", "github:acme/hello"],
        &[("PIXEL_GITHUB_BASE_URL", &base_url(&base))],
    );
    assert!(
        ok(&out).starts_with("installed plugin hello 2.0.0 in "),
        "{out:?}"
    );
    assert!(world.user_plugins().join("hello/run.sh").is_file());
    assert!(ok(&world.pixel(&["hello", "a b"])).starts_with("args=a b\napi=1\n"));
    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>()
                == ["hello", "user", "2.0.0", "1", "command", "-", "-", "-"]),
        "{listed}"
    );
    let sources = std::fs::read_to_string(world.home().join(".pixel/plugins.toml")).unwrap();
    assert!(
        sources.contains("hello = \"github:acme/hello\""),
        "{sources}"
    );
}

#[test]
fn install_github_downloads_the_release_asset_for_an_unbuilt_program() {
    let world = World::new("github-asset");
    let base = github_base(&world, "built", "bin/built", false);
    let target = pixel_plugin::release_target().expect("a platform with release assets");
    let dir = base.join("acme/built/releases/latest/download");
    std::fs::create_dir_all(&dir).unwrap();
    let asset = dir.join(format!("pixel-built-{target}"));
    std::fs::write(&asset, RUN_SCRIPT).unwrap();
    let out = world.pixel_with(
        &["install", "github:acme/built"],
        &[("PIXEL_GITHUB_BASE_URL", &base_url(&base))],
    );
    let text = ok(&out);
    assert!(
        text.starts_with("installed plugin built 2.0.0 in "),
        "{text}"
    );
    assert!(
        text.contains(&format!("downloaded {}/acme/built/releases/latest/download/pixel-built-{target} (no .sha256 published: checksum not verified)", base_url(&base))),
        "{text}"
    );
    assert!(ok(&world.pixel(&["built", "go"])).starts_with("args=go\napi=1\n"));
}

#[test]
fn install_github_without_an_asset_leaves_the_plugin_not_runnable_and_prints_the_build() {
    let world = World::new("github-unbuilt");
    let base = github_base(&world, "unbuilt", "bin/unbuilt", false);
    let out = world.pixel_with(
        &["install", "github:acme/unbuilt"],
        &[("PIXEL_GITHUB_BASE_URL", &base_url(&base))],
    );
    let text = ok(&out);
    let dir = world.user_plugins().join("unbuilt");
    assert!(
        text.starts_with("installed plugin unbuilt 2.0.0 in "),
        "{text}"
    );
    assert!(
        text.contains(
            "unbuilt is installed but not runnable: its program `bin/unbuilt` is not built"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "build it with: cargo install --git {}/acme/unbuilt.git --root {}\n",
            base_url(&base),
            dir.display()
        )),
        "{text}"
    );
    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed.contains("  ! unbuilt: not runnable: `run` = \"bin/unbuilt\""),
        "{listed}"
    );
    let ran = world.pixel(&["unbuilt"]);
    assert_eq!(ran.status.code(), Some(1), "{ran:?}");
    assert_eq!(stdout(&ran), "");
}

#[test]
fn install_github_reports_a_malformed_source_and_a_repository_that_is_not_there() {
    let world = World::new("github-bad");
    let bad = world.pixel(&["install", "github:acme"]);
    assert_eq!(bad.status.code(), Some(1), "{bad:?}");
    assert!(
        stderr(&bad).contains("github:<owner>/<repo>[@<ref>]"),
        "{}",
        stderr(&bad)
    );
    let missing = world.pixel_with(
        &["install", "github:acme/none"],
        &[(
            "PIXEL_GITHUB_BASE_URL",
            &base_url(&world.scratch.join("gh")),
        )],
    );
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert!(
        stderr(&missing).contains("cannot clone github:acme/none"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn install_twice_points_at_uninstall() {
    let world = World::new("twice");
    let source = world.source("tool", "");
    ok(&world.pixel(&["install", source.to_str().unwrap()]));
    let again = world.pixel(&["install", source.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
    assert!(
        stderr(&again).contains("run `pixel uninstall tool` first"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn install_name_picks_one_plugin_out_of_a_repository() {
    let world = World::new("named");
    let repo = world.scratch.join("many");
    plugin_dir(&repo.join("a"), "one", "");
    plugin_dir(&repo.join("b"), "two", "");
    let ambiguous = world.pixel(&["install", repo.to_str().unwrap()]);
    assert_eq!(ambiguous.status.code(), Some(1), "{ambiguous:?}");
    assert!(
        stderr(&ambiguous).contains("--name"),
        "{}",
        stderr(&ambiguous)
    );
    let named = world.pixel(&["install", repo.to_str().unwrap(), "--name", "two"]);
    assert!(
        ok(&named).starts_with("installed plugin two 1.4.2 in "),
        "{named:?}"
    );
    assert!(world.user_plugins().join("two").is_dir());
    assert!(!world.user_plugins().join("one").exists());
}

#[test]
fn the_retired_install_and_uninstall_flags_are_gone() {
    let world = World::new("old-flags");
    let out = world.pixel(&["uninstall", "--wrappers-only"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = world.pixel(&["install", "--shell", "zsh"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// A plugin named `git` whose manifest lists the commands one binary serves.
fn multi_plugin(world: &World, name: &str, commands: &[&str]) -> PathBuf {
    let list = commands
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    plugin_dir(&world.user_plugins(), name, &format!("commands = [{list}]"))
}

#[test]
fn a_listed_command_runs_the_plugin_with_the_command_word_first() {
    let world = World::new("multi");
    multi_plugin(&world, "git", &["fast-forward", "plan-rollback"]);
    let out = world.pixel(&["fast-forward", "origin", "--json"]);
    assert!(
        ok(&out).starts_with("args=fast-forward origin --json\napi=1\n"),
        "{out:?}"
    );
    let second = world.pixel(&["plan-rollback", "fail"]);
    assert!(
        ok(&second).starts_with("args=plan-rollback fail\n"),
        "{second:?}"
    );
    // The pre-rename spelling of a listed command reaches it too.
    let alias = world.pixel(&["update", "x"]);
    assert!(ok(&alias).starts_with("args=fast-forward x\n"), "{alias:?}");
    // The plugin's own name is not a command, and an unlisted moved one hints.
    let by_name = world.pixel(&["git"]);
    assert_eq!(by_name.status.code(), Some(2), "{by_name:?}");
    assert_eq!(
        stderr(&world.pixel(&["squash-branch"])),
        moved_hint("squash-branch")
    );
}

#[test]
fn list_shows_each_plugins_commands_and_a_single_name_plugin_still_works() {
    let world = World::new("multi-list");
    multi_plugin(&world, "git", &["fast-forward", "plan-rollback"]);
    let source = world.source("tool", "");
    ok(&world.pixel(&["install", source.to_str().unwrap()]));
    let listed = ok(&world.pixel(&["plugin", "list"]));
    let rows: Vec<Vec<&str>> = listed
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert!(
        rows.contains(&vec![
            "git",
            "user",
            "1.4.2",
            "1",
            "command",
            "-",
            "-",
            "fast-forward,plan-rollback"
        ]),
        "{listed}"
    );
    assert!(
        rows.contains(&vec![
            "tool", "user", "1.4.2", "1", "command", "-", "-", "-"
        ]),
        "{listed}"
    );
    assert!(ok(&world.pixel(&["tool", "a"])).starts_with("args=a\n"));
}

#[test]
fn two_plugins_listing_one_command_fail_naming_both_and_list_flags_them() {
    let world = World::new("multi-conflict");
    multi_plugin(&world, "git", &["fast-forward"]);
    multi_plugin(&world, "other", &["fast-forward"]);
    let out = world.pixel(&["fast-forward"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let message = stderr(&out);
    assert!(
        message
            .contains("command `fast-forward` is provided by both plugin `git` and plugin `other`"),
        "{message}"
    );
    assert_eq!(stdout(&out), "");
    let listed = ok(&world.pixel(&["plugin", "list"]));
    assert!(
        listed.contains("  ! git: command `fast-forward` is also provided by `other`"),
        "{listed}"
    );
    assert!(
        listed.contains("  ! other: command `fast-forward` is also provided by `git`"),
        "{listed}"
    );
}

#[test]
fn install_refuses_a_plugin_that_shares_a_command_with_an_installed_one() {
    let world = World::new("multi-install");
    let first = plugin_dir(
        &world.scratch.join("src"),
        "git",
        "commands = [\"fast-forward\"]",
    );
    ok(&world.pixel(&["install", first.to_str().unwrap()]));
    let second = plugin_dir(
        &world.scratch.join("src"),
        "other",
        "commands = [\"fast-forward\"]",
    );
    let out = world.pixel(&["install", second.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("`git` and plugin `other`"),
        "{}",
        stderr(&out)
    );
    assert!(!world.user_plugins().join("other").exists());
}
