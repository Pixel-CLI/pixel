// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel plugin …` and `pixel <plugin> …` through the real binary, with a
//! scratch HOME, a scratch git repository and a PATH of its own.

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
        pixel_command()
            .current_dir(self.repo())
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.scratch.join("bin").display()),
            )
            .env("PIXEL_METRICS", "0")
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
fn an_added_plugin_runs_with_the_args_the_environment_and_its_exit_code() {
    let world = World::new("run");
    let source = world.source("tool", "");
    let added = world.pixel(&["plugin", "add", source.to_str().unwrap()]);
    assert!(
        ok(&added).starts_with("installed plugin tool 1.4.2 in "),
        "{added:?}"
    );

    let listed = ok(&world.pixel(&["plugin", "list"]));
    let row = listed.lines().find(|l| l.starts_with("tool")).unwrap();
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>(),
        ["tool", "user", "1.4.2", "1", "command", "-", "-"]
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
                == ["tool", "repo", "1.4.2", "1", "command", "-", "yes"]),
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
    let added = ok(&world.pixel(&["plugin", "add", source.to_str().unwrap()]));
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
                == ["net", "user", "1.4.2", "1", "command", "yes", "-"]),
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
                == ["hello", "path", "-", "1", "command", "-", "-"]),
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
fn remove_deletes_the_plugin() {
    let world = World::new("remove");
    let source = world.source("tool", "");
    ok(&world.pixel(&["plugin", "add", source.to_str().unwrap()]));
    assert_eq!(
        ok(&world.pixel(&["plugin", "remove", "tool"])),
        "removed plugin tool\n"
    );
    assert!(!world.user_plugins().join("tool").exists());
    let gone = world.pixel(&["tool"]);
    assert_eq!(
        gone.status.code(),
        Some(2),
        "back to clap's unknown-command error: {gone:?}"
    );
    let missing = world.pixel(&["plugin", "remove", "tool"]);
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
fn add_reports_a_bad_source_and_keeps_the_credentials_of_a_url_out_of_the_log() {
    let world = World::new("addbad");
    let out = world.pixel(&["plugin", "add", "/definitely/not/here"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("neither a directory nor a git URL"));

    let url = "file://user:ghp_topsecret@/definitely/not/here";
    let out = world.pixel(&["plugin", "add", url]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(!stderr(&out).contains("ghp_topsecret"), "{}", stderr(&out));
    let log =
        std::fs::read_to_string(world.repo().join(".pixel/actions.jsonl")).unwrap_or_default();
    assert!(!log.contains("ghp_topsecret"), "{log}");
}
