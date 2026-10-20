#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Exercise CI's inline change classifier against real temporary Git histories."""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/ci.yml"
CLASSIFIER = textwrap.dedent(
    WORKFLOW.read_text().split("      - name: Classify the diff\n", 1)[1].split(
        "        run: |\n", 1
    )[1].split(
        "\n\n  # The branch's install.sh", 1
    )[0]
)


class CiSelection(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-ci-selection-")
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        self.env = {
            **{
                key: value
                for key, value in os.environ.items()
                if not key.startswith("GIT_")
            },
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GITHUB_EVENT_NAME": "pull_request",
            "BASE_REF": "main",
        }
        self.git("init", "-qb", "main")
        self.git("config", "user.name", "CI selection test")
        self.git("config", "user.email", "ci-selection@example.invalid")
        self.write("base", "base\n")
        self.common = self.commit()
        self.git("update-ref", "refs/remotes/origin/main", self.common)

    def git(self, *args):
        return subprocess.check_output(
            ["git", *args], cwd=self.repo, env=self.env, text=True
        ).strip()

    def write(self, name, content="changed\n"):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def classify(self, event="pull_request"):
        output = self.repo / "github-output"
        output.write_text("")
        env = {**self.env, "GITHUB_EVENT_NAME": event, "GITHUB_OUTPUT": str(output)}
        result = subprocess.run(
            ["bash", "-e", "-c", CLASSIFIER],
            cwd=self.repo,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def change(self, path):
        self.write(path)
        self.commit()

    def test_unrelated_script_skips_installer_smoke(self):
        self.change("scripts/bench/unrelated.py")
        flags = self.classify()
        self.assertEqual(flags["scripts"], "true")
        self.assertEqual(flags["installer"], "false")

    def test_installer_inputs_run_the_smoke(self):
        for path in (
            "scripts/release/install.sh",
            "scripts/test-install.py",
            ".github/workflows/ci.yml",
        ):
            with self.subTest(path=path):
                self.git("reset", "--hard", self.common)
                self.git("update-ref", "refs/remotes/origin/main", self.common)
                self.change(path)
                self.assertEqual(self.classify()["installer"], "true")

    def test_release_workflows_run_script_checks_without_installer_smoke(self):
        for path in (
            ".github/workflows/release.yml",
            ".github/workflows/release-build.yml",
        ):
            with self.subTest(path=path):
                self.git("reset", "--hard", self.common)
                self.change(path)
                flags = self.classify()
                self.assertEqual(flags["scripts"], "true")
                self.assertEqual(flags["installer"], "false")

    def test_rust_source_keeps_compile_lanes_without_installer_smoke(self):
        self.change("crates/pixel/src/main.rs")
        flags = self.classify()
        self.assertEqual(flags["cargo"], "true")
        self.assertEqual(flags["rust-source"], "true")
        self.assertEqual(flags["installer"], "false")

    def test_main_push_enables_every_lane(self):
        self.change("docs/only.md")
        flags = self.classify(event="push")
        self.assertEqual(
            flags,
            {
                "cargo": "true",
                "rust-source": "true",
                "scripts": "true",
                "installer": "true",
                "deps": "true",
            },
        )

    def test_checkout_fetches_full_history_only_for_pull_requests(self):
        self.assertIn(
            "fetch-depth: ${{ github.event_name == 'pull_request' && '0' || '1' }}",
            WORKFLOW.read_text(),
        )

    def test_pull_request_uses_the_feature_side_of_the_merge_base(self):
        self.git("switch", "-qc", "target")
        self.change("scripts/release/install.sh")
        target = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", target)
        self.git("switch", "-qc", "feature", self.common)
        self.change("scripts/bench/unrelated.py")
        flags = self.classify()
        self.assertEqual(flags["scripts"], "true")
        self.assertEqual(flags["installer"], "false")


if __name__ == "__main__":
    unittest.main()
