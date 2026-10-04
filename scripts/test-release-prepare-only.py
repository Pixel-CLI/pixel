#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/release-prepare-only.py: CI skips its gates on a
release-prepare pull request only when the diff is what prepare.sh writes.

The script is the whole guard: the branch name is free to pick, so a code
change pushed on a `release-` branch must still be tested. Each case below
is one way such a change could slip through, and must be refused; the first
is the real 0.5.2 prepare diff (#314), which must pass, or the skip never
fires at all.
"""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parent / "release-prepare-only.py"
spec = importlib.util.spec_from_file_location("release_prepare_only", SCRIPT)
rpo = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rpo)


def git(repo, *args):
    env = {**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
    return subprocess.run(
        ["git", "-C", repo, *args], check=True, capture_output=True, text=True, env=env
    ).stdout.strip()


class ReleasePrepareOnly(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.email", "t@example.com")
        git(self.repo, "config", "user.name", "t")
        self.write({
            "crates/pixel/Cargo.toml": '[package]\nname = "pixel"\nversion = "0.5.1"\n\n[dependencies]\nserde = "1"\n',
            "Cargo.lock": '[[package]]\nname = "pixel"\nversion = "0.5.1"\n\n[[package]]\nname = "serde"\nversion = "1.0.0"\nchecksum = "aaa"\n',
            ".claude-plugin/plugin.json": '{\n  "name": "pixel",\n  "version": "0.5.1",\n  "x": 1\n}\n',
            "plugin.yaml": "name: pixel\nversion: 0.5.1\n",
            "CHANGELOG.md": "## [Unreleased]\n",
            "changelog.d/1-a.fixed.md": "fix\n",
            "crates/pixel/src/lib.rs": "pub fn f() {}\n",
        })
        self.base = self.commit()

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, files):
        for path, text in files.items():
            p = Path(self.repo, path)
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)

    def commit(self):
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "--allow-empty", "-m", "c")
        return git(self.repo, "rev-parse", "HEAD")

    def bump(self):
        for path in ["crates/pixel/Cargo.toml", "Cargo.lock", ".claude-plugin/plugin.json", "plugin.yaml"]:
            p = Path(self.repo, path)
            p.write_text(p.read_text().replace("0.5.1", "0.5.2"))
        self.write({"CHANGELOG.md": "## [Unreleased]\n\n## [0.5.2]\n- fix\n"})
        Path(self.repo, "changelog.d/1-a.fixed.md").unlink()

    def reasons(self):
        head = self.commit()
        cwd = os.getcwd()
        os.chdir(self.repo)
        try:
            return rpo.violations(*rpo.read_diff(self.base, head))
        finally:
            os.chdir(cwd)

    def test_the_prepare_diff_passes(self):
        self.bump()
        self.assertEqual(self.reasons(), [])

    def test_a_source_change_is_refused(self):
        self.bump()
        self.write({"crates/pixel/src/lib.rs": "pub fn f() { panic!() }\n"})
        self.assertEqual(self.reasons(), ["crates/pixel/src/lib.rs: not a file prepare.sh writes"])

    def test_a_dependency_added_to_a_manifest_is_refused(self):
        self.bump()
        p = Path(self.repo, "crates/pixel/Cargo.toml")
        p.write_text(p.read_text() + 'evil = "1"\n')
        self.assertEqual(
            self.reasons(),
            ["crates/pixel/Cargo.toml: changes a line other than its version: 'evil = \"1\"'"],
        )

    def test_a_lockfile_dependency_bump_is_refused(self):
        self.bump()
        p = Path(self.repo, "Cargo.lock")
        p.write_text(p.read_text().replace('"1.0.0"', '"1.0.1"').replace('"aaa"', '"bbb"'))
        self.assertTrue(any(r.startswith("Cargo.lock: changes a line other than its version") for r in self.reasons()))

    def test_a_manifest_field_other_than_version_is_refused(self):
        self.bump()
        p = Path(self.repo, ".claude-plugin/plugin.json")
        p.write_text(p.read_text().replace('"x": 1', '"x": 2'))
        self.assertTrue(any(r.startswith(".claude-plugin/plugin.json: changes a line") for r in self.reasons()))

    def test_an_added_fragment_is_refused(self):
        self.bump()
        self.write({"changelog.d/2-b.added.md": "new\n"})
        self.assertEqual(self.reasons(), ["changelog.d/2-b.added.md: a fragment may only be deleted (A)"])

    def test_a_deleted_manifest_is_refused(self):
        self.bump()
        Path(self.repo, "plugin.yaml").unlink()
        self.assertEqual(self.reasons(), ["plugin.yaml: prepare.sh only edits it (D)"])

    def worktree_reasons(self):
        cwd = os.getcwd()
        os.chdir(self.repo)
        try:
            return rpo.violations(*rpo.read_diff("HEAD"))
        finally:
            os.chdir(cwd)

    # prepare.sh runs the check before anything is committed: the same rule
    # must hold against the working tree, or the local check passes a diff
    # that CI then refuses.
    def test_the_uncommitted_prepare_diff_passes(self):
        self.bump()
        self.assertEqual(self.worktree_reasons(), [])

    def test_an_uncommitted_source_change_is_refused(self):
        self.bump()
        self.write({"crates/pixel/src/lib.rs": "pub fn f() { panic!() }\n"})
        self.assertEqual(self.worktree_reasons(), ["crates/pixel/src/lib.rs: not a file prepare.sh writes"])

    def test_an_untracked_file_is_refused(self):
        self.bump()
        self.write({"crates/pixel/src/new.rs": "pub fn g() {}\n"})
        self.assertEqual(self.worktree_reasons(), ["crates/pixel/src/new.rs: not a file prepare.sh writes"])

    def test_the_exit_code_is_the_verdict(self):
        self.bump()
        head = self.commit()
        ok = subprocess.run(["python3", SCRIPT, self.base, head], cwd=self.repo, capture_output=True)
        self.assertEqual(ok.returncode, 0)
        self.write({"crates/pixel/src/lib.rs": "pub fn g() {}\n"})
        head = self.commit()
        bad = subprocess.run(["python3", SCRIPT, self.base, head], cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(bad.returncode, 1)
        self.assertIn("crates/pixel/src/lib.rs", bad.stderr)


if __name__ == "__main__":
    unittest.main()
