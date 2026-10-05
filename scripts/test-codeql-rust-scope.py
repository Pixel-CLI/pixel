#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""PR publication may omit Rust work, never changed Rust or scheduled security scans."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("codeql_scope", ROOT / "scripts/codeql-rust-scope.py")
scope = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(scope)


class ScopeContract(unittest.TestCase):
    def test_rust_and_embedded_build_inputs_never_skip(self):
        for path in ("src/main.rs", "docs/example.rs", "crates/pixel/assets/prompt.md",
                     "fuzz/input.txt", "Cargo.toml", "nested/Cargo.lock", "build.rs",
                     "rust-toolchain", "rust-toolchain.toml", "nested/.cargo/config.toml"):
            with self.subTest(path=path):
                self.assertTrue(scope.affects_rust(path))

    def test_selection_policy_edits_must_exercise_the_analysis(self):
        for path in (".github/workflows/codeql.yml", ".github/codeql/config.yml",
                     "scripts/codeql-rust-scope.py", "scripts/test-codeql-rust-scope.py"):
            with self.subTest(path=path):
                self.assertTrue(scope.affects_rust(path))

    def test_known_non_rust_changes_do_not_pay_for_rust_analysis(self):
        for path in ("README.md", "changelog.d/828-codeql.changed.md", "docs/a guide.md",
                     ".github/workflows/ci.yml", ".github/renovate.json5", "scripts/audit.py",
                     "website/layouts/index.html", "docs/motion/src/app.tsx", "LICENSE"):
            with self.subTest(path=path):
                self.assertFalse(scope.affects_rust(path))

    def test_unknown_inputs_scan_until_explicitly_classified(self):
        for path in ("generator.sh", "schema.proto", "new.config", ".gitmodules", "Makefile"):
            with self.subTest(path=path):
                self.assertTrue(scope.affects_rust(path))

    def test_every_non_pr_event_scans_without_consulting_a_diff(self):
        for event in ("push", "schedule", "workflow_dispatch", "merge_group", "unknown"):
            with self.subTest(event=event), patch.object(scope.subprocess, "check_output") as git:
                self.assertTrue(scope.should_scan(event)[0])
                git.assert_not_called()

    def test_unreadable_diff_never_claims_rust_is_unchanged(self):
        with patch.object(scope.subprocess, "check_output", side_effect=OSError("missing git")):
            self.assertTrue(scope.should_scan("pull_request")[0])
        with patch.object(scope.subprocess, "check_output", side_effect=[b"head base topic", subprocess.CalledProcessError(128, "git")]):
            self.assertTrue(scope.should_scan("pull_request")[0])

    def test_workflow_retains_real_scans_and_uses_one_rust_decision(self):
        workflow = (ROOT / ".github/workflows/codeql.yml").read_text()
        guard = "if: matrix.language != 'rust' || steps.rust-scope.outputs.run != 'false'"
        for name in ("Initialize CodeQL", "Analyze and upload results"):
            self.assertIn(f"- name: {name}\n        {guard}", workflow)
        self.assertIn("fetch-depth: 2", workflow)
        self.assertIn("--event \"$GITHUB_EVENT_NAME\"", workflow)
        self.assertIn("language: [rust, actions, python, javascript-typescript]", workflow)
        self.assertIn("build-mode: none", workflow)
        self.assertNotIn("queries:", workflow)
        self.assertNotIn("paths-ignore:", workflow)
        self.assertIn("python3 scripts/test-codeql-rust-scope.py", (ROOT / ".github/workflows/ci.yml").read_text())


class MergeDiffContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-codeql-scope-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.env = env
        self.git("init", "-q", "-b", "main")
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {}\n")
        (self.root / "README.md").write_text("Docs\n")
        self.commit("base")
        self.git("checkout", "-qb", "topic")

    def git(self, *args):
        return subprocess.check_output(["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                                        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
                                       cwd=self.root, env=self.env, stderr=subprocess.DEVNULL)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-qm", message)

    def merge(self):
        self.commit("topic")
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "--no-edit", "topic")

    def test_docs_only_merge_emits_skip_for_the_actual_cli_consumer(self):
        (self.root / "README.md").write_text("Updated docs\n")
        self.merge()
        result = subprocess.run(["python3", str(ROOT / "scripts/codeql-rust-scope.py"), "--event", "pull_request"],
                                cwd=self.root, env=self.env, text=True, capture_output=True, check=True)
        self.assertEqual(result.stdout, "run=false\n")
        self.assertTrue(scope.should_scan("schedule", self.root)[0])

    def test_depth_two_checkout_can_actually_skip_the_rust_job(self):
        (self.root / "README.md").write_text("Only docs\n")
        self.merge()
        clone = self.root / "shallow"
        self.git("clone", "-q", "--depth", "2", self.root.as_uri(), str(clone))
        self.assertFalse(scope.should_scan("pull_request", clone)[0])

    def test_rust_renamed_into_documentation_still_requires_analysis(self):
        (self.root / "src/main.rs").rename(self.root / "example.md")
        self.merge()
        self.assertTrue(scope.should_scan("pull_request", self.root)[0])

    def test_removed_rust_is_not_hidden_by_a_docs_edit(self):
        (self.root / "src/main.rs").unlink()
        (self.root / "README.md").write_text("Removed example\n")
        self.merge()
        self.assertTrue(scope.should_scan("pull_request", self.root)[0])

    def test_target_branch_rust_edits_are_not_misattributed_to_the_pr(self):
        (self.root / "README.md").write_text("PR docs\n")
        self.commit("docs")
        self.git("checkout", "-q", "main")
        (self.root / "src/main.rs").write_text("fn main() { println!(\"base\"); }\n")
        self.commit("main advanced")
        self.git("merge", "--no-ff", "--no-edit", "topic")
        self.assertFalse(scope.should_scan("pull_request", self.root)[0])

    def test_missing_merge_context_falls_back_to_full_analysis(self):
        self.assertTrue(scope.should_scan("pull_request", self.root)[0])


if __name__ == "__main__":
    unittest.main()
