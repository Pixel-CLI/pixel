//! Integration tests for the `envfile` op — additive-only .env mutations.
//!
//! The hard invariant under test throughout: NO serialized output, error
//! message, or journal record ever contains a value. A sentinel secret is
//! threaded through every subaction and grepped for at the end.

use std::fs;
use std::path::Path;

use pixel_ops::envfile::{envfile, EnvAction};

const SENTINEL: &str = "SENTINEL_SECRET_VALUE_hunter2_XYZ";

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

#[test]
fn set_replace_preserves_unrelated_lines_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = "# database config\nDB_URL=postgres://localhost\n\nexport STRIPE_KEY=\"sk_live_abc\"\n  SPACED = old value # inline\nVONAGE_SECRET=keepme\n";
    write(root, ".env", original);

    let result = envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "STRIPE_KEY".into(),
            value: SENTINEL.into(),
            create_file: false,
        },
    )
    .unwrap();

    assert_eq!(result["action"], "replaced");
    assert_eq!(result["key"], "STRIPE_KEY");
    assert_eq!(result["keys_before"], 4);
    assert_eq!(result["keys_after"], 4);
    assert!(result["snapshot"].as_str().is_some());

    let after = read(root, ".env");
    // The mutated line: prefix (incl. `export ` and key and `=`) preserved,
    // value portion replaced.
    assert!(after.contains(&format!("export STRIPE_KEY={SENTINEL}\n")));
    // Every OTHER line byte-for-byte identical.
    let expected = format!(
        "# database config\nDB_URL=postgres://localhost\n\nexport STRIPE_KEY={SENTINEL}\n  SPACED = old value # inline\nVONAGE_SECRET=keepme\n"
    );
    assert_eq!(after, expected);
}

#[test]
fn set_append_adds_key_with_newline_hygiene() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // No trailing newline on purpose.
    write(root, ".env", "A=1\nB=2");

    let result = envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "NVIDIA_API_KEY".into(),
            value: "nv-123".into(),
            create_file: false,
        },
    )
    .unwrap();

    assert_eq!(result["action"], "appended");
    assert_eq!(result["keys_before"], 2);
    assert_eq!(result["keys_after"], 3);
    assert_eq!(read(root, ".env"), "A=1\nB=2\nNVIDIA_API_KEY=nv-123\n");
}

#[test]
fn set_refuses_missing_file_without_create() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let err = envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "A".into(),
            value: SENTINEL.into(),
            create_file: false,
        },
    )
    .unwrap_err();
    assert!(err.contains("does not exist"), "unexpected error: {err}");
    assert!(!err.contains(SENTINEL), "error message leaked a value");

    // With create_file it works.
    let result = envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "A".into(),
            value: "1".into(),
            create_file: true,
        },
    )
    .unwrap();
    assert_eq!(result["action"], "appended");
    assert!(result["snapshot"].is_null(), "no snapshot for a new file");
    assert_eq!(read(root, ".env"), "A=1\n");
}

#[test]
fn snapshot_restore_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let v1 = "A=1\nB=2\n";
    write(root, ".env", v1);

    envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "B".into(),
            value: "changed".into(),
            create_file: false,
        },
    )
    .unwrap();
    assert_eq!(read(root, ".env"), "A=1\nB=changed\n");

    let snaps = envfile(root, &EnvAction::Snapshots { file: ".env".into() }).unwrap();
    assert_eq!(snaps["snapshot_count"], 1);
    let snap = &snaps["snapshots"][0];
    assert_eq!(snap["keys"], 2);
    assert_eq!(snap["bytes"], v1.len() as u64);

    let restored = envfile(
        root,
        &EnvAction::Restore {
            file: ".env".into(),
            snapshot: None,
        },
    )
    .unwrap();
    assert_eq!(read(root, ".env"), v1);
    let keys_now: Vec<&str> = restored["keys_now"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(keys_now, vec!["A", "B"]);
}

#[test]
fn restore_of_restore_returns_to_pre_restore_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let v1 = "A=1\n";
    write(root, ".env", v1);

    // Mutate → snapshot(v1) taken; file is now v2.
    envfile(
        root,
        &EnvAction::Set {
            file: ".env".into(),
            key: "A".into(),
            value: "2".into(),
            create_file: false,
        },
    )
    .unwrap();
    let v2 = read(root, ".env");
    assert_eq!(v2, "A=2\n");

    // Restore latest (v1). The pre-restore state (v2) is snapshotted first.
    envfile(
        root,
        &EnvAction::Restore {
            file: ".env".into(),
            snapshot: None,
        },
    )
    .unwrap();
    assert_eq!(read(root, ".env"), v1);

    // Restore latest again → the latest snapshot is now v2 (pre-restore
    // state), so a second restore undoes the first.
    let second = envfile(
        root,
        &EnvAction::Restore {
            file: ".env".into(),
            snapshot: None,
        },
    )
    .unwrap();
    assert_eq!(read(root, ".env"), v2);
    assert!(second["restored_from"].as_str().is_some());

    // Named restore also works: pick the OLDEST snapshot explicitly (v1).
    let snaps = envfile(root, &EnvAction::Snapshots { file: ".env".into() }).unwrap();
    let oldest = snaps["snapshots"][0]["id"].as_str().unwrap().to_string();
    envfile(
        root,
        &EnvAction::Restore {
            file: ".env".into(),
            snapshot: Some(oldest),
        },
    )
    .unwrap();
    assert_eq!(read(root, ".env"), v1);
}

