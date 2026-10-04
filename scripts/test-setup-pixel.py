#!/usr/bin/env python3
"""Exercise the consumer installer, including refusal before publishing PATH."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / '.github/actions/setup-pixel/install.sh'


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
