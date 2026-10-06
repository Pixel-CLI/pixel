// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: the `regions` manifest over a real fixture repo — index +
//! graph built for real, P0 symbols gathered, conflicts/layers/shared files
//! computed from the actual call and import edges.

use std::path::Path;
use std::process::Command;

use pixel_daemon::{Request, Service};

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

/// A fixture whose P0 set and call graph are fully determined by the
/// backticked exact tokens in the task below:
///
/// - `login.rs` defines `login_user` (calls nothing) and `logout_user`;
///   it imports `types.rs`.
/// - `session.rs` defines `start_session`, which calls `login_user`; it
///   imports `login.rs` and `types.rs`.
/// - `types.rs` defines `AuthConfig`, imported by both region files.
/// - `strings.rs` is unrelated.
fn fixture(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gpx-regions-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src/auth")).unwrap();
    std::fs::create_dir_all(dir.join("src/util")).unwrap();
    std::fs::write(
        dir.join("src/auth/login.rs"),
        "use crate::auth::types::AuthConfig;\n\n\
         pub fn login_user(name: &str, _cfg: &AuthConfig) -> bool {\n    \
         !name.is_empty()\n}\n\n\
         pub fn logout_user(name: &str) -> bool {\n    name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/auth/session.rs"),
        "use crate::auth::login::login_user;\n\
         use crate::auth::types::AuthConfig;\n\n\
         pub fn start_session(name: &str, cfg: &AuthConfig) -> bool {\n    \
         login_user(name, cfg)\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/auth/types.rs"),
        "pub struct AuthConfig {\n    pub timeout: u64,\n}\n",
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

fn run_regions(dir: &Path, task: &str, regions: bool) -> serde_json::Value {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Targets {
        task: task.to_string(),
        limit: Some(10),
        max_tier: None,
        precision: false,
        regions,
    });
    assert!(resp.ok, "targets op failed: {:?}", resp.error);
    resp.into_data()
}

/// The regions manifest body (the `regions` key of the targets response).
fn manifest(data: &serde_json::Value) -> &serde_json::Value {
    data.get("regions")
        .expect("regions key present when the flag is set")
}