#[test]
fn restore_missing_snapshot_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".env", "A=1\n");

    let err = envfile(
        root,
        &EnvAction::Restore {
            file: ".env".into(),
            snapshot: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("no snapshots"), "unexpected error: {err}");
}

#[test]
fn check_reports_missing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".env", "PRESENT_ONE=x\n# MISSING_COMMENTED=y\nPRESENT_TWO=z\n");

    let result = envfile(
        root,
        &EnvAction::Check {
            file: ".env".into(),
            require: vec![
                "PRESENT_ONE".into(),
                "MISSING_COMMENTED".into(),
                "NEVER_THERE".into(),
                "PRESENT_TWO".into(),
            ],
        },
    )
    .unwrap();

    assert_eq!(result["ok"], false);
    assert_eq!(
        result["present"],
        serde_json::json!(["PRESENT_ONE", "PRESENT_TWO"])
    );
    assert_eq!(
        result["missing"],
        serde_json::json!(["MISSING_COMMENTED", "NEVER_THERE"])
    );

    let ok = envfile(
        root,
        &EnvAction::Check {
            file: ".env".into(),
            require: vec!["PRESENT_ONE".into()],
        },
    )
    .unwrap();
    assert_eq!(ok["ok"], true);

    // Missing file → all missing, ok false, not an error.
    let gone = envfile(
        root,
        &EnvAction::Check {
            file: ".env.production".into(),
            require: vec!["X".into()],
        },
    )
    .unwrap();
    assert_eq!(gone["ok"], false);
    assert_eq!(gone["file_exists"], false);
    assert_eq!(gone["missing"], serde_json::json!(["X"]));
}

#[test]
fn inventory_lists_env_files_with_key_names_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".env", &format!("TOP_KEY={SENTINEL}\n"));
    write(root, "apps/web/.env.local", "WEB_KEY=web\nWEB_TWO=2\n");
    // Skipped directories.
    write(root, "node_modules/pkg/.env", "NM_KEY=nope\n");
    write(root, ".git/.env", "GIT_KEY=nope\n");
    write(root, "target/.env", "TARGET_KEY=nope\n");
    // Beyond depth 4 (a/b/c/d/e = depth 5).
    write(root, "a/b/c/d/e/.env", "DEEP_KEY=nope\n");

    let result = envfile(root, &EnvAction::Inventory).unwrap();
    let files = result["files"].as_array().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec![".env", "apps/web/.env.local"]);

    let top = &files[0];
    assert_eq!(top["keys"], serde_json::json!(["TOP_KEY"]));
    assert_eq!(top["line_count"], 1);
    assert_eq!(top["snapshot_count"], 0);
    let web = &files[1];
    assert_eq!(web["keys"], serde_json::json!(["WEB_KEY", "WEB_TWO"]));
}

/// THE hard invariant: run every subaction with a sentinel secret in play,
/// serialize every output (and every error), read the journal — and assert
/// the sentinel appears in NONE of them.
#[test]
fn no_output_or_journal_ever_contains_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        ".env",
        &format!("EXISTING={SENTINEL}\nOTHER={SENTINEL}-b\n"),
    );

    let mut serialized_outputs: Vec<String> = Vec::new();
    let actions: Vec<EnvAction> = vec![
        EnvAction::Inventory,
        EnvAction::Set {
            file: ".env".into(),
            key: "EXISTING".into(),
            value: format!("{SENTINEL}-new"),
            create_file: false,
        },
        EnvAction::Set {
            file: ".env".into(),
            key: "BRAND_NEW".into(),
            value: format!("{SENTINEL}-appended"),
            create_file: false,
        },
        EnvAction::Snapshots { file: ".env".into() },
        EnvAction::Restore {
            file: ".env".into(),
            snapshot: None,
        },
        EnvAction::Check {
            file: ".env".into(),
            require: vec!["EXISTING".into(), "NOPE".into()],
        },
        EnvAction::Inventory,
        // Error paths must not leak either.
        EnvAction::Set {
            file: ".env.missing".into(),
            key: "K".into(),
            value: SENTINEL.into(),
            create_file: false,
        },
        EnvAction::Restore {
            file: ".env.missing".into(),
            snapshot: Some("nope".into()),
        },
    ];
    for action in &actions {
        match envfile(root, action) {
            Ok(v) => serialized_outputs.push(serde_json::to_string(&v).unwrap()),
            Err(e) => serialized_outputs.push(e),
        }
    }

    for out in &serialized_outputs {
        assert!(
            !out.contains(SENTINEL),
            "output leaked a secret value: {out}"
        );
    }

    // Journal records must carry ts/file/action/key — never a value.
    let journal = fs::read_to_string(root.join(".pixel/env-snapshots/journal.jsonl")).unwrap();
    assert!(
        !journal.contains(SENTINEL),
        "journal leaked a secret value: {journal}"
    );
    let lines: Vec<&str> = journal.lines().collect();
    assert!(lines.len() >= 3, "expected journal records for set/set/restore");
    for line in &lines {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(record["ts"].is_string());
        assert!(record["file"].is_string());
        assert!(record["action"].is_string());
        assert!(record.get("value").is_none());
    }

    // Snapshots themselves DO contain values (they are recovery copies) —
    // but they live under .pixel/, which is never committed. Sanity-check
    // they exist so restore has something to work from.
    let snap_dir = root.join(".pixel/env-snapshots");
    assert!(snap_dir.join("journal.jsonl").exists());
}
