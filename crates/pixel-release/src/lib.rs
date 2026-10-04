// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel release-check`: the consistency a release tag must have before
//! anything is built or published.
//!
//! A library crate of its own so the mutation gate runs only these unit
//! tests for a change here; inside `pixel-cli` every mutant re-ran the whole
//! CLI contract suite (a minute or more each). The CLI keeps its
//! `release-check` contract tests.
//!
//! Three drifts have each produced a green tag and a broken release
//! elsewhere, and the release workflow used to guard only the first:
//!
//! 1. the tag names a version that `crates/pixel/Cargo.toml` does not carry;
//! 2. `Cargo.lock` is stale for a workspace member (a version was bumped
//!    without a `cargo build`), so `cargo build --locked` fails on the
//!    release runner, after the tests already passed;
//! 3. `CHANGELOG.md` has no `## [x.y.z]` heading, or the release notes are
//!    still filed under `## [Unreleased]`, so the GitHub release body is a
//!    fallback link;
//! 4. a plugin manifest (`.claude-plugin/plugin.json`, `package.json`, …)
//!    still names the previous version: Claude Code and Codex only deliver a
//!    plugin update when its version changes, so every plugin user keeps the
//!    old protocol and hook.
//!
//! Every check is a pure function over file contents so it is unit-tested
//! without a filesystem; `run` only reads the files and assembles the report.

use std::path::Path;

use serde_json::{Value, json};

/// One check: `ok` is the verdict, `detail` says what was found and, on
/// failure, what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub version: String,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "ok": self.ok(),
            "checks": self.checks.iter().map(|c| json!({
                "name": c.name,
                "ok": c.ok,
                "detail": c.detail,
            })).collect::<Vec<_>>(),
        })
    }

    /// One line per check, `[ok  ]` / `[FAIL]`, then the verdict.
    pub fn render(&self) -> String {
        let mut out = format!("release-check {}\n", self.version);
        for c in &self.checks {
            let mark = if c.ok { "ok  " } else { "FAIL" };
            out.push_str(&format!("[{mark}] {}: {}\n", c.name, c.detail));
        }
        out.push_str(if self.ok() {
            "release-check: all checks passed\n"
        } else {
            "release-check: FAILED\n"
        });
        out
    }
}

/// Accepts `1.2.3`, `v1.2.3` and `refs/tags/v1.2.3`; the result is the bare
/// `x.y.z` (a pre-release suffix such as `-rc.1` is kept). Anything that is
/// not three dot-separated numeric components is rejected.
pub fn normalize_version(input: &str) -> Option<String> {
    let s = input.trim();
    let s = s.strip_prefix("refs/tags/").unwrap_or(s);
    let s = s.strip_prefix('v').unwrap_or(s);
    let core = s.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(s.to_string())
}

/// `(name, version)` from a crate manifest's `[package]` table. Line-based
/// on purpose: only the two keys matter and the manifests are this repo's.
pub fn package_name_version(manifest: &str) -> Option<(String, String)> {
    let mut in_package = false;
    let mut name = None;
    let mut version = None;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(v) = quoted_value(t, "name") {
            name = Some(v);
        } else if let Some(v) = quoted_value(t, "version") {
            version = Some(v);
        }
    }
    Some((name?, version?))
}

/// `key = "value"` on one trimmed line, or `None`.
fn quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Workspace member paths from the root manifest's `members = [...]` list.
pub fn workspace_members(root_manifest: &str) -> Vec<String> {
    let Some(start) = root_manifest.find("members") else {
        return Vec::new();
    };
    let after = &root_manifest[start..];
    let Some(open) = after.find('[') else {
        return Vec::new();
    };
    let Some(close) = after[open..].find(']') else {
        return Vec::new();
    };
    after[open + 1..open + close]
        .split(',')
        .filter_map(|s| {
            let s = s.trim().trim_matches('"');
            (!s.is_empty()).then(|| s.to_string())
        })
        .collect()
}

/// Every `[[package]]` block of a `Cargo.lock` as `(name, version)`.
pub fn lock_packages(lock: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<(Option<String>, Option<String>)> = None;
    for line in lock.lines() {
        let t = line.trim();
        if t == "[[package]]" {
            if let Some((Some(n), Some(v))) = current.take() {
                out.push((n, v));
            }
            current = Some((None, None));
            continue;
        }
        if t.starts_with('[') {
            if let Some((Some(n), Some(v))) = current.take() {
                out.push((n, v));
            }
            continue;
        }
        if let Some((name, version)) = current.as_mut() {
            if let Some(v) = quoted_value(t, "name") {
                *name = Some(v);
            } else if let Some(v) = quoted_value(t, "version") {
                *version = Some(v);
            }
        }
    }
    if let Some((Some(n), Some(v))) = current {
        out.push((n, v));
    }
    out
}

