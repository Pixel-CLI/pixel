#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Where the error-origin probe finds a message the question describes without quoting it (issue #1197)."""

import json
import os
import re
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
MANIFEST = Path(__file__).resolve().parents[1] / "pixel-plugin.toml"
GIT_ENV = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
           "GIT_CONFIG_GLOBAL": os.devnull, "PATH": os.environ["PATH"]}


def repo(files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)
    return root


def origin(root, prompt):
    request = {"prompt": prompt, "repo_root": str(root), "brief": "", "kind": None, "session_files": []}
    out = subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True).stdout
    return out, next((line for line in out.splitlines() if line.startswith("### error origin")), None)


def probe_module(root, prompt):
    import contextlib
    import importlib.machinery
    import importlib.util
    import io
    import sys

    loader = importlib.machinery.SourceFileLoader("error_origin_probe_test", str(PROBE))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    stdin, sys.stdin = sys.stdin, io.StringIO(json.dumps({"prompt": prompt, "repo_root": str(root)}))
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            loader.exec_module(module)
    finally:
        sys.stdin = stdin
    return module


# A manual page that shares more of the question's words than the message does.
MANUAL = ("pub fn doc_long() -> &'static str {\n    r\"\nPrint each file that would be found, without searching it.\n"
          "This shows whether a file is being found or not.\n\"\n}\n")
MESSAGE = ("fn eprint_nothing_found() {\n    err_message!(\n        \"No files were found, which means a filter \\\n"
           "         ran.\\n\\\n         Running with --debug shows why files are being skipped.\"\n    );\n}\n\n"
           "pub fn run(found: usize) {\n    if found == 0 {\n        eprint_nothing_found();\n    }\n}\n")


class DescribedMessage(unittest.TestCase):
    def test_the_emitted_message_beats_a_manual_that_shares_more_words(self):
        root = repo({"src/main.rs": MESSAGE, "src/flags.rs": MANUAL})
        out, head = origin(root, "where does the message about no files being found come from and when is it printed")
        self.assertEqual(head, "### error origin — described occurrence candidate; emission unverified at src/main.rs:3 near "
                               "eprint_nothing_found (scope unresolved)")
        self.assertIn("### src/main.rs:9-13 — run → eprint_nothing_found: lexical call candidate; receiver/binding unresolved", out)

    def test_a_python_message_in_triple_quotes_is_found_on_its_own_line(self):
        source = ('_no_scope_msg = """\\\nWorking outside of the request scope.\n\nOpen one with scope().\n"""\n\n\n'
                  "def current():\n    raise RuntimeError(_no_scope_msg)\n")
        root = repo({"pkg/globals.py": source, "pkg/other.py": "def scope():\n    return 'a request scope'\n"})
        _out, head = origin(root, "where does the runtime error about working outside of the request scope come from")
        self.assertEqual(head, "### error origin — message definition; emission unverified at pkg/globals.py:2")

    def test_a_catalogue_entry_is_followed_to_the_code_that_names_its_key(self):
        root = repo({
            # The key says nothing the question says: only the catalogue's text leads to it.
            "locales/en.json": '{\n  "alerts": {\n    "invalidDrop": "Couldn\'t load the invalid file"\n  }\n}\n',
            "src/App.tsx": "function onDrop() {\n  setAlert(t(\"alerts.invalidDrop\"));\n}\n",
        })
        out, head = origin(root, "where is the alert text for a file that could not be loaded defined")
        self.assertEqual(head, "### error origin — described occurrence candidate; emission unverified at src/App.tsx:2 near "
                               "onDrop (scope unresolved)")
        self.assertIn("### locales/en.json:3-3 — message definition in catalogue; emission unverified", out)

    def test_a_string_longer_than_the_window_is_a_manual_and_never_the_origin(self):
        manual = "pub fn doc() -> &'static str {\n    r\"\n" + "No files being found is printed here.\n" * 20 + "\"\n}\n"
        root = repo({"src/doc.rs": manual})
        self.assertEqual(origin(root, "where does the message about no files being found come from")[0], "")

    def test_a_message_inside_an_inline_test_module_is_not_the_origin(self):
        source = ("pub fn ok() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n"
                  "        assert_eq!(f(), \"No files were found by the walker\");\n    }\n}\n")
        root = repo({"src/walk.rs": source})
        self.assertEqual(origin(root, "where does the message about no files being found by the walker come from")[0],
                         "")


