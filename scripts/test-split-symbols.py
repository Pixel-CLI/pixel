#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/release/split-symbols.sh, which turns the one
unstripped release build into the stripped binary users download and the
symbols archive a backtrace needs.

What matters: the archive holds the binary exactly as built (a symbolizer
needs those bytes, not a re-link), the shipped binary loses its symbol table
and still runs, and a run that would ship an unstripped binary (a strip that
changed nothing) fails instead of passing silently. A tiny C program built
with the host's `cc` stands in for pixel; the host's own strip tool is used,
so no llvm-tools install is needed.
"""
from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "release" / "split-symbols.sh"
HOST_STRIP = "/usr/bin/strip" if os.uname().sysname == "Darwin" else shutil.which("strip")

SOURCE = """
#include <stdio.h>
static int helper_with_a_local_symbol(int x) { return x * 3; }
int main(void) { printf("ran %d\\n", helper_with_a_local_symbol(14)); return 0; }
"""


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def symbols(path: Path) -> str:
    return subprocess.run(["nm", str(path)], capture_output=True, text=True).stdout


class SplitSymbols(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        src = self.dir / "main.c"
        src.write_text(SOURCE)
        self.bin = self.dir / "pixel"
        subprocess.run(["cc", "-O0", "-o", str(self.bin), str(src)], check=True)
        self.archive = self.dir / "pixel.symbols.tar.xz"

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def split(self, *args: str, strip: str | None = HOST_STRIP) -> subprocess.CompletedProcess[str]:
        env = dict(os.environ)
        if strip is not None:
            env["PIXEL_STRIP"] = strip
        return subprocess.run(
            ["sh", str(SCRIPT), *args], capture_output=True, text=True, env=env
        )

    def test_archive_should_hold_the_binary_as_built_and_the_shipped_one_should_lose_its_symbols(self) -> None:
        built = sha(self.bin)
        self.assertIn("helper_with_a_local_symbol", symbols(self.bin))
        out = self.split(str(self.bin), str(self.archive))
        self.assertEqual(out.returncode, 0, out.stderr)

        extracted = self.dir / "extracted"
        extracted.mkdir()
        subprocess.run(["tar", "-C", str(extracted), "-xJf", str(self.archive)], check=True)
        self.assertEqual(sorted(p.name for p in extracted.iterdir()), ["pixel"])
        self.assertEqual(sha(extracted / "pixel"), built)
        self.assertIn("helper_with_a_local_symbol", symbols(extracted / "pixel"))

        self.assertNotIn("helper_with_a_local_symbol", symbols(self.bin))
        self.assertLess(self.bin.stat().st_size, (extracted / "pixel").stat().st_size)
        ran = subprocess.run([str(self.bin)], capture_output=True, text=True)
        self.assertEqual(ran.stdout, "ran 42\n")
        self.assertIn(f"symbols in {self.archive}", out.stdout)

    def test_a_strip_that_changes_nothing_should_fail(self) -> None:
        out = self.split(str(self.bin), str(self.archive), strip="true")
        self.assertEqual(out.returncode, 1)
        self.assertIn("did not shrink", out.stderr)

    def test_a_missing_binary_or_argument_should_be_refused(self) -> None:
        self.assertEqual(self.split(str(self.dir / "absent"), str(self.archive)).returncode, 2)
        self.assertEqual(self.split(str(self.bin)).returncode, 2)
        self.assertFalse(self.archive.exists())


if __name__ == "__main__":
    unittest.main()
