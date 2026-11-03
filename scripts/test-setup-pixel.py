#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Exercise the consumer installer, including refusal before publishing PATH."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import importlib.util
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / '.github/actions/setup-pixel/install.sh'
spec = importlib.util.spec_from_file_location('configure', SCRIPT.with_name('configure.py'))
configure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(configure)


class CacheConfiguration(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='pixel cache ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        self.output = self.root / 'output'
        self.env = dict(GITHUB_WORKSPACE=str(self.root), GITHUB_OUTPUT=str(self.output),
                        RUNNER_OS='Linux', RUNNER_ARCH='X64', PIXEL_ACTION_PATH='.',
                        PIXEL_ACTION_VERSION='v0.8.0', PIXEL_ACTION_PREPARE='true',
                        PIXEL_ACTION_CACHE='true', PIXEL_ACTION_CACHE_VERSION='1',
                        PIXEL_ACTION_MINIMUM_SYMBOLS='1', PIXEL_ACTION_INSTALL_HOOKS='false')

    def config(self, **overrides):
        self.output.write_text('')
        with patch.dict(os.environ, self.env | overrides):
            configure.configure()
        return self.output.read_text()

    def test_agent_list_syntax_is_bounded_before_setup(self):
        for agents in ('all', 'claude', 'codex', 'claude,codex', 'agents-md'):
            self.config(PIXEL_ACTION_HOOKS_AGENT=agents)
        for agents in ('', 'claude,', 'claude codex', '--remove', 'claude\n'):
            with self.subTest(agents=agents), self.assertRaises(ValueError):
                self.config(PIXEL_ACTION_HOOKS_AGENT=agents)

    def test_cache_keys_separate_incompatible_indexes(self):
        base = self.config().split('cache-key=')[1].splitlines()[0]
        (self.root / 'nested').mkdir()
        subprocess.run(['git', 'init', '-q', str(self.root / 'nested')], check=True)
        for overrides in [dict(RUNNER_OS='macOS'), dict(RUNNER_ARCH='ARM64'),
                          dict(PIXEL_ACTION_VERSION='v0.8.1'), dict(PIXEL_ACTION_PATH='nested'),
                          dict(PIXEL_ACTION_CACHE_VERSION='2')]:
            with self.subTest(overrides=overrides):
                self.assertNotIn(f'cache-key={base}\n', self.config(**overrides))

    def test_cache_cannot_include_config_plugins_or_session_data(self):
        output = self.config()
        paths = output.split('cache-paths<<PIXEL_INDEX_PATHS\n')[1].splitlines()[:-1]
        self.assertEqual({Path(p).name for p in paths},
                         {'base.shard', 'delta.shard', 'state.json', 'graph.v2.db',
                          'graph.v2.db-wal', 'graph.v2.db-shm'})
        self.assertEqual(self.config(PIXEL_ACTION_PATH=str(self.root)), output)

    def test_invalid_inputs_and_symlinked_cache_are_refused(self):
        for overrides in [dict(PIXEL_ACTION_CACHE='yes'), dict(PIXEL_ACTION_INSTALL_HOOKS='yes'),
                          dict(PIXEL_ACTION_MINIMUM_SYMBOLS='-1'), dict(PIXEL_ACTION_VERSION='latest')]:
            with self.subTest(overrides=overrides), self.assertRaises(ValueError):
                self.config(**overrides)
        (self.root / '.pixel').symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(ValueError):
            self.config()

    def test_tracked_cache_is_never_overwritten(self):
        (self.root / '.pixel').mkdir()
        (self.root / '.pixel/state.json').write_text('{}')
        subprocess.run(['git', '-C', str(self.root), 'add', '-f', '.pixel'], check=True)
        with self.assertRaises(ValueError):
            self.config()

    def test_invalid_or_truncated_graph_cannot_be_saved(self):
        report = self.root / 'report.json'
        for symbols in [None, '1000', 0, True, 1]:
            report.write_text(json.dumps({'graph': {'symbols': symbols}}))
            with patch.dict(os.environ, self.env):
                if type(symbols) is int and symbols == 1:
                    configure.validate_report(report)
                else:
                    with self.assertRaises(ValueError):
                        configure.validate_report(report)


class SetupPixel(unittest.TestCase):
    def run_install(self, *, corrupt=False, binary_version='0.6.1', version='v0.6.1', arch='X64'):
        with tempfile.TemporaryDirectory(prefix='pixel action ') as directory:
            root = Path(directory)
            archive = 'pixel-v0.6.1-x86_64-unknown-linux-musl.tar.gz'
            payload = f'#!/bin/sh\necho "pixel {binary_version}"\n'.encode()
            with tarfile.open(root / archive, 'w:gz') as tar:
                entry = tarfile.TarInfo(archive.removesuffix('.tar.gz') + '/bin/pixel')
                entry.size, entry.mode = len(payload), 0o755
                tar.addfile(entry, io.BytesIO(payload))
            digest = hashlib.sha256((root / archive).read_bytes()).hexdigest()
            (root / (archive + '.sha256')).write_text(f'{"0" * 64 if corrupt else digest}  {archive}\n')
            fake = root / 'tools'
            fake.mkdir()
            curl = fake / 'curl'
            curl.write_text('#!/bin/bash\nwhile [[ "$1" != https:* ]]; do shift; done\nname=${1##*/}\nshift 2\ncp "$FIXTURE/$name" "$1"\n')
            curl.chmod(0o755)
            env = dict(os.environ, PATH=f'{fake}:{os.environ["PATH"]}', FIXTURE=str(root),
                       RUNNER_TEMP=str(root), RUNNER_OS='Linux', RUNNER_ARCH=arch,
                       PIXEL_ACTION_VERSION=version, PIXEL_ACTION_PREPARE='true')
            for name in ['GITHUB_PATH', 'GITHUB_OUTPUT', 'GITHUB_ENV', 'GITHUB_STEP_SUMMARY']:
                env[name] = str(root / name)
            result = subprocess.run(['bash', str(SCRIPT)], env=env, text=True, capture_output=True)
            published = (root / 'GITHUB_PATH').exists()
            if result.returncode == 0:
                installed = Path((root / 'GITHUB_PATH').read_text().strip()) / 'pixel'
                self.assertEqual(subprocess.check_output([str(installed), '-V'], text=True).strip(), 'pixel 0.6.1')
                self.assertIn('PIXEL_DAEMON_AUTO_START=0', (root / 'GITHUB_ENV').read_text())
            return result.returncode, published

    def test_verified_binary_is_available_to_later_steps_even_with_spaces(self):
        self.assertEqual(self.run_install(), (0, True))

    def test_corrupt_download_cannot_be_published(self):
        code, published = self.run_install(corrupt=True)
        self.assertNotEqual(code, 0)
        self.assertFalse(published)

    def test_wrong_version_cannot_shadow_existing_cli(self):
        code, published = self.run_install(binary_version='0.5.2')
        self.assertNotEqual(code, 0)
        self.assertFalse(published)

    def test_unpinned_version_and_unsupported_runner_are_rejected(self):
        for options in [{'version': 'latest'}, {'version': 'v0.6.1\nevil'}, {'arch': 'X86'}]:
            with self.subTest(options=options):
                code, published = self.run_install(**options)
                self.assertNotEqual(code, 0)
                self.assertFalse(published)


if __name__ == '__main__':
    unittest.main()
