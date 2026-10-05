// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The pi guard of `pixel install --repo`: one extension file under the
//! repository's `.pi/extensions/`, the directory pi auto-discovers project
//! extensions from once the project is trusted (pi 0.87,
//! `docs/extensions.md`). pi resolves it against its working directory, so
//! the guard runs when pi is started from the repository root.
//!
//! No rules file is written next to it: pi's project prompt file,
//! `.pi/APPEND_SYSTEM.md`, replaces the global `~/.pi/agent/APPEND_SYSTEM.md`
//! that `pixel install` fills, and its only project context file is the
//! repository's own, usually shared, `AGENTS.md`.
//!
//! Releases up to 0.4.0 wrote the guard and a rules block under
//! `<repo>/.pi/agent/`, a directory pi reads only under `~`. Install and
//! uninstall take pixel's files out of it, and doctor reports a guard left
//! there, since it never ran.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The guard extension, relative to the repository.
pub(crate) const EXTENSION: &str = ".pi/extensions/pixel-guard.ts";

/// The directory, relative to the repository, where releases up to 0.4.0
/// wrote the guard (`extensions/pixel-guard.ts`) and a rules block
/// (`AGENTS.md`).
pub(crate) const LEGACY_DIR: &str = ".pi/agent";

/// The repository extension with the installed Pixel executable embedded.
pub(crate) fn extension_source(exe: &Path) -> String {
    include_str!("../assets/pi-pixel.ts")
        .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
        .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
        .replace("__MANAGED_END__", config::MANAGED_END)
}

/// Whether `path` is a guard extension pixel wrote: a file carrying the
/// managed marker. Anything else under that name belongs to the user.
fn is_managed_extension(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.contains(config::MANAGED_BEGIN))
}

/// `pixel install --repo` step: write the guard to [`EXTENSION`] and take
/// pixel's files out of [`LEGACY_DIR`].
pub(crate) fn install(repo: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let ext_file = repo.join(EXTENSION);
    let source = extension_source(exe);
    let migrated = remove_legacy(repo, dry_run)?;
    let backup = if dry_run {
        None
    } else {
        if let Some(parent) = ext_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let backup = config::backup_if_changing(&ext_file, source.as_bytes())?;
        fs::write(&ext_file, &source)?;
        backup
    };
    let mut detail = format!("wrote {}", ext_file.display());
    if !migrated.is_empty() {
        detail.push_str(&format!("; removed {}", migrated.join(" ")));
    }
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            "pi guard extension installed (loads once pi trusts the project)",
        ),
        detail: Some(install::with_backup_note(detail, backup)),
    })
}

/// `pixel uninstall --repo` step: remove the guard pixel wrote to
/// [`EXTENSION`] and pixel's files in [`LEGACY_DIR`].
pub(crate) fn uninstall(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let ext_file = repo.join(EXTENSION);
    let mut removed = remove_legacy(repo, dry_run)?;
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
            remove_empty_dirs(&[&repo.join(".pi/extensions"), &repo.join(".pi")]);
        }
        removed.insert(0, EXTENSION.to_string());
    }
    let summary = if removed.is_empty() {
        "no pi guard extension found".to_string()
    } else {
        format!("removed {}", removed.join(" "))
    };
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("ext={}", ext_file.display())),
    })
}

/// Take pixel's files out of `<repo>/`[`LEGACY_DIR`]: the managed guard is
/// deleted, the managed block leaves `AGENTS.md` (deleted when nothing else
/// is left in it, backed up otherwise), and the directories are removed once
/// empty. Returns the repository-relative paths touched.
fn remove_legacy(repo: &Path, dry_run: bool) -> Result<Vec<String>> {
    let dir = repo.join(LEGACY_DIR);
    let ext_file = dir.join("extensions").join("pixel-guard.ts");
    let agents_md = dir.join("AGENTS.md");
    let mut removed = Vec::new();
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
        }
        removed.push(format!("{LEGACY_DIR}/extensions/pixel-guard.ts"));
    }
    // Best effort: an unreadable legacy file holds nothing pi ever loaded.
    let agents = fs::read_to_string(&agents_md).unwrap_or_default();
    if agents.contains(config::MANAGED_BEGIN) {
        let rest = config::strip_managed_block(&agents);
        if !dry_run {
            if rest.trim().is_empty() {
                fs::remove_file(&agents_md)?;
            } else {
                config::backup_if_changing(&agents_md, rest.as_bytes())?;
                fs::write(&agents_md, &rest)?;
            }
        }
        removed.push(format!("{LEGACY_DIR}/AGENTS.md"));
    }
    if !dry_run {
        remove_empty_dirs(&[&dir.join("extensions"), &dir]);
    }
    Ok(removed)
}

