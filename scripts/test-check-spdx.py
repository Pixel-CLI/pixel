#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Contract of check-spdx.py.

A source file in scope without both SPDX lines fails the Test job, so a new
file cannot silently drop the copyright and license statements OpenSSF gold
requires. `--fix` must keep a shebang first and must not mistake a Rust
inner attribute (`#![...]`) for one, or the file stops working. Files out of
scope (an excluded prefix, a YAML file outside the CI definitions) are never
reported, and this repository passes.
"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-spdx.py")
HEADER_RS = "// SPDX-FileCopyrightText: The Pixel contributors\n// SPDX-License-Identifier: MIT\n"


class CheckSpdx(unittest.TestCase):
    def repo(self, files):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        for path, text in files.items():
            file = root / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(text)
        subprocess.run(["git", "-C", str(root), "add", "-A"], check=True)
        return root

    def run_check(self, root, *flags):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *flags, str(root)],
            capture_output=True,
            text=True,
        )

    def test_a_file_without_the_header_fails_with_its_path(self):
        root = self.repo({"src/a.rs": HEADER_RS + "fn a() {}\n", "src/b.rs": "fn b() {}\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout.splitlines(),
            ["src/b.rs: missing the SPDX header (SPDX-FileCopyrightText: The Pixel contributors / SPDX-License-Identifier: MIT)"],
        )

    def test_one_line_alone_is_not_enough(self):
        root = self.repo({"a.py": "# SPDX-License-Identifier: MIT\nprint(1)\n"})
        self.assertEqual(self.run_check(root).returncode, 1)

    def test_fix_keeps_the_shebang_first_and_then_passes(self):
        root = self.repo({"run.sh": "#!/bin/sh\necho hi\n"})
        self.assertEqual(self.run_check(root, "--fix").returncode, 0)
        self.assertEqual(
            (root / "run.sh").read_text(),
            "#!/bin/sh\n# SPDX-FileCopyrightText: The Pixel contributors\n"
            "# SPDX-License-Identifier: MIT\n\necho hi\n",
        )
        self.assertEqual(self.run_check(root).returncode, 0)

    def test_fix_puts_the_header_above_a_rust_inner_attribute(self):
        root = self.repo({"lib.rs": "#![forbid(unsafe_code)]\nfn a() {}\n"})
        self.run_check(root, "--fix")
        self.assertEqual(
            (root / "lib.rs").read_text(),
            HEADER_RS + "\n#![forbid(unsafe_code)]\nfn a() {}\n",
        )

    def test_out_of_scope_files_are_never_reported(self):
        root = self.repo({
            "crates/pixel-install/assets/pi-pixel.ts": "export {};\n",
            "config.yaml": "a: 1\n",
            "README.md": "# readme\n",
            ".github/workflows/ci.yml": "# SPDX-FileCopyrightText: The Pixel contributors\n# SPDX-License-Identifier: MIT\nname: CI\n",
        })
        self.assertEqual(self.run_check(root).returncode, 0)

    def test_a_workflow_and_a_git_hook_are_in_scope(self):
        root = self.repo({".github/workflows/ci.yml": "name: CI\n", ".githooks/pre-push": "#!/bin/sh\n"})
        result = self.run_check(root)
        self.assertEqual(
            sorted(line.split(":")[0] for line in result.stdout.splitlines()),
            [".githooks/pre-push", ".github/workflows/ci.yml"],
        )

    def test_this_repository_passes(self):
        result = self.run_check(SCRIPT.resolve().parent.parent)
        self.assertEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