/// The CLI crate must carry the tagged version.
pub fn check_cli_version(cli_manifest: &str, expected: &str) -> Check {
    match package_name_version(cli_manifest) {
        Some((name, version)) if version == expected => {
            Check::pass("cli-version", format!("{name} {version} matches"))
        }
        Some((name, version)) => Check::fail(
            "cli-version",
            format!(
                "{name} is {version}, tag says {expected}; bump `version` in crates/pixel/Cargo.toml or tag v{version}"
            ),
        ),
        None => Check::fail(
            "cli-version",
            "no [package] name/version in crates/pixel/Cargo.toml",
        ),
    }
}

/// `Cargo.lock` must carry every workspace member at its manifest version:
/// a stale entry makes `cargo build --locked` fail on the release runner.
pub fn check_lock(lock: &str, members: &[(String, String)]) -> Check {
    let packages = lock_packages(lock);
    let mut stale = Vec::new();
    for (name, version) in members {
        match packages.iter().find(|(n, _)| n == name) {
            Some((_, locked)) if locked == version => {}
            Some((_, locked)) => stale.push(format!("{name} {locked} (manifest {version})")),
            None => stale.push(format!("{name} missing from Cargo.lock")),
        }
    }
    if stale.is_empty() {
        Check::pass(
            "cargo-lock",
            format!(
                "{} workspace members at their manifest version",
                members.len()
            ),
        )
    } else {
        Check::fail(
            "cargo-lock",
            format!(
                "stale: {}; run `cargo build` (or `cargo update -p <name>`) and commit Cargo.lock",
                stale.join(", ")
            ),
        )
    }
}

/// `CHANGELOG.md` must have a `## [x.y.z]` heading and an empty
/// `## [Unreleased]` section: the release body is cut from the heading, and
/// notes left under Unreleased would be silently dropped from it.
pub fn check_changelog(changelog: &str, version: &str) -> Check {
    let heading = format!("## [{version}]");
    let mut has_heading = false;
    let mut in_unreleased = false;
    let mut unreleased_entries = 0usize;
    for line in changelog.lines() {
        if line.starts_with("## ") {
            in_unreleased = line.starts_with("## [Unreleased]");
            if line.starts_with(&heading)
                && line[heading.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| c == ' ')
            {
                has_heading = true;
            }
            continue;
        }
        if in_unreleased && line.trim_start().starts_with("- ") {
            unreleased_entries += 1;
        }
    }
    match (has_heading, unreleased_entries) {
        (true, 0) => Check::pass("changelog", format!("{heading} present, Unreleased empty")),
        (true, n) => Check::fail(
            "changelog",
            format!(
                "{heading} present but {n} entr{} still under ## [Unreleased]; move them under the release heading",
                if n == 1 { "y" } else { "ies" }
            ),
        ),
        (false, n) if n > 0 => Check::fail(
            "changelog",
            format!(
                "no {heading} heading; {n} entr{} under ## [Unreleased] — cut the release section first",
                if n == 1 { "y" } else { "ies" }
            ),
        ),
        (false, _) => Check::fail("changelog", format!("no {heading} heading in CHANGELOG.md")),
    }
}

/// Every plugin manifest that carries a version, relative to the repository
/// root. `.agents/skills/release/prepare.sh` bumps the same list.
pub const PLUGIN_MANIFESTS: &[&str] = &[
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    ".devin-plugin/plugin.json",
    ".qoder-plugin/plugin.json",
    "gemini-extension.json",
    "package.json",
    "plugin.yaml",
];

/// The version a plugin manifest declares: the top-level `"version"` of a
/// JSON manifest, the `version:` line of a YAML one.
pub fn manifest_version(rel: &str, content: &str) -> Option<String> {
    if rel.ends_with(".yaml") {
        return content
            .lines()
            .find_map(|line| line.strip_prefix("version:"))
            .map(|v| v.trim().trim_matches('"').to_string());
    }
    let doc: Value = serde_json::from_str(content).ok()?;
    doc.get("version")?.as_str().map(str::to_string)
}

