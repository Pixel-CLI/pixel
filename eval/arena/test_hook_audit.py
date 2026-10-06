# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

import json
import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace

from hook_audit import ALLOWED, PIXEL, audit, run_hook


def pixel_hooks():
    commands = sorted(ALLOWED)
    return {"hooks": {f"event-{index}": [{"type": "command", "command": " ".join(command)}]
                      for index, command in enumerate(commands)}}


class HookAuditTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.repo = self.root / "repo"
        self.home.mkdir()
        self.repo.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def test_raw_requires_no_hooks_and_writes_receipt(self):
        receipt = self.root / "raw.json"
        audit("raw", self.home, self.repo, receipt, "1", self.root / "no-system-config")
        self.assertEqual(json.loads(receipt.read_text())["commands"], [])

    def test_pixel_requires_exact_11_and_wraps_one_prompt_hook(self):
        hooks = pixel_hooks()
        path = self.home / "hooks.json"
        path.write_text(json.dumps(hooks))
        receipt = self.root / "pixel.json"
        audit("pixel", self.home, self.repo, receipt, "2", self.root / "no-system-config")
        updated = json.loads(path.read_text())
        wrapped = [command for command in _commands(updated) if "arena-hook-audit.py" in command]
        self.assertEqual(len(wrapped), 1)
        self.assertIn("/out/pixel-hook-2.jsonl", wrapped[0])
        self.assertTrue(json.loads(receipt.read_text())["trust_bypass_safe"])

    def test_foreign_hook_fails_closed(self):
        (self.home / "hooks.json").write_text(json.dumps({"hooks": {"UserPromptSubmit": [
            {"type": "command", "command": "echo foreign"}
        ]}}))
        with self.assertRaisesRegex(RuntimeError, "foreign or unrecognized"):
            audit("pixel", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_bare_pixel_name_is_not_an_allowed_executable(self):
        (self.home / "hooks.json").write_text(json.dumps({"hooks": {"UserPromptSubmit": [
            {"type": "command", "command": "pixel run-hook prompt-submit --provider codex"}
        ]}}))
        with self.assertRaisesRegex(RuntimeError, "foreign or unrecognized"):
            audit("pixel", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_plugin_hook_fails_closed(self):
        (self.home / "plugin.json").write_text(json.dumps({"hooks": {"UserPromptSubmit": []}}))
        with self.assertRaisesRegex(RuntimeError, "plugin hook"):
            audit("raw", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_non_command_hook_object_fails_closed(self):
        hooks = pixel_hooks()
        next(iter(hooks["hooks"].values())).append({"type": "prompt", "prompt": "foreign"})
        (self.home / "hooks.json").write_text(json.dumps(hooks))
        with self.assertRaisesRegex(RuntimeError, "non-command or unsupported"):
            audit("pixel", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_unknown_hook_tree_leaf_fails_closed(self):
        (self.home / "hooks.json").write_text(json.dumps({"hooks": {
            "UserPromptSubmit": [{"mystery": "foreign hook"}],
        }}))
        with self.assertRaisesRegex(RuntimeError, "unrecognized nonempty"):
            audit("raw", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_plugin_registry_fails_closed(self):
        (self.home / "config.toml").write_text('plugins = ["foreign"]\n')
        with self.assertRaisesRegex(RuntimeError, "plugin registry"):
            audit("raw", self.home, self.repo, self.root / "audit.json", "1", self.root / "no-system-config")

    def test_runtime_receipt_contains_response_not_prompt_input(self):
        context = "indexed caller candidates"
        stdout = json.dumps({"hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit", "additionalContext": context,
        }}).encode()
        proc = subprocess.CompletedProcess([PIXEL], 0, stdout, b"hook stderr")
        receipt = self.root / "runtime.jsonl"
        output = bytearray()
        fake_stdin = SimpleNamespace(buffer=io.BytesIO(b"PRIVATE PROMPT"))
        fake_stdout = SimpleNamespace(buffer=SimpleNamespace(write=output.extend, flush=lambda: None))
        fake_stderr = SimpleNamespace(buffer=SimpleNamespace(write=lambda _: None, flush=lambda: None))
        with patch("hook_audit.subprocess.run", return_value=proc), \
             patch("hook_audit.sys.stdin", fake_stdin), \
             patch("hook_audit.sys.stdout", fake_stdout), \
             patch("hook_audit.sys.stderr", fake_stderr):
            result = run_hook(receipt, [PIXEL, "run-hook", "prompt-submit", "--provider", "codex"])
        record = json.loads(receipt.read_text())
        self.assertEqual(result, 0)
        self.assertTrue(record["forwarded_to_codex"])
        self.assertEqual(record["additional_context"], context)
        self.assertEqual(record["stderr"], "hook stderr")
        self.assertNotIn(b"PRIVATE PROMPT", receipt.read_bytes())
        self.assertEqual(bytes(output), stdout)

    def test_empty_successful_hook_response_is_valid_abstention(self):
        proc = subprocess.CompletedProcess([PIXEL], 0, b"", b"")
        receipt = self.root / "empty.jsonl"
        with patch("hook_audit.subprocess.run", return_value=proc), \
             patch("hook_audit.sys.stdin", SimpleNamespace(buffer=io.BytesIO())), \
             patch("hook_audit.sys.stdout", SimpleNamespace(buffer=SimpleNamespace(write=lambda _: None, flush=lambda: None))), \
             patch("hook_audit.sys.stderr", SimpleNamespace(buffer=SimpleNamespace(write=lambda _: None, flush=lambda: None))):
            result = run_hook(receipt, [PIXEL, "run-hook", "prompt-submit", "--provider", "codex"])
        record = json.loads(receipt.read_text())
        self.assertEqual(result, 0)
        self.assertTrue(record["response_valid"])
        self.assertFalse(record["emitted_context"])
        self.assertFalse(record["forwarded_to_codex"])

    def test_arena_rejects_retired_caller_facts_flag_before_creating_results(self):
        with tempfile.TemporaryDirectory(prefix="arena-flags-") as temporary:
            root = Path(temporary)
            docker_bin = root / "bin"
            docker_bin.mkdir()
            docker = docker_bin / "docker"
            docker.write_text("#!/bin/sh\nexit 99\n")
            docker.chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{docker_bin}:{os.environ['PATH']}",
                "REPO_SNAPSHOT": str(root / "repo"),
                "ARENA_RESULTS_DIR": str(root / "results"),
            }
            runner = Path(__file__).parents[1] / "arena.sh"
            result = subprocess.run(
                ["bash", str(runner), "--arms", "raw pixel", "--tasks", "g4-transfer-callers",
                 "--codex-caller-facts"],
                check=False, capture_output=True, cwd=runner.parents[1], env=env, text=True,
            )
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("--codex-caller-facts is retired", result.stderr)
            self.assertFalse((root / "results").exists())


def _commands(value):
    if isinstance(value, dict):
        if isinstance(value.get("command"), str):
            yield value["command"]
        for key, child in value.items():
            if key != "command":
                yield from _commands(child)
    elif isinstance(value, list):
        for child in value:
            yield from _commands(child)


if __name__ == "__main__":
    unittest.main()
