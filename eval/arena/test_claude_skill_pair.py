#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measurement-contract tests for the bounded Claude skill pair runner."""

import json
import shutil
import subprocess
import time
import tempfile
import traceback
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
        self.assertIsNone(row["cli_init_model"])

    def test_cli_init_model_is_reported_separately_from_model_usage(self):
        row = self.parse([
            {"type": "system", "subtype": "init", "model": "gateway-sonnet"},
            {"type": "result", "result": "Done", "modelUsage": {"gateway-sonnet": {}}},
        ])

        self.assertEqual(row["cli_init_model"], "gateway-sonnet")
        self.assertEqual(row["resolved_model_usage"], {"gateway-sonnet": {}})


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


class ClaudeSnapshotIntegrityTests(unittest.TestCase):
    def snapshot_fixture(self, root: Path) -> tuple[Path, Path, dict]:
        project = root / "project"
        pixel_dir = project / ".pixel"
        pixel_dir.mkdir(parents=True)
        (pixel_dir / "graph.v2.db").write_bytes(b"pinned graph")
        (project / "src.rs").write_text("pinned source")
        skill = project / ".claude/skills/pixel-impact/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text("treatment")
        pixel = root / "pixel"
        pixel.write_bytes(b"pinned pixel binary")
        query_argv = [str(pixel), "impact", "symbol", "--no-refresh", "--depth", "2",
                      "--json", "--metrics", "off"]
        manifest = {
            "pixel_binary_sha256": claude_skill_pair.digest(pixel.read_bytes()),
            "graph_sha256": claude_skill_pair.digest((pixel_dir / "graph.v2.db").read_bytes()),
            "project_source_sha256": claude_skill_pair.directory_digest(
                project, ignored_regular_files={claude_skill_pair.SKILL_TREATMENT_FILE}
            ),
            "pre_model_graph_argv": query_argv,
            "pre_model_graph_query_sha256": claude_skill_pair.digest(b"[{}]"),
        }
        return project, pixel, manifest

    def test_regular_accounting_file_contents_do_not_count_as_source_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pixel = root / ".pixel"
            pixel.mkdir()
            (pixel / "graph.v2.db").write_bytes(b"graph")
            source = root / "src.rs"
            source.write_text("source")
            before = claude_skill_pair.directory_digest(root)

            (pixel / "actions.jsonl").write_text("after action\n")
            (pixel / "calls.json").write_text("after calls\n")

            self.assertEqual(claude_skill_pair.directory_digest(root), before)
            (pixel / "actions.jsonl").write_text("later action\n")
            (pixel / "calls.json").write_text("later calls\n")
            self.assertEqual(claude_skill_pair.directory_digest(root), before)

    def test_source_graph_and_unexpected_sidecar_changes_are_detected(self):
        for relative in ("src.rs", ".pixel/graph.v2.db", ".pixel/unexpected.json"):
            with self.subTest(path=relative), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                pixel = root / ".pixel"
                pixel.mkdir()
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("before")
                before = claude_skill_pair.directory_digest(root)

                path.write_text("after")

                self.assertNotEqual(claude_skill_pair.directory_digest(root), before)

    def test_accounting_symlink_target_changes_are_detected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pixel = root / ".pixel"
            pixel.mkdir()
            (root / "first.jsonl").write_text("first")
            (root / "second.jsonl").write_text("second")
            accounting = pixel / "actions.jsonl"
            accounting.write_text("regular accounting file")
            regular_digest = claude_skill_pair.directory_digest(root)
            accounting.unlink()
            accounting.symlink_to(root / "first.jsonl")
            before = claude_skill_pair.directory_digest(root)
            self.assertNotEqual(before, regular_digest)

            accounting.unlink()
            accounting.symlink_to(root / "second.jsonl")

            self.assertNotEqual(claude_skill_pair.directory_digest(root), before)

    def test_pre_arm_verification_rejects_source_graph_or_binary_changes_before_query(self):
        mutations = {
            "source": ("src.rs", b"changed source"),
            "graph": (".pixel/graph.v2.db", b"changed graph"),
            "pixel": (None, b"changed binary"),
        }
        for name, (relative, contents) in mutations.items():
            with self.subTest(change=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                project, pixel, manifest = self.snapshot_fixture(root)
                changed_path = pixel if relative is None else project / relative
                changed_path.write_bytes(contents)
                with mock.patch.object(claude_skill_pair, "command") as run_query:
                    with self.assertRaisesRegex(RuntimeError, "changed since preflight"):
                        claude_skill_pair.verify_preflight_snapshot(project, str(pixel), manifest)
                run_query.assert_not_called()

    def test_pre_arm_verification_rejects_changed_query_and_allows_accounting_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, pixel, manifest = self.snapshot_fixture(root)

            def successful_query(argv, *, cwd, timeout):
                self.assertEqual(argv, manifest["pre_model_graph_argv"])
                self.assertEqual(cwd, project)
                self.assertEqual(timeout, 5)
                accounting = project / ".pixel/actions.jsonl"
                accounting.write_text("read query accounting\n")
                (project / ".pixel/calls.json").write_text("read query calls\n")
                return subprocess.CompletedProcess(argv, 0, "[{}]", "")

            with mock.patch.object(claude_skill_pair, "command", side_effect=successful_query):
                query_ms = claude_skill_pair.verify_preflight_snapshot(project, str(pixel), manifest)
            self.assertGreaterEqual(query_ms, 0)

            with mock.patch.object(claude_skill_pair, "command", return_value=subprocess.CompletedProcess(
                manifest["pre_model_graph_argv"], 0, "[{\"changed\":true}]", ""
            )):
                with self.assertRaisesRegex(RuntimeError, "graph query changed since preflight"):
                    claude_skill_pair.verify_preflight_snapshot(project, str(pixel), manifest)

    def test_both_arm_commands_share_the_narrow_pixel_read_allowance(self):
        raw = claude_skill_pair.claude_run_argv("claude", "prompt", "sonnet", "medium", 10, 1.0)
        skill = claude_skill_pair.claude_run_argv("claude", "prompt", "sonnet", "medium", 10, 1.0)

        self.assertEqual(raw, skill)
        self.assertEqual(raw[raw.index("--allowedTools") + 1],
                         "Bash(pixel impact * --no-refresh *)")


class ClaudeCliCapabilityTests(unittest.TestCase):
    def test_pair_with_result_event_but_error_exit_is_not_successful(self):
        results = [
            {"result_found": True, "is_error": False, "exit_code": 0},
            {"result_found": True, "is_error": True, "exit_code": 1},
        ]

        self.assertFalse(claude_skill_pair.successful_pair(results))

    def test_pair_rejects_nonzero_exit_without_error_result(self):
        results = [
            {"result_found": True, "is_error": False, "exit_code": 0},
            {"result_found": True, "is_error": False, "exit_code": 1},
        ]

        self.assertFalse(claude_skill_pair.successful_pair(results))

    def test_pair_rejects_error_result_with_zero_exit(self):
        results = [
            {"result_found": True, "is_error": False, "exit_code": 0},
            {"result_found": True, "is_error": True, "exit_code": 0},
        ]

        self.assertFalse(claude_skill_pair.successful_pair(results))

    def test_pair_requires_both_arms_to_exit_successfully(self):
        results = [
            {"result_found": True, "is_error": False, "exit_code": 0},
            {"result_found": True, "is_error": False, "exit_code": 0},
        ]

        self.assertTrue(claude_skill_pair.successful_pair(results))

    def test_hidden_max_turns_flag_is_verified_by_parser_diagnostic(self):
        help_text = "--setting-sources --permission-mode --permission-prompts --allowedTools --disallowedTools " \
            "--max-budget-usd --no-session-persistence --effort"
        with mock.patch.object(claude_skill_pair, "command", side_effect=[
            subprocess.CompletedProcess([], 0, "2.1.289\n", ""),
            subprocess.CompletedProcess([], 0, help_text, ""),
            subprocess.CompletedProcess([], 1, "", "error: option '--max-turns <turns>' argument missing"),
        ]) as run:
            version, help_hash = claude_skill_pair.version_and_help("claude")

        self.assertEqual(version, "2.1.289")
        self.assertEqual(help_hash, claude_skill_pair.digest(help_text.encode()))
        self.assertEqual(run.call_args_list[-1].args[0], ["claude", "--max-turns"])
        self.assertEqual(run.call_args_list[-1].kwargs["timeout"], 5)

    def test_unknown_max_turns_option_is_rejected(self):
        help_text = "--setting-sources --permission-mode --permission-prompts --allowedTools --disallowedTools " \
            "--max-budget-usd --no-session-persistence --effort"
        with mock.patch.object(claude_skill_pair, "command", side_effect=[
            subprocess.CompletedProcess([], 0, "2.1.289\n", ""),
            subprocess.CompletedProcess([], 0, help_text, ""),
            subprocess.CompletedProcess([], 1, "", "error: unknown option '--max-turns'"),
        ]):
            with self.assertRaisesRegex(RuntimeError, "lacks required CLI option --max-turns"):
                claude_skill_pair.version_and_help("claude")


class ClaudeCredentialIsolationTests(unittest.TestCase):
    def setUp(self):
        platform = mock.patch.object(claude_skill_pair.sys, "platform", "darwin")
        platform.start()
        self.addCleanup(platform.stop)

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

    def test_executable_resolution_uses_path_and_reports_missing_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "claude"
            executable.write_text("fixture")
            executable.chmod(0o755)

            with mock.patch.object(claude_skill_pair.shutil, "which", return_value=str(executable)):
                self.assertEqual(claude_skill_pair.resolve_executable(None, "claude"), executable.resolve())
            with mock.patch.object(claude_skill_pair.shutil, "which", return_value=None):
                with self.assertRaisesRegex(RuntimeError, "add it to PATH or pass --claude"):
                    claude_skill_pair.resolve_executable(None, "claude")

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
            pixel.chmod(0o755)
            claude = root / "claude"
            claude.write_text("fixture")
            claude.chmod(0o755)
            sentinel = {
                "accessToken": "sentinel-access-secret",
                "refreshToken": "sentinel-refresh-secret",
            }
            args = mock.Mock(
                repo=str(repo), results_dir=str(results), skill=str(skill),
                scenario=str(scenario), revision="HEAD", auth_config_dir=str(root / "auth"),
                credentials_file=None, claude=str(claude), pixel=str(pixel), symbol="symbol",
                model="sonnet", effort="medium", max_turns=1, max_budget_usd=1.0,
                auth_mode="oauth", gateway_settings=None,
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


class ClaudeOAuthOutputPrivacyTests(unittest.TestCase):
    def test_execute_redacts_oauth_values_before_saving_or_parsing_cli_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            skill = project / ".claude/skills/pixel-impact/SKILL.md"
            skill.parent.mkdir(parents=True)
            skill_source = "---\ndisable-model-invocation: true\n---\nUse the impact skill.\n"
            skill.write_text(skill_source)
            scenario = root / "scenario.json"
            scenario.write_text(json.dumps({"id": "privacy", "prompt": "Find callers."}))
            results = root / "results"
            results.mkdir()
            claude = root / "claude"
            claude.write_text("fixture")
            claude.chmod(0o755)
            pixel = root / "pixel"
            pixel.write_text("fixture pixel binary")
            pixel.chmod(0o755)

            access = 'oauth-access-canary-"-雪'
            refresh = "oauth-refresh-canary-\\secret"
            credentials = root / "credentials.json"
            credentials.write_text(json.dumps({"claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
            }}))
            credentials.chmod(0o600)
            oauth = {"accessToken": access, "refreshToken": refresh}
            manifest = {
                "workspace": str(project),
                "claude_version": "2.1.289",
                "auth_mode": "oauth",
                "credential_source": "explicit-file",
                "scenario_sha256": claude_skill_pair.digest(scenario.read_bytes()),
                "skill_source_sha256": claude_skill_pair.digest(skill_source.encode()),
                "model_alias": "sonnet",
                "effort": "medium",
                "repo_commit": "fixture-commit",
                "workspace_parent": str(root / "workspace-parent"),
            }
            (results / "preflight.json").write_text(json.dumps(manifest))
            stream = "".join(json.dumps(event) + "\n" for event in (
                {"type": "system", "subtype": "init", "model": "claude-sonnet-4-5"},
                {"type": "result", "result": f"Observed {access} and {refresh}.",
                 "usage": {"input_tokens": 7, "output_tokens": 3,
                           "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
            ))
            args = mock.Mock(
                results_dir=str(results), claude=str(claude), pixel=str(pixel),
                auth_mode="oauth", gateway_settings=None, auth_config_dir=None,
                credentials_file=str(credentials), scenario=str(scenario), skill=str(skill),
                model="sonnet", effort="medium", max_turns=3, max_budget_usd=1.0,
                timeout=30,
            )
            cli_result = subprocess.CompletedProcess(
                args=[str(claude)], returncode=0, stdout=stream,
                stderr=f"diagnostic included {access} and {refresh}",
            )

            with (
                mock.patch.object(claude_skill_pair, "version_and_help", return_value=("2.1.289", "help")),
                mock.patch.object(claude_skill_pair, "load_oauth_credentials", return_value=oauth),
                mock.patch.object(claude_skill_pair, "verify_preflight_snapshot", return_value=12),
                mock.patch.object(claude_skill_pair, "command", side_effect=[
                    cli_result, cli_result,
                    subprocess.CompletedProcess(args=["score"], returncode=0,
                                                stdout="score recorded\n", stderr=""),
                ]),
            ):
                rows = claude_skill_pair.execute(args)

            saved_text = "\n".join(
                path.read_text(errors="replace")
                for path in results.rglob("*") if path.is_file()
            )
            for secret in (access, refresh, json.dumps(access)[1:-1], json.dumps(refresh)[1:-1]):
                self.assertNotIn(secret, saved_text)
            self.assertIn("<redacted-oauth-credential>", saved_text)
            self.assertEqual([row["cli_init_model"] for row in rows],
                             ["claude-sonnet-4-5", "claude-sonnet-4-5"])
            transcript = results / "privacy-raw.claude.jsonl"
            parsed = claude_skill_pair.parse_stream(transcript)
            self.assertEqual(parsed["answer"], "Observed <redacted-oauth-credential> and <redacted-oauth-credential>.")


class ClaudeGatewayIsolationTests(unittest.TestCase):
    def setUp(self):
        platform = mock.patch.object(claude_skill_pair.sys, "platform", "darwin")
        platform.start()
        self.addCleanup(platform.stop)

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

    def settings(self, root: Path) -> Path:
        path = root / "settings.json"
        path.write_text(json.dumps({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "gateway-token-secret",
                "ANTHROPIC_BASE_URL": "https://private-gateway.invalid/v1",
                "ANTHROPIC_CUSTOM_HEADERS": "X-Private: header-secret",
                "ANTHROPIC_MODEL": "gateway-default",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gateway-sonnet",
                "ANTHROPIC_API_KEY": "must-not-forward-this",
                "CLAUDE_CODE_OAUTH_TOKEN": "must-not-forward-this-either",
            },
            "hooks": {"SessionStart": [{"command": "must-not-load"}]},
            "permissions": {"allow": ["must-not-load"]},
        }))
        path.chmod(0o600)
        return path

    def test_gateway_allowlist_is_loaded_without_other_settings_or_receipt_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            settings = self.settings(root)

            selected = claude_skill_pair.load_gateway_settings(settings)
            receipt = {
                "auth_mode": "configured-gateway",
                "configured_environment_keys": sorted(selected),
            }
            serialized = json.dumps(receipt)

            self.assertEqual(set(selected), {
                "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "ANTHROPIC_CUSTOM_HEADERS",
                "ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL",
            })
            for secret in ("gateway-token-secret", "private-gateway.invalid", "header-secret",
                           "must-not-forward-this", "must-not-load"):
                self.assertNotIn(secret, serialized)
            self.assertNotIn("ANTHROPIC_API_KEY", selected)
            self.assertNotIn("CLAUDE_CODE_OAUTH_TOKEN", selected)
            self.assertEqual(
                claude_skill_pair.model_selection_receipt("sonnet", selected),
                {
                    "requested_cli_model": "sonnet",
                    "selection_source": "explicit --model CLI argument",
                    "configured_alias_override_key": "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "configured_default_model_present": True,
                },
            )

    def test_both_run_environments_receive_the_same_gateway_snapshot_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            settings = self.settings(root)
            gateway = claude_skill_pair.load_gateway_settings(settings)
            with mock.patch.dict(claude_skill_pair.os.environ, {
                "ANTHROPIC_API_KEY": "ambient-banned",
                "ANTHROPIC_AUTH_TOKEN": "ambient-different-token",
                "ANTHROPIC_BASE_URL": "https://ambient.invalid",
                "CLAUDE_CODE_OAUTH_TOKEN": "ambient-oauth",
            }):
                raw = claude_skill_pair.claude_run_environment(
                    "/tools/pixel", root / "home", root, root / "config-raw", gateway
                )
                skill = claude_skill_pair.claude_run_environment(
                    "/tools/pixel", root / "home", root, root / "config-skill", gateway
                )

            for env in (raw, skill):
                self.assertEqual(env["ANTHROPIC_AUTH_TOKEN"], "gateway-token-secret")
                self.assertEqual(env["ANTHROPIC_BASE_URL"], "https://private-gateway.invalid/v1")
                self.assertEqual(env["ANTHROPIC_CUSTOM_HEADERS"], "X-Private: header-secret")
                self.assertNotIn("ANTHROPIC_API_KEY", env)
                self.assertNotIn("CLAUDE_CODE_OAUTH_TOKEN", env)
                self.assertNotIn("hooks", env)
                self.assertNotIn("permissions", env)
            self.assertEqual(
                {key: raw[key] for key in gateway},
                {key: skill[key] for key in gateway},
            )
            self.assertNotEqual(raw["CLAUDE_CONFIG_DIR"], skill["CLAUDE_CONFIG_DIR"])

    def test_gateway_settings_errors_do_not_echo_private_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            settings = root / "settings.json"
            settings.write_text(json.dumps({"env": {
                "ANTHROPIC_AUTH_TOKEN": "unlogged-private-token",
                "ANTHROPIC_BASE_URL": "https://unlogged.invalid",
                "ANTHROPIC_CUSTOM_HEADERS": {"X-Canary": "unlogged-header"},
            }}))
            settings.chmod(0o600)
            with self.assertRaises(RuntimeError) as caught:
                claude_skill_pair.load_gateway_settings(settings)
            self.assertEqual(str(caught.exception), "allowlisted gateway settings must be strings")
            self.assertNotIn("unlogged-private-token", str(caught.exception))
            self.assertNotIn("unlogged.invalid", "".join(traceback.format_exception(caught.exception)))

    def test_gateway_source_snapshot_is_nonsecret_and_changes_with_file_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            settings = self.settings(Path(directory))
            original_env, original_source = claude_skill_pair.load_gateway_settings_snapshot(settings)
            serialized_source = json.dumps(original_source)
            contents = settings.read_text().replace("gateway-token-secret", "gateway-token-changed")
            settings.write_text(contents)
            changed_env, changed_source = claude_skill_pair.load_gateway_settings_snapshot(settings)

            self.assertEqual(original_env.keys(), changed_env.keys())
            self.assertEqual(original_source["resolved_path"], str(settings.resolve()))
            self.assertNotEqual(original_source, changed_source)
            self.assertNotIn("gateway-token-secret", serialized_source)
            self.assertNotIn("private-gateway.invalid", serialized_source)

    def test_execute_rejects_gateway_source_changed_since_preflight(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            settings = self.settings(root)
            _, source = claude_skill_pair.load_gateway_settings_snapshot(settings)
            contents = settings.read_text().replace("gateway-token-secret", "gateway-token-changed")
            settings.write_text(contents)
            results = root / "results"
            results.mkdir()
            workspace = root / "workspace"
            workspace.mkdir()
            claude = root / "claude"
            claude.write_text("fixture")
            claude.chmod(0o755)
            pixel = root / "pixel"
            pixel.write_text("fixture")
            pixel.chmod(0o755)
            (results / "preflight.json").write_text(json.dumps({
                "workspace": str(workspace),
                "claude_version": "2.1.289",
                "auth_mode": "configured-gateway",
                "auth_environment_keys": sorted(claude_skill_pair.load_gateway_settings(settings)),
                "gateway_settings_source": source,
            }))
            args = mock.Mock(
                results_dir=str(results), claude=str(claude), pixel=str(pixel),
                auth_mode="configured-gateway", gateway_settings=str(settings),
            )
            with mock.patch.object(claude_skill_pair, "version_and_help", return_value=("2.1.289", "")):
                with self.assertRaisesRegex(RuntimeError, "settings changed since preflight"):
                    claude_skill_pair.execute(args)

    def test_gateway_output_redacts_token_endpoint_and_headers(self):
        sensitive = {
            "ANTHROPIC_AUTH_TOKEN": "token-canary",
            "ANTHROPIC_BASE_URL": "https://gateway-canary.invalid/v1",
            "ANTHROPIC_CUSTOM_HEADERS": "X-Canary: header-canary\nX-Second: second-header-canary",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "gateway-sonnet-model",
        }
        output = ("error token-canary https://gateway-canary.invalid/v1 "
                  "X-Canary: header-canary header-canary second-header-canary "
                  "gateway-sonnet-model")

        cleaned = claude_skill_pair.redact_gateway_values(output, sensitive)

        for value in (
            sensitive["ANTHROPIC_AUTH_TOKEN"],
            sensitive["ANTHROPIC_BASE_URL"],
            "X-Canary: header-canary\nX-Second: second-header-canary",
            "header-canary",
            "second-header-canary",
        ):
            self.assertNotIn(value, cleaned)
        self.assertEqual(cleaned.count("<redacted-gateway-setting>"), 5)
        self.assertIn("gateway-sonnet-model", cleaned)

    def test_gateway_output_redacts_json_escaped_header_values(self):
        header_value = 'header-café-雪-quote-"-and-slash-\\-secret'
        gateway_env = {
            "ANTHROPIC_AUTH_TOKEN": "token-canary",
            "ANTHROPIC_BASE_URL": "https://gateway-canary.invalid/v1",
            "ANTHROPIC_CUSTOM_HEADERS": "X-Private: " + header_value,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "gateway-sonnet-model",
        }
        serialized = json.dumps({
            "error": "gateway rejected X-Private: " + header_value,
            "model": "gateway-sonnet-model",
        })

        cleaned = claude_skill_pair.redact_gateway_values(serialized, gateway_env)

        self.assertNotIn(header_value, cleaned)
        self.assertNotIn(json.dumps(header_value)[1:-1], cleaned)
        self.assertIn("gateway-sonnet-model", cleaned)

    def test_reported_model_mismatch_remains_visible_after_gateway_redaction(self):
        gateway_env = {
            "ANTHROPIC_AUTH_TOKEN": "private-token",
            "ANTHROPIC_BASE_URL": "https://private.invalid",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "gateway-sonnet",
        }

        def parsed_model(model: str) -> dict:
            stream = "".join(json.dumps(event) + "\n" for event in (
                {"type": "system", "subtype": "init", "model": model},
                {"type": "result", "result": "Done", "modelUsage": {model: {}}},
            ))
            redacted = claude_skill_pair.redact_gateway_values(stream, gateway_env)
            with tempfile.TemporaryDirectory() as directory:
                transcript = Path(directory) / "stream.jsonl"
                transcript.write_text(redacted)
                return claude_skill_pair.parse_stream(transcript)

        same = [
            {"arm": "raw", "result_found": True, "is_error": False,
             **parsed_model("gateway-sonnet")},
            {"arm": "skill", "result_found": True, "is_error": False,
             **parsed_model("gateway-sonnet")},
        ]
        result = claude_skill_pair.require_matching_reported_models(same)
        self.assertEqual(result["status"], "matched")
        self.assertIn("not attested", result["source"])
        changed = [*same]
        changed[1] = {
            **same[1],
            **parsed_model("different-model"),
        }
        with self.assertRaisesRegex(RuntimeError, "identity differs between arms"):
            claude_skill_pair.require_matching_reported_models(changed)

    def test_pair_without_any_cli_reported_model_is_unverifiable(self):
        rows = [
            {"arm": arm, "result_found": True, "is_error": False,
             "cli_init_model": None, "resolved_model_usage": None}
            for arm in ("raw", "skill")
        ]
        with self.assertRaisesRegex(RuntimeError, "did not report model identity"):
            claude_skill_pair.require_matching_reported_models(rows)

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

    def test_default_login_uses_its_private_file_before_keychain(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            self.write_credential(home / ".claude/.credentials.json", self.credential())
            with mock.patch.object(claude_skill_pair.Path, "home", return_value=home):
                with mock.patch.object(claude_skill_pair.subprocess, "run") as run:
                    actual = claude_skill_pair.load_oauth_credentials(None)
            self.assertEqual(actual["accessToken"], "test-access-token")
            run.assert_not_called()

    def test_default_login_uses_unscoped_keychain_for_current_account(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.CompletedProcess(
                args=[], returncode=0, stdout=json.dumps(self.credential()).encode(), stderr=b"",
            )
            with mock.patch.object(claude_skill_pair.Path, "home", return_value=Path(directory)):
                with mock.patch.dict(claude_skill_pair.os.environ, {"USER": "arena-user"}):
                    with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result) as run:
                        actual = claude_skill_pair.load_oauth_credentials(None)
            self.assertEqual(actual["refreshToken"], "test-refresh-token")
            self.assertEqual(run.call_args.args[0], [
                "/usr/bin/security", "find-generic-password", "-a", "arena-user",
                "-s", "Claude Code-credentials", "-w",
            ])

    def test_explicit_default_directory_stays_scoped_and_does_not_fall_back(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            config = home / ".claude"
            result = subprocess.CompletedProcess(args=[], returncode=44, stdout=b"", stderr=b"")
            with mock.patch.object(claude_skill_pair.Path, "home", return_value=home):
                with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result) as run:
                    with self.assertRaisesRegex(RuntimeError, "Keychain has no Claude OAuth"):
                        claude_skill_pair.load_oauth_credentials(config)
            run.assert_called_once()
            argv = run.call_args.args[0]
            self.assertEqual(argv[argv.index("-s") + 1],
                             "Claude Code-credentials-" + claude_skill_pair.digest(str(config).encode())[:8])

    def test_scoped_keychain_uses_claude_nfc_path_spelling(self):
        result = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=json.dumps(self.credential()).encode(), stderr=b"",
        )
        with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result) as run:
            claude_skill_pair.load_oauth_credentials(Path("/tmp/pixel-cafe\u0301"), source="macos-keychain")
        argv = run.call_args.args[0]
        self.assertEqual(argv[argv.index("-s") + 1], "Claude Code-credentials-98f38245")

    def test_missing_file_uses_scoped_keychain_entry_without_exposing_secret(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            config.mkdir()
            secret_json = json.dumps(self.credential())
            result = subprocess.CompletedProcess(
                args=[], returncode=0, stdout=secret_json.encode(),
                stderr=b'password: "{"',
            )
            with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result) as run:
                actual = claude_skill_pair.load_oauth_credentials(config)

            service = "Claude Code-credentials-" + claude_skill_pair.digest(
                str(config).encode())[:8]
            self.assertEqual(claude_skill_pair.credential_source(config), "macos-keychain")
            self.assertEqual(actual["accessToken"], "test-access-token")
            argv = run.call_args.args[0]
            env = run.call_args.kwargs["env"]
            self.assertIn(service, argv)
            self.assertEqual(argv[-1], "-w")
            self.assertNotIn("ANTHROPIC_API_KEY", env)

            isolated = Path(directory) / "isolated-config"
            isolated.mkdir(mode=0o700)
            claude_skill_pair.write_isolated_credentials(isolated, actual)
            saved = isolated / ".credentials.json"
            self.assertEqual(saved.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(saved.read_text()), {"claudeAiOauth": actual})

    def test_invalid_keychain_bytes_fail_without_exposing_secret(self):
        for payload in (b'{"sentinel-secret":', b'\xffsentinel-secret'):
            with self.subTest(payload_type="utf8" if payload.startswith(b"{") else "invalid-utf8"):
                result = subprocess.CompletedProcess(
                    args=[], returncode=0, stdout=payload, stderr=b"",
                )
                with tempfile.TemporaryDirectory() as directory:
                    with mock.patch.object(claude_skill_pair.subprocess, "run", return_value=result):
                        try:
                            claude_skill_pair.load_oauth_credentials(Path(directory))
                        except RuntimeError as error:
                            self.assertEqual(str(error), "macOS Keychain Claude OAuth payload is invalid")
                            self.assertNotIn("sentinel-secret", "".join(traceback.format_exception(error)))
                        else:
                            self.fail("invalid Keychain payload was accepted")

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
