# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT

"""Keep the shipped dictionary reproducible from its maintained lemma data."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class LexiconGeneration(unittest.TestCase):
    def test_generator_reproduces_the_shipped_lexicon_without_the_mvp_checkout(self):
        source = Path(__file__).resolve().parent
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            generator = root / "generate.py"
            generator.write_bytes((source / "generate.py").read_bytes())
            output = root / "fr-en.tsv"
            run = subprocess.run(
                [sys.executable, "-I", str(generator), str(output)],
                cwd=root, capture_output=True, text=True, check=False,
            )
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertEqual(output.read_bytes(), (source / "fr-en.tsv").read_bytes())


if __name__ == "__main__":
    unittest.main()
