// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::os::unix::fs::PermissionsExt;

use clap::CommandFactory;

use super::*;
use crate::cli::Cli;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pixel-plugin-cmd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn host_at(dir: &Path) -> Host {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    Host {
        home: dir.join("home"),
        repo_root: None,
        path: bin.into_os_string(),
        github_base: None,
    }
}

fn env() -> Env<'static> {
    Env {
        repo_root: Path::new("/repo"),
        graph_db: Path::new("/repo/.pixel/graph.db"),
        bin: Path::new("/bin/pixel"),
    }
}

fn executable(path: &Path) {
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_name_no_plugin_provides_is_left_to_clap() {
    let dir = scratch("unknown");
    let host = host_at(&dir);
    assert!(matches!(
        plan(&host, "made-up-cmd", &[], env()),
        Plan::Unknown
    ));
    assert!(matches!(
        plan(&host, "Not Valid", &[], env()),
        Plan::Unknown
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_on_path_runs_and_receives_the_args() {
    let dir = scratch("path");
    let host = host_at(&dir);
    let exe = dir.join("bin/pixel-made-up-cmd");
    executable(&exe);
    let args = vec![OsString::from("a"), OsString::from("--b")];
    let Plan::Run(process) = plan(&host, "made-up-cmd", &args, env()) else {
        panic!("expected the PATH plugin to run");
    };
    assert_eq!(process.get_program(), exe.as_os_str());
    assert_eq!(process.get_args().collect::<Vec<_>>(), ["a", "--b"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resolution_failure_stops_with_its_reason_and_exit_one() {
    let dir = scratch("untrusted");
    let host = Host {
        repo_root: Some(dir.join("repo")),
        ..host_at(&dir)
    };
    let plugin = dir.join("repo/.pixel/plugins/tool");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("pixel-plugin.toml"),
        "name = \"tool\"\nversion = \"1\"\napi = 1\ndescription = \"d\"\nrun = \"run\"\ncapabilities = [\"command\"]\n",
    )
    .unwrap();
    std::fs::write(plugin.join("run"), "#!/bin/sh\n").unwrap();
    let Plan::Stop(message, code) = plan(&host, "tool", &[], env()) else {
        panic!("expected a refusal");
    };
    assert_eq!(code, 1);
    assert!(message.contains("`pixel plugin trust tool`"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The clap error for `line`, built on a thread with room for the whole
/// command tree (the test thread's default stack is too small for it).
fn parse_error(line: &'static str) -> clap::Error {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            Cli::command()
                .try_get_matches_from(line.split_whitespace())
                .unwrap_err()
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn only_the_top_level_unknown_word_is_dispatched() {
    let top = parse_error("pixel nosuchcmd arg");
    assert!(names_the_unknown_command(&top, "nosuchcmd"));
    assert!(!names_the_unknown_command(&top, "arg"));
    let nested = parse_error("pixel plugin nosuchcmd");
    assert!(
        !names_the_unknown_command(&nested, "plugin"),
        "a nested typo is clap's to report"
    );
    let usage = parse_error("pixel search-content");
    assert!(!names_the_unknown_command(&usage, "search-content"));
}

fn installed(dir: &Path, name: &str, binary: Binary) -> pixel_plugin::Added {
    pixel_plugin::Added {
        name: name.to_owned(),
        dir: dir.join(name),
        manifest: pixel_plugin::Manifest {
            name: name.to_owned(),
            version: "1.4.2".to_owned(),
            api: 1,
            description: "d".to_owned(),
            run: "bin/tool".to_owned(),
            capabilities: vec![pixel_plugin::Capability::Command],
            network: false,
            commands: vec![],
            skill: None,
            probe: None,
        },
        binary,
        skill: None,
    }
}

#[test]
fn the_install_report_names_what_was_downloaded_or_how_to_build_it() {
    let dir = Path::new("/h/.pixel/plugins");
    assert_eq!(
        render_installed(&installed(dir, "tool", Binary::Present)),
        "installed plugin tool 1.4.2 in /h/.pixel/plugins/tool\n"
    );
    let url = "https://github.com/acme/tool/releases/latest/download/pixel-tool-x".to_owned();
    let digest = "ab".repeat(32);
    let verified = Binary::Downloaded(pixel_plugin::Downloaded {
        url: url.clone(),
        verified: true,
        sha256: digest.clone(),
    });
    assert_eq!(
        render_installed(&installed(dir, "tool", verified)),
        format!(
            "installed plugin tool 1.4.2 in /h/.pixel/plugins/tool\ndownloaded {url} (sha256 {digest} verified)\n"
        )
    );
    let unverified = Binary::Downloaded(pixel_plugin::Downloaded {
        url: url.clone(),
        verified: false,
        sha256: digest.clone(),
    });
    assert!(
        render_installed(&installed(dir, "tool", unverified)).ends_with(&format!(
            "downloaded {url} (UNVERIFIED: no .sha256 published, installed by --allow-unverified; sha256 {digest})\n"
        ))
    );
    let missing = Binary::Missing {
        git_url: "https://github.com/acme/tool.git".to_owned(),
        reference: Some("v2".to_owned()),
        download_error: "cannot download u: HTTP 404".to_owned(),
    };
    let text = render_installed(&installed(dir, "tool", missing));
    assert!(
        text.contains("tool is installed but not runnable: its program `bin/tool` is not built and no release asset was usable (cannot download u: HTTP 404)\n"),
        "{text}"
    );
    assert!(
        text.contains("build it with: cargo install --git https://github.com/acme/tool.git --rev v2 --root /h/.pixel/plugins/tool\n"),
        "{text}"
    );
}

#[test]
fn install_json_should_report_the_asset_url_digest_and_verification() {
    let dir = Path::new("/h/.pixel/plugins");
    let url = "https://github.com/acme/tool/releases/latest/download/pixel-tool-x";
    let unverified = Binary::Downloaded(pixel_plugin::Downloaded {
        url: url.to_owned(),
        verified: false,
        sha256: "cd".repeat(32),
    });
    assert_eq!(
        installed_json(&installed(dir, "tool", unverified)),
        json!({
            "name": "tool",
            "version": "1.4.2",
            "dir": "/h/.pixel/plugins/tool",
            "network": false,
            "binary": {
                "kind": "downloaded",
                "url": url,
                "sha256": "cd".repeat(32),
                "verified": false,
            },
        })
    );
    let missing = Binary::Missing {
        git_url: "https://github.com/acme/tool.git".to_owned(),
        reference: None,
        download_error: "no digest".to_owned(),
    };
    assert_eq!(
        installed_json(&installed(dir, "tool", missing))["binary"],
        json!({
            "kind": "missing",
            "git_url": "https://github.com/acme/tool.git",
            "reference": null,
            "download_error": "no digest",
        })
    );
    assert_eq!(
        installed_json(&installed(dir, "tool", Binary::Present))["binary"],
        json!({ "kind": "present" })
    );
}

#[test]
fn a_shipped_program_without_a_release_asset_names_the_build_that_replaces_it() {
    let shipped = Binary::Shipped {
        git_url: "https://github.com/acme/tool.git".to_owned(),
        reference: None,
        download_error: "cannot download u: HTTP 404".to_owned(),
    };
    assert_eq!(
        render_installed(&installed(Path::new("/h/.pixel/plugins"), "tool", shipped)),
        "installed plugin tool 1.4.2 in /h/.pixel/plugins/tool\n\
         tool runs the repository's own `bin/tool`: no release asset replaced it (cannot download u: HTTP 404)\n\
         if `tool` then reports a missing program, build it into the plugin with: \
         cargo install --git https://github.com/acme/tool.git --root /h/.pixel/plugins/tool --force\n"
    );
}

#[test]
fn a_network_plugin_tells_the_user_to_enable_it() {
    let mut added = installed(Path::new("/h"), "net", Binary::Present);
    added.manifest.network = true;
    assert!(
        render_installed(&added)
            .ends_with("net declares network access: run `pixel plugin enable net` to allow it\n")
    );
}

#[test]
fn a_plugins_exit_code_is_its_own_and_a_signal_is_128_plus_it() {
    assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
    assert_eq!(exit_code(ExitStatus::from_raw(7 << 8)), 7);
    assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
    assert_eq!(exit_code(ExitStatus::from_raw(15)), 143);
}
