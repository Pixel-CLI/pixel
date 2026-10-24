// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::path::Path;

use super::{
    CHECKS, CheckSpec, CheckStatus, DoctorCheck, DoctorReport, Repair, RepairOutcome, RepairStatus,
    capped, judge_repair, names_check, one_line, render_catalogue, render_repairs, repair_plan,
    run_repair, selected, shell_path_check, spec, split_home_repairs, validate_selection,
};
use crate::InstallError;

fn finding(id: &str, status: CheckStatus, reason: Option<&str>, fix: Option<&str>) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        status,
        required: true,
        duration_ms: 0,
        summary: format!("{id} summary"),
        reason: reason.map(Into::into),
        detail: None,
        fix: fix.map(Into::into),
        repair: None,
    }
}

/// A flagged check carrying the catalogue repair `steps`.
fn repairable(id: &str, status: CheckStatus, fix: &str, steps: &[&[&str]]) -> DoctorCheck {
    DoctorCheck {
        repair: Some(argv(steps)),
        ..finding(id, status, None, Some(fix))
    }
}

fn argv(steps: &[&[&str]]) -> Vec<Vec<String>> {
    steps.iter().map(|step| ids(step)).collect()
}

fn report(checks: Vec<DoctorCheck>, skipped: usize) -> DoctorReport {
    super::doctor_report(Path::new("/bin/pixel"), Path::new("/home"), checks, skipped)
}

#[test]
fn doctor_report_counts_each_status_and_fails_only_on_red() {
    let passing = report(
        vec![
            finding("a", CheckStatus::Green, None, None),
            finding("b", CheckStatus::Green, None, None),
            finding("c", CheckStatus::Yellow, None, None),
        ],
        4,
    );
    assert!(passing.ok);
    assert_eq!(
        (
            passing.summary.green,
            passing.summary.yellow,
            passing.summary.red,
            passing.summary.skipped
        ),
        (2, 1, 0, 4)
    );
    assert_eq!(passing.executable_path, "/bin/pixel");
    assert_eq!(passing.home, "/home");
    let failing = report(vec![finding("a", CheckStatus::Red, None, None)], 0);
    assert!(!failing.ok);
    assert_eq!(
        (
            failing.summary.green,
            failing.summary.yellow,
            failing.summary.red
        ),
        (0, 0, 1)
    );
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(ToString::to_string).collect()
}

/// The exit code gates CI and the agent loop: a check exactly at the
/// threshold must fail it, a milder one must not.
#[test]
fn fails_should_trip_at_the_threshold_and_not_below() {
    let green = report(vec![finding("a", CheckStatus::Green, None, None)], 0);
    let yellow = report(
        vec![
            finding("a", CheckStatus::Green, None, None),
            finding("b", CheckStatus::Yellow, None, None),
        ],
        0,
    );
    let red = report(
        vec![finding("c", CheckStatus::Red, Some("broken"), None)],
        0,
    );
    assert!(!green.fails(CheckStatus::Yellow));
    assert!(!green.fails(CheckStatus::Red));
    assert!(yellow.fails(CheckStatus::Yellow));
    assert!(
        !yellow.fails(CheckStatus::Red),
        "yellow alone passes the default gate"
    );
    assert!(red.fails(CheckStatus::Red));
    assert!(red.fails(CheckStatus::Yellow));
}

#[test]
fn check_status_should_display_its_serialized_name() {
    for status in [CheckStatus::Green, CheckStatus::Yellow, CheckStatus::Red] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::Value::String(status.to_string())
        );
    }
    assert_eq!(CheckStatus::Yellow.to_string(), "yellow");
}

/// The terminal form lists what needs attention, red first, each with the
/// command that repairs it, and hides the green checks behind the tally.
#[test]
fn report_display_should_list_red_before_yellow_with_fix_lines() {
    let text = report(
        vec![
            finding("binary.path", CheckStatus::Green, None, None),
            finding(
                "facts.freshness",
                CheckStatus::Yellow,
                None,
                Some("pixel build-index --history '/r'"),
            ),
            finding(
                "install.agent-prompt",
                CheckStatus::Red,
                Some("stale\n  prompt"),
                Some("pixel setup"),
            ),
            finding("daemon.epistemics", CheckStatus::Yellow, None, None),
        ],
        2,
    )
    .to_string();
    let expected = [
        "pixel doctor: ran 4 check(s), skipped 2 — 1 green, 2 yellow, 1 red",
        "  [red] install.agent-prompt: stale prompt",
        "    fix: pixel setup",
        "  [yellow] facts.freshness: facts.freshness summary",
        "    fix: pixel build-index --history '/r'",
        "  [yellow] daemon.epistemics: daemon.epistemics summary",
        "",
    ]
    .join("\n");
    assert_eq!(text, expected);
}