#[test]
fn regions_absent_without_the_flag() {
    let dir = fixture("flag");
    let data = run_regions(
        &dir,
        "fix `login_user` and `start_session` auth flow",
        false,
    );
    assert!(
        data.get("regions").is_none(),
        "regions must not be computed unless --regions is passed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_manifest_shape_and_witnesses() {
    let dir = fixture("shape");
    let data = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let m = manifest(&data);

    // Every region carries a symbol line range and its context reference.
    let regions = m["regions"].as_array().unwrap();
    assert!(
        regions.len() >= 3,
        "expected the P0 symbols as regions: {regions:?}"
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
        assert_eq!(
            r["context_ref"].as_str().unwrap(),
            r["uid"].as_str().unwrap(),
            "the context reference is the uid `pixel pack-context` resolves"
        );
        assert!(
            r["end_line"].as_u64().unwrap() >= r["start_line"].as_u64().unwrap(),
            "line range must be well-formed: {r:?}"
        );
    }

    // Every conflict carries a reason (the witness).
    let conflicts = m["conflicts"].as_array().unwrap();
    assert!(!conflicts.is_empty(), "the fixture must produce conflicts");
    for c in conflicts {
        let reason = c["reason"].as_str().unwrap();
        assert!(
            [
                "same file",
                "call edge",
                "import adjacency",
                "same name",
                "graph unavailable"
            ]
            .contains(&reason),
            "unknown conflict reason: {reason}"
        );
    }

    // Merge-order layers and the epistemics envelope are present. (The
    // daemon returns the raw analysis report; the CLI nests `layers` under
    // `merge_order` and attaches the envelope when it writes the file.)
    assert!(m["layers"].as_array().is_some());
    assert!(m.get("lower_bound").is_some());
    assert!(m["caps"].as_array().is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_conflicts_pin_the_conservative_direction() {
    let dir = fixture("conflicts");
    let data = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let m = manifest(&data);
    let login_user = "src/auth/login.rs#login_user#function";
    let logout_user = "src/auth/login.rs#logout_user#function";
    let start_session = "src/auth/session.rs#start_session#function";

    let reasons: Vec<(&str, &str, &str)> = m["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["a"].as_str().unwrap(),
                c["b"].as_str().unwrap(),
                c["reason"].as_str().unwrap(),
            )
        })
        .collect();

    // Same file: the two login.rs symbols must never be editable in parallel.
    assert!(
        reasons.contains(&(login_user, logout_user, "same file")),
        "same-file conflict missing: {reasons:?}"
    );
    // Call edge: start_session calls login_user.
    assert!(
        reasons.contains(&(login_user, start_session, "call edge")),
        "call-edge conflict missing: {reasons:?}"
    );
    // Import adjacency: session.rs imports login.rs, so logout_user (in the
    // imported file) conflicts with start_session too.
    assert!(
        reasons.contains(&(logout_user, start_session, "import adjacency")),
        "import-adjacency conflict missing: {reasons:?}"
    );

    // The conservative direction: a pair that MIGHT conflict MUST conflict.
    // Here every cross-file pair conflicts, so no pair may be called disjoint.
    let region_uids: Vec<&str> = m["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uid"].as_str().unwrap())
        .collect();
    for (i, a) in region_uids.iter().enumerate() {
        for b in &region_uids[i + 1..] {
            assert!(
                reasons
                    .iter()
                    .any(|(x, y, _)| (x == a && y == b) || (x == b && y == a)),
                "regions {a} and {b} must not be called disjoint: {reasons:?}"
            );
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_merge_order_puts_callees_first() {
    let dir = fixture("layers");
    let data = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let m = manifest(&data);
    let layers = m["layers"].as_array().unwrap();

    // start_session calls login_user: the callee merges in an earlier layer.
    let layer_of = |needle: &str| -> usize {
        layers
            .iter()
            .find(|l| {
                l["regions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|u| u.as_str().unwrap().contains(needle))
            })
            .map_or_else(
                || panic!("no layer contains {needle}: {layers:?}"),
                |l| l["layer"].as_u64().unwrap() as usize,
            )
    };
    let login_layer = layer_of("login_user");
    let session_layer = layer_of("start_session");
    assert!(
        login_layer < session_layer,
        "the callee (login_user, layer {login_layer}) must merge before its \
         caller (start_session, layer {session_layer})"
    );

    // Layers ascend and are contiguous from 0.
    let nums: Vec<u64> = layers
        .iter()
        .map(|l| l["layer"].as_u64().unwrap())
        .collect();
    let mut sorted = nums.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(nums, sorted, "layers ascend in order");
    assert_eq!(nums[0], 0, "layers start at 0");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_shared_files_carry_the_importer_witness() {
    let dir = fixture("shared");
    let data = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let m = manifest(&data);
    let shared = m["shared_files"].as_array().unwrap();

    // types.rs is imported by both login.rs and session.rs.
    let types = shared
        .iter()
        .find(|s| s["file"].as_str().unwrap() == "src/auth/types.rs")
        .expect("types.rs must be declared shared");
    let importers: Vec<&str> = types["imported_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(
        importers,
        vec!["src/auth/login.rs", "src/auth/session.rs"],
        "the importers are the witness that the file is shared"
    );

    // strings.rs is imported by nobody: not shared.
    assert!(
        !shared
            .iter()
            .any(|s| s["file"].as_str().unwrap().contains("strings")),
        "a file imported by no region is not shared"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_shared_file_outside_p0_is_detected() {
    // A file imported by two region files but not itself P0 must still
    // appear in shared_files — the import edges from region files to
    // non-P0 files are retained for this purpose.
    let dir = fixture("extshared");
    let data = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let m = manifest(&data);
    let shared = m["shared_files"].as_array().unwrap();

    // types.rs is imported by both login.rs and session.rs but is not P0.
    let types = shared
        .iter()
        .find(|s| s["file"].as_str().unwrap() == "src/auth/types.rs")
        .expect("types.rs (non-P0, imported by 2 region files) must be shared");
    let importers: Vec<&str> = types["imported_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(
        importers,
        vec!["src/auth/login.rs", "src/auth/session.rs"],
        "the importers are the witness that the file is shared"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_is_deterministic() {
    let dir = fixture("det");
    let a = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let b = run_regions(&dir, "fix `login_user` and `start_session` auth flow", true);
    let strip = |mut v: serde_json::Value| {
        v.as_object_mut().unwrap().remove("stats");
        v.as_object_mut().unwrap().remove("graph_build");
        v.as_object_mut().unwrap().remove("regions");
        v
    };
    // The regions manifest itself must be byte-identical across runs.
    let ra = a.get("regions").unwrap().clone();
    let rb = b.get("regions").unwrap().clone();
    assert_eq!(ra, rb, "the regions manifest must be deterministic");
    assert_eq!(strip(a), strip(b));
    std::fs::remove_dir_all(&dir).ok();
}
