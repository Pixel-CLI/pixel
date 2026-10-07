// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel doctor`'s exit contract: 0 when no check reaches `--fail-on`, 1
//! when one does, 2 when the checks could not run. Agents and CI gate on the
//! code alone, so a red report that exits 0 reads as healthy.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, pixel_command};

fn doctor(home: &Path, repo: &Path, args: &[&str]) -> Output {
    pixel_command()
        .arg("doctor")
        .arg(repo)
        .args(["--shell", "zsh"])
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap()
}

fn fixture(tag: &str) -> (Scratch, Scratch) {
    let home = Scratch::for_test("doctor-cli-home", tag);
    let repo = Scratch::for_test("doctor-cli-repo", tag);
    (home, repo)
}

#[test]
fn doctor_should_exit_1_with_the_fix_when_a_check_is_red() {
    let (home, repo) = fixture("red");
    let out = doctor(&home, &repo, &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("pixel doctor: ran "), "{text}");
    // A bare home has no Claude task hooks: red, with the install as the fix.
    assert!(
        text.contains(&format!(
            "  [red] install.claude-hooks: {} not found — run `pixel install`\n",
            home.join(".claude/settings.json").display()
        )),
        "{text}"
    );
    // No Pixel prompt is deployed any more, so its absence is healthy.
    assert!(!text.contains("install.agent-prompt"), "{text}");
    assert!(
        text.contains("    fix: pixel install --shell zsh\n"),
        "{text}"
    );
    assert!(
        !text.contains("[green]"),
        "green checks stay in the tally: {text}"
    );
}