#[test]
fn report_display_should_omit_the_skip_count_when_nothing_was_skipped() {
    let text = report(
        vec![finding("binary.path", CheckStatus::Green, None, None)],
        0,
    )
    .to_string();
    assert_eq!(
        text,
        "pixel doctor: ran 1 check(s) — 1 green, 0 yellow, 0 red\n"
    );
}

#[test]
fn render_catalogue_should_align_ids_and_mark_checks_without_a_fix() {
    let text = render_catalogue(&[
        CheckSpec {
            id: "binary.path",
            fix: None,
        },
        CheckSpec {
            id: "index.freshness",
            fix: Some("pixel prepare-repo {root}"),
        },
    ]);
    assert_eq!(
        text,
        [
            "binary.path      -",
            "index.freshness  pixel prepare-repo {root}",
            ""
        ]
        .join("\n")
    );
}

/// `--only`/`--skip` address checks by id, so two entries sharing one
/// would make a selection ambiguous.
#[test]
fn checks_should_have_unique_ids() {
    let mut seen = std::collections::HashSet::new();
    for check in CHECKS {
        assert!(seen.insert(check.id), "duplicate id {}", check.id);
    }
}

#[test]
fn spec_should_return_the_catalogue_entry() {
    assert_eq!(
        spec("history.freshness").fix,
        Some("pixel prepare --history {root}")
    );
}

#[test]
#[should_panic(expected = "doctor check `nope` is missing from CHECKS")]
fn spec_should_panic_on_an_uncatalogued_id() {
    let _ = spec("nope");
}

#[test]
fn selected_should_keep_only_listed_ids_and_drop_skipped_ones() {
    assert!(
        selected(&[], &[], "binary.path"),
        "no selection runs everything"
    );
    assert!(selected(&ids(&["binary.path"]), &[], "binary.path"));
    assert!(!selected(&ids(&["binary.path"]), &[], "daemon.health"));
    assert!(!selected(&[], &ids(&["daemon.health"]), "daemon.health"));
    assert!(selected(&[], &ids(&["daemon.health"]), "binary.path"));
}

/// A side build (`pixel-dev`) leaves the home install to the managed
/// `pixel` with `--skip 'install.*'`: the group must reach every
/// `install.` check, and nothing else, not even an id that merely starts
/// with the same letters.
#[test]
fn selected_should_treat_a_group_selector_as_every_check_of_that_group() {
    let expected_home = [
        "install.claude-hooks",
        "install.codex-hooks",
        "install.codex-hook-review",
        "install.devin",
        "install.gemini",
        "install.antigravity",
        "install.opencode",
        "install.pi-package",
        "install.skill",
    ];
    let home: Vec<&str> = CHECKS
        .iter()
        .map(|spec| spec.id)
        .filter(|id| id.starts_with("install."))
        .collect();
    assert_eq!(
        home, expected_home,
        "the exact install group in the catalogue"
    );
    let skip = ids(&["install.*"]);
    for spec in CHECKS {
        assert_eq!(
            selected(&[], &skip, spec.id),
            !home.contains(&spec.id),
            "{}",
            spec.id
        );
    }
    let only = ids(&["repo.*"]);
    assert!(selected(&only, &[], "repo.claude-hooks"));
    assert!(!selected(&only, &[], "install.claude-hooks"));
    assert!(
        !names_check("install.*", "installer.x"),
        "a group ends at its dot"
    );
    assert!(!names_check("install.*", "install"), "a group is not an id");
    assert!(names_check("binary.path", "binary.path"));
    assert!(!names_check("binary.path", "binary.executable"));
}

