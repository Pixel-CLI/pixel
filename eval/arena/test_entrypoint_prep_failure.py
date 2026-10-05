#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Regression tests for arena preparation failures stopping before Codex."""

import os
import pathlib
import subprocess
import tempfile
import unittest


class PrepFailureStopsCodexTest(unittest.TestCase):
    def test_skill_pilot_prepares_shared_graph_without_installing_hooks(self):
        with tempfile.TemporaryDirectory(prefix="arena-skill-prep-") as temporary:
            root = pathlib.Path(temporary)
            bin_dir = root / "bin"
            home = root / "home"
            repo = root / "repo"
            bin_dir.mkdir()
            home.mkdir()
            repo.mkdir()
            calls = root / "pixel-calls"
            codex_called = root / "codex-called"
            pixel = bin_dir / "pixel"
            pixel.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" >> \"$PIXEL_CALLS\"\n"
                "[ \"$1\" = prepare-repo ] || exit 91\n"
                "exit 42\n"
            )
            pixel.chmod(0o755)
            codex = bin_dir / "codex"
            codex.write_text("#!/bin/sh\nprintf called > \"$CODEX_CALLED\"\n")
            codex.chmod(0o755)
            environment = os.environ.copy()
            environment.update({
                "ARM_TOOL": "raw",
                "ARENA_SKILL_PILOT": "1",
                "ARENA_REPO_DIR": str(repo),
                "CODEX_CALLED": str(codex_called),
                "HOME": str(home),
                "PATH": f"{bin_dir}{os.pathsep}{environment['PATH']}",
                "PIXEL_CALLS": str(calls),
                "REP": "1",
                "TASKS": "g5-transfer-status-impact",
            })
            entrypoint = pathlib.Path(__file__).with_name("entrypoint.sh")
            result = subprocess.run(
                ["bash", str(entrypoint)], check=False, capture_output=True,
                cwd=repo, env=environment, text=True,
            )

            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertEqual(calls.read_text().split()[0:2], ["prepare-repo", "--no-daemon"])
            self.assertEqual(calls.read_text().split()[2], str(repo))
            self.assertFalse(codex_called.exists())

    def test_failed_pixel_install_stops_before_codex(self):
        self.assert_prep_failure_stops_codex(fail_command="install", expected_rc=41)

    def test_failed_pixel_index_stops_before_codex(self):
        self.assert_prep_failure_stops_codex(
            fail_command="prepare-repo", expected_rc=42, graph_prep=True
        )

    def test_missing_graph_after_successful_prepare_stops_before_codex(self):
        self.assert_prep_failure_stops_codex(
            fail_command=None, expected_rc=1, graph_prep=True
        )

    def assert_prep_failure_stops_codex(self, *, fail_command, expected_rc,
                                        graph_prep=False):
        with tempfile.TemporaryDirectory(prefix="arena-prep-failure-") as temporary:
            root = pathlib.Path(temporary)
            bin_dir = root / "bin"
            home = root / "home"
            repo = root / "repo"
            bin_dir.mkdir()
            home.mkdir()
            repo.mkdir()
            calls = root / "pixel-calls"
            codex_called = root / "codex-called"
            pixel = bin_dir / "pixel"
            fail_clause = (
                f"[ \"$1\" != \"{fail_command}\" ] || exit {expected_rc}\n"
                if fail_command else ""
            )
            pixel.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" >> \"$PIXEL_CALLS\"\n"
                f"{fail_clause}"
            )
            pixel.chmod(0o755)
            codex = bin_dir / "codex"
            codex.write_text("#!/bin/sh\nprintf called > \"$CODEX_CALLED\"\n")
            codex.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "ARM_TOOL": "pixel",
                    "ARENA_REPO_DIR": str(repo),
                    "CODEX_CALLED": str(codex_called),
                    "HOME": str(home),
                    "PATH": f"{bin_dir}{os.pathsep}{environment['PATH']}",
                    "PIXEL_CALLS": str(calls),
                    "PIXEL_ARENA_PREP_GRAPH": "1" if graph_prep else "0",
                    "REP": "1",
                    "TASKS": "g3-rename-modal",
                }
            )
            entrypoint = pathlib.Path(__file__).with_name("entrypoint.sh")
            result = subprocess.run(
                ["bash", str(entrypoint)],
                check=False,
                capture_output=True,
                cwd=repo,
                env=environment,
                text=True,
            )

            self.assertEqual(result.returncode, expected_rc, result.stderr)
            if fail_command:
                self.assertIn("arm preparation failed: arm=pixel", result.stderr)
            else:
                self.assertIn("graph preparation completed without", result.stderr)
            self.assertFalse(codex_called.exists())
            self.assertTrue(calls.exists())
            expected_command = fail_command or "prepare-repo"
            self.assertIn(expected_command, calls.read_text())


if __name__ == "__main__":
    unittest.main()
