#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/homebrew-formula.py, the step release.yml runs to write
the Homebrew formula and the Linux bottles, checked without a tag.

What must hold for a release to install through the tap:

- the formula is exactly `fixtures/homebrew/pixel.rb.template` with the
  digests filled in: the macOS and install parts are the formula every release
  shipped before bottles, so a Mac installs as before;
- each Linux bottle is the keg Homebrew pours (`pixel/<version>/bin/pixel`,
  the binary byte for byte, executable), under the name Homebrew requests from
  a plain root_url, and the formula names its real digest: a wrong one makes
  `brew install` refuse the download;
- the same archives give the same bottles, so a re-run of the release job
  cannot publish a digest the tap does not carry;
- an archive that disagrees with its `.sha256`, or lacks the binary, stops the
  release job before anything is written.
"""

import gzip
import hashlib
import io
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "homebrew-formula.py"
TEMPLATE = HERE / "fixtures/homebrew/pixel.rb.template"
TAG = "v9.8.7"
TARGETS = ("aarch64-apple-darwin", "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl")
BOTTLES = {"aarch64-unknown-linux-musl": "arm64_linux", "x86_64-unknown-linux-musl": "x86_64_linux"}


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class HomebrewFormulaContract(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="pixel-homebrew-contract-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        self.binaries = {}
        for target in TARGETS:
            self.binaries[target] = f"#!/bin/sh\necho pixel 9.8.7 {target}\n".encode()
            self.write_archive(target, self.binaries[target])

    def write_archive(self, target, binary, with_binary=True):
        """An archive shaped like the build job's: pixel-<tag>-<target>/ with
        bin/pixel, README.md and LICENSE, and its `<sha>  <name>` file."""
        top = f"pixel-{TAG}-{target}"
        out = io.BytesIO()
        with gzip.GzipFile(filename="", mode="wb", fileobj=out, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w") as tar:
                files = [("README.md", b"# pixel\n", 0o644), ("LICENSE", b"MIT\n", 0o644)]
                if with_binary:
                    files.append(("bin/pixel", binary, 0o755))
                for rel, data, mode in files:
                    info = tarfile.TarInfo(f"{top}/{rel}")
                    info.size, info.mode, info.mtime = len(data), mode, 1_700_000_000
                    tar.addfile(info, io.BytesIO(data))
        name = f"{top}.tar.gz"
        (self.artifacts / name).write_bytes(out.getvalue())
        (self.artifacts / f"{name}.sha256").write_text(f"{sha256(out.getvalue())}  {name}\n")

    def run_script(self, out, *, env=None, args=None):
        environ = {k: v for k, v in os.environ.items() if k != "PIXEL_RELEASE_URL"}
        environ.update(env or {})
        return subprocess.run(
            [sys.executable, str(SCRIPT), *(args or [TAG, str(self.artifacts), str(out)])],
            capture_output=True, text=True, env=environ, check=False,
        )

    def generate(self, name="out", env=None):
        out = self.root / name
        result = self.run_script(out, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return out

    def archive_sha(self, target):
        return (self.artifacts / f"pixel-{TAG}-{target}.tar.gz.sha256").read_text().split()[0]

    def expected_formula(self, out, base="https://github.com/Pixel-CLI/pixel/releases/download"):
        text = TEMPLATE.read_text()
        values = {
            "{MAC}": self.archive_sha("aarch64-apple-darwin"),
            "{ARM}": self.archive_sha("aarch64-unknown-linux-musl"),
            "{INTEL}": self.archive_sha("x86_64-unknown-linux-musl"),
            "{BOTTLE_ARM}": sha256((out / "pixel-9.8.7.arm64_linux.bottle.tar.gz").read_bytes()),
            "{BOTTLE_X86}": sha256((out / "pixel-9.8.7.x86_64_linux.bottle.tar.gz").read_bytes()),
        }
        for placeholder, value in values.items():
            text = text.replace(placeholder, value)
        return text.replace("https://github.com/Pixel-CLI/pixel/releases/download", base)

    def test_the_formula_is_the_template_with_the_real_digests(self):
        out = self.generate()
        self.assertEqual((out / "pixel.rb").read_text(), self.expected_formula(out))
        self.assertEqual(
            sorted(p.name for p in out.iterdir()),
            ["pixel-9.8.7.arm64_linux.bottle.tar.gz", "pixel-9.8.7.x86_64_linux.bottle.tar.gz",
             "pixel.rb"],
        )

    def test_each_linux_bottle_is_the_keg_homebrew_pours(self):
        out = self.generate()
        for target, tag in BOTTLES.items():
            with tarfile.open(out / f"pixel-9.8.7.{tag}.bottle.tar.gz") as bottle:
                members = {m.name: m for m in bottle.getmembers()}
                self.assertEqual(
                    sorted(members),
                    ["pixel/9.8.7", "pixel/9.8.7/LICENSE", "pixel/9.8.7/README.md",
                     "pixel/9.8.7/bin", "pixel/9.8.7/bin/pixel"],
                )
                binary = members["pixel/9.8.7/bin/pixel"]
                self.assertEqual(binary.mode, 0o755, "Homebrew links bin/pixel as it finds it")
                self.assertEqual(bottle.extractfile(binary).read(), self.binaries[target])
                self.assertEqual(members["pixel/9.8.7/LICENSE"].mode, 0o644)
                self.assertTrue(members["pixel/9.8.7/bin"].isdir())

    def test_the_same_archives_give_the_same_bottles(self):
        first, second = self.generate("one"), self.generate("two")
        for name in ("pixel-9.8.7.arm64_linux.bottle.tar.gz",
                     "pixel-9.8.7.x86_64_linux.bottle.tar.gz", "pixel.rb"):
            self.assertEqual((first / name).read_bytes(), (second / name).read_bytes(), name)

    def test_the_release_url_can_point_at_a_local_server(self):
        out = self.generate(env={"PIXEL_RELEASE_URL": "http://127.0.0.1:8765/"})
        formula = (out / "pixel.rb").read_text()
        self.assertEqual(formula, self.expected_formula(out, base="http://127.0.0.1:8765"))
        self.assertIn('    root_url "http://127.0.0.1:8765/v9.8.7"\n', formula)

    def test_an_archive_that_disagrees_with_its_digest_stops_before_writing(self):
        name = f"pixel-{TAG}-x86_64-unknown-linux-musl.tar.gz"
        (self.artifacts / f"{name}.sha256").write_text(f"{'0' * 64}  {name}\n")
        out = self.root / "out"
        result = self.run_script(out)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"{name}: its .sha256 does not match the archive", result.stderr)
        self.assertFalse(out.exists(), "nothing may be written for a release whose bytes differ")

    def test_an_archive_without_the_binary_stops_the_job(self):
        self.write_archive("aarch64-unknown-linux-musl", b"", with_binary=False)
        result = self.run_script(self.root / "out")
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"no pixel-{TAG}-aarch64-unknown-linux-musl/bin/pixel", result.stderr)
        self.assertNotIn("Traceback", result.stderr, "the job log must say what is missing")
        self.assertFalse((self.root / "out").exists(), "no partial set of bottles may be left")

    def test_a_tag_without_its_v_is_refused(self):
        result = self.run_script(self.root / "out", args=["9.8.7", str(self.artifacts), "out"])
        self.assertEqual(result.returncode, 2)
        self.assertIn("homebrew-formula.py <tag> <artifacts-dir> <out-dir>", result.stderr)

    @unittest.skipUnless(shutil.which("ruby"), "ruby not installed")
    def test_the_formula_is_valid_ruby(self):
        out = self.generate()
        check = subprocess.run(["ruby", "-c", str(out / "pixel.rb")], capture_output=True, text=True)
        self.assertEqual(check.returncode, 0, check.stderr)


if __name__ == "__main__":
    unittest.main()