class EvidenceQualification(unittest.TestCase):
    def test_documentation_and_definitions_do_not_claim_an_emission(self):
        for body, category in [
            ('def explain():\n    """The amber river is frozen."""\n', 'documentation occurrence; no emission proved'),
            ('NOTICE = "The amber river is frozen"\n', 'message definition; emission unverified'),
        ]:
            with self.subTest(category=category):
                out, _ = origin(repo({'engine.py': body}), 'where is "The amber river is frozen" raised')
                self.assertIn(category, out)
                self.assertNotIn('is raised', out)
                self.assertNotIn('emits it', out)

    def test_local_emission_keeps_the_last_source_line(self):
        out, _ = origin(repo({'engine.py': 'def fail():\n    raise ValueError("The amber river is frozen")\n'}),
                        'where is "The amber river is frozen" raised')
        self.assertIn('local raise syntax', out)
        self.assertIn('2|     raise ValueError("The amber river is frozen")', out,
                      'the evidence must include the actual raise, even when it is the final source line')

    def test_conversions_and_rethrows_keep_the_branch_and_dependency_boundary(self):
        for statement in ['raise GatewayError(exc)', 'raise']:
            with self.subTest(statement=statement):
                source = ('from transport.errors import LinkDisconnected\n\ndef perform():\n'
                          '    try:\n        connect()\n    except LinkDisconnected as exc:\n'
                          '        if retry_allowed():\n            recover()\n        ' + statement + '\n')
                out, _ = origin(repo({'engine.py': source}), 'where does the link disconnected error come from')
                self.assertIn('conversion/rethrow candidate', out)
                self.assertIn('if retry_allowed():', out)
                self.assertIn(f'9|         {statement}', out, 'quote the source raise, not only a role label')
                self.assertIn('dependency boundary', out)
                self.assertIn('external emission not inspected', out)
                self.assertIn('message identity unverified', out)

    def test_a_homonymous_receiver_is_never_a_proven_call_edge(self):
        source = ('def send():\n    raise ValueError("The amber river is frozen")\n\n'
                  'class Reporter:\n    def send(self):\n        return "log only"\n\n'
                  'def unrelated(reporter):\n    reporter.send()\n\ndef execute():\n    send()\n')
        out, _ = origin(repo({'engine.py': source}), 'where is "The amber river is frozen" raised')
        self.assertIn('local raise syntax', out)
        edges = [line for line in out.splitlines() if ' → send:' in line]
        self.assertTrue(edges, 'keep a bounded level of call context, rather than hiding all callers')
        self.assertTrue(all('receiver/binding unresolved' in edge for edge in edges), edges)
        self.assertNotIn('the lines leading to the call', out)