#[test]
fn validate_selection_should_refuse_unknown_and_conflicting_ids() {
    assert!(validate_selection(&ids(&["binary.path"]), &ids(&["daemon.health"])).is_ok());
    assert!(matches!(
        validate_selection(&ids(&["binary.path", "nope"]), &[]),
        Err(InstallError::UnknownDoctorCheck(id)) if id == "nope"
    ));
    assert!(matches!(
        validate_selection(&[], &ids(&["typo"])),
        Err(InstallError::UnknownDoctorCheck(id)) if id == "typo"
    ));
    assert!(matches!(
        validate_selection(&ids(&["binary.path"]), &ids(&["binary.path"])),
        Err(InstallError::ConflictingDoctorSelection(id)) if id == "binary.path"
    ));
    assert!(validate_selection(&[], &ids(&["install.*"])).is_ok());
    assert!(
        matches!(
            validate_selection(&[], &ids(&["instal.*"])),
            Err(InstallError::UnknownDoctorCheck(id)) if id == "instal.*"
        ),
        "a group no check belongs to is as wrong as a mistyped id"
    );
}

/// `--fix` runs a catalogue command by dropping its leading word, so a
/// template that did not start with `pixel` would run the wrong program.
#[test]
fn every_catalogue_command_should_be_a_chain_of_pixel_invocations() {
    for check in CHECKS {
        for command in check.fix.iter().flat_map(|fix| fix.split(" && ")) {
            assert!(command.starts_with("pixel "), "{}: {command}", check.id);
        }
    }
}

