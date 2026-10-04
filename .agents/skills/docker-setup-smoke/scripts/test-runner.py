#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Verify mode selection, failed-run evidence and the fake model script without Docker."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


def assert_pinned_base(case, build, image):
    """The build names the base by tag and by a full sha256 digest, never by tag alone."""
    case.assertIn(f'BASE_IMAGE={image}', build)
    digests = [arg for arg in build if arg.startswith('BASE_DIGEST=')]
    case.assertEqual(len(digests), 1, build)
    case.assertRegex(digests[0], r'^BASE_DIGEST=[0-9a-f]{64}$')


class RunnerContract(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        scripts = self.root / "repo/.agents/skills/docker-setup-smoke/scripts"
        scripts.mkdir(parents=True)
        self.runner = scripts / "run.sh"
        shutil.copyfile(Path(__file__).with_name("run.sh"), self.runner)
        shutil.copyfile(Path(__file__).with_name("Dockerfile"), scripts / "Dockerfile")
        tools = self.root / "bin"
        tools.mkdir()
        docker = tools / "docker"
        docker.write_text("""#!/usr/bin/env python3
import json, os, sys
with open(os.environ['CALL_LOG'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1] == 'build':
    print('build output retained')
    sys.exit(int(os.environ.get('BUILD_EXIT', '0')))
if sys.argv[1:3] == ['image', 'inspect']:
    print('sha256:' + 'b' * 64)
if sys.argv[1] == 'run':
    print('container output retained')
    sys.exit(int(os.environ.get('RUN_EXIT', '0')))
""")
        docker.chmod(0o755)
        git = tools / "git"
        # A leaked GIT_DIR would make the runner record another repository's HEAD.
        git.write_text('#!/bin/sh\nif [ -n "${GIT_DIR:-}" ]; then echo leaked; else printf \'%040d\\n\' 1; fi\n')
        git.chmod(0o755)
        self.log = self.root / "calls.jsonl"
        self.env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}", CALL_LOG=str(self.log))

    def run_case(self, *args, exit_code=0, build_exit=0):
        env = dict(self.env, RUN_EXIT=str(exit_code), BUILD_EXIT=str(build_exit))
        result = subprocess.run(["sh", str(self.runner), *args], env=env,
                                capture_output=True, text=True, timeout=10)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, calls

    def test_source_modes_build_the_requested_head_instead_of_a_release_or_merge_ref(self):
        sha = "a" * 40
        for args, ref in [(('--source', 'main'), 'refs/heads/main'),
                          (('--pr', '427'), 'refs/pull/427/head'), (('--source', sha), sha)]:
            with self.subTest(args=args):
                result, calls = self.run_case(*args)
                self.assertEqual(result.returncode, 0, result.stderr)
                run = [call for call in calls if call[0] == 'run'][-1]
                self.assertIn(f'PIXEL_SOURCE_REF={ref}', run)
                self.assertIn('PIXEL_BOOTSTRAP=source.sh', run)
                self.assertIn('PIXEL_BOOTSTRAP_TIMEOUT=1800', run)
                self.assertIn('pixel-setup-smoke:source', run)
                self.assertNotIn('refs/pull/427/merge', run)
                build = [call for call in calls if call[0] == 'build'][-1]
                assert_pinned_base(self, build, 'rust:1.98.1-bookworm')
                self.assertIn('pixel-setup-smoke:source', build)

    def test_release_mode_downloads_the_named_release(self):
        result, calls = self.run_case('v0.6.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        run = next(call for call in calls if call[0] == 'run')
        self.assertIn('PIXEL_RELEASE=v0.6.1', run)
        self.assertIn('PIXEL_BOOTSTRAP=bootstrap.sh', run)
        self.assertIn('PIXEL_SOURCE_REF=', run)
        self.assertIn('pixel-setup-smoke:release', run)
        build = next(call for call in calls if call[0] == 'build')
        assert_pinned_base(self, build, 'debian:bookworm-slim')
        evidence = next((self.root / 'repo/target/docker-setup-smoke').iterdir())
        self.assertIn('image: pixel-setup-smoke:release sha256:' + 'b' * 64,
                      (evidence / 'identity.txt').read_text())

    def test_distribution_modes_install_through_the_channel_a_new_user_would_use(self):
        cases = [('--installer', 'installer.sh', 'tester', 'debian:bookworm-slim',
                  'APT_SOURCE_PARTS=/etc/apt/sources.list.d/'),
                 ('--brew', 'brew.sh', 'linuxbrew', 'homebrew/brew',
                  'APT_SOURCE_PARTS=/nonexistent')]
        for flag, bootstrap, user, base, apt in cases:
            with self.subTest(flag=flag):
                result, calls = self.run_case(flag)
                self.assertEqual(result.returncode, 0, result.stderr)
                run = [call for call in calls if call[0] == 'run'][-1]
                self.assertIn(f'PIXEL_BOOTSTRAP={bootstrap}', run)
                self.assertIn(f'PIXEL_TEST_USER={user}', run)
                self.assertIn('PIXEL_RELEASE=latest', run)
                self.assertIn(f'pixel-setup-smoke:{flag[2:]}', run)
                build = [call for call in calls if call[0] == 'build'][-1]
                assert_pinned_base(self, build, base)
                self.assertIn(apt, build)

    def test_provenance_names_this_checkout_even_under_an_inherited_git_dir(self):
        self.env['GIT_DIR'] = str(self.root / 'elsewhere.git')
        result, _ = self.run_case('v0.6.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        identity = (next((self.root / 'repo/target/docker-setup-smoke').iterdir()) / 'identity.txt').read_text()
        self.assertIn('checkout: ' + '0' * 39 + '1\n', identity)
        command = next(line for line in identity.splitlines() if line.startswith('command: '))
        self.assertEqual(command, f"command: sh '{self.runner}' 'v0.6.1'")

    def test_agents_flag_adds_pinned_agent_clis_and_runs_the_sessions_after_the_checks(self):
        result, calls = self.run_case('--agents', '--pr', '427')
        self.assertEqual(result.returncode, 0, result.stderr)
        build = next(call for call in calls if call[0] == 'build')
        self.assertIn('NODE_VERSION=v24.21.0', build)
        packages = next(arg for arg in build if arg.startswith('AGENT_PACKAGES='))
        for pinned in ('@anthropic-ai/claude-code@2.', '@openai/codex@0.', '@earendil-works/pi-coding-agent@0.'):
            self.assertIn(pinned, packages)
        self.assertIn('pixel-setup-smoke:source-agents', build)
        run = next(call for call in calls if call[0] == 'run')
        self.assertIn('PIXEL_AGENTS=1', run)
        self.assertIn('PIXEL_SOURCE_REF=refs/pull/427/head', run)
        self.assertLess(run[-1].index('checks.sh'), run[-1].index('agents.sh'))

    def test_without_agents_flag_no_node_or_agent_cli_is_installed(self):
        result, calls = self.run_case('v0.6.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        build = next(call for call in calls if call[0] == 'build')
        self.assertIn('NODE_VERSION=', build)
        self.assertIn('AGENT_PACKAGES=', build)
        self.assertIn('PIXEL_AGENTS=0', next(call for call in calls if call[0] == 'run'))

    def test_ambiguous_or_invalid_selectors_never_provision_a_container(self):
        for args in [('v0.6.1', '--source', 'main'), ('--pr', '0'), ('--source', 'bad'), ('--pr', '427;echo'),
                     ('--brew', 'v0.6.1'), ('--installer', '--brew'), ('--brew', '--agents'),
                     ('--agents', '--agents')]:
            with self.subTest(args=args):
                result, calls = self.run_case(*args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])

    def test_container_failure_keeps_its_status_and_exports_evidence_before_cleanup(self):
        result, calls = self.run_case('--pr', '427', exit_code=23)
        self.assertEqual(result.returncode, 23)
        self.assertLess(next(i for i, call in enumerate(calls) if call[0] == 'cp'),
                        next(i for i, call in enumerate(calls) if call[0] == 'rm'))
        evidence = next((self.root / 'repo/target/docker-setup-smoke').iterdir())
        self.assertEqual((evidence / 'exit-status.txt').read_text(), '23\n')
        self.assertIn('container output retained', (evidence / 'run.log').read_text())

    def test_image_build_failure_keeps_its_log_and_never_starts_the_checks(self):
        result, calls = self.run_case('v0.6.1', build_exit=17)
        self.assertEqual(result.returncode, 1)
        self.assertFalse(any(call[0] == 'run' for call in calls))
        evidence = next((self.root / 'repo/target/docker-setup-smoke').iterdir())
        self.assertEqual((evidence / 'exit-status.txt').read_text(), '1\n')
        self.assertIn('build output retained', (evidence / 'build.log').read_text())


class FakeModelScript(unittest.TestCase):
    """The scripted model must walk both wire formats through the same steps."""

    def setUp(self):
        os.environ['FAKE_LLM_COMMANDS'] = 'pixel search-content -F x\ngrep -rn x'
        spec = importlib.util.spec_from_file_location('fake_llm', Path(__file__).with_name('fake-llm.py'))
        self.llm = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.llm)

    def test_messages_calls_each_command_in_order_then_answers(self):
        tools = [{'name': 'Read'}, {'name': 'Bash'}]
        blocks, stop = self.llm.anthropic_turn({'tools': tools, 'messages': [{'role': 'user', 'content': 'q'}]})
        self.assertEqual((blocks[0]['input']['command'], stop), ('pixel search-content -F x', 'tool_use'))
        result = {'role': 'user', 'content': [{'type': 'tool_result', 'content': 'ok'}]}
        blocks, stop = self.llm.anthropic_turn({'tools': tools, 'messages': [result]})
        self.assertEqual((blocks[0]['input']['command'], stop), ('grep -rn x', 'tool_use'))
        blocks, stop = self.llm.anthropic_turn({'tools': tools, 'messages': [result, result]})
        self.assertEqual((blocks, stop), ([{'type': 'text', 'text': 'FAKE_LLM_DONE'}], 'end_turn'))

    def test_a_request_without_a_shell_tool_gets_the_final_answer(self):
        blocks, stop = self.llm.anthropic_turn({'tools': [{'name': 'Read'}], 'messages': []})
        self.assertEqual(stop, 'end_turn')

    def test_responses_fills_the_shell_tool_shape_codex_offers(self):
        shapes = [({'cmd': {'type': 'string'}}, {'cmd': 'pixel search-content -F x'}),
                  ({'command': {'type': 'array'}}, {'command': ['bash', '-lc', 'pixel search-content -F x']})]
        for props, expected in shapes:
            tool = {'type': 'function', 'name': 'exec_command', 'parameters': {'properties': props}}
            item = self.llm.responses_turn({'tools': [tool], 'input': []})[0]
            self.assertEqual(json.loads(item['arguments']), expected)
        done = self.llm.responses_turn({'tools': [tool], 'input': [{'type': 'function_call_output'}] * 2})
        self.assertEqual(done[0]['content'][0]['text'], 'FAKE_LLM_DONE')


if __name__ == '__main__':
    unittest.main()
