#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Doc admission must preserve code intent, quoted ranges and source fidelity (#1234)."""
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / 'probe'
ROOT = Path(__file__).resolve().parents[3]
GIT_ENV = {'PATH': os.environ['PATH'], 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1'}
GUIDE = '# Environment variables\n\nEnvironment variables configure loading.\nFirst setting.\nSecond setting.\nLast setting.\n'


def output(prompt, files=None, kind='config', brief=''):
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        for name, content in (files or {'docs/guide.md': GUIDE}).items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(content)
        for args in (['init', '-q'], ['add', '.']):
            subprocess.run(['git', *args], cwd=root, env=GIT_ENV, check=True, capture_output=True)
        request = {'repo_root': str(root), 'prompt': prompt, 'kind': kind, 'brief': brief, 'session_files': []}
        return subprocess.run([str(PROBE)], input=json.dumps(request), text=True, capture_output=True,
                              check=True).stdout


def headers(text):
    return [line for line in text.splitlines() if line.startswith('### ')]


class Admission(unittest.TestCase):
    def test_source_navigation_and_unclassified_questions_do_not_receive_matching_docs(self):
        for kind in ('lookup', 'tests', 'impact', 'rationale', None):
            with self.subTest(kind=kind):
                prompt = ('environment variables configure loading' if kind is None
                          else 'where is the environment variable loading constant defined')
                self.assertEqual(output(prompt, kind=kind), '')

    def test_config_and_overview_keep_documentation_with_exact_source_rows(self):
        for kind in ('config', 'overview', 'flow', 'bugfix'):
            with self.subTest(kind=kind):
                text = output('how do environment variables configure loading', kind=kind)
                self.assertEqual(headers(text), ['### docs/guide.md:1-6 — Environment variables (documentation section)'])
                self.assertIn('3| Environment variables configure loading.\n', text)

    def test_flow_question_leaves_the_common_budget_for_the_existing_code_block(self):
        spec = importlib.util.spec_from_file_location('merge_spec', ROOT / 'scripts/bench/probe-test.py')
        merge_spec = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(merge_spec)
        brief = '[PIXEL:BRIEF]\n### src/main.py:1-1 — entry\n1| dispatch()\n'
        code = ('### src/worker.py:10-12 — useful call path\n10| return handler(request)\n'
                '11| if failure:\n12|     raise failure\n')
        docs = output('trace environment variable loading through load_config', kind='rationale', brief=brief)
        merged = merge_spec.merge(brief, [('doc-sections', 'append', docs), ('flow-rank', 'append', code)], cap=400)
        self.assertIn(code.strip(), merged)
        self.assertNotIn('(documentation section)', merged)

    def test_overlap_is_split_into_exact_unquoted_ranges_despite_a_different_label(self):
        brief = '### docs/guide.md:3-4 — native excerpt\n3| Environment variables configure loading.\n4| First setting.\n'
        text = output('environment variables configure loading', brief=brief)
        self.assertEqual(headers(text), ['### docs/guide.md:1-2 — Environment variables (documentation section)',
                                         '### docs/guide.md:5-6 — Environment variables (documentation section)'])
        self.assertNotIn('3| ', text)
        self.assertNotIn('4| ', text)
        self.assertIn('5| Second setting.\n', text)

    def test_a_fully_quoted_section_is_not_replaced_by_a_lower_ranked_section(self):
        quoted = ''.join('### ' + name + ':1-6 — native excerpt\n' + '\n'.join(
            f'{n}| {line}' for n, line in enumerate(GUIDE.splitlines(), 1)) + '\n'
            for name in ('docs/a.md', 'docs/b.md'))
        files = {'docs/a.md': GUIDE, 'docs/b.md': GUIDE,
                 'docs/c.md': '# Environment variables\n\nAnother setting.\n'}
        self.assertEqual(output('environment variables configure loading', files, brief=quoted), '')

    def test_a_pointer_without_quoted_rows_does_not_prevent_inline_documentation(self):
        text = output('environment variables configure loading', brief='read: docs/guide.md:1-6\n')
        self.assertIn('### docs/guide.md:1-6', text)

    def test_section_limit_announces_the_unquoted_tail_without_extending_the_header(self):
        text = output('environment variables configure loading', {'docs/guide.md': GUIDE + 'More prose.\n' * 150})
        self.assertEqual(headers(text), ['### docs/guide.md:1-121 — Environment variables (documentation section)'])
        self.assertIn('121| More prose.\n', text)
        self.assertNotIn('122| ', text)
        self.assertIn('excerpt truncated: section continues after line 121 (ends at 156)', text)

    def test_long_rows_announce_the_cut_and_keep_the_exact_source_prefix(self):
        long = 'A' * 240
        text = output('environment variables configure loading', {'docs/guide.md': GUIDE + long + '\n'})
        self.assertIn('7| ' + long[:160] + ' … [truncated]\n', text)
        self.assertNotIn(long[:161], text)


if __name__ == '__main__':
    unittest.main()