/// Remove each directory in order when it is empty; a directory that still
/// holds anything, or is absent, is left as it is.
fn remove_empty_dirs(dirs: &[&Path]) {
    for dir in dirs {
        // `remove_dir` refuses a non-empty directory, which is the rule here.
        let _ = fs::remove_dir(dir);
    }
}

/// The state of a repository's pi guard, as `pixel doctor` reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuardState {
    /// No guard anywhere: `install --repo` was not run for pi.
    Absent,
    /// The managed guard sits at [`EXTENSION`].
    Installed(PathBuf),
    /// A file at [`EXTENSION`] that pixel did not write.
    Foreign(PathBuf),
    /// Only the guard of an older release, in [`LEGACY_DIR`], which pi never
    /// loads in a project.
    Legacy(PathBuf),
}

/// Where the repository's pi guard stands.
pub(crate) fn guard_state(repo: &Path) -> GuardState {
    let ext_file = repo.join(EXTENSION);
    if ext_file.is_file() {
        return if is_managed_extension(&ext_file) {
            GuardState::Installed(ext_file)
        } else {
            GuardState::Foreign(ext_file)
        };
    }
    let legacy = repo
        .join(LEGACY_DIR)
        .join("extensions")
        .join("pixel-guard.ts");
    if is_managed_extension(&legacy) {
        GuardState::Legacy(legacy)
    } else {
        GuardState::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_ext(repo: &Path) -> PathBuf {
        repo.join(LEGACY_DIR)
            .join("extensions")
            .join("pixel-guard.ts")
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn managed(text: &str) -> String {
        format!(
            "{}\n{text}\n{}\n",
            config::MANAGED_BEGIN,
            config::MANAGED_END
        )
    }

    #[test]
    fn extension_source_should_name_the_binary_and_carry_the_marker() {
        let source = extension_source(Path::new("/opt/bin/pixel"));
        assert!(
            source.contains("const PIXEL_BIN = \"/opt/bin/pixel\";"),
            "{source}"
        );
        assert!(source.contains(config::MANAGED_BEGIN), "{source}");
        assert!(source.contains(config::MANAGED_END), "{source}");
        assert!(source.contains("pi.registerTool({"), "{source}");
        assert!(
            source.contains("name: \"pixel_project\", label: \"Pixel (project)\""),
            "project extension must not collide with the global Pixel tool"
        );
        assert!(
            !source.contains("const pixelToolAlreadyRegistered = pi.getAllTools()"),
            "getAllTools is unavailable during extension loading"
        );
        assert!(
            source.contains("pi.on(\"session_start\", activatePixelTool)"),
            "an existing Pixel tool must be available after session restore"
        );
        assert!(source.contains("pi.on(\"tool_call\""), "{source}");
        assert!(source.contains("return { block: true"), "{source}");
        assert!(
            source.contains("export default function activate(pi: ExtensionAPI)"),
            "{source}"
        );
    }

    #[test]
    fn install_should_write_the_guard_where_pi_discovers_project_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let step = install(dir.path(), Path::new("/opt/bin/pixel"), false).unwrap();
        let written = fs::read_to_string(dir.path().join(".pi/extensions/pixel-guard.ts")).unwrap();
        assert_eq!(written, extension_source(Path::new("/opt/bin/pixel")));
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!dir.path().join(LEGACY_DIR).exists());
    }

    #[test]
    fn install_should_move_a_legacy_guard_and_drop_a_rules_file_holding_only_pixel() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), &managed("old guard"));
        write(
            &repo.join(LEGACY_DIR).join("AGENTS.md"),
            &managed("old rules"),
        );

        let step = install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert!(repo.join(EXTENSION).is_file());
        assert!(!repo.join(LEGACY_DIR).exists(), "empty legacy dir must go");
        let detail = step.detail.unwrap();
        assert!(
            detail.contains("removed .pi/agent/extensions/pixel-guard.ts .pi/agent/AGENTS.md"),
            "{detail}"
        );
    }

    #[test]
    fn install_should_keep_user_text_and_files_in_the_legacy_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agents = repo.join(LEGACY_DIR).join("AGENTS.md");
        write(&agents, &format!("mine\n{}", managed("old rules")));
        write(
            &repo.join(LEGACY_DIR).join("extensions/other.ts"),
            "user ext",
        );
        write(&legacy_ext(repo), &managed("old guard"));

        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert_eq!(fs::read_to_string(&agents).unwrap(), "mine\n");
        assert!(repo.join(LEGACY_DIR).join("extensions/other.ts").is_file());
        assert!(!legacy_ext(repo).exists());
    }

    #[test]
    fn install_should_leave_an_unmanaged_legacy_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), "the user's own guard");
        write(&repo.join(LEGACY_DIR).join("AGENTS.md"), "the user's rules");

        let step = install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert_eq!(
            fs::read_to_string(legacy_ext(repo)).unwrap(),
            "the user's own guard"
        );
        assert_eq!(
            fs::read_to_string(repo.join(LEGACY_DIR).join("AGENTS.md")).unwrap(),
            "the user's rules"
        );
        assert!(!step.detail.unwrap().contains("removed"));
    }

    #[test]
    fn install_dry_run_should_write_and_remove_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), &managed("old guard"));

        let step = install(repo, Path::new("/opt/bin/pixel"), true).unwrap();

        assert!(!repo.join(EXTENSION).exists());
        assert!(legacy_ext(repo).is_file());
        assert!(step.summary.starts_with("[dry-run]"), "{}", step.summary);
        assert!(step.detail.unwrap().contains("removed"));
    }

    #[test]
    fn uninstall_should_remove_the_guard_and_the_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();
        write(&legacy_ext(repo), &managed("old guard"));

        let step = uninstall(repo, false).unwrap();

        assert!(!repo.join(EXTENSION).exists());
        assert!(!legacy_ext(repo).exists());
        assert!(!repo.join(".pi").exists(), "empty .pi dirs must go");
        assert_eq!(
            step.summary,
            "removed .pi/extensions/pixel-guard.ts .pi/agent/extensions/pixel-guard.ts"
        );
    }

    #[test]
    fn uninstall_should_keep_a_foreign_extension_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(EXTENSION), "the user's own guard");

        let step = uninstall(repo, false).unwrap();

        assert_eq!(
            fs::read_to_string(repo.join(EXTENSION)).unwrap(),
            "the user's own guard"
        );
        assert_eq!(step.summary, "no pi guard extension found");
    }

    #[test]
    fn uninstall_dry_run_should_report_without_removing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        let step = uninstall(repo, true).unwrap();

        assert!(repo.join(EXTENSION).is_file());
        assert_eq!(
            step.summary,
            "[dry-run] would report: removed .pi/extensions/pixel-guard.ts"
        );
    }

    #[test]
    fn remove_empty_dirs_should_keep_a_directory_that_holds_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full");
        let empty = dir.path().join("empty");
        write(&full.join("f"), "x");
        fs::create_dir_all(&empty).unwrap();

        remove_empty_dirs(&[&full, &empty]);

        assert!(full.join("f").is_file());
        assert!(!empty.exists());
    }

    #[test]
    fn guard_state_should_tell_every_case_apart() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), &managed("old guard"));
        assert_eq!(guard_state(repo), GuardState::Legacy(legacy_ext(repo)));

        write(&repo.join(EXTENSION), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Foreign(repo.join(EXTENSION)));

        write(&repo.join(EXTENSION), &managed("guard"));
        assert_eq!(
            guard_state(repo),
            GuardState::Installed(repo.join(EXTENSION))
        );
    }

    /// Runs `script` under bun against the extension with pi's SDK imports
    /// stubbed and `PIXEL_BIN` pointing at an argv-echoing shell stub; the
    /// script sees `classify`, `commandFor`, `run` and `activate`. Returns
    /// its stdout, or `None` where bun is not installed.
    fn run_in_bun(script: &str) -> Option<String> {
        if std::process::Command::new("bun")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("pixel-stub");
        write(
            &stub,
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'pixel 0.0.0';;\n  --help) printf '  status  s\\n  search-content  s\\n  scope-task  s\\n  repo-state  s\\n';;\n  status) echo '{\"index\":{\"base_files\":1},\"graph\":{\"present\":true}}';;\n  scope-task) echo '{\"path\":\"a.rs\"}';;\n  repo-state) echo '{}';;\n  *) echo \"$@\";;\nesac\n",
        );
        fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let source = extension_source(&stub)
            .replace(
                "import { Type } from \"@earendil-works/pi-ai\";",
                "const Type: any = new Proxy({}, { get: () => () => ({}) });",
            )
            .replace(
                "import type { ExtensionAPI } from \"@earendil-works/pi-coding-agent\";",
                "type ExtensionAPI = any;",
            );
        write(
            &dir.path().join("ext.ts"),
            &format!("{source}\nexport {{ classify, commandFor, run, metricsBox, activate }};\n"),
        );
        write(
            &dir.path().join("t.ts"),
            &format!(
                "import {{ classify, commandFor, run, metricsBox, activate }} from \"./ext.ts\";\n{script}\n"
            ),
        );
        let out = std::process::Command::new("bun")
            .arg("t.ts")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn classify_should_block_a_bounded_read_of_a_bootstrap_path_until_pixel_was_called() {
        let script = r#"
const root = "/repo";
const paths = new Set(["src/a.rs"]);
const read = { path: "src/a.rs", limit: 50 };
const before = classify("read", read, root, paths, { pixelHealthy: true, pixelCalled: false });
const after = classify("read", read, root, paths, { pixelHealthy: true, pixelCalled: true });
const outside = classify("read", { path: "/etc/hosts" }, root, paths, { pixelHealthy: true, pixelCalled: false });
console.log(JSON.stringify([before.kind, after.kind, after.reason, outside.kind]));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        assert_eq!(
            out.trim(),
            r#"["blocked","exception","bounded read after a Pixel call","exception"]"#
        );
    }

    #[test]
    fn tool_result_should_unlock_bootstrap_reads_only_after_a_successful_pixel_tool_result() {
        let script = r#"
process.env.PIXEL_POLICY = "enforce";
process.env.PIXEL_PI_RETRIEVAL = "1";
const root = process.cwd();
const verdicts: Record<string, string> = {};
for (const [toolName, isError] of [["none", false], ["pixel", false], ["pixel_project", false], ["pixel", true], ["read", false]] as const) {
  const handlers: Record<string, any> = {};
  activate({ on: (n: string, f: any) => { handlers[n] = f; }, registerTool: () => {}, getActiveTools: () => [], getAllTools: () => [], setActiveTools: () => {} });
  await handlers["before_agent_start"]({ prompt: "fix the failing parser test please" }, { cwd: root });
  if (toolName !== "none") await handlers["tool_result"]({ toolName, isError, content: [] }, { cwd: root });
  const out = await handlers["tool_call"]({ toolName: "read", input: { path: "a.rs", limit: 10 } }, { cwd: root });
  verdicts[`${toolName}/${isError}`] = out?.block ? "blocked" : "allowed";
}
console.log(JSON.stringify(verdicts));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        assert_eq!(
            out.trim(),
            r#"{"none/false":"blocked","pixel/false":"allowed","pixel_project/false":"allowed","pixel/true":"blocked","read/false":"blocked"}"#
        );
    }

    #[test]
    fn run_should_keep_the_metrics_box_for_tool_calls_and_silence_it_for_probes() {
        let script = r#"
console.log(JSON.stringify([
  run("/", ["search-content", "q"]).trim(),
  run("/", ["search-content", "q"], true).trim(),
]));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        assert_eq!(
            out.trim(),
            r#"["search-content q","search-content q --metrics off"]"#
        );
    }

    #[test]
    fn run_box_should_return_only_the_box_lines_of_stderr_unless_quiet() {
        let script = r#"
const box = "warn: diag\n🟩 pixel x\n  │\n  └───\nafter";
console.log(JSON.stringify([metricsBox(box), metricsBox("only diag"), metricsBox("🟩 pixel y\n  │\n\nnext")]));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        assert_eq!(
            out.trim(),
            r#"["🟩 pixel x\n  │\n  └───","","🟩 pixel y\n  │"]"#
        );
    }

    #[test]
    fn classify_should_name_why_a_read_is_blocked_and_unlock_on_a_pixel_call_alone() {
        let script = r#"
const s = (pixelCalled: boolean) => ({ pixelHealthy: true, pixelCalled });
const why = (input: any, called: boolean) => classify("read", input, "/repo", new Set(), s(called));
console.log(JSON.stringify([
  why({ path: "a.rs", limit: 5 }, false).reason,
  why({ path: "a.rs" }, true).reason,
  why({ path: "a.rs", limit: 201 }, true).reason,
  why({ path: ".env.local", limit: 5 }, true).reason,
  why({ path: "a.rs", limit: 200 }, true).kind,
]));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        let tail = ". Call pixel first, then read with a limit of at most 200 lines";
        let want = [
            format!("Read blocked: path not resolved by pixel yet{tail}"),
            format!("Read blocked: no limit given{tail}"),
            format!("Read blocked: limit 201 exceeds 200{tail}"),
            format!("Read blocked: credential path{tail}"),
            "exception".to_string(),
        ];
        assert_eq!(out.trim(), serde_json::to_string(&want).unwrap());
    }

    #[test]
    fn command_for_should_reject_an_empty_search_query_even_with_a_symbol() {
        let script = r#"
const attempt = (action: any, p: any) => { try { return commandFor(action, p); } catch (e) { return String(e); } };
console.log(JSON.stringify([
  attempt("search_content", { query: "  " }),
  attempt("search_content", { symbol: "x" }),
  attempt("search_content", { query: "needle" }),
  attempt("impact", { symbol: "x" }),
]));
"#;
        let Some(out) = run_in_bun(script) else {
            return;
        };
        assert_eq!(
            out.trim(),
            r#"["Error: search_content requires a goal, query, or symbol","Error: search_content requires a goal, query, or symbol",[["search-content","needle","--json","--limit","40"]],[["impact","x","--json"]]]"#
        );
    }

    #[test]
    fn extension_source_should_pass_metrics_off_only_from_the_quiet_path() {
        let source = extension_source(Path::new("/opt/bin/pixel"));
        assert!(
            source.contains("...(quiet ? [\"--metrics\", \"off\"] : [])"),
            "tool-facing runs must not hide the metrics box"
        );
        assert!(
            source.contains("event.toolName === \"pixel\" || event.toolName === \"pixel_project\""),
            "the global pixel tool must mark pixel as called"
        );
        assert!(
            source.contains("!state.pixelCalled ? \"path not resolved by pixel yet\""),
            "bootstrap paths must not unlock reads before a pixel call"
        );
    }

    /// The bash fence ports `enforce_leaf` from `crates/pixel/src/guard.rs`.
    /// The reason strings and decision-function names are part of the host
    /// contract: the audit log carries them and they tell the user which
    /// pixel command reaches the same indexed answer.
    #[test]
    fn extension_source_should_carry_the_bash_leaf_table() {
        let source = extension_source(Path::new("/opt/bin/pixel"));
        for marker in [
            "function splitShellSegments",
            "function tokenizeShell",
            "function argReadsRepo",
            "function readableRepoFile",
            "function enforceLeaf",
            "function enforceLeafDecision",
            "\"repository read: use pixel search-content or pixel pack-context <uid>\"",
            "\"repository search: use pixel search-content\"",
            "\"repository discovery: use pixel list-areas or find-code\"",
            "\"repository discovery: use pixel find-code or list-areas\"",
            "\"credential path\"",
            "\"repository inspection: use pixel repo-state\"",
            "\"repository inspection: use pixel review-changes\"",
            "\"repository inspection: use pixel commit-history\"",
        ] {
            assert!(
                source.contains(marker),
                "missing marker: {marker}\n{source}"
            );
        }
    }
}