/// The three outcomes of the shell probe, driven through a script the
/// test controls: the success guard's two directions and the empty
/// stdout case each change exactly one of them.
#[test]
fn shell_path_check_reports_each_shell_outcome() {
    let dir = std::env::temp_dir().join(format!("pixel-doctor-shell-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let make_shell = |tag: &str, lookup: &str, body: &str| {
        let shell = dir.join(tag);
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\n\
                 if [ \"$#\" -ne 3 ] || [ \"$1\" != \"-l\" ] || [ \"$2\" != \"-c\" ] || [ \"$3\" != \"{lookup}\" ]; then\n\
                 \techo \"unexpected lookup args: $*\" >&2\n\
                 \texit 2\n\
                 fi\n\
                 {body}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        shell.to_string_lossy().into_owned()
    };
    let posix = "command -v pixel";

    let green = make_shell("green", posix, "echo /fake/bin/pixel; exit 0");
    let (status, detail) = shell_path_check(Some(&green)).unwrap();
    assert_eq!(status, CheckStatus::Green, "{detail:?}");
    assert_eq!(
        detail.detail.as_ref().unwrap()["resolved"],
        "/fake/bin/pixel"
    );

    // Exit 1: `command -v` found nothing — the shell works, pixel is not
    // reachable from it. Yellow, not red: no catalogue command repairs it.
    // The detail carries the shell's own words (an exit 1 a profile
    // script printed an error into is diagnosable), which is what tells
    // this miss apart from an empty-success one.
    let yellow = make_shell("yellow", posix, "exit 1");
    let (status, detail) = shell_path_check(Some(&yellow)).unwrap();
    assert_eq!(status, CheckStatus::Yellow, "{detail:?}");
    let yellow = detail.detail.unwrap();
    assert!(yellow["exit_status"].is_string(), "{yellow}");

    // Success with empty stdout is the same miss, not a green check —
    // and it is not the exit-1 shape either: nothing to report yet.
    let empty = make_shell("empty", posix, "exit 0");
    let (status, detail) = shell_path_check(Some(&empty)).unwrap();
    assert_eq!(status, CheckStatus::Yellow, "{detail:?}");
    assert_eq!(detail.detail.as_ref().unwrap()["shell"], empty);
    assert!(
        detail.detail.as_ref().unwrap()["exit_status"].is_null(),
        "an empty success has no exit status to report: {detail:?}"
    );

    // A shell named `fish` is asked fish's own lookup, `which pixel`;
    // the argument guard fails the probe if the POSIX query is sent.
    let fish = make_shell("fish", "which pixel", "echo /fake/bin/pixel; exit 0");
    let (status, detail) = shell_path_check(Some(&fish)).unwrap();
    assert_eq!(status, CheckStatus::Green, "{detail:?}");
    assert_eq!(
        detail.detail.as_ref().unwrap()["resolved"],
        "/fake/bin/pixel"
    );

    // A shell the OS cannot exec: red, a different failure from a miss.
    let err = shell_path_check(Some(dir.join("no-such-shell").to_string_lossy().as_ref()))
        .expect_err("a spawn failure is red");
    assert!(err.contains("failed to run shell"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A login shell stuck in its startup files is reported, never waited
/// on: the injectable deadline keeps the test at milliseconds where
/// production holds the shell to five seconds.
#[test]
fn shell_path_check_reports_a_shell_that_does_not_answer_in_time() {
    let dir = std::env::temp_dir().join(format!("pixel-doctor-timeout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let shell = dir.join("slow-zsh");
    std::fs::write(&shell, "#!/bin/sh\nsleep 5\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let timeout = std::time::Duration::from_millis(100);
    let err = super::shell_path_check_within(Some(shell.to_string_lossy().as_ref()), timeout)
        .expect_err("a stalled shell is red, not a hang");
    assert!(err.contains("did not answer within"), "{err}");
    assert!(err.contains(&format!("{timeout:?}")), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stalled shell must not outlive the probe: `bounded_output` returns
/// `Timeout` under the deadline instead of `Command::output()`'s
/// indefinite wait on a login shell stuck in its startup files.
#[test]
fn bounded_output_times_out_on_a_stalled_shell() {
    let started = std::time::Instant::now();
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg("sleep 5");
    let outcome = super::bounded_output(&mut command, std::time::Duration::from_millis(100));
    assert!(
        matches!(&outcome, Err(super::ProbeFailure::Timeout)),
        "a stalled shell is a timeout, got {outcome:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the deadline must bound the wait, took {:?}",
        started.elapsed()
    );
}

/// Twelve checks share `pixel setup`: the plan runs it once, lists
/// every check it repairs, and keeps the report's order.
/// A side build's `--fix` keeps every repair but the home install's: a
/// `pixel setup` step without `--repo`, even inside a chain, is left to
/// the managed pixel, while the managed pixel itself runs everything.
#[test]
fn split_home_repairs_should_leave_the_home_install_to_the_managed_pixel() {
    let repair = |command: &str, steps: &[&[&str]]| Repair {
        command: command.into(),
        steps: argv(steps),
        checks: ids(&["x"]),
    };
    let home = repair("pixel setup", &[&["setup", "--shell", "fish"]]);
    let chained = repair(
        "pixel daemon stop && pixel setup",
        &[&["daemon", "stop", "/r"], &["setup"]],
    );
    let repo = repair("pixel setup --repo /r", &[&["setup", "--repo", "/r"]]);
    let prepare = repair("pixel prepare-repo /r", &[&["prepare-repo", "/r"]]);
    let plan = vec![home.clone(), chained.clone(), repo.clone(), prepare.clone()];
    assert_eq!(
        split_home_repairs(plan.clone(), true),
        (vec![repo, prepare], vec![home, chained])
    );
    assert_eq!(split_home_repairs(plan.clone(), false), (plan, vec![]));
}

#[test]
fn repair_plan_should_run_each_command_once_in_report_order() {
    let install: &[&[&str]] = &[&["setup"]];
    let prepare: &[&[&str]] = &[&["prepare-repo", "/r"]];
    let plan = repair_plan(&report(
        vec![
            finding("binary.path", CheckStatus::Green, None, None),
            repairable(
                "install.agent-prompt",
                CheckStatus::Red,
                "pixel setup",
                install,
            ),
            finding(
                "install.rtk-backup",
                CheckStatus::Yellow,
                None,
                Some("rm '/h/rtk.json'"),
            ),
            repairable("rule.parity", CheckStatus::Yellow, "pixel setup", install),
            repairable(
                "index.freshness",
                CheckStatus::Yellow,
                "pixel prepare-repo '/r'",
                prepare,
            ),
            repairable(
                "graph.freshness",
                CheckStatus::Red,
                "pixel prepare-repo '/r'",
                prepare,
            ),
        ],
        0,
    ));
    assert_eq!(
        plan,
        vec![
            Repair {
                command: "pixel setup".into(),
                steps: argv(install),
                checks: ids(&["install.agent-prompt", "rule.parity"]),
            },
            Repair {
                command: "pixel prepare-repo '/r'".into(),
                steps: argv(prepare),
                checks: ids(&["index.freshness", "graph.freshness"]),
            },
        ]
    );
}

fn sh_repair(steps: &[&[&str]]) -> Repair {
    Repair {
        command: "c".into(),
        steps: argv(steps),
        checks: vec![],
    }
}

#[test]
fn run_repair_should_run_every_step_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let append = |word: &str| format!("echo {word} >> '{}'", log.display());
    let (first, second) = (append("one"), append("two"));
    let repair = sh_repair(&[&["-c", &first], &["-c", &second]]);
    assert_eq!(run_repair(Path::new("/bin/sh"), &repair), Ok(()));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
}

/// Like `&&`: once a step fails the rest of the repair does not run, and
/// the error names the step and quotes its stderr.
#[test]
fn run_repair_should_stop_at_the_first_failing_step() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let touch = format!("touch '{}'", marker.display());
    let repair = sh_repair(&[&["-c", "echo boom >&2; exit 3"], &["-c", &touch]]);
    let error = run_repair(Path::new("/bin/sh"), &repair).unwrap_err();
    assert_eq!(
        error,
        "`pixel -c echo boom >&2; exit 3` failed (exit status: 3): boom"
    );
    assert!(!marker.exists(), "the step after the failure ran");
    let silent = run_repair(Path::new("/bin/sh"), &sh_repair(&[&["-c", "exit 4"]]));
    assert_eq!(
        silent,
        Err("`pixel -c exit 4` failed (exit status: 4)".into())
    );
}

#[test]
fn run_repair_should_report_a_binary_that_cannot_start() {
    let error = run_repair(Path::new("/nonexistent/pixel"), &sh_repair(&[&["setup"]])).unwrap_err();
    assert!(
        error.starts_with("`pixel setup` could not start: "),
        "{error}"
    );
}

/// A command that exits 0 is not proof: the verdict comes from the checks
/// re-run after every repair.
#[test]
fn judge_repair_should_trust_the_rerun_checks_over_the_exit_code() {
    let after = report(
        vec![
            finding("install.agent-prompt", CheckStatus::Green, None, None),
            finding("index.freshness", CheckStatus::Green, None, None),
            finding("graph.freshness", CheckStatus::Yellow, None, None),
        ],
        0,
    );
    let repair = |checks: &[&str]| Repair {
        command: "pixel x".into(),
        steps: vec![],
        checks: ids(checks),
    };
    let fixed = judge_repair(repair(&["install.agent-prompt"]), Ok(()), &after);
    assert_eq!(
        (fixed.status, fixed.still_flagged),
        (RepairStatus::Fixed, vec![])
    );
    let stuck = judge_repair(
        repair(&["index.freshness", "graph.freshness"]),
        Ok(()),
        &after,
    );
    assert_eq!(
        (stuck.status, stuck.still_flagged, stuck.error),
        (RepairStatus::NotConverged, ids(&["graph.freshness"]), None)
    );
    let vanished = judge_repair(repair(&["daemon.health"]), Ok(()), &after);
    assert_eq!(
        (vanished.status, vanished.still_flagged),
        (RepairStatus::NotConverged, ids(&["daemon.health"]))
    );
    let failed = judge_repair(
        repair(&["install.agent-prompt"]),
        Err("boom".into()),
        &after,
    );
    assert_eq!(
        (failed.status, failed.error.as_deref()),
        (RepairStatus::Failed, Some("boom"))
    );
}

#[test]
fn render_repairs_should_tally_and_detail_every_outcome() {
    let outcome = |status, checks: &[&str], still: &[&str], error: Option<&str>| RepairOutcome {
        command: format!("pixel {status}"),
        checks: ids(checks),
        status,
        still_flagged: ids(still),
        error: error.map(Into::into),
    };
    let text = render_repairs(&[
        outcome(RepairStatus::Fixed, &["a", "b"], &[], None),
        outcome(RepairStatus::NotConverged, &["c", "d"], &["d"], None),
        outcome(
            RepairStatus::Failed,
            &["e"],
            &["e"],
            Some("`pixel e` failed"),
        ),
    ]);
    let expected = [
        "pixel doctor --fix: ran 3 repair(s) — 1 fixed, 1 not converged, 1 failed",
        "  [fixed] pixel fixed (a, b)",
        "  [not converged] pixel not converged (c, d)",
        "    still flagged: d",
        "  [failed] pixel failed (e)",
        "    error: `pixel e` failed",
        "    still flagged: e",
        "",
    ]
    .join("\n");
    assert_eq!(text, expected);
    assert_eq!(
        render_repairs(&[]),
        "pixel doctor --fix: nothing to repair automatically\n"
    );
}

#[test]
fn repair_status_should_serialize_in_snake_case() {
    assert_eq!(
        serde_json::to_value(RepairStatus::NotConverged).unwrap(),
        "not_converged"
    );
}

#[test]
fn one_line_should_fold_newlines_and_control_characters_into_single_spaces() {
    assert_eq!(
        one_line("  error:\n\tbad\x1b[31m  thing \r\n"),
        "error: bad [31m thing"
    );
    assert_eq!(one_line("plain"), "plain");
}

#[test]
fn capped_should_keep_text_at_the_limit_and_cut_beyond_it() {
    assert_eq!(capped("abcd", 4), "abcd", "exactly at the cap is kept");
    assert_eq!(capped("abcde", 4), "abc…");
    assert_eq!(
        capped("éééé", 3).chars().count(),
        3,
        "counts characters, not bytes"
    );
}

// -- rule-vs-binary parity: extraction + normalization ------------------

// -- scenario consistency ------------------------------------------------

// ---- the install checks, on a throwaway home ----------------------------------

mod machine {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::super::{CheckStatus, DoctorOptions, DoctorReport, doctor};
    use crate::install::{InstallOptions, install};

    /// A binary that answers `--version`, so `binary.executable` has something to run.
    fn fake_pixel(dir: &Path) -> PathBuf {
        let exe = dir.join("bin/pixel");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "#!/bin/sh\necho pixel 0.0.0\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    struct Machine {
        dir: tempfile::TempDir,
        exe: PathBuf,
    }

    impl Machine {
        fn new() -> Machine {
            let dir = tempfile::tempdir().unwrap();
            let exe = fake_pixel(dir.path());
            fs::create_dir_all(dir.path().join("home")).unwrap();
            Machine { dir, exe }
        }

        fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.home().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// Every host configured, then set up.
        fn set_up() -> Machine {
            let machine = Machine::new();
            for relative in [
                ".claude/settings.json",
                ".codex/hooks.json",
                ".config/devin/config.json",
                ".gemini/settings.json",
                ".gemini/config/hooks.json",
                ".pi/agent/settings.json",
            ] {
                machine.write(relative, "{}\n");
            }
            fs::create_dir_all(machine.home().join(".config/opencode")).unwrap();
            machine.install();
            machine
        }

        fn install(&self) {
            let report = install(&InstallOptions {
                home: Some(self.home()),
                executable_path: Some(self.exe.clone()),
                ..InstallOptions::default()
            })
            .unwrap();
            assert!(report.ok, "{report:?}");
        }

        fn doctor(&self) -> DoctorReport {
            doctor(
                &DoctorOptions {
                    home: Some(self.home()),
                    executable_path: Some(self.exe.clone()),
                    skip: vec!["binary.shell-path".into()],
                    ..DoctorOptions::default()
                },
                |_| {},
            )
            .unwrap()
        }

        fn status(&self, id: &str) -> (CheckStatus, String) {
            let report = self.doctor();
            let check = report
                .checks
                .iter()
                .find(|check| check.id == id)
                .unwrap_or_else(|| panic!("no check {id}"));
            (
                check.status,
                check
                    .reason
                    .clone()
                    .unwrap_or_else(|| check.summary.clone()),
            )
        }
    }

    #[test]
    fn a_machine_that_was_set_up_is_green_everywhere() {
        let machine = Machine::set_up();
        let report = machine.doctor();
        let flagged: Vec<_> = report
            .checks
            .iter()
            .filter(|check| check.status != CheckStatus::Green)
            .map(|check| {
                (
                    check.id.clone(),
                    check
                        .reason
                        .clone()
                        .unwrap_or_else(|| check.summary.clone()),
                )
            })
            .collect();
        // The one finding a fresh Codex always has: it has not reviewed the hooks.
        assert_eq!(
            flagged
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["install.codex-hook-review"],
            "{flagged:?}"
        );
        assert!(flagged[0].1.contains("/hooks"), "{}", flagged[0].1);
        assert!(report.ok);
        assert_eq!(report.summary.skipped, 1);
    }

    #[test]
    fn a_machine_without_agents_expects_nothing_of_them() {
        let machine = Machine::new();
        let report = machine.doctor();
        for check in &report.checks {
            if check.id == "install.skill" {
                assert_eq!(check.status, CheckStatus::Yellow, "{check:?}");
                assert!(check.summary.contains("pixel setup"), "{}", check.summary);
            } else if check.id.starts_with("install.") {
                assert_eq!(
                    check.status,
                    CheckStatus::Green,
                    "{}: {}",
                    check.id,
                    check.summary
                );
            }
        }
        assert!(report.ok);
    }

    #[test]
    fn a_missing_brief_part_is_named() {
        let machine = Machine::set_up();
        let path = machine.home().join(".claude/settings.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        fs::write(&path, value.to_string()).unwrap();
        let (status, reason) = machine.status("install.claude-hooks");
        assert_eq!(status, CheckStatus::Red);
        assert!(reason.contains("brief part 2/3 missing"), "{reason}");
        assert!(reason.contains("pixel setup"), "{reason}");
    }

    #[test]
    fn hooks_registered_twice_or_for_another_binary_are_flagged() {
        let machine = Machine::set_up();
        let path = machine.home().join(".claude/settings.json");
        let original = fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&original).unwrap();

        let again = value["hooks"]["Stop"][0].clone();
        value["hooks"]["Stop"].as_array_mut().unwrap().push(again);
        fs::write(&path, value.to_string()).unwrap();
        let (status, reason) = machine.status("install.claude-hooks");
        assert_eq!(
            (status, reason.contains("more than once")),
            (CheckStatus::Red, true),
            "{reason}"
        );

        let mut value: serde_json::Value = serde_json::from_str(&original).unwrap();
        value["hooks"]["Stop"].as_array_mut().unwrap().push(json!({"hooks": [{"type": "command",
            "command": "'/usr/local/bin/pixel' run-hook task-event --provider claude --event stop"}]}));
        fs::write(&path, value.to_string()).unwrap();
        let (status, reason) = machine.status("install.claude-hooks");
        assert_eq!(
            status,
            CheckStatus::Green,
            "foreign hooks are preserved: {reason}"
        );

        fs::write(
            &path,
            original.replace(machine.exe.to_str().unwrap(), "/opt/other/pixel-dev"),
        )
        .unwrap();
        let (status, reason) = machine.status("install.claude-hooks");
        assert_eq!(status, CheckStatus::Yellow, "{reason}");
        assert!(reason.contains("/opt/other/pixel-dev"), "{reason}");
    }

    #[test]
    fn a_brief_split_that_the_configuration_no_longer_asks_for_is_yellow() {
        let machine = Machine::set_up();
        machine.write(".pixel/config.yaml", "brief_parts: 2\n");
        let (status, reason) = machine.status("install.claude-hooks");
        assert_eq!(status, CheckStatus::Yellow, "{reason}");
        assert!(reason.contains("`brief_parts` is 2"), "{reason}");
    }

    #[test]
    fn gemini_opencode_and_pi_go_red_when_a_piece_is_gone_or_stale() {
        let machine = Machine::set_up();
        let gemini = machine.home().join(".gemini/settings.json");
        let original = fs::read_to_string(&gemini).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&original).unwrap();
        value["hooks"].as_object_mut().unwrap().remove("AfterAgent");
        fs::write(&gemini, value.to_string()).unwrap();
        let (status, reason) = machine.status("install.gemini");
        assert_eq!(
            (status, reason.contains("AfterAgent")),
            (CheckStatus::Red, true),
            "{reason}"
        );
        fs::write(&gemini, original).unwrap();

        let plugin = machine
            .home()
            .join(".config/opencode/plugins/pixel-brief.js");
        let current = fs::read_to_string(&plugin).unwrap();
        fs::write(&plugin, format!("{current}// edited\n")).unwrap();
        let (status, reason) = machine.status("install.opencode");
        assert_eq!(
            (status, reason.contains("stale")),
            (CheckStatus::Red, true),
            "{reason}"
        );
        fs::write(&plugin, "// mine\n").unwrap();
        assert_eq!(machine.status("install.opencode").0, CheckStatus::Yellow);
        fs::remove_file(&plugin).unwrap();
        assert_eq!(machine.status("install.opencode").0, CheckStatus::Red);

        let package = machine
            .home()
            .join(".local/share/pixel/pi-package/extensions/pixel.ts");
        fs::write(&package, "// stale\n").unwrap();
        let (status, reason) = machine.status("install.pi-package");
        assert_eq!(
            (status, reason.contains("pixel setup")),
            (CheckStatus::Red, true),
            "{reason}"
        );
    }

    #[test]
    fn antigravity_is_red_without_its_brief_and_green_with_a_users_own() {
        let machine = Machine::set_up();
        assert_eq!(machine.status("install.antigravity").0, CheckStatus::Green);
        let hooks = machine.home().join(".gemini/config/hooks.json");
        fs::write(&hooks, "{}").unwrap();
        let (status, reason) = machine.status("install.antigravity");
        assert_eq!(
            (status, reason.contains("pixel-brief")),
            (CheckStatus::Red, true),
            "{reason}"
        );
        fs::write(&hooks, r#"{"pixel-brief": {"enabled": true, "PreInvocation": [{"type": "command", "command": "mine"}]}}"#).unwrap();
        assert_eq!(machine.status("install.antigravity").0, CheckStatus::Green);
    }

    #[test]
    fn the_skill_is_red_when_missing_from_a_root_and_yellow_when_it_differs() {
        let machine = Machine::set_up();
        assert_eq!(machine.status("install.skill").0, CheckStatus::Green);
        let codex = machine.home().join(".codex/skills/pixel/SKILL.md");
        fs::write(&codex, "mine\n").unwrap();
        let (status, reason) = machine.status("install.skill");
        assert_eq!(
            (status, reason.contains("keeps a backup")),
            (CheckStatus::Yellow, true),
            "{reason}"
        );
        fs::remove_file(&codex).unwrap();
        let (status, reason) = machine.status("install.skill");
        assert_eq!(
            (status, reason.contains(".codex")),
            (CheckStatus::Red, true),
            "{reason}"
        );
    }

    #[test]
    fn a_repository_config_that_does_not_read_is_red_and_the_live_checks_run() {
        let machine = Machine::set_up();
        let repo = machine.dir.path().join("repo");
        fs::create_dir_all(repo.join(".pixel")).unwrap();
        fs::write(repo.join(".pixel/config.yaml"), "- not\n- a mapping\n").unwrap();
        let mut ran = false;
        let report = doctor(
            &DoctorOptions {
                home: Some(machine.home()),
                executable_path: Some(machine.exe.clone()),
                repo_root: Some(repo),
                only: vec!["repo.*".into(), "daemon.health".into()],
                ..DoctorOptions::default()
            },
            |runner| {
                ran = true;
                runner.check("daemon.health", || Err("no daemon".into()));
            },
        )
        .unwrap();
        assert!(ran);
        let by_id = |id: &str| report.checks.iter().find(|check| check.id == id).unwrap();
        assert_eq!(by_id("repo.config").status, CheckStatus::Red);
        assert!(
            by_id("repo.config")
                .reason
                .as_deref()
                .unwrap()
                .contains("must be a mapping")
        );
        assert_eq!(by_id("daemon.health").status, CheckStatus::Red);
        assert!(
            by_id("daemon.health")
                .fix
                .as_deref()
                .unwrap()
                .starts_with("pixel daemon start '")
        );
    }

    #[test]
    fn the_binary_checks_report_a_missing_and_a_working_binary() {
        let machine = Machine::new();
        assert_eq!(machine.status("binary.path").0, CheckStatus::Green);
        assert_eq!(machine.status("binary.executable").0, CheckStatus::Green);
        fs::remove_file(&machine.exe).unwrap();
        let report = doctor(
            &DoctorOptions {
                home: Some(machine.home()),
                executable_path: Some(machine.exe.clone()),
                only: vec!["binary.path".into(), "binary.executable".into()],
                ..DoctorOptions::default()
            },
            |_| {},
        )
        .unwrap();
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == CheckStatus::Red)
        );
        assert!(!report.ok);
    }
}
