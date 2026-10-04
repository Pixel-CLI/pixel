// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A repository can commit `.pixel/`, links included. Pixel must then
//! neither write, truncate nor chmod through those links, nor trust the
//! directory's content: these tests run the built binary on a fresh clone
//! of such a repository, and on a checkout where the links were planted
//! without being tracked, and read the link targets back (content and mode).

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::support::{Scratch, pixel_command};

const VICTIM: &str = "victim content\n";
const VICTIM_MODE: u32 = 0o644;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A file outside every repository, `0644`, that a link will point at.
fn victim(base: &Path, name: &str) -> PathBuf {
    let path = base.join(name);
    fs::write(&path, VICTIM).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(VICTIM_MODE)).unwrap();
    path
}

fn assert_intact(path: &Path) {
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        VICTIM,
        "{} was written",
        path.display()
    );
    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, VICTIM_MODE, "{} was chmodded", path.display());
}

fn pixel(args: &[&str]) -> Output {
    pixel_command()
        .args(args)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}

/// `repo/` with one source file, committed.
fn repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("a.rs"), "fn main() {}\n").unwrap();
    git(&repo, &["add", "a.rs"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

/// The commands that wrote through the links on 0.6.1: every logged command
/// appends to `actions.jsonl`, and a history query takes the history lock.
fn run_logged_and_history_commands(root: &Path) -> (Output, Output) {
    let root = root.to_str().unwrap();
    (
        pixel(&["search-content", "-F", "main", root]),
        pixel(&["search-history", "main", root]),
    )
}

#[test]
fn committed_pixel_links_should_neither_be_followed_nor_trusted_in_a_clone() {
    let base = Scratch::for_test("sidecar-trust", "committed");
    let log_victim = victim(&base, "victim.txt");
    let lock_victim = victim(&base, "victim2.txt");
    let origin = repo(&base);
    // Relative, as a committed link must be to reach a file of the person
    // who clones: from `clone/.pixel/`, two levels up is `base`.
    symlink("../../victim.txt", origin.join(".pixel/actions.jsonl")).unwrap();
    symlink("../../victim2.txt", origin.join(".pixel/history.db.lock")).unwrap();
    git(&origin, &["add", "-f", ".pixel"]);
    git(&origin, &["commit", "-qm", "plant"]);
    git(&base, &["clone", "-q", "repo", "clone"]);
    let clone = base.join("clone");

    let (search, history) = run_logged_and_history_commands(&clone);

    assert_intact(&log_victim);
    assert_intact(&lock_victim);
    for output in [&search, &history] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "a tracked .pixel was used: {stderr}"
        );
        assert!(
            stderr.contains("git tracks files under it") && stderr.contains(".pixel/"),
            "the refusal names the cause: {stderr}"
        );
    }
}

#[test]
fn planted_untracked_links_should_not_be_followed() {
    let base = Scratch::for_test("sidecar-trust", "untracked");
    let log_victim = victim(&base, "victim.txt");
    let lock_victim = victim(&base, "victim2.txt");
    let build_victim = victim(&base, "victim3.txt");
    let root = repo(&base);
    symlink(&log_victim, root.join(".pixel/actions.jsonl")).unwrap();
    symlink(&lock_victim, root.join(".pixel/history.db.lock")).unwrap();
    symlink(&build_victim, root.join(".pixel/build.lock")).unwrap();

    run_logged_and_history_commands(&root);

    assert_intact(&log_victim);
    assert_intact(&lock_victim);
    assert_intact(&build_victim);
}

#[test]
fn a_linked_pixel_dir_should_be_refused_without_touching_its_target() {
    let base = Scratch::for_test("sidecar-trust", "linked-dir");
    let root = repo(&base);
    let elsewhere = base.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir(root.join(".pixel")).unwrap();
    symlink(&elsewhere, root.join(".pixel")).unwrap();

    let (search, _) = run_logged_and_history_commands(&root);

    let stderr = String::from_utf8_lossy(&search.stderr);
    assert!(!search.status.success(), "{stderr}");
    assert!(stderr.contains("is a symbolic link"), "{stderr}");
    let mode = fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "the link target was chmodded");
    let written: Vec<_> = fs::read_dir(&elsewhere).unwrap().collect();
    assert!(written.is_empty(), "written through the link: {written:?}");
}
