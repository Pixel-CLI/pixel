// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::process::Command;

use pixel_task::{Check, CheckKind, Criterion, Observation, ObservationKind, TaskContract};
use serde_json::json;

pub fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Task test")
        .env("GIT_COMMITTER_NAME", "Task test")
        .env("GIT_AUTHOR_EMAIL", "task@example.com")
        .env("GIT_COMMITTER_EMAIL", "task@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

pub fn repo() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-q"]);
    std::fs::write(directory.path().join("source.txt"), "correct\n").unwrap();
    std::fs::write(directory.path().join(".gitignore"), ".pixel/\ntarget/\n").unwrap();
    git(directory.path(), &["add", "."]);
    git(
        directory.path(),
        &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"],
    );
    directory
}

pub fn contract(script: &str) -> TaskContract {
    TaskContract {
        objective: "source contains the correct value".into(),
        checks: vec![Check {
            id: "value".into(),
            kind: CheckKind::Argv,
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: ".".into(),
            timeout_ms: 2000,
            required: true,
        }],
        criteria: vec![Criterion {
            id: "correct-value".into(),
            description: "source contains correct".into(),
            checks: vec!["value".into()],
        }],
        ..TaskContract::default()
    }
}

pub fn observations() -> Vec<Observation> {
    [ObservationKind::Scope, ObservationKind::Impact]
        .into_iter()
        .map(|kind| Observation {
            kind,
            source_id: String::new(),
            complete: true,
            data: json!({"fixture":true}),
        })
        .collect()
}