class DiagnosticEmission(unittest.TestCase):
    PROMPT = "where is the error printed when a search path does not exist produced"
    SOURCE = ("fn paths() {\n"
              "    if filesystem::is_existing_directory(path) {\n"
              "        normalize(path);\n"
              "    } else {\n"
              "        print_error!(\n"
              "            \"Search path {:?} is not a directory.\",\n"
              "            path\n"
              "        );\n"
              "    }\n"
              "}\n"
              "fn path_separator(search_path: &str) {\n"
              "    println!(\"Search path separator is configured here: {}\", search_path);\n"
              "}\n"
              "const HELP: &str = \"Set path separator when printing file paths\";\n")

    def test_emitter_with_matching_condition_beats_same_word_option_help(self):
        root = repo({"src/cli.rs": self.SOURCE})
        out, _ = origin(root, self.PROMPT)
        self.assertIn("### src/cli.rs:1-11 — error-emission candidate", out)
        self.assertIn("is_existing_directory(path)", out)
        self.assertIn("Search path {:?} is not a directory", out)
        module = probe_module(root, self.PROMPT)
        self.assertEqual(module.origins(str(root), self.PROMPT)[1], 6,
                         "the original lexical search returns a same-file candidate")
        self.assertNotIn("error origin — described occurrence candidate", out,
                         "the diagnostic emitter supersedes a weaker same-file lexical neighbor")

    def test_different_literal_error_uses_the_same_emitter_rule(self):
        root = repo({"src/open.rs": (
            "fn open(path: &Path) {\n"
            "    if !path.exists() {\n"
            "        eprintln!(\"Source path was not found: {}\", path.display());\n"
            "    }\n"
            "}\n")})
        prompt = "where does the error for a missing source path come from and when does it print"
        out, _ = origin(root, prompt)
        self.assertIn("### src/open.rs:1-5 — error-emission candidate", out)
        self.assertIn("Source path was not found", out)

    def test_second_emitter_in_same_file_is_not_hidden_by_first(self):
        root = repo({"src/open.rs": (
            "fn warning() { eprintln!(\"Search path warning\"); }\n"
            "fn open(path: &Path) {\n"
            "    if !path.exists() {\n"
            "        eprintln!(\"Missing source path was not found: {}\", path.display());\n"
            "    }\n"
            "}\n")})
        prompt = "where is the error emitted for a missing source path that was not found"
        out, _ = origin(root, prompt)
        self.assertIn("### src/open.rs:2-6 — error-emission candidate", out)
        self.assertIn("if !path.exists()", out)
        self.assertNotIn("Search path warning", out)

    def test_exact_quoted_origin_survives_a_same_file_diagnostic_candidate(self):
        root = repo({"src/open.rs": (
            "fn unrelated() { eprintln!(\"Missing source path failed to load\"); }\n"
            "fn open(path: &Path) {\n"
            "    if !path.exists() {\n"
            "        eprintln!(\"The source path is missing\");\n"
            "    }\n"
            "}\n")})
        prompt = 'where is "The source path is missing" emitted'
        out, _ = origin(root, prompt)
        self.assertIn("error origin — quoted occurrence candidate", out)
        self.assertIn("error-emission candidate", out)

    def test_same_words_in_non_emitting_help_are_not_an_emission(self):
        root = repo({"src/help.rs": 'const HELP: &str = "Search path does not exist in the help output";\n'})
        module = probe_module(root, self.PROMPT)
        self.assertIsNone(module.error_emission(str(root), self.PROMPT))

    def test_emitter_requires_two_salient_terms(self):
        root = repo({"src/cli.rs": self.SOURCE})
        module = probe_module(root, "where is the error printed")
        self.assertIsNone(module.error_emission(str(root), "where is the error printed"))

    def test_untracked_and_tracked_symlink_emitters_are_not_read(self):
        root = repo({"src/main.rs": "fn main() {}\n"})
        untracked = root / "src/untracked.rs"
        untracked.write_text(self.SOURCE)
        external = Path(tempfile.mkdtemp()) / "outside.rs"
        external.write_text(self.SOURCE)
        link = root / "src/external.rs"
        link.symlink_to(external)
        subprocess.run(["git", "add", "src/external.rs"], cwd=root, env=GIT_ENV, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-q", "-m", "symlink"], cwd=root, env=GIT_ENV, check=True, capture_output=True)
        module = probe_module(root, self.PROMPT)
        self.assertIsNone(module.error_emission(str(root), self.PROMPT))

    def test_unparseable_python_is_only_a_candidate(self):
        source = 'def broken(:\n    raise ValueError("The amber river is frozen")\n'
        out, _ = origin(repo({'engine.py': source}), 'where is "The amber river is frozen" raised')
        self.assertIn('occurrence candidate; emission unverified', out)
        self.assertNotIn('local raise syntax', out)


class ReviewRegressions(unittest.TestCase):
    def test_a_global_message_is_not_inside_the_previous_function(self):
        source = 'def unrelated():\n    return None\n\nNOTICE = "The amber river is frozen"\n'
        out, head = origin(repo({'engine.py': source}), 'where is "The amber river is frozen" raised')
        self.assertIn('message definition; emission unverified', head)
        self.assertNotIn('in unrelated', head)
        self.assertNotIn(' → unrelated:', out)

    def test_nested_scopes_are_not_attributed_as_conversions_of_the_handler(self):
        for nested in ['def later():', 'async def later():', 'class Later:']:
            with self.subTest(nested=nested):
                source = ('from transport.errors import LinkDisconnected\n\ndef perform():\n'
                          '    try:\n        connect()\n    except LinkDisconnected as exc:\n'
                          '        ' + nested + '\n            raise GatewayError(exc)\n        return None\n')
                out, _ = origin(repo({'engine.py': source}), 'where does the link disconnected error come from')
                self.assertNotIn('conversion/rethrow candidate', out)
                self.assertNotIn('local raise/rethrow syntax in this handler', out)

    def test_import_name_shadowing_does_not_prove_a_dependency_binding(self):
        source = ('from transport.errors import LinkDisconnected\n\ndef perform():\n'
                  '    class LinkDisconnected(Exception):\n        pass\n    try:\n        connect()\n'
                  '    except LinkDisconnected as exc:\n        raise GatewayError(exc)\n')
        out, _ = origin(repo({'engine.py': source}), 'where does the link disconnected error come from')
        self.assertIn('dependency boundary candidate: matching import name; binding unresolved', out)
        self.assertNotIn('dependency boundary: imported caught type', out)

    def test_second_level_context_keeps_conditions_without_composing_a_call_path(self):
        source = ('def emit():\n    raise ValueError("The amber river is frozen")\n\n'
                  'def first():\n    emit()\n\ndef entry(enabled):\n    if enabled:\n        first()\n')
        out, _ = origin(repo({'engine.py': source}), 'where is "The amber river is frozen" raised')
        self.assertIn('if enabled:', out)
        self.assertIn('independent context, no composed path', out)
        self.assertTrue(all('binding unresolved' in l for l in out.splitlines() if ' → ' in l))

    def test_direct_sites_are_not_hidden_by_an_indirect_homonym_in_the_same_function(self):
        source = ('def emit():\n    raise ValueError("The amber river is frozen")\n\n'
                  'def first():\n    emit()\n\ndef parallel(obj, ready):\n    obj.first()\n'
                  '    if not ready:\n        emit()\n')
        out, _ = origin(repo({'engine.py': source}), 'where is "The amber river is frozen" raised')
        self.assertIn('parallel → emit:', out)
        self.assertIn('if not ready:', out)
        self.assertIn('parallel → first:', out)
        self.assertTrue(all('binding unresolved' in l for l in out.splitlines() if ' → ' in l))


class StringPart(unittest.TestCase):
    def test_an_unclosed_string_of_escapes_is_read_in_linear_time(self):
        import importlib.machinery
        import importlib.util
        import io
        import sys
        import time
        loader = importlib.machinery.SourceFileLoader("error_origin_probe", str(PROBE))
        module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        stdin, sys.stdin = sys.stdin, io.StringIO(json.dumps({"prompt": "x", "repo_root": tempfile.mkdtemp()}))
        try:
            loader.exec_module(module)   # the probe runs main() on import: on an empty repo it prints nothing
        finally:
            sys.stdin = stdin
        started = time.monotonic()
        self.assertEqual(module.string_part('x = "' + "\\a" * 40), "")
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertEqual(module.string_part('err("No files \\"were\\" found", x)'), 'No files \\"were\\" found')


class DefaultDependencyBoundary(unittest.TestCase):
    PROMPT = "Where is the default 404 page produced?"

    def test_traces_an_imported_default_handler_through_request_completion(self):
        root = repo({
            "package.json": '{"dependencies":{"@demo/response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('@demo/response-writer');\n"
                           "function serve(req, res, callback) {\n"
                           "  const done = callback || renderEnd(req, res);\n"
                           "  router.handle(req, res, done);\n"
                           "}\n"),
        })
        out, _ = origin(root, self.PROMPT)
        self.assertIn("default handler callback candidate", out)
        self.assertIn("@demo/response-writer", out)
        self.assertIn("callee behavior and external response text are not inspected", out)
        self.assertIn("passed as the final argument to router.handle", out)
        self.assertIn("call behavior unverified", out)
        self.assertIn("### lib/app.js:1-1 — imported dependency binding", out)
        self.assertIn("### lib/app.js:2-3 — optional callback fallback", out)
        self.assertIn("### package.json:1-1 — declared non-development dependency", out)

    def test_a_404_example_without_a_default_fallback_is_not_a_boundary(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": "function serve(req, res) { router.handle(req, res); }\n",
            "examples/not-found.js": "res.status(404).send('Cannot GET ' + req.url);\n",
        })
        out, _ = origin(root, self.PROMPT)
        self.assertNotIn("default handler callback candidate", out)

    def test_a_default_boundary_does_not_promote_a_custom_404_example_as_its_origin(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('response-writer');\n"
                           "function serve(req, res, callback) { const done = callback || renderEnd(req,res);\n"
                           "  router.handle(req,res,done); }\n"),
            "examples/not-found.js": "res.status(404).send('Cannot GET ' + req.url);\n",
        })
        out, _ = origin(root, "where is the default 404 page that says Cannot GET produced")
        self.assertIn("default handler callback candidate", out)
        self.assertNotIn("examples/not-found.js", out)

    def test_an_undeclared_import_is_not_a_dependency_boundary(self):
        root = repo({
            "package.json": '{"dependencies":{"some-other-package":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('unlisted-renderer');\n"
                           "function serve(req, res, callback) { const done = callback || renderEnd(req, res);\n"
                           "  router.handle(req, res, done); }\n"),
        })
        out, _ = origin(root, self.PROMPT)
        self.assertNotIn("default handler callback candidate", out)

    def test_a_shadowed_import_binding_is_not_resolved_to_the_package(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('response-writer');\n"
                           "function serve(req, res, callback, renderEnd) {\n"
                           "  const done = callback || renderEnd(req, res);\n"
                           "  router.handle(req, res, done);\n}\n"),
        })
        out, _ = origin(root, self.PROMPT)
        self.assertNotIn("default handler callback candidate", out)

    def test_a_fallback_must_be_passed_to_request_completion_in_the_same_function(self):
        cases = [
            ("function configure(options, callback) { const done = callback || renderEnd(options); builder.handle(options, done); }\n",
             "unrelated callback"),
            ("function choose(req, res, callback) { const done = callback || renderEnd(req, res); }\n"
             "function route(req, res) { router.handle(req, res, done); }\n", "separate function"),
            ("function serve(req, res, callback) { const done = renderEnd(req, res); router.handle(req, res, done); }\n",
             "no optional callback fallback"),
        ]
        for source, label in cases:
            with self.subTest(label=label):
                root = repo({
                    "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
                    "lib/app.js": "const renderEnd = require('response-writer');\n" + source,
                })
                out, _ = origin(root, self.PROMPT)
                self.assertNotIn("default handler callback candidate", out)

    def test_an_unrelated_telemetry_call_does_not_establish_request_completion(self):
        for telemetry in ("metrics.record", "router_metrics.finish"):
            with self.subTest(telemetry=telemetry):
                root = repo({
                    "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
                    "lib/app.js": ("const renderEnd = require('response-writer');\n"
                                   "function serve(req, res, callback) {\n"
                                   "  const done = callback || renderEnd(req, res);\n"
                                   f"  {telemetry}(req, res, done);\n"
                                   "}\n"),
                })
                out, _ = origin(root, self.PROMPT)
                self.assertNotIn("default handler callback candidate", out)

    def test_a_generic_dispatcher_operation_is_not_framework_specific(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('response-writer');\n"
                           "function serve(req, res, callback) {\n"
                           "  const done = callback || renderEnd(req, res);\n"
                           "  request_dispatcher.dispatch(req, res, done);\n"
                           "}\n"),
        })
        out, _ = origin(root, self.PROMPT)
        self.assertIn("default handler callback candidate", out)
        self.assertIn("request_dispatcher.dispatch(req, res, done)", out)

    def test_comments_and_strings_cannot_supply_import_or_call_evidence(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("// const renderEnd = require('response-writer');\n"
                           "const HELP = `function serve(req, res, callback) { const done = callback || renderEnd(); router.handle(req,res,done); }`;\n"),
        })
        out, _ = origin(root, self.PROMPT)
        self.assertNotIn("default handler callback candidate", out)

    def test_unrelated_prompt_is_not_admitted_and_requests_wrapper_is_not_a_new_gain(self):
        root = repo({
            "package.json": '{"dependencies":{"response-writer":"^1.0.0"}}\n',
            "lib/app.js": ("const renderEnd = require('response-writer');\n"
                           "function serve(req, res, callback) { const done = callback || renderEnd(req,res);\n"
                           "  router.handle(req,res,done); }\n"),
        })
        out, _ = origin(root, "Where is the timeout response produced?")
        self.assertNotIn("default handler callback candidate", out)
        requests = repo({
            "src/requests/adapters.py": ("from urllib3.exceptions import MaxRetryError\n"
                                         "def send():\n    try:\n        go()\n"
                                         "    except MaxRetryError as exc:\n"
                                         "        raise ConnectionError(exc)\n"),
        })
        out, _ = origin(requests, "where does the text of the max retries exceeded connection error come from")
        self.assertNotIn("default handler callback candidate", out)


class ManifestAdmission(unittest.TestCase):
    def test_default_not_found_source_questions_admit_without_generic_404_admission(self):
        manifest = tomllib.loads(MANIFEST.read_text())
        gate = re.compile(manifest["brief-probe"]["when_prompt"])
        positives = [
            "where is the default 404 page that says Cannot GET produced",
            "Where is the standard not-found response generated?",
            "Where is the 404 response message generated?",
        ]
        negatives = [
            "What does HTTP status 404 mean?",
            "Where does an unrelated parser produce status code 404?",
            "continue",
            "What is the weather?",
        ]
        for prompt in positives:
            with self.subTest(prompt=prompt):
                self.assertIsNotNone(gate.search(prompt))
        for prompt in negatives:
            with self.subTest(prompt=prompt):
                self.assertIsNone(gate.search(prompt))


if __name__ == "__main__":
    unittest.main()
