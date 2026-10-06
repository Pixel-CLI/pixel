// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! CLI round-trip: `scope-task --regions` writes `.pixel/regions.json` beside
//! `targets.json` — symbol line ranges + context reference, conservative
//! conflict pairs with reasons, merge-order layers, and shared files.
//! `--no-manifest` and `--read-only` suppress it.

use std::path::Path;
use std::process::Command;

use crate::support::{Scratch, pixel_command};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn gitpixel(dir: &Path, args: &[&str]) -> std::process::Output {
    pixel_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// Fixture: login.rs (login_user, logout_user), session.rs (start_session
/// calls login_user, imports login + types), types.rs (AuthConfig),
/// util/strings.rs (pad_left). Task names login_user + start_session.
///
/// `tag` must be unique per test: `Scratch::for_test` keys on the process id,
/// so parallel tests in one binary collide on a shared tag.
fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("gpx-regions-cli", tag);
    std::fs::create_dir_all(dir.join("src/auth")).unwrap();
    std::fs::create_dir_all(dir.join("src/util")).unwrap();
    std::fs::write(
        dir.join("src/auth/login.rs"),
        "use crate::auth::types::AuthConfig;\n\npub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n\npub fn logout_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/auth/session.rs"),
        "use crate::auth::login::login_user;\nuse crate::auth::types::AuthConfig;\n\npub fn start_session(name: &str) -> bool {\n    login_user(name)\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/auth/types.rs"),
        "pub struct AuthConfig {\n    pub ttl_secs: u64,\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/util/strings.rs"),
        "pub fn pad_left(s: &str, n: usize) -> String {\n    format!(\"{s:>n$}\")\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

