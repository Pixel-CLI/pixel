// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel rename` end-to-end: a real subprocess against a real repo fixture.
//! The assertions read the files back off disk — JSON shape alone would not
//! prove the rewrite happened.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-rename-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/login.ts"),
            "export function loginUser(name: string): boolean {\n    return name.length > 0;\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/caller.ts"),
            "import { loginUser } from \"./login\";\n\nexport function go(): boolean {\n    return loginUser(\"someone\");\n}\n",
        )
        .unwrap();
        // A same-text decoy: the comment names the symbol but is not code.
        fs::write(
            root.join("src/caller.ts"),
            "import { loginUser } from \"./login\";\n\nexport function go(): boolean {\n    // loginUser is validated upstream\n    return loginUser(\"someone\");\n}\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["commit", "-qm", "fixture"],
        ] {
            let result = Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        Self(root.canonicalize().unwrap())
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(PIXEL);
        cmd.current_dir(&self.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .args(args)
            .output()
            .unwrap()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.0.join(rel)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rename_rewrites_definition_call_and_import() {
    let fixture = Fixture::new();
    let out = fixture.run(&["rename", "loginUser", "authenticate", ".", "--json"]);
    assert_success(&out);
    let data: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data["old_name"], "loginUser");
    assert_eq!(data["new_name"], "authenticate");
    assert!(data["edit_count"].as_u64().unwrap() >= 3);

    let login = fixture.read("src/login.ts");
    let caller = fixture.read("src/caller.ts");
    assert!(login.contains("function authenticate("), "{login}");
    assert!(!login.contains("loginUser"), "{login}");
    assert!(caller.contains("import { authenticate }"), "{caller}");
    assert!(caller.contains("return authenticate("), "{caller}");
    // The comment's same-name text survived: the graph never claimed it.
    assert!(
        caller.contains("// loginUser is validated upstream"),
        "{caller}"
    );
}

#[test]
fn rename_dry_run_leaves_files_untouched() {
    let fixture = Fixture::new();
    let before_login = fixture.read("src/login.ts");
    let before_caller = fixture.read("src/caller.ts");
    let out = fixture.run(&[
        "rename",
        "loginUser",
        "authenticate",
        ".",
        "--dry-run",
        "--json",
    ]);
    assert_success(&out);
    let data: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data["dry_run"], true);
    assert!(data["applied"].is_null());
    assert!(data["edit_count"].as_u64().unwrap() >= 3);
    assert_eq!(fixture.read("src/login.ts"), before_login);
    assert_eq!(fixture.read("src/caller.ts"), before_caller);
}

#[test]
fn rename_reports_ambiguous_names_and_honors_file_disambiguation() {
    let fixture = Fixture::new();
    // A second same-named function in another file.
    fs::write(
        fixture.0.join("src/other.ts"),
        "export function loginUser(id: number): boolean {\n    return id > 0;\n}\n",
    )
    .unwrap();

    let ambiguous = fixture.run(&["rename", "loginUser", "authenticate", ".", "--json"]);
    assert_success(&ambiguous);
    let data: Value = serde_json::from_slice(&ambiguous.stdout).unwrap();
    assert!(data["candidates"].as_array().unwrap().len() >= 2);

    let out = fixture.run(&[
        "rename",
        "loginUser",
        "authenticate",
        ".",
        "--file",
        "src/login.ts",
        "--json",
    ]);
    assert_success(&out);
    let login = fixture.read("src/login.ts");
    let other = fixture.read("src/other.ts");
    assert!(login.contains("function authenticate("), "{login}");
    assert!(other.contains("function loginUser("), "{other}");
}

#[test]
fn rename_human_output_names_sites_skips_and_unclaimed_text() {
    let fixture = Fixture::new();
    let out = fixture.run(&["rename", "loginUser", "authenticate", "."]);
    assert_success(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("renamed loginUser → authenticate"),
        "{stdout}"
    );
    assert!(stdout.contains("src/login.ts"), "{stdout}");
    assert!(stdout.contains("src/caller.ts"), "{stdout}");
    // Edit kinds are named, not blank: SiteKind::as_str feeds this column.
    assert!(stdout.contains("definition"), "{stdout}");
    assert!(stdout.contains("call"), "{stdout}");
    assert!(stdout.contains("import"), "{stdout}");
    // The comment decoy is reported as unclaimed text, not silently left.
    assert!(stdout.contains("unclaimed"), "{stdout}");

    // Dry-run says "would rename" instead — a different verb, not a flag echo.
    let fixture2 = Fixture::new();
    let out = fixture2.run(&["rename", "loginUser", "authenticate", ".", "--dry-run"]);
    assert_success(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would rename loginUser"), "{stdout}");
    assert!(!stdout.contains("renamed loginUser →"), "{stdout}");
}

#[test]
fn rename_rejects_an_invalid_new_name() {
    let fixture = Fixture::new();
    let out = fixture.run(&["rename", "loginUser", "9bad name", ".", "--json"]);
    assert!(!out.status.success());
}