/// Every plugin manifest present must declare the release version.
/// `manifests` is `(relative path, content)`; an absent manifest is not
/// listed by the caller.
pub fn check_plugin_versions(manifests: &[(&str, String)], version: &str) -> Check {
    let stale: Vec<String> = manifests
        .iter()
        .filter_map(|(rel, content)| match manifest_version(rel, content) {
            Some(v) if v == version => None,
            Some(v) => Some(format!("{rel} is {v}")),
            None => Some(format!("{rel} declares no version")),
        })
        .collect();
    if stale.is_empty() {
        Check::pass(
            "plugin-versions",
            format!("{} plugin manifests at {version}", manifests.len()),
        )
    } else {
        Check::fail(
            "plugin-versions",
            format!(
                "{}; run .agents/skills/release/prepare.sh or set them to {version}",
                stale.join(", ")
            ),
        )
    }
}

fn read(repo: &Path, rel: &str) -> Result<String, String> {
    std::fs::read_to_string(repo.join(rel)).map_err(|e| format!("{rel}: {e}"))
}

/// Run every check against `repo`. `Err` only when a file cannot be read or
/// the version string is malformed; a failed check is a non-`ok` report.
pub fn run(repo: &Path, version_input: &str) -> Result<Report, String> {
    let version = normalize_version(version_input).ok_or_else(|| {
        format!("`{version_input}` is not a version (expected x.y.z, vx.y.z or refs/tags/vx.y.z)")
    })?;
    let root_manifest = read(repo, "Cargo.toml")?;
    let cli_manifest = read(repo, "crates/pixel/Cargo.toml")?;
    let lock = read(repo, "Cargo.lock")?;
    let changelog = read(repo, "CHANGELOG.md")?;
    let mut members = Vec::new();
    for member in workspace_members(&root_manifest) {
        let manifest = read(repo, &format!("{member}/Cargo.toml"))?;
        let (name, version) = package_name_version(&manifest)
            .ok_or_else(|| format!("{member}/Cargo.toml: no [package] name/version"))?;
        members.push((name, version));
    }
    let mut plugin_manifests = Vec::new();
    for rel in PLUGIN_MANIFESTS {
        if repo.join(rel).is_file() {
            plugin_manifests.push((*rel, read(repo, rel)?));
        }
    }
    Ok(Report {
        checks: vec![
            check_cli_version(&cli_manifest, &version),
            check_lock(&lock, &members),
            check_changelog(&changelog, &version),
            check_plugin_versions(&plugin_manifests, &version),
        ],
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI: &str = "[package]\nname = \"pixel-cli\"\nversion = \"0.2.3\"\nedition.workspace = true\n\n[[bin]]\nname = \"pixel\"\n";

    #[test]
    fn normalize_accepts_tag_shapes_and_rejects_the_rest() {
        assert_eq!(normalize_version("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(normalize_version("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(
            normalize_version("refs/tags/v1.2.3").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            normalize_version(" v10.20.30 ").as_deref(),
            Some("10.20.30")
        );
        assert_eq!(
            normalize_version("1.2.3-rc.1").as_deref(),
            Some("1.2.3-rc.1")
        );
        for bad in ["", "v", "1.2", "1.2.3.4", "1.x.3", "vv1.2.3", "1..3"] {
            assert_eq!(normalize_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn package_name_version_reads_only_the_package_table() {
        assert_eq!(
            package_name_version(CLI),
            Some(("pixel-cli".to_string(), "0.2.3".to_string()))
        );
        // A `version` under another table (a dependency) is not the crate's.
        let dep_only = "[dependencies]\nname = \"x\"\nversion = \"9.9.9\"\n";
        assert_eq!(package_name_version(dep_only), None);
        let no_version = "[package]\nname = \"x\"\nedition = \"2024\"\n";
        assert_eq!(package_name_version(no_version), None);
    }

    #[test]
    fn workspace_members_parses_one_line_and_multi_line_lists() {
        let one = "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n";
        assert_eq!(workspace_members(one), vec!["crates/a", "crates/b"]);
        let multi =
            "[workspace]\nmembers = [\n  \"crates/a\",\n  \"crates/b\",\n]\nresolver = \"3\"\n";
        assert_eq!(workspace_members(multi), vec!["crates/a", "crates/b"]);
        assert!(workspace_members("[workspace]\nresolver = \"3\"\n").is_empty());
    }

    const LOCK: &str = "# lock\nversion = 4\n\n[[package]]\nname = \"pixel-cli\"\nversion = \"0.2.3\"\ndependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"pixel-ops\"\nversion = \"0.2.2\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry\"\n";

    #[test]
    fn lock_packages_lists_every_block() {
        assert_eq!(
            lock_packages(LOCK),
            vec![
                ("pixel-cli".to_string(), "0.2.3".to_string()),
                ("pixel-ops".to_string(), "0.2.2".to_string()),
                ("serde".to_string(), "1.0.0".to_string()),
            ]
        );
    }

    fn members(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn cli_version_must_equal_the_tag() {
        let ok = check_cli_version(CLI, "0.2.3");
        assert!(ok.ok, "{ok:?}");
        assert!(ok.detail.contains("pixel-cli 0.2.3"));
        let bad = check_cli_version(CLI, "0.2.4");
        assert!(!bad.ok);
        assert!(
            bad.detail.contains("is 0.2.3, tag says 0.2.4"),
            "{}",
            bad.detail
        );
        let none = check_cli_version("[dependencies]\n", "0.2.3");
        assert!(!none.ok);
    }

    #[test]
    fn lock_check_flags_stale_and_missing_members_only() {
        let fresh = check_lock(
            LOCK,
            &members(&[("pixel-cli", "0.2.3"), ("pixel-ops", "0.2.2")]),
        );
        assert!(fresh.ok, "{fresh:?}");
        assert!(fresh.detail.starts_with("2 workspace members"));

        let stale = check_lock(
            LOCK,
            &members(&[("pixel-cli", "0.2.4"), ("pixel-ops", "0.2.2")]),
        );
        assert!(!stale.ok);
        assert!(
            stale.detail.contains("pixel-cli 0.2.3 (manifest 0.2.4)"),
            "{}",
            stale.detail
        );
        assert!(!stale.detail.contains("pixel-ops"), "{}", stale.detail);
        assert!(stale.detail.contains("cargo build"));

        let missing = check_lock(LOCK, &members(&[("pixel-new", "0.1.0")]));
        assert!(!missing.ok);
        assert!(missing.detail.contains("pixel-new missing from Cargo.lock"));
    }

    #[test]
    fn changelog_needs_the_heading_and_an_empty_unreleased_section() {
        let cut = "# Changelog\n\n## [Unreleased]\n\n## [0.2.3] - 2026-09-12\n\n### Added\n- thing\n\n## [0.2.2] - 2026-09-12\n- old\n";
        let ok = check_changelog(cut, "0.2.3");
        assert!(ok.ok, "{ok:?}");

        let left_over = "## [Unreleased]\n\n### Fixed\n- not moved\n- nor this\n\n## [0.2.3] - 2026-09-12\n- thing\n";
        let bad = check_changelog(left_over, "0.2.3");
        assert!(!bad.ok);
        assert!(
            bad.detail.contains("2 entries still under ## [Unreleased]"),
            "{}",
            bad.detail
        );

        let not_cut = "## [Unreleased]\n- pending\n\n## [0.2.2] - 2026-09-12\n- old\n";
        let bad = check_changelog(not_cut, "0.2.3");
        assert!(!bad.ok);
        assert!(
            bad.detail.contains("no ## [0.2.3] heading; 1 entry under"),
            "{}",
            bad.detail
        );

        let absent = "## [Unreleased]\n\n## [0.2.2] - 2026-09-12\n- old\n";
        let bad = check_changelog(absent, "0.2.3");
        assert!(!bad.ok);
        assert_eq!(bad.detail, "no ## [0.2.3] heading in CHANGELOG.md");

        // `## [0.2.30]` must not satisfy a check for 0.2.3.
        let near = "## [Unreleased]\n\n## [0.2.30] - 2026-09-12\n- x\n";
        assert!(!check_changelog(near, "0.2.3").ok);
    }

    /// `run` reads the four files (plus one manifest per member) from the
    /// repository and assembles the report; a missing file is an error that
    /// names it, not a failed check.
    #[test]
    fn run_reads_the_workspace_files_and_names_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("pixel-release-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/pixel")).unwrap();
        std::fs::create_dir_all(dir.join("crates/pixel-ops")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/pixel\", \"crates/pixel-ops\"]\n",
        )
        .unwrap();
        std::fs::write(dir.join("crates/pixel/Cargo.toml"), CLI).unwrap();
        std::fs::write(
            dir.join("crates/pixel-ops/Cargo.toml"),
            "[package]\nname = \"pixel-ops\"\nversion = \"0.2.2\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("Cargo.lock"), LOCK).unwrap();
        std::fs::write(
            dir.join("CHANGELOG.md"),
            "## [Unreleased]\n\n## [0.2.3] - 2026-09-12\n- thing\n",
        )
        .unwrap();
        let report = run(&dir, "v0.2.3").unwrap();
        assert!(report.ok(), "{}", report.render());
        assert_eq!(report.version, "0.2.3");
        assert!(report.render().contains("2 workspace members"));
        assert!(report.render().contains("0 plugin manifests at 0.2.3"));
        assert!(!run(&dir, "0.2.4").unwrap().ok());

        // A present plugin manifest is checked; an absent one is not required.
        std::fs::write(dir.join("package.json"), r#"{"version": "0.2.2"}"#).unwrap();
        let stale = run(&dir, "0.2.3").unwrap();
        assert!(!stale.ok());
        assert!(
            stale.render().contains("package.json is 0.2.2"),
            "{}",
            stale.render()
        );
        std::fs::write(dir.join("package.json"), r#"{"version": "0.2.3"}"#).unwrap();
        assert!(run(&dir, "0.2.3").unwrap().ok());

        std::fs::remove_file(dir.join("CHANGELOG.md")).unwrap();
        let err = run(&dir, "0.2.3").unwrap_err();
        assert!(err.starts_with("CHANGELOG.md: "), "{err}");
        assert!(run(&dir, "nope").unwrap_err().contains("not a version"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_version_reads_json_and_yaml_manifests() {
        assert_eq!(
            manifest_version("package.json", r#"{"name": "p", "version": "0.2.4"}"#).as_deref(),
            Some("0.2.4")
        );
        assert_eq!(manifest_version("plugin.json", r#"{"name": "p"}"#), None);
        assert_eq!(manifest_version("plugin.json", r#"{"version": 2}"#), None);
        assert_eq!(manifest_version("plugin.json", "not json"), None);
        assert_eq!(
            manifest_version("plugin.yaml", "name: pixel\nversion: 0.2.4\n").as_deref(),
            Some("0.2.4")
        );
        assert_eq!(
            manifest_version("plugin.yaml", "version: \"1.0.0\"\n").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(manifest_version("plugin.yaml", "name: pixel\n"), None);
    }

    #[test]
    fn plugin_versions_must_all_equal_the_release() {
        let manifests = vec![
            ("package.json", r#"{"version": "0.2.4"}"#.to_string()),
            ("plugin.yaml", "version: 0.2.3\n".to_string()),
            ("gemini-extension.json", "{}".to_string()),
        ];
        let check = check_plugin_versions(&manifests, "0.2.4");
        assert!(!check.ok);
        assert_eq!(check.name, "plugin-versions");
        assert_eq!(
            check.detail,
            "plugin.yaml is 0.2.3, gemini-extension.json declares no version; run .agents/skills/release/prepare.sh or set them to 0.2.4"
        );
        let check = check_plugin_versions(&manifests[..1], "0.2.4");
        assert!(check.ok);
        assert_eq!(check.detail, "1 plugin manifests at 0.2.4");
    }

    /// The manifests in this repository follow the workspace version between
    /// releases too, and prepare.sh bumps exactly the checked list: a
    /// manifest added to one and not the other fails here, not on a tag.
    #[test]
    fn repository_plugin_manifests_match_the_workspace_and_prepare_bumps_them() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cli = std::fs::read_to_string(repo.join("crates/pixel/Cargo.toml")).unwrap();
        let (_, version) = package_name_version(&cli).unwrap();
        let manifests: Vec<(&str, String)> = PLUGIN_MANIFESTS
            .iter()
            .map(|rel| (*rel, std::fs::read_to_string(repo.join(rel)).unwrap()))
            .collect();
        let check = check_plugin_versions(&manifests, &version);
        assert!(check.ok, "{}", check.detail);
        let prepare =
            std::fs::read_to_string(repo.join(".agents/skills/release/prepare.sh")).unwrap();
        for rel in PLUGIN_MANIFESTS {
            assert!(prepare.contains(rel), "prepare.sh does not bump {rel}");
        }
    }

    #[test]
    fn report_verdict_and_rendering_follow_the_checks() {
        let report = Report {
            version: "0.2.3".to_string(),
            checks: vec![
                Check::pass("cli-version", "fine"),
                Check::fail("cargo-lock", "stale"),
            ],
        };
        assert!(!report.ok());
        let text = report.render();
        assert!(text.starts_with("release-check 0.2.3\n"));
        assert!(text.contains("[ok  ] cli-version: fine\n"));
        assert!(text.contains("[FAIL] cargo-lock: stale\n"));
        assert!(text.ends_with("release-check: FAILED\n"));
        let json = report.to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][1]["name"], "cargo-lock");
        assert_eq!(json["checks"][1]["ok"], false);

        let all_ok = Report {
            version: "0.2.3".to_string(),
            checks: vec![Check::pass("cli-version", "fine")],
        };
        assert!(all_ok.ok());
        assert!(
            all_ok
                .render()
                .ends_with("release-check: all checks passed\n")
        );
        assert_eq!(all_ok.to_json()["ok"], true);
    }
}
