// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel completions <shell>` and `pixel man`: both are generated from the
//! live clap tree, so a script or page that misses a subcommand of
//! `pixel --help` is one a user cannot tab to or read about. The scripts are
//! text checked here, never sourced: no host shell is needed.

use crate::support::{built_in_commands, pixel_command};

fn stdout_of(args: &[&str]) -> String {
    let out = pixel_command().args(args).output().unwrap();
    assert!(out.status.success(), "pixel {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn completions_should_name_every_subcommand_for_each_supported_shell() {
    let commands = built_in_commands();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let script = stdout_of(&["completions", shell]);
        let missing: Vec<&String> = commands
            .iter()
            .filter(|name| !script.contains(name.as_str()))
            .collect();
        assert!(missing.is_empty(), "{shell} completions miss {missing:?}");
    }
}

#[test]
fn fish_completions_should_register_against_the_pixel_command() {
    let script = stdout_of(&["completions", "fish"]);
    assert!(
        script.contains("complete -c pixel ") && script.contains("-a \"build-index\""),
        "fish script does not complete `pixel build-index`:\n{script}"
    );
}

#[test]
fn zsh_completions_should_define_the_pixel_completion_function() {
    let script = stdout_of(&["completions", "zsh"]);
    assert!(
        script.starts_with("#compdef pixel\n"),
        "zsh script lacks its #compdef header:\n{}",
        &script[..script.len().min(200)]
    );
    assert!(
        script.contains("'build-index:"),
        "zsh script misses build-index"
    );
}

#[test]
fn completions_should_end_quietly_when_the_reader_closes_the_pipe() {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = pixel_command()
        .args(["completions", "zsh"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read one byte, then close the pipe, as `| head -c1` does.
    let mut first = [0u8; 1];
    child.stdout.take().unwrap().read_exact(&mut first).unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{out:?}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn completions_should_refuse_an_unknown_shell() {
    let out = pixel_command()
        .args(["completions", "tcsh"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn man_should_render_a_section_1_page_listing_every_subcommand() {
    let page = stdout_of(&["man"]);
    assert!(
        page.contains("\n.TH pixel 1 "),
        "no `.TH pixel 1` header:\n{}",
        &page[..page.len().min(300)]
    );
    let missing: Vec<String> = built_in_commands()
        .into_iter()
        .filter(|name| !page.contains(&format!("pixel\\-{}", name.replace('-', "\\-"))))
        .collect();
    assert!(missing.is_empty(), "man page misses {missing:?}");
}
