#!/usr/bin/env python3
"""Replay the release race using real commits, including a content-neutral merge."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / '.agents/skills/release/check-candidate.py'


class CandidateContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        self.env = {**os.environ, 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
        self.git('init', '-qb', 'main')
        self.git('config', 'user.name', 'Release test')
        self.git('config', 'user.email', 'release@example.invalid')
        self.write('CHANGELOG.md', '## [Unreleased]\n')
        self.write('changelog.d/1.fixed.md', '**cli:** fixed\n')
        self.base = self.commit()
        self.git('switch', '-qc', 'prepare')
        self.write('CHANGELOG.md', '## [Unreleased]\n\n## [0.7.0]\n- fixed\n')
        (self.repo / 'changelog.d/1.fixed.md').unlink()
        self.head = self.commit()
        self.git('switch', '-q', 'main')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.repo, env=self.env, text=True).strip()

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git('add', '-A')
        self.git('commit', '-q', '--allow-empty', '-m', 'fixture')
        return self.git('rev-parse', 'HEAD')

    def run_guard(self, tip, merge=None):
        args = ['python3', str(SCRIPT), self.base, self.head, '--tip', tip]
        if merge:
            args += ['--merge', merge]
        return subprocess.run(args, cwd=self.repo, env=self.env, capture_output=True, text=True)

    def squash(self):
        self.git('merge', '--squash', 'prepare')
        return self.commit()

    def test_exact_candidate_can_merge_and_tag_even_if_main_later_advances(self):
        result = self.run_guard(self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        merge = self.squash()
        self.write('later', 'not shipped')
        tip = self.commit()
        result = self.run_guard(tip, merge)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_main_advancing_before_merge_invalidates_even_an_identical_tree(self):
        tip = self.commit()  # no content change: ancestry still changed
        self.assertNotEqual(self.run_guard(tip).returncode, 0)
        merge = self.squash()
        result = self.run_guard(merge, merge)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('merge parent', result.stderr)

    def test_editing_the_merge_content_cannot_ship_under_candidate_validation(self):
        self.squash()
        self.write('unexpected-code', 'unvalidated')
        self.git('add', '-A')
        self.git('commit', '-q', '--amend', '--no-edit')
        merge = self.git('rev-parse', 'HEAD')
        result = self.run_guard(merge, merge)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('merge tree', result.stderr)

    def test_merge_must_belong_to_the_release_target(self):
        merge = self.squash()
        self.assertNotEqual(self.run_guard(self.base, merge).returncode, 0)

    def test_unreleased_fragments_and_code_changes_refuse_premerge(self):
        for name in ('changelog.d/missing.fixed.md', 'code.py'):
            with self.subTest(name=name):
                self.git('switch', '-q', 'prepare')
                self.write(name, 'left out of the release')
                self.head = self.commit()
                self.assertNotEqual(self.run_guard(self.base).returncode, 0)
                self.git('reset', '--hard', 'HEAD^')

    def test_a_preexisting_fragment_left_uncut_refuses(self):
        self.git('switch', '-q', 'prepare')
        self.git('restore', '--source', self.base, '--', 'changelog.d')
        self.head = self.commit()
        result = self.run_guard(self.base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unreleased changelog fragments', result.stderr)

    def test_missing_or_empty_candidate_cannot_pass(self):
        for head in (self.base, 'does-not-exist'):
            self.head = head
            self.assertNotEqual(self.run_guard(self.base).returncode, 0)


if __name__ == '__main__':
    unittest.main()
