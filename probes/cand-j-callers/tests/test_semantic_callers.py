#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Synthetic integration checks for source-derived caller fallbacks."""
import json
from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader
import os
import shutil
import subprocess
import tempfile
import time
import unittest
from unittest import mock
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
SPEC = spec_from_loader("cand_j_callers_probe", SourceFileLoader("cand_j_callers_probe", str(PROBE)))
PROBE_MODULE = module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE_MODULE)
FAKE_PIXEL = """#!/usr/bin/env python3
import json
print(json.dumps({"symbols": [], "edges": []}))
"""


def run(prompt, files, pixel_script=FAKE_PIXEL, observe_git=False):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for rel, text in files.items():
            path = root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "add", "."], cwd=root, check=True)
        fake = root / "fake-pixel"
        fake.write_text(pixel_script)
        fake.chmod(0o755)
        marker = root / "git-invoked"
        env = {**os.environ, "PIXEL_BIN": str(fake)}
        if observe_git:
            wrapper_dir = root / "test-bin"
            wrapper_dir.mkdir()
            wrapper = wrapper_dir / "git"
            wrapper.write_text(
                "#!/bin/sh\n"
                "for arg in \"$@\"; do\n"
                "  if [ \"$arg\" = \"ls-files\" ]; then : > \"$PIXEL_TEST_GIT_MARKER\"; fi\n"
                "done\n"
                f"exec {shutil.which('git')} \"$@\"\n"
            )
            wrapper.chmod(0o755)
            env.update({"PATH": f"{wrapper_dir}:{env['PATH']}", "PIXEL_TEST_GIT_MARKER": str(marker)})
        req = {"repo_root": str(root), "prompt": prompt, "brief": ""}
        result = subprocess.run(
            [str(PROBE)], input=json.dumps(req), text=True, capture_output=True,
            check=True, env=env,
        )
        return (result.stdout, marker.exists()) if observe_git else result.stdout


