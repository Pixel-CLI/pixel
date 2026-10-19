#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Source identities must survive a shared setting name; never guess a binding."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import sys

PROBE = Path(os.environ.get('PROBE_UNDER_TEST', Path(__file__).resolve().parents[1] / 'probe'))

LOADER = '''import os
class Options:
    def __init__(self, defaults):
        self.data = defaults.copy()
    def load_env_file(self, env_name, silent=False):
        """doc-sentinel: deliberately excluded from executable evidence."""
        path = os.environ.get(env_name)
        if not path:
            if silent:
                return False
            raise ValueError("literal-sentinel")
        return self.load_file(path, silent=silent)
    def load_file(self, path, silent=False):
        values = parse_file(path)
        return self.apply(values)
    def apply(self, values):
        for key, value in values.items():
            self[key] = value
        diagnostic = f"fstring-sentinel {key}"
        password = 99887766  # comment-sentinel
'''


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

    def test_a_maximum_bound_question_matches_a_zoom_constant(self):
        root = self.repo({'packages/common/src/constants.ts':
                          'export const MIN_ZOOM = 0.1;\nexport const MAX_ZOOM = 30;\n'})
        out = self.emit(root, 'where is the maximum zoom level defined and what is its value')
        self.assertIn('MAX_ZOOM | constant default 30 | packages/common/src/constants.ts:2', out)
        self.assertNotIn('MIN_ZOOM', out)
        self.assertIn('2| export const MAX_ZOOM = 30;', out)

    def test_a_maximum_pool_question_matches_the_pool_bound(self):
        root = self.repo({'packages/common/src/constants.ts':
                          'export const MAX_POOL_SIZE = 64;\nexport const MAX_IMAGE_SIZE = 8192;\n'})
        out = self.emit(root, 'where is the maximum pool size defined and what is its value')
        self.assertIn('MAX_POOL_SIZE | constant default 64 | packages/common/src/constants.ts:1', out)
        self.assertNotIn('MAX_IMAGE_SIZE', out)

    def test_a_maximum_question_does_not_match_an_unrelated_bound(self):
        root = self.repo({'packages/common/src/constants.ts':
                          'export const REMOTE_MODEL = "local";\n'
                          'export const MAX_IMAGE_SIZE = 8192;\n'
                          '// database cache tracks used size between measurements\n'
                          'export const EVICT_STEP = 25;\n'})
        for prompt in ('where is the maximum remote database pool size defined',
                       'where is the max remote database pool size defined',
                       'where is the minimum remote database pool size defined',
                       'where is the min remote database pool size defined'):
            with self.subTest(prompt=prompt):
                out = self.emit(root, prompt)
                self.assertNotIn('REMOTE_MODEL', out)
                self.assertNotIn('MAX_IMAGE_SIZE', out)
                self.assertNotIn('EVICT_STEP', out)

    def test_existing_config_intent_keeps_matching_remote_setting(self):
        root = self.repo({'packages/common/src/constants.ts': 'export const REMOTE_MODEL = "local";\n'})
        out = self.emit(root, 'where is the remote model setting configured')
        self.assertIn('REMOTE_MODEL | constant default "local"', out)

    def test_the_manifest_routes_a_maximum_bound_lookup_without_config_words(self):
        worktree = PROBE.parents[2]
        path = worktree / 'scripts/bench/probe-test.py'
        spec = importlib.util.spec_from_file_location('probe_test', path)
        probe_test = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe_test)
        probe = probe_test.Probe(PROBE.parent)
        self.assertTrue(probe.applies('lookup', 'where is the maximum zoom level defined and what is its value'))

    def test_literal_setting_calls_are_selected_by_generic_prompt_key(self):
        root = self.repo({'src/app.js': "app.set('etag', 'weak');\n",
                          'src/other.js': "app.set('trust proxy', true);\n"})
        out = self.emit(root, 'where is the default etag setting defined and what is its value')
        self.assertIn("app.set('etag', 'weak');", out)
        self.assertIn('### src/app.js:1-1 — literal setting call candidate', out)
        self.assertNotIn('trust proxy', out)

    def test_literal_setting_calls_require_tracked_non_symlink_source(self):
        root = self.repo({'src/app.js': "app.set('etag', 'weak');\n"}, tracked=['src/app.js'])
        (root / 'src/untracked.js').write_text("app.set('etag', 'untracked-sentinel');\n")
        external = root.parent / (root.name + '-external-setter')
        external.write_text("app.set('etag', 'symlink-sentinel');\n")
        self.addCleanup(external.unlink)
        (root / 'src/linked.js').symlink_to(external)
        subprocess.run(['git', 'add', '--', 'src/linked.js'], cwd=root, check=True)
        out = self.emit(root, 'where is the default etag setting defined and what is its value')
        self.assertIn("app.set('etag', 'weak');", out)
        self.assertNotIn('untracked-sentinel', out)
        self.assertNotIn('symlink-sentinel', out)

    def test_literal_setting_calls_keep_the_source_file_size_bound(self):
        out = self.emit(self.repo({'src/app.js': "app.set('etag', 'weak');\n",
                                   'src/large.js': "app.set('etag', 'oversize-sentinel');\n" + 'x' * (400 * 1024)}),
                        'where is the default etag setting defined and what is its value')
        self.assertIn("app.set('etag', 'weak');", out)
        self.assertNotIn('oversize-sentinel', out)

    def test_literal_setting_source_masks_secret_values(self):
        out = self.emit(self.repo({'src/app.js': 'app.set("API_TOKEN", "setter-secret-sentinel");\n'}),
                        'where is the API_TOKEN environment setting defined')
        self.assertIn('literal setting call candidate', out)
        self.assertNotIn('setter-secret-sentinel', out)

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

    def test_env_named_file_loader_keeps_failure_mode_and_overwrite_chain_together(self):
        out = self.emit(self.repo({'loader.py': LOADER}), 'Which environment-file overrides apply?')
        for evidence in ['Candidate Options.load_env_file @ loader.py:5', 'self.data = defaults.copy()',
                         'path = os.environ.get(env_name)', 'if silent:', 'return False',
                         'self.load_file(path, silent=silent)', 'self.apply(values)', 'self[key] = value']:
            self.assertIn(evidence, out, 'the lookup alone cannot establish how defaults are overwritten')
        self.assertIn('active mode, file contents and effective values unknown', out)
        self.assertNotIn('doc-sentinel', out)
        self.assertNotIn('### loader.py:5-7', out, 'a source range must not credit the omitted docstring')

    def test_loader_candidates_do_not_collapse_homonymous_methods_across_classes(self):
        source = LOADER + LOADER.replace('import os\n', '').replace('Options', 'OtherOptions')
        out = self.emit(self.repo({'loader.py': source}), 'Which environment-file overrides apply?')
        self.assertIn('Candidate Options.load_env_file @ loader.py:5', out)
        self.assertIn('Candidate OtherOptions.load_env_file', out)
        self.assertEqual(out.count('self[key] = value'), 2)

    def test_cli_setting_follows_option_name_to_its_function_scoped_runtime_value(self):
        source = '''
trait Flag {}
struct WorkerCount;
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { "workers" }
    fn doc_short(&self) -> &'static str { "Set worker count." }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    let workers = if let Some(workers) = low.workers {
        workers
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    workers
}
'''
        out = self.emit(self.repo({'flags.rs': source}), 'how many workers does the program use and where can I override it')
        self.assertIn('matched setting consumed within one function', out)
        self.assertIn('available_parallelism()', out)
        self.assertIn('low.workers', out)
        self.assertIn('matching CLI setting declaration and override', out)
        self.assertIn('fn name_long', out)
        self.assertIn('args.workers', out)
        self.assertRegex(out, r"### flags\.rs:\d+-\d+ — matching CLI setting declaration and override")

    def test_cli_setting_does_not_join_an_unrelated_parallelism_function(self):
        source = '''
trait Flag {}
struct WorkerCount;
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { "workers" }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn configured(low: LowArgs) -> Option<usize> { low.workers }
fn unrelated_capacity() -> usize { std::thread::available_parallelism().map_or(1, |n| n.get()) }
'''
        out = self.emit(self.repo({'flags.rs': source}), 'how many workers does the program use and where can I override it')
        self.assertEqual(out, '')

    def test_cli_setting_selector_matches_a_different_option_name(self):
        source = '''
trait Flag {}
struct PoolSize;
impl Flag for PoolSize {
    fn name_long(&self) -> &'static str { "database-pool" }
    fn update(&self, args: &mut LowArgs) { args.pool_size = Some(12); }
}
fn resolve(low: LowArgs) -> usize {
    let pool_size = low.pool_size.unwrap_or(12);
    pool_size
}
'''
        out = self.emit(self.repo({'flags.rs': source}), 'where can I override the database pool option')
        self.assertIn('database-pool', out)
        self.assertIn('args.pool_size', out)
        self.assertIn('low.pool_size', out)
        self.assertIn('matched setting consumed within one function', out)

    def test_cli_setting_selector_ignores_test_only_flag_implementations(self):
        source = '''
trait Flag {}
struct WorkerCount;
#[cfg(test)]
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { "workers" }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    low.workers.unwrap_or(8)
}
'''
        out = self.emit(self.repo({'flags.rs': source}), 'how many workers does the program use and where can I override it')
        self.assertEqual(out, '')

    def test_cli_setting_selector_ignores_flag_implementations_in_test_modules(self):
        source = '''
trait Flag {}
struct WorkerCount;
#[cfg(test)]
mod tests {
    use super::*;
    impl Flag for WorkerCount {
        fn name_long(&self) -> &'static str { "workers" }
        fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
    }
}
fn resolve(low: LowArgs) -> usize {
    low.workers.unwrap_or(8)
}
'''
        out = self.emit(self.repo({'flags.rs': source}), 'how many workers does the program use and where can I override it')
        self.assertEqual(out, '')

    def test_cli_setting_resolves_an_explicit_short_option_to_its_runtime_value(self):
        source = '''
trait Flag {}
struct LineBuffered;
impl Flag for LineBuffered {
    fn name_long(&self) -> &'static str { "line-buffered" }
    fn update(&self, args: &mut LowArgs) { args.buffer = BufferMode::Line; }
}
struct Threads;
impl Flag for Threads {
    fn name_short(&self) -> Option<u8> { Some(b'j') }
    fn name_long(&self) -> &'static str { "threads" }
    fn doc_short(&self) -> &'static str { "Set the approximate number of threads." }
    fn update(&self, args: &mut LowArgs) { args.threads = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    if let Some(threads) = low.threads { threads }
    else { std::thread::available_parallelism().map_or(1, |n| n.get()).min(12) }
}
'''
        out = self.emit(self.repo({'flags.rs': source}),
                        'When I omit -j, how is the worker count calculated, and what command-line flag lets me set it?')
        self.assertIn("Some(b'j')", out)
        self.assertIn('name_long', out)
        self.assertIn('available_parallelism()', out)
        self.assertIn('low.threads', out)
        self.assertNotIn('LineBuffered', out, 'the prose token "command-line" cannot outrank explicit -j')

    def test_cli_setting_short_option_does_not_match_a_different_flag(self):
        source = '''
trait Flag {}
struct Threads;
impl Flag for Threads {
    fn name_short(&self) -> Option<u8> { Some(b'k') }
    fn name_long(&self) -> &'static str { "threads" }
    fn update(&self, args: &mut LowArgs) { args.threads = Some(8); }
}
fn resolve(low: LowArgs) -> usize { low.threads.unwrap_or(8) }
'''
        out = self.emit(self.repo({'flags.rs': source}), 'When I omit -j, how is the worker count calculated?')
        self.assertEqual(out, '')

    def test_compiled_setting_connects_the_named_compiler_and_exact_getter_consumer(self):
        root = self.repo({
            'lib/application.js': '''
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  this.settings[setting] = val;
  switch (setting) {
    case 'trust proxy':
      this.set('trust proxy fn', compileTrust(val));
      break;
  }
};
''',
            'lib/utils.js': '''
exports.compileTrust = function(val) {
  const API_TOKEN = "compiler-secret-sentinel";
  if (typeof val === 'function') return val;
  if (typeof val === 'number') return function(a, i){ return i < val };
  return proxyaddr.compile(val || []);
};
''',
            'lib/request.js': '''
defineGetter(req, 'ip', function ip(){
  var trust = this.app.get('trust proxy fn');
  return proxyaddr(this, trust);
});
''',
        })
        out = self.emit(root, 'Which code compiles the trust proxy setting, and how does req.ip use it?')
        for evidence in ["case 'trust proxy'", "compileTrust(val)", 'exports.compileTrust',
                         "defineGetter(req, 'ip'", "get('trust proxy fn')", 'proxyaddr(this, trust)']:
            self.assertIn(evidence, out)
        self.assertNotIn('compiler-secret-sentinel', out)

    def test_compiled_setting_does_not_join_a_different_getter_or_comment_decoy(self):
        root = self.repo({
            'lib/application.js': '''
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  switch (setting) { case 'trust proxy': this.set('trust proxy fn', compileTrust(val)); }
};
// case 'trust proxy': this.set('trust proxy fn', fakeCompile(val));
''',
            'lib/utils.js': "exports.compileTrust = function(val) { return proxyaddr.compile(val || []); };\n",
            'lib/request.js': '''
defineGetter(req, 'ip', function ip(){ return proxyaddr(this, this.app.get('session proxy fn')); });
defineGetter(req, 'secure', function secure(){ return this.app.get('trust proxy fn'); });
''',
        })
        out = self.emit(root, 'Which code compiles the trust proxy setting, and how does req.ip use it?')
        self.assertNotIn('named getter reads the matching compiled setting', out)
        self.assertNotIn('exact imported setting compiler implementation', out)
        self.assertNotIn('proxyaddr(this, trust)', out)

    def test_compiled_setting_ignores_comment_only_compile_branches(self):
        root = self.repo({
            'lib/application.js': '''
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  switch (setting) { case 'other proxy': this.set('other proxy fn', compileTrust(val)); }
};
// case 'trust proxy': this.set('trust proxy fn', compileTrust(val));
''',
            'lib/utils.js': "exports.compileTrust = function(val) { return proxyaddr.compile(val || []); };\n",
            'lib/request.js': '''
defineGetter(req, 'ip', function ip(){ return proxyaddr(this, this.app.get('trust proxy fn')); });
''',
        })
        out = self.emit(root, 'Which code compiles the trust proxy setting, and how does req.ip use it?')
        self.assertNotIn('setting branch installs a compiled function', out)
        self.assertNotIn('named getter reads the matching compiled setting', out)

    def test_persistent_browser_preference_pairs_exact_subject_key_and_resolves_import(self):
        root = self.repo({
            'src/preferences.ts': '''import { PREFERENCE_KEYS } from "./keys";
export function useTheme() {
  const theme = localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME) || "light";
  localStorage.setItem(PREFERENCE_KEYS.COLOR_THEME, theme);
  return theme;
}
export function useAccount() {
  const token = localStorage.getItem(PREFERENCE_KEYS.ACCOUNT_TOKEN);
  localStorage.setItem(PREFERENCE_KEYS.ACCOUNT_TOKEN, token || "");
  return token;
}
''',
            'src/keys.ts': '''export const PREFERENCE_KEYS = {
  COLOR_THEME: "draw-color-scheme",
  ACCOUNT_TOKEN: "account-secret",
} as const;
''',
        })
        out = self.emit(root, 'how does the color theme setting persist between sessions')
        self.assertIn('browser preference read/write using the same subject-matched storage key', out)
        self.assertIn('localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME)', out)
        self.assertIn('localStorage.setItem(PREFERENCE_KEYS.COLOR_THEME, theme)', out)
        self.assertIn('COLOR_THEME: "draw-color-scheme"', out)
        self.assertNotIn('ACCOUNT_TOKEN', out)

    def test_persistent_browser_preference_rejects_unpaired_and_unrelated_storage_keys(self):
        root = self.repo({
            'src/preferences.ts': '''import { PREFERENCE_KEYS } from "./keys";
export function useTheme() {
  return localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME);
}
export function useAccount() {
  localStorage.setItem(PREFERENCE_KEYS.ACCOUNT_TOKEN, "value");
}
''',
            'src/keys.ts': '''export const PREFERENCE_KEYS = {
  COLOR_THEME: "draw-color-scheme",
  ACCOUNT_TOKEN: "account-secret",
} as const;
''',
        })
        out = self.emit(root, 'how does the color theme setting persist between sessions')
        self.assertNotIn('browser preference read/write using the same subject-matched storage key', out)
        self.assertNotIn('localStorage.getItem', out)
        self.assertNotIn('localStorage.setItem', out)

    def test_an_unconnected_read_or_absent_writer_is_not_a_loader_chain(self):
        for source in [LOADER.replace('self.load_file(path,', 'self.load_file(other_path,'),
                       LOADER.replace('self[key] = value', 'print(key)')]:
            with self.subTest(source=source):
                out = self.emit(self.repo({'loader.py': source}), 'Which environment-file overrides apply?')
                self.assertNotIn('configuration loader', out)

    def test_loader_literals_fstrings_numbers_comments_and_docstrings_are_not_values(self):
        out = self.emit(self.repo({'loader.py': LOADER}), 'Which environment-file overrides apply?')
        self.assertIn('self[key] = value', out, 'generic key identifiers must retain the overwrite evidence')
        self.assertIn('<masked>', out)
        for sentinel in ['literal-sentinel', 'fstring-sentinel', '99887766', 'comment-sentinel', 'doc-sentinel']:
            self.assertNotIn(sentinel, out)

    def test_loader_source_requires_tracking_and_does_not_follow_symlinks(self):
        root = self.repo({'loader.py': LOADER, 'keep.js': 'const OTHER_LIMIT = 10;\n'}, tracked=['keep.js'])
        self.assertNotIn('configuration loader', self.emit(root, 'Which environment-file overrides apply?'))
        (root / 'link.py').symlink_to(root / 'loader.py')
        subprocess.run(['git', 'add', '--', 'link.py'], cwd=root, check=True)
        self.assertNotIn('configuration loader', self.emit(root, 'Which environment-file overrides apply?'))

    def test_loader_evidence_stays_absent_for_unrelated_intent(self):
        self.assertEqual(self.emit(self.repo({'loader.py': LOADER}), 'Give me a tour of this repository'), '')

    def test_loader_candidate_cap_reports_unresolved_omissions(self):
        source = 'import os\n' + ''.join(LOADER.replace('import os\n', '').replace('Options', f'Options{i}') for i in range(3))
        out = self.emit(self.repo({'loader.py': source}), 'Which environment-file overrides apply?')
        self.assertEqual(out.count('Candidate Options'), 2)
        self.assertIn('Additional loader candidates omitted: 1; no unique loader established.', out)


if __name__ == '__main__':
    unittest.main(verbosity=2)