/// `--json` changes the format, not the verdict: the report is still whole
/// and parsable, and the exit code still says it is unhealthy.
#[test]
fn doctor_json_should_keep_the_exit_code_and_carry_the_fix() {
    let (home, repo) = fixture("json");
    let out = doctor(&home, &repo, &["--json", "--skip", "install.pi-prompt"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["summary"]["skipped"], 1, "{report}");
    let checks = report["checks"].as_array().unwrap();
    let find = |id: &str| checks.iter().find(|c| c["id"] == id);
    assert!(find("install.pi-prompt").is_none(), "--skip left it out");
    assert_eq!(
        find("install.claude-hooks").unwrap()["fix"],
        "pixel install --shell zsh",
        "the home install reads the profile of the shell the check read"
    );
    // A home with no deployed prompt is green, so the check carries no fix.
    let prompt = find("install.agent-prompt").unwrap();
    assert_eq!(prompt["status"], "green", "{prompt}");
    assert!(prompt.get("fix").is_none(), "{prompt}");
    // The repo checks judge the path given on the command line, and their
    // fix names it.
    let index = find("index.freshness").expect("repo checks ran");
    assert_eq!(
        index["fix"],
        format!("pixel prepare-repo '{}'", repo.display()),
        "{index}"
    );
    // The checks that judged a deployed Pixel prompt are retired with it.
    for retired in ["install.subagent-prompt", "rule.parity", "rule.scenarios"] {
        assert!(find(retired).is_none(), "{retired}: {report}");
    }
    // `--shell zsh` decides which profile the wrapper check reads first.
    let profiles = &find("install.legacy-wrappers").unwrap()["detail"]["profiles_checked"];
    assert!(
        profiles[0].as_str().unwrap().ends_with(".zshrc"),
        "{profiles}"
    );
}

#[test]
fn doctor_should_exit_0_when_the_selected_checks_are_green() {
    let (home, repo) = fixture("only");
    let out = doctor(&home, &repo, &["--only", "binary.path"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("pixel doctor: ran 1 check(s), skipped "),
        "{text}"
    );
}

/// A yellow finding passes the default gate and fails `--fail-on yellow`,
/// the posture for "confirm everything is green".
#[test]
fn doctor_fail_on_yellow_should_turn_a_yellow_check_into_exit_1() {
    let (home, repo) = fixture("yellow");
    // No daemon serves the scratch repository: the epistemics probe is
    // skipped, which is yellow.
    let only = ["--only", "daemon.epistemics"];
    let default = doctor(&home, &repo, &only);
    assert_eq!(default.status.code(), Some(0), "{default:?}");
    assert!(
        String::from_utf8_lossy(&default.stdout)
            .contains("[yellow] daemon.epistemics: no daemon running — epistemics probe skipped"),
        "{default:?}"
    );
    let strict = doctor(&home, &repo, &[only[0], only[1], "--fail-on", "yellow"]);
    assert_eq!(strict.status.code(), Some(1), "{strict:?}");
}

#[test]
fn doctor_should_exit_2_on_an_unknown_check_id() {
    let (home, repo) = fixture("unknown");
    let out = doctor(&home, &repo, &["--only", "install.shell-wrappers"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "no partial report: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("unknown doctor check `install.shell-wrappers`"),
        "{out:?}"
    );
}

#[test]
fn doctor_list_should_name_every_check_with_its_fix() {
    let (home, repo) = fixture("list");
    let out = doctor(&home, &repo, &["--list"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        pixel_install::doctor::CHECKS.len(),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("facts.freshness")
            && l.ends_with("pixel build-index --history {root}")),
        "{text}"
    );

    let json = doctor(&home, &repo, &["--list", "--json"]);
    let catalogue: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(catalogue[0]["id"], "binary.path", "{catalogue}");
    assert_eq!(catalogue[0]["fix"], serde_json::Value::Null);
}

/// No release deploys a Pixel prompt any more, so a copy an earlier
/// `pixel install` left is retired whatever its content, the bundled text
/// included: every ordinary command of an installed `pixel` names it in one
/// stderr line, while `doctor`, which reports it itself, a home where nothing
/// was ever deployed, and the developer builds (`pixel-dev`, a binary run
/// from `target/`, #549) stay quiet.
#[test]
fn a_retired_deployed_prompt_is_named_by_ordinary_commands_but_not_by_doctor() {
    let (home, repo) = fixture("stale-prompt");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let install = |name: &str| {
        let installed = bin.join(name);
        std::fs::hard_link(env!("CARGO_BIN_EXE_pixel"), &installed)
            .or_else(|_| std::fs::copy(env!("CARGO_BIN_EXE_pixel"), &installed).map(drop))
            .unwrap();
        installed
    };
    let run = |exe: &std::path::Path| {
        std::process::Command::new(exe)
            .args(["action-log", "--limit", "1"])
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("HOME", &*home)
            .current_dir(crate::support::neutral_cwd())
            .output()
            .unwrap()
    };
    let installed = install("pixel");
    let ordinary = || run(&installed);
    let never_installed = ordinary();
    assert!(never_installed.status.success(), "{never_installed:?}");
    let stderr = String::from_utf8_lossy(&never_installed.stderr).into_owned();
    assert!(!stderr.contains("pixel install"), "{stderr}");

    let deployed = home.join(".local/share/pixel");
    std::fs::create_dir_all(&deployed).unwrap();
    let note = format!(
        "note: agent-prompt.md deployed by an earlier `pixel install` is retired in this pixel ({}) — run `pixel install` to remove it",
        env!("CARGO_PKG_VERSION")
    );
    // An older release's text and the text this release still bundles are
    // both retired: the note does not compare contents.
    for content in [
        "# an older release's prompt\n",
        include_str!("../../../pixel-install/assets/pixel-agent-prompt.md"),
    ] {
        std::fs::write(deployed.join("agent-prompt.md"), content).unwrap();
        let warned = ordinary();
        assert!(warned.status.success(), "{warned:?}");
        let stderr = String::from_utf8_lossy(&warned.stderr).into_owned();
        let notes: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with("note: agent-prompt.md"))
            .collect();
        assert_eq!(notes, [note.as_str()], "{stderr}");
    }
    std::fs::write(
        deployed.join("agent-prompt.md"),
        "# an older release's prompt\n",
    )
    .unwrap();

    // The same binary as a developer build: installed as the `pixel-dev`
    // side build, or run from cargo's `target/` as built. The deployed
    // prompts are the managed pixel's, and the `pixel install` the note
    // names would move every repository's hooks onto this build.
    for developer_build in [
        install("pixel-dev"),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_pixel")),
    ] {
        let quiet = run(&developer_build);
        assert!(quiet.status.success(), "{quiet:?}");
        let stderr = String::from_utf8_lossy(&quiet.stderr).into_owned();
        assert!(
            !stderr.contains("pixel install"),
            "{}: {stderr}",
            developer_build.display()
        );
    }

    let report = doctor(&home, &repo, &["--only", "install.agent-prompt"]);
    let stderr = String::from_utf8_lossy(&report.stderr).into_owned();
    assert!(!stderr.contains("note: agent-prompt.md"), "{stderr}");
    let text = String::from_utf8_lossy(&report.stdout).into_owned();
    // No host reads a deployed prompt any more: doctor calls the copy an
    // earlier release left retired, and `pixel install` removes it.
    assert!(
        text.contains(&format!(
            "  [red] install.agent-prompt: retired Pixel prompt file(s) remain: {} — run `pixel install` to remove them\n",
            deployed.join("agent-prompt.md").display()
        )),
        "{text}"
    );
    assert_eq!(report.status.code(), Some(1), "{report:?}");
}

/// A `pixel-dev` `--fix` leaves every home-install repair to the managed
/// pixel: an empty home stays empty, the red `install.claude-hooks` keeps its
/// `fix:` line and colour, and stderr says which command was left.
#[test]
fn a_side_build_doctor_fix_should_leave_the_home_install_alone() {
    let (home, repo) = fixture("side-build-fix");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let side_build = bin.join("pixel-dev");
    std::fs::hard_link(env!("CARGO_BIN_EXE_pixel"), &side_build)
        .or_else(|_| std::fs::copy(env!("CARGO_BIN_EXE_pixel"), &side_build).map(drop))
        .unwrap();
    let out = std::process::Command::new(&side_build)
        .arg("doctor")
        .arg(&*repo)
        .args(["--shell", "zsh", "--only", "install.claude-hooks", "--fix"])
        .env("HOME", &*home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env_remove("XDG_CONFIG_HOME")
        .current_dir(crate::support::neutral_cwd())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stderr.contains("pixel doctor --fix: left pixel install --shell zsh to the managed pixel"),
        "{stderr}"
    );
    assert!(!stderr.contains("running pixel install"), "{stderr}");
    for written in [".claude", ".codex", ".local/share/pixel", ".pi"] {
        assert!(!home.join(written).exists(), "{written} written: {stderr}");
    }
    assert!(
        stdout.contains(&format!(
            "  [red] install.claude-hooks: {} not found — run `pixel install`\n    fix: pixel install --shell zsh\n",
            home.join(".claude/settings.json").display()
        )),
        "{stdout}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "the unrepaired check stays red: {out:?}"
    );
}

/// `doctor` with `XDG_CONFIG_HOME` removed, so the `pixel install` a `--fix`
/// runs writes nothing outside the scratch home.
fn doctor_fixing(home: &Path, repo: &Path, args: &[&str]) -> Output {
    pixel_command()
        .arg("doctor")
        .arg(repo)
        .args(["--shell", "zsh"])
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env_remove("XDG_CONFIG_HOME")
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap()
}

/// The reason `--fix` exists: two checks share `pixel install`, which runs
/// once, with the `--shell` the checks read, and the verdict and exit code
/// come from the checks re-run afterwards, not from the first pass. The
/// prompts an earlier release deployed are red, and the install removes them.
#[test]
fn doctor_fix_should_run_a_shared_repair_once_and_report_the_rerun() {
    let (home, repo) = fixture("fix");
    std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
    let deployed = home.join(".local/share/pixel");
    std::fs::create_dir_all(&deployed).unwrap();
    std::fs::write(
        deployed.join("agent-prompt.md"),
        "# an older release's prompt\n",
    )
    .unwrap();
    std::fs::write(
        deployed.join("subagent-prompt.md"),
        "# an older release's sub-agent prompt\n",
    )
    .unwrap();
    let only = [
        "--only",
        "install.agent-prompt",
        "--only",
        "install.pi-impact",
    ];
    let before = doctor_fixing(&home, &repo, &only);
    assert_eq!(before.status.code(), Some(1), "{before:?}");
    let text = String::from_utf8(before.stdout).unwrap();
    assert!(
        text.ends_with("rerun with `--fix` to apply the 1 repair command(s) above\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  [red] install.agent-prompt: retired Pixel prompt file(s) remain: {}, {} — run `pixel install` to remove them\n",
            deployed.join("agent-prompt.md").display(),
            deployed.join("subagent-prompt.md").display()
        )),
        "{text}"
    );
    assert!(
        text.contains("    fix: pixel install --shell zsh\n"),
        "{text}"
    );

    let fixed = doctor_fixing(&home, &repo, &[only[0], only[1], only[2], only[3], "--fix"]);
    assert_eq!(fixed.status.code(), Some(0), "{fixed:?}");
    let text = String::from_utf8(fixed.stdout).unwrap();
    assert_eq!(
        text,
        [
            "pixel doctor --fix: ran 1 repair(s) — 1 fixed, 0 not converged, 0 failed",
            "  [fixed] pixel install --shell zsh (install.agent-prompt, install.pi-impact)",
            "pixel doctor: ran 2 check(s), skipped 28 — 2 green, 0 yellow, 0 red",
            "",
        ]
        .join("\n")
    );
    let stderr = String::from_utf8_lossy(&fixed.stderr);
    assert_eq!(
        stderr.matches("pixel doctor --fix: running ").count(),
        1,
        "{stderr}"
    );
    for retired in ["agent-prompt.md", "subagent-prompt.md"] {
        assert!(
            !deployed.join(retired).exists(),
            "`pixel install` removes {retired}"
        );
    }
}

/// `--json` carries each repair's verdict beside the re-run report; a repair
/// that fails is `failed` with the step's error, its check stays flagged, and
/// the exit code still says unhealthy.
#[test]
fn doctor_fix_json_should_report_a_repair_that_failed() {
    let (home, repo) = fixture("fix-json");
    // `~/.claude` is a file: `pixel install` cannot write its settings.
    std::fs::write(home.join(".claude"), "not a directory\n").unwrap();
    let out = doctor_fixing(
        &home,
        &repo,
        &["--only", "install.claude-hooks", "--fix", "--json"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let repairs = report["repairs"].as_array().expect("repairs");
    assert_eq!(repairs.len(), 1, "{report}");
    assert_eq!(repairs[0]["command"], "pixel install --shell zsh");
    assert_eq!(repairs[0]["status"], "failed", "{report}");
    assert!(
        repairs[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("`pixel install --shell zsh` failed (exit status: "),
        "{report}"
    );
    assert_eq!(
        repairs[0]["still_flagged"],
        serde_json::json!(["install.claude-hooks"])
    );
    assert_eq!(report["checks"][0]["status"], "red", "{report}");
}

/// Without `--fix` nothing runs and the JSON shape is unchanged: the retired
/// prompt that makes the check red is still there, byte for byte.
#[test]
fn doctor_without_fix_should_leave_the_home_untouched() {
    let (home, repo) = fixture("no-fix");
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
    std::fs::write(&prompt, "# an older release's prompt\n").unwrap();
    let out = doctor_fixing(&home, &repo, &["--only", "install.agent-prompt", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report.get("repairs").is_none(), "{report}");
    assert_eq!(report["checks"][0]["status"], "red", "{report}");
    assert_eq!(
        report["checks"][0]["fix"], "pixel install --shell zsh",
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(&prompt).unwrap(),
        "# an older release's prompt\n"
    );
    assert!(!home.join(".claude").exists(), "no install ran");
}

#[test]
fn doctor_fix_should_say_so_when_nothing_can_be_repaired_automatically() {
    let (home, repo) = fixture("fix-none");
    let out = doctor_fixing(&home, &repo, &["--only", "binary.path", "--fix"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("pixel doctor --fix: nothing to repair automatically\n"),
        "{text}"
    );
}

/// History is built on the first history command. A repository that never
/// ran one is healthy, and checking it must not create the db: the old red
/// verdict on an empty db made `doctor --fix` run a full history build that
/// nobody had asked for.
#[test]
fn doctor_should_pass_a_repo_whose_history_was_never_built_and_leave_it_unbuilt() {
    let (home, repo) = fixture("history-not-built");
    std::fs::write(repo.join("lib.rs"), "pub fn seed() {}\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-q", "-m", "seed"],
    ] {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&*repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
    let out = doctor(
        &home,
        &repo,
        &["--only", "facts.freshness", "--json", "--fail-on", "yellow"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = &report["checks"][0];
    assert_eq!(check["id"], "facts.freshness", "{report}");
    assert_eq!(check["status"], "green", "{report}");
    assert_eq!(check["detail"]["present"], false, "{report}");
    assert!(
        !repo.join(".pixel").join("history.db").exists(),
        "the check must not create history.db"
    );
}

/// A fake shell makes the probe hermetic: the login shells of a developer
/// machine carry a system-level PATH (macOS `path_helper`) the test cannot
/// reset, but `--shell` accepts any executable, and the check runs it with
/// `-l -c "command -v pixel"`/`which pixel` exactly as it would the real
/// one. The fixture rejects any other invocation, so a probe that drops the
/// login flag or changes the query fails the test instead of returning the
/// programmed outcome anyway; `name` also picks the dialect (`fish` is the
/// Fish branch).
fn fake_shell(tag: &str, name: &str, lookup: &str, script: &str) -> Scratch {
    let dir = Scratch::for_test("doctor-cli-shell", tag);
    let shell = dir.join(name);
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\n\
             if [ \"$#\" -ne 3 ] || [ \"$1\" != \"-l\" ] || [ \"$2\" != \"-c\" ] || [ \"$3\" != \"{lookup}\" ]; then\n\
             \techo \"unexpected lookup args: $*\" >&2\n\
             \texit 2\n\
             fi\n\
             {script}\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn shell_path_doctor(shell: &Path, repo: &Path) -> Output {
    pixel_command()
        .arg("doctor")
        .arg(repo)
        .args(["--only", "binary.shell-path", "--json"])
        .arg("--shell")
        .arg(shell.as_os_str())
        .env("CODEX_HOME", std::env::temp_dir().join("doctor-cli-codex"))
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap()
}

/// The check reports what the shell resolves, so an agent's shells and the
/// doctor agree on where `pixel` comes from.
#[test]
fn shell_path_should_be_green_when_the_shell_resolves_pixel() {
    let (_, repo) = fixture("shell-path-green");
    let shell = fake_shell(
        "shell-path-green",
        "fake-zsh",
        "command -v pixel",
        "echo /fake/bin/pixel\nexit 0",
    );
    let out = shell_path_doctor(shell.join("fake-zsh").as_path(), &repo);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "green", "{report}");
    assert_eq!(report["summary"]["green"], 1, "{report}");
    assert_eq!(
        report["checks"][0]["detail"]["resolved"], "/fake/bin/pixel",
        "{report}"
    );
}

/// The Fish branch asks `which pixel`, not the POSIX `command -v`; a fixture
/// named `fish` exercises it, and its argument guard fails the probe if the
/// POSIX query is sent instead.
#[test]
fn shell_path_should_use_the_fish_lookup_for_a_fish_shell() {
    let (_, repo) = fixture("shell-path-fish");
    let shell = fake_shell(
        "shell-path-fish",
        "fish",
        "which pixel",
        "echo /fake/bin/pixel\nexit 0",
    );
    let out = shell_path_doctor(shell.join("fish").as_path(), &repo);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "green", "{report}");
    assert_eq!(
        report["checks"][0]["detail"]["resolved"], "/fake/bin/pixel",
        "{report}"
    );
}

/// A shell that cannot resolve `pixel` is the silent failure agents live
/// with: every pixel command is "command not found" inside the harness
/// while outside it works. Yellow — the binary itself runs, and no
/// catalogue command repairs a PATH — and `--fail-on yellow` makes it a
/// gate.
#[test]
fn shell_path_should_flag_a_shell_that_cannot_resolve_pixel() {
    let (_, repo) = fixture("shell-path-yellow");
    let shell = fake_shell(
        "shell-path-yellow",
        "fake-zsh",
        "command -v pixel",
        "exit 1",
    );
    let out = shell_path_doctor(shell.join("fake-zsh").as_path(), &repo);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "yellow", "{report}");
    assert_eq!(report["summary"]["yellow"], 1, "{report}");

    let strict = pixel_command()
        .arg("doctor")
        .arg(&*repo)
        .arg("--shell")
        .arg(shell.join("fake-zsh").as_os_str())
        .args(["--only", "binary.shell-path", "--fail-on", "yellow"])
        .env("CODEX_HOME", std::env::temp_dir().join("doctor-cli-codex"))
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1), "{strict:?}");
}

/// A spawn failure of the shell itself is a different failure from "the
/// shell could not resolve pixel": it is red, not yellow.
#[test]
fn shell_path_should_go_red_when_the_shell_cannot_run() {
    let (_, repo) = fixture("shell-path-red");
    let shell_dir = Scratch::for_test("doctor-cli-shell", "shell-path-red");
    let out = shell_path_doctor(shell_dir.join("no-such-shell").as_path(), &repo);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "red", "{report}");
}

/// The `"hooks"` object of a Devin config: one group per event, each holding
/// the commands given for it.
fn devin_hooks(entries: &[(&str, &str)]) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for (event, command) in entries {
        hooks.insert(
            (*event).to_string(),
            serde_json::json!([{"hooks":[{"command":command,"type":"command"}]}]),
        );
    }
    serde_json::Value::Object(hooks)
}

/// Devin keeps its native retrieval. The check is green with no
/// `~/.config/devin`, green on a config that holds only foreign hooks naming
/// the same verbs, and red while retired Pixel hooks remain. `pixel install`
/// removes those retired hooks, registers current task hooks, and keeps the
/// foreign hooks intact.
#[test]
fn doctor_devin_hooks_is_green_without_a_pixel_hook_and_red_until_install_removes_one() {
    let (home, repo) = fixture("devin-hooks-absent");
    let out = doctor(&home, &repo, &["--only", "install.devin-hooks", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "green", "{report}");

    let (home, repo) = fixture("devin-hooks-retired");
    std::fs::create_dir_all(home.join(".config/devin")).unwrap();
    let config = home.join(".config/devin/config.json");
    let foreign = [
        ("SessionStart", "/opt/foreign run-hook session-start"),
        ("UserPromptSubmit", "/opt/foreign run-hook prompt-submit"),
        ("PostCompaction", "/opt/foreign run-hook post-compaction"),
    ];
    std::fs::write(
        &config,
        serde_json::json!({"hooks": devin_hooks(&foreign)}).to_string(),
    )
    .unwrap();
    let out = doctor(&home, &repo, &["--only", "install.devin-hooks", "--json"]);
    assert_eq!(out.status.code(), Some(0), "foreign hooks only: {out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["checks"][0]["status"], "green", "{report}");
    assert_eq!(
        report["checks"][0]["summary"],
        "no retired Pixel hook; the agent keeps its own tools"
    );

    // An earlier release's Pixel hooks beside the foreign ones: red.
    let retired = [
        (
            "SessionStart",
            "/usr/local/bin/pixel run-hook session-start --provider devin",
        ),
        (
            "PreToolUse",
            "/usr/local/bin/pixel run-hook guard --provider devin",
        ),
    ];
    let mut hooks = devin_hooks(&foreign);
    let retired_hooks = devin_hooks(&retired);
    hooks["SessionStart"]
        .as_array_mut()
        .unwrap()
        .extend(retired_hooks["SessionStart"].as_array().unwrap().clone());
    hooks["PreToolUse"] = retired_hooks["PreToolUse"].clone();
    std::fs::write(&config, serde_json::json!({"hooks": hooks}).to_string()).unwrap();
    let out = doctor(&home, &repo, &["--only", "install.devin-hooks"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!(
            "  [red] install.devin-hooks: retired Pixel hooks remain in {} — run `pixel install` to remove them\n    fix: pixel install --shell zsh\n",
            config.display()
        )),
        "{text}"
    );

    // The repair: `pixel install` takes Pixel's hooks out and keeps the rest.
    let out = doctor_fixing(&home, &repo, &["--only", "install.devin-hooks", "--fix"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("  [fixed] pixel install --shell zsh (install.devin-hooks)\n"),
        "{text}"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    let mut commands: Vec<(String, String)> = Vec::new();
    for (event, groups) in value["hooks"].as_object().unwrap() {
        for group in groups.as_array().unwrap() {
            for hook in group["hooks"].as_array().unwrap() {
                commands.push((event.clone(), hook["command"].as_str().unwrap().to_string()));
            }
        }
    }
    commands.sort();
    let mut expected: Vec<(String, String)> = foreign
        .iter()
        .map(|(event, command)| ((*event).to_string(), (*command).to_string()))
        .collect();
    expected.sort();
    assert!(
        expected.iter().all(|hook| commands.contains(hook)),
        "foreign hooks were not preserved: {value}"
    );
    assert!(
        !commands
            .iter()
            .any(|(_, command)| command.contains("/usr/local/bin/pixel run-hook")),
        "retired Pixel hooks remain: {value}"
    );
}
