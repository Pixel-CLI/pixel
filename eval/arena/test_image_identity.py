#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Black-box tests for arena image pinning and compatibility checks."""

import json
import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
RUNNER = ROOT / "eval" / "arena.sh"


class ArenaImageIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="arena-image-identity-")
        self.root = pathlib.Path(self.temp.name)
        self.bin_dir = self.root / "bin"
        self.home = self.root / "home"
        self.repo = self.root / "repo"
        self.scenarios = self.root / "scenarios"
        self.results = self.root / "results"
        self.bin_dir.mkdir()
        self.home.mkdir()
        self.repo.mkdir()
        self.scenarios.mkdir()
        self._make_snapshot_repo()
        (self.scenarios / "tiny.json").write_text(json.dumps({
            "id": "tiny",
            "must": [{"pattern": "ok", "points": 1}],
            "never": [],
        }))
        (self.bin_dir / "docker").write_text(self._fake_docker())
        (self.bin_dir / "docker").chmod(0o755)

    def tearDown(self):
        self.temp.cleanup()

    def _make_snapshot_repo(self):
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        subprocess.run(["git", "-C", str(self.repo), "config", "user.email",
                        "arena-test@example.invalid"], check=True)
        subprocess.run(["git", "-C", str(self.repo), "config", "user.name",
                        "Arena test"], check=True)
        (self.repo / "README.md").write_text("fixture\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "README.md"], check=True)
        subprocess.run(["git", "-C", str(self.repo), "commit", "-qm", "fixture"], check=True)

    def _add_pixel_only_context(self):
        (self.repo / "AGENTS.md").write_text("Pixel-only instructions.\nPIXEL retrieval only.\n")
        rules = self.repo / ".agents" / "rules"
        rules.mkdir(parents=True)
        (rules / "pixel-only.md").write_text("Pixel-only rule.\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "AGENTS.md", ".agents/rules/pixel-only.md"],
                       check=True)
        subprocess.run(["git", "-C", str(self.repo), "commit", "-qm", "add Pixel-only context"],
                       check=True)

    def _fake_docker(self):
        return """#!/usr/bin/env python3
import json
import os
import pathlib
import sys

args = sys.argv[1:]
calls_path = pathlib.Path(os.environ["DOCKER_CALLS"])
with calls_path.open("a") as calls:
    calls.write(json.dumps(args) + "\\n")

state_path = pathlib.Path(os.environ["DOCKER_STATE"])
state = json.loads(state_path.read_text()) if state_path.exists() else {}
command = args[0]
if command == "image" and args[1] == "inspect":
    reference = args[-1]
    images = {
        "pixel-arena:raw": os.environ["RAW_IMAGE_ID"],
        os.environ["PIXEL_IMAGE_REF"]: os.environ["FAKE_PIXEL_IMAGE_ID"],
    }
    image_id = images.get(reference)
    if image_id is None:
        sys.exit(1)
    if "--format" in args:
        print(image_id)
elif command == "run":
    image_id = args[args.index("codex") + 1]
    print(os.environ["CODEX_" + ("RAW" if image_id == os.environ["RAW_IMAGE_ID"] else "PIXEL") + "_VERSION"])
elif command == "create":
    name = args[args.index("--name") + 1]
    state[name] = args[-1]
    state_path.write_text(json.dumps(state))
elif command == "inspect":
    if "--format" in args:
        name = args[-1]
        actual = os.environ.get("ACTUAL_IMAGE_OVERRIDE", state[name])
        if (os.environ.get("ACTUAL_IMAGE_ARM_OVERRIDE") == "pixel"
                and "arena-pixel-" in name):
            actual = "sha256:wrong-image"
        print(actual)
    elif "-f" in args:
        print("0")
elif command in ("start", "wait", "logs", "rm", "build"):
    pass
else:
    print("unexpected docker call: " + repr(args), file=sys.stderr)
    sys.exit(90)
"""

    def run_arena(self, **extra_env):
        environment = os.environ.copy()
        environment.update({
            "ARENA_RESULTS_DIR": str(self.results),
            "ARENA_SCENARIOS_DIR": str(self.scenarios),
            "AUTH": str(self.home / "auth.json"),
            "CODEX_RAW_VERSION": "codex-test-1",
            "CODEX_PIXEL_VERSION": "codex-test-1",
            "DOCKER_CALLS": str(self.root / "docker-calls.jsonl"),
            "DOCKER_STATE": str(self.root / "docker-state.json"),
            "PATH": f"{self.bin_dir}{os.pathsep}{environment['PATH']}",
            "PIXEL_ARENA_IMAGE": "pixel-arena:candidate",
            "PIXEL_IMAGE_REF": "pixel-arena:candidate",
            "FAKE_PIXEL_IMAGE_ID": "sha256:pixel-candidate",
            "PIXEL_IMAGE_SOURCE": "existing",
            "RAW_IMAGE_ID": "sha256:raw-base",
            "REPO_SNAPSHOT": str(self.repo),
            "RUN_ID": "identity-test",
        })
        environment.update(extra_env)
        return subprocess.run(
            ["bash", str(RUNNER), "--arms", "raw pixel", "--tasks", "tiny",
             "--reps", "1"],
            check=False,
            capture_output=True,
            cwd=ROOT,
            env=environment,
            text=True,
        )

    def docker_calls(self):
        calls_path = self.root / "docker-calls.jsonl"
        if not calls_path.exists():
            return []
        return [json.loads(line) for line in calls_path.read_text().splitlines()]

    def test_creates_containers_by_pinned_image_id_and_records_identity(self):
        result = self.run_arena()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.docker_calls()
        creates = [call for call in calls if call[0] == "create"]
        self.assertEqual([call[-1] for call in creates],
                         ["sha256:raw-base", "sha256:pixel-candidate"], calls)
        for arm, image_id in (("raw", "sha256:raw-base"),
                              ("pixel", "sha256:pixel-candidate")):
            metadata = json.loads(
                (self.results / f"container-image-{arm}-1.json").read_text()
            )
            self.assertEqual(metadata, {
                "arm": arm,
                "expected_image_id": image_id,
                "actual_container_image_id": image_id,
                "matches": True,
            })
        self.assertEqual(sum(call[0] == "start" for call in calls), 2)

    def test_mismatched_created_image_is_removed_without_starting(self):
        result = self.run_arena(ACTUAL_IMAGE_OVERRIDE="sha256:wrong-image")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("container image mismatch", result.stdout + result.stderr)
        metadata = json.loads(
            (self.results / "container-image-raw-1.json").read_text()
        )
        self.assertFalse(metadata["matches"])
        calls = self.docker_calls()
        self.assertFalse(any(call[0] == "start" for call in calls))
        self.assertTrue(any(call[0] == "rm" for call in calls))

    def test_failed_second_arm_is_marked_and_successful_first_arm_is_cleaned(self):
        result = self.run_arena(ACTUAL_IMAGE_ARM_OVERRIDE="pixel")

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("=== ranking", result.stdout)
        self.assertTrue((self.results / "pixel-tiny-1.failed").is_file())
        self.assertFalse((self.results / "raw-tiny-1.failed").exists())
        self.assertTrue((self.results / "ranking.json").is_file())
        calls = self.docker_calls()
        self.assertTrue(any(call[0] == "start" and any("arena-raw-" in arg for arg in call)
                            for call in calls))
        self.assertFalse(any(call[0] == "start" and any("arena-pixel-" in arg for arg in call)
                             for call in calls))
        self.assertTrue(any(call[0] == "wait" for call in calls))
        self.assertTrue(any(call[0] == "rm" and any("arena-raw-" in arg for arg in call)
                            for call in calls))
        self.assertTrue(any(call[0] == "rm" and any("arena-pixel-" in arg for arg in call)
                            for call in calls))

    def test_pixel_only_context_scrub_replaces_all_removed_lines_with_empty_files(self):
        self._add_pixel_only_context()

        result = self.run_arena()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        raw_snapshot = self.results / "snapshot-raw-1"
        self.assertEqual((raw_snapshot / "AGENTS.md").read_text(), "")
        self.assertEqual((raw_snapshot / ".agents/rules/pixel-only.md").read_text(), "")

    def test_snapshot_scrub_failure_marks_arm_and_prevents_its_container(self):
        self._add_pixel_only_context()
        grep = self.bin_dir / "grep"
        grep.write_text("#!/bin/sh\nexit 2\n")
        grep.chmod(0o755)

        result = self.run_arena()

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("could not scrub Pixel references from AGENTS.md", result.stderr)
        self.assertIn("=== ranking", result.stdout)
        self.assertTrue((self.results / "raw-tiny-1.failed").is_file())
        self.assertFalse((self.results / "pixel-tiny-1.failed").exists())
        calls = self.docker_calls()
        self.assertFalse(any(call[0] == "create" and any("arena-raw-" in arg for arg in call)
                             for call in calls))
        self.assertTrue(any(call[0] == "create" and any("arena-pixel-" in arg for arg in call)
                            for call in calls))

    def test_different_codex_versions_stop_before_arm_containers(self):
        result = self.run_arena(CODEX_PIXEL_VERSION="codex-test-2")

        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("Codex versions differ", result.stdout + result.stderr)
        calls = self.docker_calls()
        self.assertEqual(sum(call[0] == "run" for call in calls), 2)
        self.assertFalse(any(call[0] in ("create", "start") for call in calls))


if __name__ == "__main__":
    unittest.main()
