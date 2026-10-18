#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Where the error-origin probe finds a message the question describes without quoting it (issue #1197)."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
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
        self.assertEqual(head, "### error origin — the message the question describes is raised at src/main.rs:3 in "
                               "eprint_nothing_found")
        self.assertIn("### src/main.rs:9-13 — run → eprint_nothing_found: the lines leading to the call", out)

    def test_a_python_message_in_triple_quotes_is_found_on_its_own_line(self):
        source = ('_no_scope_msg = """\\\nWorking outside of the request scope.\n\nOpen one with scope().\n"""\n\n\n'
                  "def current():\n    raise RuntimeError(_no_scope_msg)\n")
        root = repo({"pkg/globals.py": source, "pkg/other.py": "def scope():\n    return 'a request scope'\n"})
        _out, head = origin(root, "where does the runtime error about working outside of the request scope come from")
        self.assertEqual(head, "### error origin — the message the question describes is raised at pkg/globals.py:2")

    def test_a_catalogue_entry_is_followed_to_the_code_that_names_its_key(self):
        root = repo({
            # The key says nothing the question says: only the catalogue's text leads to it.
            "locales/en.json": '{\n  "alerts": {\n    "invalidDrop": "Couldn\'t load the invalid file"\n  }\n}\n',
            "src/App.tsx": "function onDrop() {\n  setAlert(t(\"alerts.invalidDrop\"));\n}\n",
        })
        out, head = origin(root, "where is the alert text for a file that could not be loaded defined")
        self.assertEqual(head, "### error origin — the message the question describes is raised at src/App.tsx:2 in "
                               "onDrop")
        self.assertIn("### locales/en.json:3-3 — the text, in its message catalogue", out)

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


if __name__ == "__main__":
    unittest.main()
