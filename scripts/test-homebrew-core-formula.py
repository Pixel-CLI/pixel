#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/homebrew-core-formula.py: the formula a homebrew-core
pull request carries, which must build pixel from source.

Each rule below is one homebrew-core would reject the formula over:

- the stable source is the tag's archive with its real sha256, never a branch;
- nothing prebuilt is installed and no bottle is declared (homebrew-core
  builds its own), and the build uses only the pure-Rust `model2vec` feature:
  the default `fastembed` downloads an ONNX Runtime binary while building;
- Rust is a build dependency only;
- `brew test` exercises pixel for real, offline, and leaves no daemon;
- no caveat or post-install step writes into the user's home.
- the binary is built without its release check (`PIXEL_UPDATE_CHECK=off`):
  Homebrew owns updates and refuses software that upgrades itself.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "homebrew-core-formula.py"
TEMPLATE = HERE / "fixtures/homebrew/pixel-core.rb.template"
TAG_URL = "https://github.com/Pixel-CLI/pixel/archive/refs/tags/v9.8.7.tar.gz"


class HomebrewCoreFormulaContract(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="pixel-homebrew-core-contract-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.tarball = self.root / "pixel-9.8.7.tar.gz"
        self.tarball.write_bytes(b"source archive bytes")
        self.sha = hashlib.sha256(b"source archive bytes").hexdigest()

    def run_script(self, *args, env=None):
        environ = {k: v for k, v in os.environ.items() if k != "PIXEL_SOURCE_URL"}
        environ.update(env or {})
        return subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True,
                              text=True, env=environ, check=False)

    def generate(self, env=None):
        out = self.root / "Formula/pixel.rb"
        result = self.run_script("v9.8.7", str(self.tarball), str(out), env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return out.read_text()

    def expected(self, url=TAG_URL):
        return TEMPLATE.read_text().replace("{URL}", url).replace("{SHA}", self.sha)

    def test_the_formula_is_the_template_with_the_tag_archive_and_its_digest(self):
        self.assertEqual(self.generate(), self.expected())

    def test_the_source_url_can_point_at_a_local_archive(self):
        formula = self.generate(env={"PIXEL_SOURCE_URL": "file:///srv/pixel-9.8.7.tar.gz"})
        self.assertEqual(formula, self.expected(url="file:///srv/pixel-9.8.7.tar.gz"))

    def test_the_formula_keeps_to_what_homebrew_core_accepts(self):
        formula = self.generate()
        self.assertIn(f'  url "{TAG_URL}"\n  sha256 "{self.sha}"\n', formula)
        self.assertIn('  depends_on "rust" => :build\n', formula)
        self.assertEqual(formula.count("depends_on"), 1, "no runtime dependency")
        self.assertIn('"--no-default-features", "--features", "model2vec"', formula)
        self.assertIn('*std_cargo_args(path: "crates/pixel")', formula)
        for refused in ("bin.install", "bottle do", "def caveats", "fastembed\"", "post_install"):
            self.assertNotIn(refused, formula)
        self.assertIn('ENV["PIXEL_DAEMON_AUTO_START"] = "0"', formula)
        # homebrew-core refuses software that updates itself.
        self.assertIn('  def install\n    # Homebrew owns updates: no release check, notice or upgrade offer.\n    ENV["PIXEL_UPDATE_CHECK"] = "off"\n', formula)
        self.assertIn('shell_output("#{bin}/pixel list-signatures m.py")', formula)

    def test_a_missing_archive_is_an_error_and_writes_nothing(self):
        out = self.root / "Formula/pixel.rb"
        result = self.run_script("v9.8.7", str(self.root / "absent.tar.gz"), str(out))
        self.assertEqual(result.returncode, 1)
        self.assertIn("absent.tar.gz: no such source archive", result.stderr)
        self.assertFalse(out.exists())

    def test_a_tag_without_its_v_is_refused(self):
        result = self.run_script("9.8.7", str(self.tarball), str(self.root / "pixel.rb"))
        self.assertEqual(result.returncode, 2)
        self.assertIn("homebrew-core-formula.py <tag> <source-tarball> <out-file>", result.stderr)

    @unittest.skipUnless(shutil.which("ruby"), "ruby not installed")
    def test_the_formula_is_valid_ruby(self):
        self.generate()
        check = subprocess.run(["ruby", "-c", str(self.root / "Formula/pixel.rb")],
                               capture_output=True, text=True)
        self.assertEqual(check.returncode, 0, check.stderr)


if __name__ == "__main__":
    unittest.main()