fn regions_manifest(dir: &Path) -> serde_json::Value {
    let path = dir.join(".pixel/regions.json");
    assert!(path.exists(), "regions manifest not written");
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

#[test]
fn regions_manifest_written_beside_targets() {
    let dir = fixture("shape");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");

    // Both manifests exist side by side.
    assert!(
        dir.join(".pixel/targets.json").exists(),
        "targets.json missing"
    );
    let m = regions_manifest(&dir);

    // Metadata: version, kind, note, task.
    assert_eq!(m["version"], 1);
    assert_eq!(m["kind"], "evidence");
    assert!(
        m["note"]
            .as_str()
            .unwrap()
            .contains("not an action recommendation"),
        "note must disclaim action recommendation: {:?}",
        m["note"]
    );
    assert_eq!(m["task"], "fix `login_user` and `start_session` auth flow");
    assert!(m["created_unix"].as_u64().unwrap() > 0);
    assert!(!m["head_oid"].as_str().unwrap().is_empty());

    // Regions: symbol line ranges + context reference.
    let regions = m["regions"].as_array().unwrap();
    assert!(
        regions.len() >= 3,
        "expected at least 3 regions, got {regions:?}"
    );
    for r in regions {
        for key in [
            "uid",
            "name",
            "kind",
            "file",
            "start_line",
            "end_line",
            "context_ref",
        ] {
            assert!(r.get(key).is_some(), "region missing {key}: {r:?}");
        }
        assert!(
            r["end_line"].as_u64().unwrap() >= r["start_line"].as_u64().unwrap(),
            "end_line must be >= start_line: {r:?}"
        );
        assert_eq!(
            r["context_ref"].as_str().unwrap(),
            r["uid"].as_str().unwrap(),
            "context_ref must equal uid"
        );
    }
    let names: Vec<&str> = regions
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"login_user"),
        "login_user region missing: {names:?}"
    );
    assert!(
        names.contains(&"start_session"),
        "start_session region missing: {names:?}"
    );

    // Conflicts: pairs with reasons.
    let conflicts = m["conflicts"].as_array().unwrap();
    assert!(
        !conflicts.is_empty(),
        "conflicts must not be empty for this fixture"
    );
    let known_reasons = [
        "same file",
        "call edge",
        "import adjacency",
        "same name",
        "graph unavailable",
    ];
    for c in conflicts {
        assert!(c["a"].as_str().is_some(), "conflict missing a: {c:?}");
        assert!(c["b"].as_str().is_some(), "conflict missing b: {c:?}");
        let reason = c["reason"].as_str().unwrap();
        assert!(
            known_reasons.contains(&reason),
            "unknown conflict reason: {reason}"
        );
    }

    // Merge-order layers.
    let layers = m["merge_order"]["layers"].as_array().unwrap();
    assert!(!layers.is_empty(), "merge_order.layers must not be empty");
    for l in layers {
        assert!(l["layer"].as_u64().is_some(), "layer missing index: {l:?}");
        assert!(
            l["regions"].as_array().is_some(),
            "layer missing regions: {l:?}"
        );
    }

    // Shared files.
    assert!(m.get("shared_files").is_some(), "shared_files key missing");

    // Epistemics envelope.
    assert!(m["epistemics"].get("lower_bound").is_some());
    assert!(m["epistemics"].get("caps").is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_conflicts_pin_the_conservative_direction() {
    let dir = fixture("conflicts");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    let m = regions_manifest(&dir);
    let regions = m["regions"].as_array().unwrap();
    let uid = |name: &str| -> String {
        regions
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("region {name} not found"))["uid"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let login_user = uid("login_user");
    let logout_user = uid("logout_user");
    let start_session = uid("start_session");

    let conflicts = m["conflicts"].as_array().unwrap();
    let has_conflict = |a: &str, b: &str| -> bool {
        conflicts.iter().any(|c| {
            let ca = c["a"].as_str().unwrap();
            let cb = c["b"].as_str().unwrap();
            (ca == a && cb == b) || (ca == b && cb == a)
        })
    };

    // Same file: login_user and logout_user both in login.rs.
    assert!(
        has_conflict(&login_user, &logout_user),
        "same-file pair must conflict"
    );
    // Call edge: start_session calls login_user.
    assert!(
        has_conflict(&login_user, &start_session),
        "call-edge pair must conflict"
    );
    // Import adjacency: session.rs imports login.rs (logout_user's file).
    assert!(
        has_conflict(&logout_user, &start_session),
        "import-adjacency pair must conflict"
    );

    // The conservative direction: every conflict carries a structural witness
    // (same file, call edge, or import adjacency) — never a bare "disjoint".
    for c in conflicts {
        let reason = c["reason"].as_str().unwrap();
        assert!(
            reason == "same file" || reason == "call edge" || reason == "import adjacency",
            "conflict reason must be a structural witness: {reason}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_merge_order_puts_callees_first() {
    let dir = fixture("merge");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    let m = regions_manifest(&dir);
    let regions = m["regions"].as_array().unwrap();
    let uid = |name: &str| -> String {
        regions
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("region {name} not found"))["uid"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let login_user = uid("login_user");
    let start_session = uid("start_session");

    let layers = m["merge_order"]["layers"].as_array().unwrap();
    let layer_of = |u: &str| -> u64 {
        layers
            .iter()
            .find(|l| {
                l["regions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r.as_str() == Some(u))
            })
            .unwrap_or_else(|| panic!("region {u} not in any layer"))["layer"]
            .as_u64()
            .unwrap()
    };

    // login_user (callee) must merge before start_session (caller).
    assert!(
        layer_of(&login_user) < layer_of(&start_session),
        "callee must merge before caller"
    );

    // Layers ascend contiguously from 0.
    let mut indices: Vec<u64> = layers
        .iter()
        .map(|l| l["layer"].as_u64().unwrap())
        .collect();
    indices.sort_unstable();
    for (i, &idx) in indices.iter().enumerate() {
        assert_eq!(idx, i as u64, "layers must be contiguous from 0");
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_shared_files_carry_the_importer_witness() {
    let dir = fixture("shared");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    let m = regions_manifest(&dir);
    let shared = m["shared_files"].as_array().unwrap();

    // types.rs is imported by session.rs (a region file) — must be shared.
    let types_entry = shared
        .iter()
        .find(|f| f["file"].as_str().unwrap().contains("types.rs"))
        .expect("types.rs must be in shared_files");
    let importers = types_entry["imported_by"].as_array().unwrap();
    assert!(
        importers
            .iter()
            .any(|p| p.as_str().unwrap().contains("session.rs")),
        "session.rs must be listed as importer: {importers:?}"
    );

    // strings.rs is imported by no region file — must NOT be shared.
    assert!(
        !shared
            .iter()
            .any(|f| f["file"].as_str().unwrap().contains("strings.rs")),
        "strings.rs must not be shared"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_absent_without_the_flag() {
    let dir = fixture("absent");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    assert!(
        !dir.join(".pixel/regions.json").exists(),
        "regions.json must not be written without --regions"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_suppressed_by_no_manifest() {
    let dir = fixture("nomanifest");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--no-manifest",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    assert!(
        !dir.join(".pixel/regions.json").exists(),
        "--no-manifest must suppress regions.json"
    );
    assert!(
        !dir.join(".pixel/targets.json").exists(),
        "--no-manifest must suppress targets.json"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_suppressed_by_read_only() {
    let dir = fixture("readonly");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--read-only",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    assert!(
        !dir.join(".pixel/regions.json").exists(),
        "--read-only must suppress regions.json"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_json_output_carries_regions_key() {
    let dir = fixture("jsonout");
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        data.get("regions").is_some(),
        "JSON output must carry a regions key"
    );
    // The wire format is the raw RegionsReport object.
    let regions = data["regions"].as_object().unwrap();
    assert!(
        regions.contains_key("regions"),
        "regions report must contain a regions array"
    );
    assert!(
        regions.contains_key("conflicts"),
        "regions report must contain a conflicts array"
    );
    assert!(
        regions.contains_key("layers"),
        "regions report must contain a layers array"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_is_deterministic() {
    let dir = fixture("determinism");
    let args = [
        "scope-task",
        "fix `login_user` and `start_session` auth flow",
        ".",
        "--regions",
        "--json",
    ];
    let out1 = gitpixel(&dir, &args);
    assert!(out1.status.success());
    let m1 = regions_manifest(&dir);

    // Remove and re-run; the manifest must be identical.
    std::fs::remove_file(dir.join(".pixel/regions.json")).unwrap();
    let out2 = gitpixel(&dir, &args);
    assert!(out2.status.success());
    let m2 = regions_manifest(&dir);

    // created_unix is a wall-clock timestamp; strip it before comparing.
    let strip = |mut v: serde_json::Value| {
        v.as_object_mut().unwrap().remove("created_unix");
        v
    };
    assert_eq!(
        strip(m1),
        strip(m2),
        "regions manifest must be deterministic"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_clear_removes_regions_json() {
    let dir = fixture("clear");
    // Write both manifests.
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success());
    assert!(dir.join(".pixel/targets.json").exists());
    assert!(dir.join(".pixel/regions.json").exists());

    // --clear removes both.
    let out = gitpixel(&dir, &["scope-task", "--clear", "."]);
    assert!(out.status.success());
    assert!(!dir.join(".pixel/targets.json").exists());
    assert!(
        !dir.join(".pixel/regions.json").exists(),
        "--clear must remove regions.json"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_removed_when_new_targets_without_regions() {
    let dir = fixture("stale");
    // Write both manifests.
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--regions",
            "--json",
        ],
    );
    assert!(out.status.success());
    assert!(dir.join(".pixel/regions.json").exists());

    // A new targets.json without --regions removes the stale regions.json.
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` and `start_session` auth flow",
            ".",
            "--json",
        ],
    );
    assert!(out.status.success());
    assert!(dir.join(".pixel/targets.json").exists());
    assert!(
        !dir.join(".pixel/regions.json").exists(),
        "stale regions.json must be removed when writing targets without --regions"
    );
    std::fs::remove_dir_all(&dir).ok();
}
