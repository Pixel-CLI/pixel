#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Source identities must survive a shared setting name; never guess a binding."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import sys

PROBE = Path(os.environ.get('PROBE_UNDER_TEST', Path(__file__).resolve().parents[1] / 'probe'))


class DeclarationIdentity(unittest.TestCase):
    def repo(self, files, tracked=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        subprocess.run(['git', 'init', '-q', str(root)], check=True)
        for rel, text in files.items():
            path = root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(['git', 'add', '--', *(files if tracked is None else tracked)], cwd=root, check=True)
        return root

    def emit(self, root, prompt):
        req = {'prompt': prompt, 'repo_root': str(root), 'brief': '', 'kind': 'config', 'session_files': []}
        proc = subprocess.run([sys.executable, '-I', str(PROBE)], input=json.dumps(req), text=True,
                              capture_output=True, timeout=5, check=True)
        return proc.stdout

    def test_beta_keeps_its_declaration_instead_of_inheriting_alpha_default(self):
        root = self.repo({
            'a.js': 'export const DEFAULT_TIMEOUT = 1101;\nexport function alpha() { return DEFAULT_TIMEOUT; }\n',
            'b.js': 'export const DEFAULT_TIMEOUT = 2202;\nexport function beta() { return DEFAULT_TIMEOUT; }\n',
        })
        out = self.emit(root, 'Where is the DEFAULT_TIMEOUT setting defined and read in beta?')
        self.assertIn('DEFAULT_TIMEOUT @ b.js:1 | candidate value 2202', out)
        self.assertIn('DEFAULT_TIMEOUT @ a.js:1 | candidate value 1101', out)
        self.assertIn('2| export function beta()', out)
        self.assertIn('lexical binding and override precedence unknown', out)
        self.assertNotIn('constant default 1101', out, 'a module-local value cannot become the default of beta')

    def test_same_file_scopes_keep_distinct_coordinates_without_inventing_bindings(self):
        root = self.repo({'scope.js': 'function alpha() {\n  const DEFAULT_TIMEOUT = 1101;\n}\n'
                                    'function beta() {\n  const DEFAULT_TIMEOUT = 2202;\n}\n'})
        out = self.emit(root, 'Where is the DEFAULT_TIMEOUT setting in beta?')
        self.assertIn('DEFAULT_TIMEOUT @ scope.js:2 | candidate value 1101', out)
        self.assertIn('DEFAULT_TIMEOUT @ scope.js:5 | candidate value 2202', out)
        self.assertIn('4| function beta()', out)
        self.assertIn('lexical binding and override precedence unknown', out)

    def test_a_unique_constant_retains_its_source_value(self):
        out = self.emit(self.repo({'only.js': 'export const REQUEST_LIMIT = 37;\n'}),
                        'Where is the REQUEST_LIMIT setting declared?')
        self.assertIn('REQUEST_LIMIT | constant default 37 | only.js:1', out)
        self.assertIn('1| export const REQUEST_LIMIT = 37;', out)
        self.assertNotIn('distinct declaration candidates', out)

    def test_an_unrelated_intent_has_no_registry_output(self):
        root = self.repo({'only.js': 'export const REQUEST_LIMIT = 37;\n'})
        self.assertEqual(self.emit(root, 'Give me a tour of this repository'), '')

    def test_env_candidates_do_not_establish_mode_precedence(self):
        root = self.repo({'.env.development': 'APP_PORT=3101\n', '.env.production': 'APP_PORT=4101\n',
                          'main.js': 'const port = process.env.APP_PORT || 2101;\n'})
        out = self.emit(root, 'Which APP_PORT configuration applies in production?')
        self.assertIn('APP_PORT @ .env.development:1 | candidate value 3101', out)
        self.assertIn('APP_PORT @ .env.production:1 | candidate value 4101', out)
        self.assertIn('lexical binding and override precedence unknown', out)
        self.assertNotIn('set in .env.development', out)
        self.assertIn('process.env.APP_PORT || 2101', out)

    def test_untracked_env_templates_are_not_evidence(self):
        root = self.repo({'main.js': 'const port = process.env.APP_PORT || 2101;\n',
                          '.env.example': 'APP_PORT=5101\n', '.env.development': 'APP_PORT=6101\n'},
                         tracked=['main.js'])
        out = self.emit(root, 'What is the APP_PORT environment variable default?')
        self.assertIn('2101', out)
        self.assertNotIn('5101', out)
        self.assertNotIn('6101', out)
        self.assertNotIn('### .env.', out)

    def test_a_tracked_env_template_keeps_its_nonsecret_value(self):
        root = self.repo({'.env.example': 'APP_PORT=3101\n',
                          'main.js': 'const port = process.env.APP_PORT || 2101;\n'})
        out = self.emit(root, 'What is the APP_PORT environment variable default?')
        self.assertIn('set in .env.example:1 = 3101', out)
        self.assertIn('code fallback 2101', out)
        self.assertIn('1| APP_PORT=3101', out)

    def test_env_lane_retains_the_general_scan_line_budget(self):
        root = self.repo({'.env.example': 'SERVICE_ORIGIN=' + 'x' * 301 + '\n',
                          'main.js': 'const origin = process.env.SERVICE_ORIGIN;\n'})
        out = self.emit(root, 'Where is the SERVICE_ORIGIN environment variable declared?')
        self.assertNotIn('### .env.example:', out, 'an alternate input lane must preserve the existing scan bound')
        self.assertNotIn('x' * 100, out)

    def test_env_line_budget_counts_utf8_bytes_like_ripgrep(self):
        root = self.repo({'.env.example': 'SERVICE_ORIGIN=' + 'é' * 150 + '\n',
                          'main.js': 'const origin = process.env.SERVICE_ORIGIN;\n'})
        out = self.emit(root, 'Where is the SERVICE_ORIGIN environment variable declared?')
        self.assertNotIn('### .env.example:', out, 'multibyte text cannot bypass the shared byte budget')
        self.assertNotIn('é' * 100, out)

    def test_a_tracked_env_symlink_is_not_followed(self):
        root = self.repo({'main.js': 'const port = process.env.APP_PORT || 2101;\n'})
        external = root.parent / (root.name + '-external-env')
        external.write_text('APP_PORT=7101\n')
        self.addCleanup(external.unlink)
        (root / '.env.development').symlink_to(external)
        subprocess.run(['git', 'add', '--', '.env.development'], cwd=root, check=True)
        out = self.emit(root, 'What is the APP_PORT environment variable default?')
        self.assertNotIn('7101', out)
        self.assertNotIn('### .env.development:', out)

    def test_secrets_are_masked_in_candidates_fallbacks_and_source_context(self):
        root = self.repo({'.env.development': 'SERVICE_TOKEN=env-sentinel\n',
                          'main.js': 'const session = process.env.SERVICE_TOKEN || "fallback-sentinel";\n'})
        out = self.emit(root, 'Where is the SERVICE_TOKEN environment variable default?')
        self.assertIn('SERVICE_TOKEN', out)
        self.assertIn('<masked>', out)
        for sentinel in ['env-sentinel', 'fallback-sentinel']:
            self.assertNotIn(sentinel, out, 'a value cannot leak through a different evidence sink')

    def test_candidate_context_masks_a_neighbouring_secret(self):
        root = self.repo({'a.js': 'export const REQUEST_LIMIT = 37;\nconst API_KEY = "neighbor-sentinel";\n',
                          'b.js': 'export const REQUEST_LIMIT = 42;\n'})
        out = self.emit(root, 'Where is the REQUEST_LIMIT setting declared?')
        self.assertIn('REQUEST_LIMIT @ a.js:1', out)
        self.assertIn('REQUEST_LIMIT @ b.js:1', out)
        self.assertNotIn('neighbor-sentinel', out)

    def test_bounded_candidates_report_omissions_instead_of_claiming_a_unique_default(self):
        root = self.repo({f'{i}.js': f'export const REQUEST_LIMIT = {30+i};\n' for i in range(6)})
        out = self.emit(root, 'Where is the REQUEST_LIMIT setting declared?')
        self.assertEqual(out.count('REQUEST_LIMIT @'), 4)
        self.assertIn('2 additional candidates omitted; no unique default established', out)


if __name__ == '__main__':
    unittest.main(verbosity=2)
