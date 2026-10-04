// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel commit --message-file`: a multi-paragraph commit message from a
//! file (or stdin) lands in the commit verbatim, and `-m` and `-F` together
//! are a usage error before any git state is touched.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::support::{Scratch, pixel_command};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn seeded_repo(tag: &str) -> Scratch {
    let repo = Scratch::for_test("pixel-publish-cli", tag);
    std::fs::write(repo.join("lib.rs"), "pub fn seed() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    repo
}

const MESSAGE: &str =
    "feat(cli): message file\n\nSecond paragraph, kept as is.\n\n- a bullet\n- another\n";

#[test]
fn publish_should_commit_the_file_body_when_message_file_is_given() {
    let repo = seeded_repo("file");
    std::fs::write(repo.join("lib.rs"), "pub fn seed() -> u8 { 1 }\n").unwrap();
    let msg = repo.join("msg.txt");
    std::fs::write(&msg, MESSAGE).unwrap();
    let out = pixel_command()
        .current_dir(&repo)
        .args(["commit", "--message-file"])
        .arg(&msg)
        .args(["--files", "lib.rs", "--request-id", "publish-file-1"])
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let body = git(&repo, &["log", "-1", "--format=%B"]);
    assert_eq!(body.trim_end(), MESSAGE.trim_end());
}

#[test]
fn publish_should_read_stdin_when_message_file_is_dash() {
    let repo = seeded_repo("stdin");
    std::fs::write(repo.join("lib.rs"), "pub fn seed() -> u8 { 2 }\n").unwrap();
    let mut child = pixel_command()
        .current_dir(&repo)
        .args([
            "commit",
            "-F",
            "-",
            "--files",
            "lib.rs",
            "--request-id",
            "publish-stdin-1",
        ])
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), MESSAGE.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let body = git(&repo, &["log", "-1", "--format=%B"]);
    assert_eq!(body.trim_end(), MESSAGE.trim_end());
}

#[test]
fn publish_should_refuse_both_message_flags_and_leave_head_alone() {
    let repo = seeded_repo("conflict");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("lib.rs"), "pub fn seed() -> u8 { 3 }\n").unwrap();
    let msg = repo.join("msg.txt");
    std::fs::write(&msg, MESSAGE).unwrap();
    let out = pixel_command()
        .current_dir(&repo)
        .args(["commit", "-m", "inline", "--message-file"])
        .arg(&msg)
        .args(["--request-id", "publish-conflict-1"])
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);

    let blank = repo.join("blank.txt");
    std::fs::write(&blank, "\n\n").unwrap();
    let out = pixel_command()
        .current_dir(&repo)
        .args(["commit", "-F"])
        .arg(&blank)
        .args(["--request-id", "publish-blank-1"])
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("commit message is empty"),
        "{out:?}"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
}
