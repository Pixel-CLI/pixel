#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Exercise workflow matrix expressions: cheap PRs must retain release coverage."""

import json
from pathlib import Path
import re
from types import SimpleNamespace
import unittest

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / '.github/workflows'
LINUX = {'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl'}
MAC = 'aarch64-apple-darwin'


def expression(text, key, event, base):
    match = re.search(r'^\s+' + key + r': \$\{\{ (.+) \}\}$', text, re.M)
    assert match, f'missing matrix expression: {key}'
    # These expressions use only ==, &&, ||, fromJSON and startsWith on strings.
    # Evaluate the workflow itself, not a second copy of its event policy.
    source = match[1].replace('&&', ' and ').replace('||', ' or ')
    return eval(source, {'__builtins__': {}}, {
        'github': SimpleNamespace(event_name=event, base_ref=base),
        'fromJSON': json.loads, 'startsWith': str.startswith,
    })


def cross_lanes(event, base):
    text = (WORKFLOWS / 'cross-build.yml').read_text()
    matrix = text.split('      matrix:\n', 1)[1].split('    steps:', 1)[0]
    # Fail if matrix syntax grows beyond the subset expanded by this test.
    assert 'include:' not in matrix
    exclusions = re.findall(r'- target: (\S+)\n\s+profile: (\S+)', matrix)
    lanes = {(target, profile)
             for target in expression(matrix, 'target', event, base)
             for profile in expression(matrix, 'profile', event, base)}
    return lanes - set(exclusions)


class Cadence(unittest.TestCase):
    def test_ordinary_and_stacked_prs_do_not_allocate_macos(self):
        for base, profile in [('main', 'dev-release'), ('perf/cosine-lanes', 'release')]:
            with self.subTest(base=base):
                self.assertEqual(cross_lanes('pull_request', base),
                                 {(target, profile) for target in LINUX})

    def test_main_proves_native_release_without_duplicate_macos_profile(self):
        self.assertEqual(cross_lanes('push', ''),
                         {(target, profile) for target in LINUX
                          for profile in ('release', 'dev-release')} | {(MAC, 'release')})

    def test_maintenance_cannot_rely_on_a_main_push(self):
        self.assertEqual(cross_lanes('pull_request', 'release/0.48'),
                         {(target, 'release') for target in LINUX | {MAC}})

    def test_homebrew_prs_are_linux_but_native_checks_remain_reachable(self):
        text = (WORKFLOWS / 'homebrew-core.yml').read_text()
        self.assertIn('  schedule:', text)
        self.assertIn('  workflow_dispatch:', text)
        for event, expected in [('pull_request', ['ubuntu-26.04']),
                                ('schedule', ['macos-15', 'ubuntu-26.04']),
                                ('workflow_dispatch', ['macos-15', 'ubuntu-26.04'])]:
            with self.subTest(event=event):
                self.assertEqual(expression(text, 'os', event, ''), expected)


if __name__ == '__main__':
    unittest.main()