class SemanticCallerFallbacks(unittest.TestCase):
    def test_id_factory_reports_definition_and_id_assignment_callers(self):
        out = run("Which modules call the element ID generator?", {
            "src/ids.ts": "export const makeId = () => nanoid();\n",
            "src/copy.ts": "import { makeId } from './ids';\nconst copy = { id: makeId() };\n",
            "src/store.ts": "import { makeId as createId } from './ids';\nreturn { id: createId() };\n",
        })
        self.assertIn("src/ids.ts", out)
        self.assertIn("src/copy.ts", out)
        self.assertIn("src/store.ts", out)
        self.assertIn("id: makeId()", out)

    def test_id_factory_resolves_typescript_alias_and_barrel_export(self):
        out = run("Which modules call the element ID generator?", {
            "tsconfig.json": '{"compilerOptions":{"baseUrl":".","paths":{"@core/*":["src/*"]}}}',
            "src/random.ts": "export const makeElementId = () => nanoid();\n",
            "src/index.ts": "export * from './random';\n",
            "src/canvas.ts": "import { makeElementId as createId } from '@core/index';\nconst element = { id: createId() };\n",
        })
        self.assertIn("src/random.ts", out)
        self.assertIn("src/canvas.ts", out)
        self.assertIn("id: createId()", out)

    def test_shadowed_import_is_not_counted_as_factory_call(self):
        out = run("Which modules call the element ID generator?", {
            "src/ids.ts": "export const makeElementId = () => nanoid();\n",
            "src/canvas.ts": "import { makeElementId } from './ids';\n"
                "function render() { const makeElementId = () => fallback(); return { id: makeElementId() }; }\n",
        })
        self.assertEqual(out, "")

    def test_id_factory_reference_in_comment_or_string_is_not_a_callsite(self):
        out = run("Which modules call the element ID generator?", {
            "src/ids.ts": "export const makeElementId = () => nanoid();\n",
            "src/canvas.ts": "import { makeElementId } from './ids';\n"
                "// makeElementId() creates an element\n"
                "const note = 'makeElementId()';\n",
        })
        self.assertEqual(out, "")

    def test_import_spelled_only_in_comment_does_not_bind_factory_call(self):
        out = run("Which modules call the element ID generator?", {
            "src/ids.ts": "export const makeElementId = () => nanoid();\n",
            "src/canvas.ts": "/* import { makeElementId } from './ids'; */\n"
                "const shape = { id: makeElementId() };\n",
        })
        self.assertEqual(out, "")

    def test_explicit_member_reports_same_object_header_method_callers(self):
        out = run("Which parts of lib call req.get?", {
            "lib/request.js": "req.get =\n  req.header = function header(name) {\n  return this.headers[name];\n};\n"
                "req.range = function range(size) {\n  return this.get('Range');\n};\n"
                "req.protocol = function protocol() {\n  return this.get('X-Forwarded-Proto');\n};\n",
            "examples/demo.js": "req.get('Content-Type');\n",
        })
        self.assertIn("lib/request.js", out)
        self.assertIn("this.get('Range')", out)
        self.assertIn("this.get('X-Forwarded-Proto')", out)
        self.assertNotIn("examples/demo.js", out)

    def test_request_context_push_binds_factory_receiver_and_method_owner(self):
        out = run("Which code calls the request context when it is pushed?", {
            "src/ctx.py": "class _Globals:\n    def push(self):\n        pass\n\n"
                "class AppContext:\n    \"\"\"A request context merged into the app context.\"\"\"\n"
                "    def push(self):\n        self.activate_request()\n",
            "src/app.py": "class App:\n    def request_context(self, env):\n        return AppContext(env)\n\n"
                "    def wsgi_app(self, env):\n        ctx = self.request_context(env)\n        ctx.push()\n",
        })
        self.assertIn("AppContext.push", out)
        self.assertIn("wsgi_app", out)
        self.assertIn("ctx.push()", out)
        self.assertNotIn("_Globals.push", out)

    def test_generic_workflow_id_question_abstains(self):
        out = run("Can you generate an ID for this workflow ticket?", {
            "src/ids.ts": "export const makeId = () => nanoid();\n",
            "src/copy.ts": "const copy = { id: makeId() };\n",
        })
        self.assertEqual(out, "")

    def test_element_lookup_name_is_not_a_random_id_factory(self):
        out = run("Which modules call the element ID generator?", {
            "src/dom.ts": "export const getElementById = (id) => document.getElementById(id);\n"
                "export const unrelated = () => {\n  const random = Math.random();\n  return random;\n};\n",
            "src/use.ts": "const element = getElementById('canvas');\n",
        })
        self.assertEqual(out, "")

    def test_same_named_id_factories_in_different_modules_are_ambiguous(self):
        out = run("Which modules call the element ID generator?", {
            "src/ids/element.ts": "export const makeElementId = () => nanoid();\n",
            "src/ids/copy.ts": "export const makeElementId = () => lookupElement();\n",
            "src/canvas.ts": "import { makeElementId } from './ids/element';\nconst shape = { id: makeElementId() };\n",
            "src/duplicate.ts": "import { makeElementId } from './ids/copy';\nconst duplicate = { id: makeElementId() };\n",
        })
        self.assertEqual(out, "")

    def test_member_fallback_ignores_other_objects_this_method(self):
        out = run("Which parts of lib call req.get?", {
            "lib/request.js": "req.get = function get(name) { return headers[name]; };\n"
                "req.range = function range(size) { return this.get('Range'); };\n"
                "class OtherRequest {\n  range() { return this.get('not a request header'); }\n}\n",
        })
        self.assertIn("this.get('Range')", out)
        self.assertNotIn("not a request header", out)

    def test_getter_helper_is_followed_only_with_receiver_binding_proof(self):
        out = run("Which parts of lib call req.get?", {
            "lib/request.js": "req.get = function get(name) { return headers[name]; };\n"
                "function defineGetter(obj, name, getter) {\n"
                "  Object.defineProperty(obj, name, { configurable: true, get: getter });\n"
                "}\n"
                "defineGetter(req, 'protocol', function protocol() {\n"
                "  return this.get('X-Forwarded-Proto');\n"
                "});\n",
        })
        self.assertIn("this.get('X-Forwarded-Proto')", out)
        self.assertIn("getter calls this.get on the proven receiver", out)

    def test_getter_call_does_not_trust_comment_or_string_helper_definition(self):
        out = run("Which parts of lib call req.get?", {
            "lib/request.js": "req.get = function get(name) { return headers[name]; };\n"
                "/* function defineGetter(obj, name, getter) { "
                "Object.defineProperty(obj, name, { get: getter }); } */\n"
                "const note = \"function defineGetter(obj, name, getter) {\";\n"
                "defineGetter(req, 'protocol', function protocol() {\n"
                "  return this.get('X-Forwarded-Proto');\n"
                "});\n",
        })
        self.assertNotIn("X-Forwarded-Proto", out)

    def test_graph_resolved_symbol_does_not_emit_semantic_fallback_blocks(self):
        pixel = '''#!/usr/bin/env python3
import json
print(json.dumps({"symbols": [{"kind": "function", "name": "makeElementId", "path": "src/ids.ts",
  "start_line": 1, "end_line": 1, "uid": "sym-1"}], "edges": [], "edge_limit": 20}))
'''
        out = run("Which modules call the ID generator makeElementId?", {
            "src/ids.ts": "export const makeElementId = () => nanoid();\n",
            "src/canvas.ts": "import { makeElementId } from './ids';\nconst shape = { id: makeElementId() };\n",
        }, pixel)
        self.assertNotIn("ID factory makeElementId", out)

    def test_tracked_source_enforces_one_inventory_file_cap(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("a.js", "b.js", "c.js", "d.js"):
                (root / name).write_text("const value = 1;\n")
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            previous = PROBE_MODULE.MAX_SOURCE_FILES
            try:
                PROBE_MODULE.MAX_SOURCE_FILES = 2
                texts, partial = PROBE_MODULE.tracked_source(root)
            finally:
                PROBE_MODULE.MAX_SOURCE_FILES = previous
            self.assertEqual(sorted(texts), ["a.js", "b.js"])
            self.assertTrue(partial)

    def test_tracked_source_enforces_aggregate_source_byte_cap(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("a.js", "b.js"):
                (root / name).write_text("abc\n")
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            previous = PROBE_MODULE.MAX_SOURCE_BYTES
            try:
                PROBE_MODULE.MAX_SOURCE_BYTES = 5
                texts, partial = PROBE_MODULE.tracked_source(root)
            finally:
                PROBE_MODULE.MAX_SOURCE_BYTES = previous
            self.assertEqual(sorted(texts), ["a.js"])
            self.assertTrue(partial)

    def test_tracked_source_stops_a_stalled_git_inventory_at_deadline(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            wrapper_dir = root / "bin"
            wrapper_dir.mkdir()
            git = wrapper_dir / "git"
            git.write_text("#!/usr/bin/env python3\nimport time\ntime.sleep(3)\n")
            git.chmod(0o755)
            previous = PROBE_MODULE.MAX_SOURCE_SCAN_MS
            started = time.monotonic()
            try:
                PROBE_MODULE.MAX_SOURCE_SCAN_MS = 100
                with mock.patch.dict(os.environ, {"PATH": f"{wrapper_dir}:{os.environ['PATH']}"}):
                    texts, partial = PROBE_MODULE.tracked_source(root)
            finally:
                PROBE_MODULE.MAX_SOURCE_SCAN_MS = previous
            elapsed = time.monotonic() - started
            self.assertEqual(texts, {})
            self.assertTrue(partial)
            self.assertLess(elapsed, 1.0)


if __name__ == "__main__":
    unittest.main()
