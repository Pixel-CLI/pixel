#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measurement-contract tests for the bounded Claude skill pair runner."""

import json
import shutil
import subprocess
import time
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from eval import claude_skill_pair


class ClaudeStreamMeasurementTests(unittest.TestCase):
    def parse(self, events: list[dict]) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            transcript = Path(directory) / "stream.jsonl"
            transcript.write_text("".join(json.dumps(event) + "\n" for event in events))
            return claude_skill_pair.parse_stream(transcript)

    def test_repeated_tool_snapshot_counts_once_and_keeps_latest_block(self):
        earlier = {
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": "toolu-1", "name": "Bash", "input": {"command": ""}},
                {"type": "tool_use", "id": "toolu-2", "name": "Skill", "input": {"skill": "pixel-impact"}},
            ]},
        }
        later = {
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": "toolu-1", "name": "Bash", "input": {"command": "pixel impact transferPageToGhost --no-refresh"}},
                {"type": "tool_use", "id": "toolu-2", "name": "Skill", "input": {"skill": "pixel-impact"}},
            ]},
        }

        row = self.parse([earlier, later, {"type": "result", "result": "Found callers."}])

        self.assertEqual(row["tool_use_count"], 2)
        self.assertEqual(row["pixel_impact_calls"], ["pixel impact transferPageToGhost --no-refresh"])
        self.assertEqual(row["skill_invocations"], [{"skill": "pixel-impact"}])

    def test_cache_categories_are_preserved_and_included_in_gross_proxy(self):
        row = self.parse([{"type": "result", "result": "Done", "usage": {
            "input_tokens": 11,
            "output_tokens": 13,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 7,
        }}])

        self.assertEqual(
            {key: row[key] for key in ("input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens")},
            {"input_tokens": 11, "output_tokens": 13, "cache_read_tokens": 5, "cache_creation_tokens": 7},
        )
        self.assertEqual(claude_skill_pair.token_totals(row), {
            "total_input_tokens": 23,
            "gross_tokens_proxy": 36,
        })

    def test_missing_usage_stays_unknown_on_error_result(self):
        row = self.parse([{"type": "result", "is_error": True, "subtype": "error_max_turns"}])

        self.assertTrue(row["result_found"])
        self.assertIs(row["is_error"], True)
        self.assertEqual(row["subtype"], "error_max_turns")
        self.assertIsNone(row["input_tokens"])
        self.assertIsNone(row["output_tokens"])
        self.assertIsNone(row["cache_read_tokens"])
        self.assertIsNone(row["cache_creation_tokens"])
        self.assertEqual(claude_skill_pair.token_totals(row), {
            "total_input_tokens": None,
            "gross_tokens_proxy": None,
        })

    def test_explicit_zero_usage_is_not_missing(self):
        row = self.parse([{"type": "result", "is_error": True, "subtype": "error_api", "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        }}])

        self.assertEqual(claude_skill_pair.token_totals(row), {
            "total_input_tokens": 0,
            "gross_tokens_proxy": 0,
        })


class ClaudeCredentialIsolationTests(unittest.TestCase):
    def credential(self) -> dict:
        return {"claudeAiOauth": {
            "accessToken": "test-access-token",
            "refreshToken": "test-refresh-token",
            "expiresAt": (time.time() + 3600) * 1000,
            "refreshTokenExpiresAt": (time.time() + 86400) * 1000,
        }}

    def write_credential(self, path: Path, value: dict) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))
        path.chmod(0o600)

    def test_preflight_artifact_contains_source_not_oauth_secret(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "repo"
            (repo / ".pixel").mkdir(parents=True)
            (repo / ".pixel/graph.v2.db").write_bytes(b"fixture graph")
            results = root / "results"
            results.mkdir()
            skill = root / "SKILL.md"
            skill.write_text("---\ndisable-model-invocation: true\n---\n")
            scenario = root / "scenario.json"
            scenario.write_text(json.dumps({"id": "fixture", "prompt": "Question"}))
            pixel = root / "pixel"
            pixel.write_text("fixture")
            claude = root / "claude"
            claude.write_text("fixture")
            sentinel = {
                "accessToken": "sentinel-access-secret",
                "refreshToken": "sentinel-refresh-secret",
            }
            args = mock.Mock(
                repo=str(repo), results_dir=str(results), skill=str(skill),
                scenario=str(scenario), revision="HEAD", auth_config_dir=str(root / "auth"),
                credentials_file=None, claude=str(claude), pixel=str(pixel), symbol="symbol",
                model="sonnet", effort="medium", max_turns=1, max_budget_usd=1.0,
            )
            impact_result = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="[{}]", stderr=""
            )
            with (
                mock.patch.object(claude_skill_pair, "git", side_effect=["commit", "tree"]),
                mock.patch.object(claude_skill_pair, "stage_archive"),
                mock.patch.object(claude_skill_pair, "credential_source", return_value="macos-keychain"),
                mock.patch.object(claude_skill_pair, "load_oauth_credentials", return_value=sentinel),
                mock.patch.object(claude_skill_pair, "MANAGED", ()),
                mock.patch.object(claude_skill_pair, "version_and_help", return_value=("2.0", "help-hash")),
                mock.patch.object(claude_skill_pair, "command", side_effect=[
                    subprocess.CompletedProcess(args=[], returncode=0, stdout="pixel 1.0\n", stderr=""),
                    impact_result,
                ]),
            ):
                manifest = claude_skill_pair.preflight(args)

            persisted = (results / "preflight.json").read_text()
            self.assertEqual(manifest["credential_source"], "macos-keychain")
            self.assertTrue(manifest["oauth_credentials_present"])
            self.assertNotIn("sentinel-access-secret", persisted)
            self.assertNotIn("sentinel-refresh-secret", persisted)
            self.assertNotIn("accessToken", persisted)
            self.assertNotIn("refreshToken", persisted)
            shutil.rmtree(manifest["workspace_parent"], ignore_errors=True)

    def test_nonregular_explicit_credential_path_fails_without_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config"
            self.write_credential(config / ".credentials.json", self.credential())
            explicit = root / "credential-directory"
            explicit.mkdir()

            with mock.patch.object(claude_skill_pair.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "must be a regular file"):
                    claude_skill_pair.load_oauth_credentials(config, explicit)

            run.assert_not_called()

    def test_explicit_private_file_precedes_config_file_and_keychain(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config"
            config.mkdir()
            explicit = root / "explicit.json"
            first = self.credential()
            second = {"claudeAiOauth": {
                **first["claudeAiOauth"], "accessToken": "different-access-token",
            }}
            self.write_credential(explicit, first)
            self.write_credential(config / ".credentials.json", second)

            with mock.patch.object(claude_skill_pair.subprocess, "run") as run:
                actual = claude_skill_pair.load_oauth_credentials(config, explicit)

            self.assertEqual(claude_skill_pair.credential_source(config, explicit), "explicit-file")
            self.assertEqual(actual["accessToken"], "test-access-token")
            run.assert_not_called()

    def test_private_config_file_precedes_keychain(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            self.write_credential(config / ".credentials.json", self.credential())

            with mock.patch.object(claude_skill_pair.subprocess, "run") as run:
                actual = claude_skill_pair.load_oauth_credentials(config)

            self.assertEqual(claude_skill_pair.credential_source(config), "config-file")
            self.assertEqual(actual["refreshToken"], "test-refresh-token")
            run.assert_not_called()

    def test_missing_file_uses_scoped_keychain_entry_without_exposing_secret(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            config.mkdir()
            secret_json = json.dumps(self.credential())
            result = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="",
                stderr="password: " + json.dumps(secret_json),
            )
            with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result) as run:
                actual = claude_skill_pair.load_oauth_credentials(config)

            service = "Claude Code-credentials-" + claude_skill_pair.digest(
                str(config.resolve()).encode())[:8]
            self.assertEqual(claude_skill_pair.credential_source(config), "macos-keychain")
            self.assertEqual(actual["accessToken"], "test-access-token")
            argv = run.call_args.args[0]
            env = run.call_args.kwargs["env"]
            self.assertIn(service, argv)
            self.assertNotIn("ANTHROPIC_API_KEY", env)

            isolated = Path(directory) / "isolated-config"
            isolated.mkdir(mode=0o700)
            claude_skill_pair.write_isolated_credentials(isolated, actual)
            saved = isolated / ".credentials.json"
            self.assertEqual(saved.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(saved.read_text()), {"claudeAiOauth": actual})

    def test_missing_file_and_keychain_entry_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            config.mkdir()
            result = subprocess.CompletedProcess(args=[], returncode=44, stdout="", stderr="")
            with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(RuntimeError, "Keychain has no Claude OAuth"):
                    claude_skill_pair.load_oauth_credentials(config)

    def test_expired_refresh_token_fails_before_model_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            expired = self.credential()
            expired["claudeAiOauth"]["refreshTokenExpiresAt"] = (time.time() - 60) * 1000
            self.write_credential(config / ".credentials.json", expired)

            with self.assertRaisesRegex(RuntimeError, "refresh credential is expired"):
                claude_skill_pair.load_oauth_credentials(config)


if __name__ == "__main__":
    unittest.main()
