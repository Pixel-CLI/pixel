#!/usr/bin/env python3
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT
"""The preserved Pixel site keeps its attribution without relaxing other files."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "check_spdx", Path(__file__).parent / "ci" / "check-spdx.py"
)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)
PIXEL = "# SPDX-FileCopyrightText: The Pixel contributors\n"
RTO = "# SPDX-FileCopyrightText: The RTO contributors\n"
LICENSE = "# SPDX-License-Identifier: MIT\n"
RUST_PIXEL = "// SPDX-FileCopyrightText: The Pixel contributors\n"
RUST_RTO = "// SPDX-FileCopyrightText: The RTO contributors\n"
RUST_LICENSE = "// SPDX-License-Identifier: MIT\n"


class PreservedAttribution(unittest.TestCase):
    def check_file(self, path, body, historical=()):
        with tempfile.TemporaryDirectory() as tmp:
            file = Path(tmp) / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(body)
            with patch.object(CHECK, "tracked", return_value=[path]), patch.object(
                CHECK, "historical_rto_paths", return_value=set(historical)
            ):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    return CHECK.main([tmp])

    def test_preserved_site_files_accept_their_pixel_attribution(self):
        for path in (
            ".github/workflows/pages.yml",
            "website/og/render.sh",
            "docs/motion/prompt-submit/render-recording.ts",
        ):
            with self.subTest(path=path):
                self.assertEqual(self.check_file(path, PIXEL + LICENSE), 0)

    def test_new_files_default_to_pixel_attribution(self):
        path = "scripts/other.py"
        with tempfile.TemporaryDirectory() as tmp:
            file = Path(tmp) / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("print('new')\n")
            with patch.object(CHECK, "tracked", return_value=[path]), patch.object(
                CHECK, "historical_rto_paths", return_value=set()
            ):
                self.assertEqual(CHECK.main(["--fix", tmp]), 0)
            self.assertTrue(file.read_text().startswith(PIXEL + LICENSE))

    def test_historical_rto_attribution_is_preserved(self):
        path = "crates/pixel/src/lib.rs"
        self.assertEqual(self.check_file(path, RTO + LICENSE, historical={path}), 0)

    def test_unrecognized_rto_attribution_is_rejected(self):
        path = "crates/pixel/src/new.rs"
        self.assertEqual(self.check_file(path, RTO + LICENSE), 1)

    def test_historical_rto_attribution_still_requires_the_license(self):
        path = "crates/pixel/src/lib.rs"
        self.assertEqual(self.check_file(path, RTO, historical={path}), 1)

    def test_unknown_attribution_is_rejected(self):
        path = "crates/pixel/src/lib.rs"
        self.assertEqual(
            self.check_file(path, "# SPDX-FileCopyrightText: Unknown\n" + LICENSE, historical={path}),
            1,
        )

    def test_squash_like_import_history_recognizes_legacy_and_fixes_new_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            legacy = root / "crates/pixel/src/lib.rs"
            manifest = root / "docs/imports/rto-pixel-2026-10-24.json"
            legacy.parent.mkdir(parents=True)
            manifest.parent.mkdir(parents=True)
            legacy.write_text(RUST_RTO + RUST_LICENSE + "pub fn legacy() {}\n")
            manifest.write_text("{}\n")

            def git(*args):
                return subprocess.run(
                    ["git", "-C", str(root), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                )

            git("init", "-q")
            git("config", "user.name", "Pixel test")
            git("config", "user.email", "pixel@example.invalid")
            git("add", ".")
            git("commit", "-qm", "squashed RTO import")

            new = root / "crates/pixel/src/new.rs"
            new.write_text(RUST_RTO + RUST_LICENSE + "pub fn new() {}\n")
            git("add", str(new))
            git("commit", "-qm", "add a new source file")

            self.assertEqual(CHECK.historical_rto_paths(root), {"crates/pixel/src/lib.rs"})
            output = io.StringIO()
            errors = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                self.assertEqual(CHECK.main([tmp]), 1)
            self.assertIn("crates/pixel/src/new.rs", output.getvalue())
            self.assertNotIn("crates/pixel/src/lib.rs", output.getvalue())

            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(CHECK.main(["--fix", tmp]), 0)
            self.assertTrue(new.read_text().startswith(RUST_PIXEL + RUST_LICENSE))
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(CHECK.main([tmp]), 0)

    def test_missing_import_manifest_history_fails_with_diagnostic(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "scripts/new.py"
            source.parent.mkdir(parents=True)
            source.write_text(RTO + LICENSE + "print('new')\n")
            subprocess.run(["git", "-C", tmp, "init", "-q"], check=True)
            subprocess.run(["git", "-C", tmp, "config", "user.name", "Pixel test"], check=True)
            subprocess.run(["git", "-C", tmp, "config", "user.email", "pixel@example.invalid"], check=True)
            subprocess.run(["git", "-C", tmp, "add", "."], check=True)
            subprocess.run(["git", "-C", tmp, "commit", "-qm", "without import manifest"], check=True)

            errors = io.StringIO()
            with contextlib.redirect_stderr(errors):
                self.assertEqual(CHECK.main([tmp]), 1)
            self.assertIn("cannot find the commit that introduced", errors.getvalue())
            self.assertIn(CHECK.IMPORT_MANIFEST, errors.getvalue())

    def test_preserved_files_still_require_the_license(self):
        self.assertEqual(self.check_file("website/og/render.sh", PIXEL), 1)

    def test_preserved_files_reject_unknown_attribution(self):
        self.assertEqual(self.check_file("website/og/render.sh", "# SPDX-FileCopyrightText: Unknown\n" + LICENSE), 1)

    def test_preserved_pixel_file_does_not_accept_rto_attribution(self):
        self.assertEqual(self.check_file("website/og/render.sh", RTO + LICENSE), 1)


if __name__ == "__main__":
    unittest.main()
