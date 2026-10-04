//! `pixel space` contract: audit disk taken by `.pixel/` shards across a
//! tree (table and `--json`), and `--delete --yes` remove them.

use std::path::PathBuf;

use crate::support::pixel_command;

fn pixel(args: &[&str]) -> (bool, String, String) {
    let out = pixel_command().args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("px-space-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `.pixel` shard of `bytes` bytes.
fn shard_of(root: &std::path::Path, bytes: usize) {
    let shard = root.join(".pixel");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("base.shard"), vec![0u8; bytes]).unwrap();
}

#[test]
fn table_lists_each_project_and_the_accumulated_total() {
    let base = scratch("table");
    shard_of(&base.join("one"), 100);
    shard_of(&base.join("two"), 200);
    let (_ok, stdout, _err) = pixel(&["space", base.to_str().unwrap()]);
    assert!(stdout.contains("one"), "{stdout}");
    assert!(stdout.contains("two"), "{stdout}");
    assert!(
        stdout.contains("total across 2 projects (300 bytes)"),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn json_reports_per_project_bytes_and_the_total() {
    let base = scratch("json");
    shard_of(&base.join("one"), 1_024); // exactly 1 KiB
    let (ok, stdout, _err) = pixel(&["space", "--json", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["total_bytes"], 1_024);
    assert_eq!(value["entries"][0]["bytes"], 1_024);
    assert_eq!(value["entries"][0]["human"], "1.0 KiB");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn delete_with_yes_removes_the_shards_but_keeps_the_projects() {
    let base = scratch("delete");
    let proj = base.join("one");
    shard_of(&proj, 100);
    assert!(proj.join(".pixel").exists());
    let (ok, stdout, _err) = pixel(&["space", "--delete", "--yes", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    assert!(!proj.join(".pixel").exists(), "shard removed");
    assert!(proj.exists(), "project untouched");
    assert!(stdout.contains("removed "), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}

/// The non-`--yes` delete path reads the confirmation from stdin: a
/// `y` proceeds, so `interactive_confirm` is exercised (and its
/// body-replacement mutant judged) rather than only the `--yes` shortcut.
#[test]
fn delete_without_yes_proceeds_on_a_stdin_yes() {
    use std::io::Write;
    use std::process::Stdio;

    let base = scratch("confirm");
    let proj = base.join("one");
    shard_of(&proj, 100);
    assert!(proj.join(".pixel").exists());
    let mut child = pixel_command()
        .args(["space", "--delete", base.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !proj.join(".pixel").exists(),
        "shard removed after a stdin yes"
    );
    assert!(proj.exists(), "project untouched");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("removed "), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn empty_tree_reports_no_shards() {
    let base = scratch("empty");
    let (ok, stdout, _err) = pixel(&["space", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("no `.pixel` index shards"), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}

/// A non-yes confirmation aborts the delete: nothing is removed and the run
/// reports the abort. Guards the destructive branch — an unconditional
/// confirm would delete the shard here and the existence assert fails.
#[test]
fn delete_without_yes_aborts_on_a_stdin_no() {
    use std::io::Write;
    use std::process::Stdio;

    let base = scratch("decline");
    let proj = base.join("one");
    shard_of(&proj, 100);
    assert!(proj.join(".pixel").exists());
    let mut child = pixel_command()
        .args(["space", "--delete", base.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("aborted"), "{stderr}");
    assert!(proj.join(".pixel").exists(), "shard kept after a decline");
    let _ = std::fs::remove_dir_all(&base);
}
