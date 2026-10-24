// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Shared fixtures: a committed crate to ask questions of, and a command
//! that cannot touch the checkout or the user's home.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A temporary directory with an empty home and runtime dir beside the repo.
pub struct Fixture {
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn runtime(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    /// `pixel` with this fixture's home, runtime and cache, working in the repo.
    pub fn pixel(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pixel"));
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("XDG_CACHE_HOME", self.home().join(".cache"))
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            // Where hosts keep their configuration follows these variables on a
            // real machine; a test home must not inherit the runner's.
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CODEX_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("PIXEL_METRICS")
            .env_remove("PIXEL_BRIEF")
            .env_remove("PIXEL_BRIEF_STEPS")
            .env_remove("PIXEL_EDIT_HOOKS")
            .env_remove("PIXEL_METRICS_ROUND_TRIP_MS");
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.pixel(args).output().expect("pixel runs")
    }

    pub fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    pub fn write(&self, path: &str, text: &str) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A committed crate: `b::go` calls `a::run`; `tests/it.rs` calls `go`.
pub fn project() -> Fixture {
    let fixture = Fixture {
        dir: tempfile::tempdir().unwrap(),
    };
    std::fs::create_dir_all(fixture.root()).unwrap();
    std::fs::create_dir_all(fixture.home()).unwrap();
    std::fs::create_dir_all(fixture.runtime()).unwrap();
    fixture.git(&["init", "-q"]);
    fixture.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    fixture.write("src/lib.rs", "pub mod a;\npub mod b;\n");
    fixture.write("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n");
    fixture.write(
        "src/b.rs",
        "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n",
    );
    fixture.write(
        "tests/it.rs",
        "use demo::b::go;\n\nfn t() {\n    go();\n}\n",
    );
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "one"]);
    let prepared = fixture.run(&["prepare"]);
    assert!(prepared.status.success(), "{}", stderr(&prepared));
    fixture
}

/// The action-log lines of the fixture, parsed.
pub fn action_log(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(root.join(".pixel/actions.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
