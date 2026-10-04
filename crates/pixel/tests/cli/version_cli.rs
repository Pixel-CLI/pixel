// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel -V` / `pixel --version`: the one-line form stays exactly
//! `pixel x.y.z` (the Homebrew formula test and `pixel doctor` read it), and
//! the long form carries the build provenance `build.rs` captured. The test
//! binary is built from this checkout, so the commit is known here and in
//! CI: an `unknown` commit means the build script lost its git access.

use std::process::Command;

fn pixel(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn short_version_is_one_line() {
    assert_eq!(
        pixel(&["-V"]),
        format!("pixel {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn long_version_reports_commit_target_rustc_and_date_from_this_checkout() {
    let text = pixel(&["--version"]);
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(format!("pixel {}", env!("CARGO_PKG_VERSION")).as_str())
    );

    let commit = lines
        .next()
        .and_then(|l| l.strip_prefix("commit: "))
        .expect("commit line");
    let sha = commit.strip_suffix("-dirty").unwrap_or(commit);
    assert!(
        sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "commit must be the checkout's full sha, got {commit:?}"
    );

    let target = lines
        .next()
        .and_then(|l| l.strip_prefix("target: "))
        .expect("target line");
    assert!(target.contains('-'), "a target triple, got {target:?}");

    let rustc = lines
        .next()
        .and_then(|l| l.strip_prefix("rustc: "))
        .expect("rustc line");
    assert!(rustc.starts_with("rustc "), "got {rustc:?}");

    let built = lines
        .next()
        .and_then(|l| l.strip_prefix("built: "))
        .expect("built line");
    let parts: Vec<&str> = built.split('-').collect();
    assert!(
        parts.len() == 3 && parts[0].len() == 4 && parts[1].len() == 2 && parts[2].len() == 2,
        "YYYY-MM-DD, got {built:?}"
    );
    assert_eq!(lines.next(), None, "nothing after the provenance block");
}
